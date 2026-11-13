use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use sqlx::{Row, SqlitePool};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use tower_http::services::ServeDir;
use uuid::Uuid;
use wetube_backend::{accounts, db, now};
use wetube_core::{
    APP_VERSION, ApiError, DiagnosticJob, JobState, Library, LoginRequest, Movie, Role, Session,
    SystemStatus,
};

const COOKIE_NAME: &str = "wetube_session";
const SESSION_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
    origin: String,
    authority: String,
    secure_cookie: bool,
    dummy_hash: Arc<String>,
    login_window: Arc<Mutex<(Instant, usize)>>,
    password_slots: Arc<Semaphore>,
}

pub async fn router(pool: SqlitePool, origin: &str, static_dir: &Path) -> anyhow::Result<Router> {
    db::check_version(&pool).await?;
    let url = url::Url::parse(origin)?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "origin must be an http(s) origin without a path, credentials, query, or fragment"
    );
    let origin = url.origin().ascii_serialization();
    let authority = origin.split_once("://").unwrap().1.to_owned();
    let dummy_hash =
        tokio::task::spawn_blocking(|| accounts::hash_password(&accounts::random_token()))
            .await??;
    let state = AppState {
        pool,
        origin,
        authority,
        secure_cookie: url.scheme() == "https",
        dummy_hash: Arc::new(dummy_hash),
        login_window: Arc::new(Mutex::new((Instant::now(), 0))),
        password_slots: Arc::new(Semaphore::new(2)),
    };

    let api = Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/movies", get(movies))
        .route("/status", get(status))
        .route("/jobs", get(jobs))
        .route("/jobs/diagnostics", post(queue_diagnostics))
        .fallback(|| async { HttpError(StatusCode::NOT_FOUND, "Unknown API route".into()) });

    Ok(Router::new()
        .route("/health/live", get(|| async { "ok" }))
        .route("/health/ready", get(ready))
        .nest("/api/v1", api)
        .fallback_service(ServeDir::new(static_dir).append_index_html_on_directories(true))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), boundary))
        .with_state(state))
}

struct HttpError(StatusCode, String);

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

fn internal(error: impl std::fmt::Display) -> HttpError {
    tracing::error!(%error, "request failed");
    HttpError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "An internal error occurred".into(),
    )
}

fn unauthorized() -> HttpError {
    HttpError(StatusCode::UNAUTHORIZED, "Sign in to continue".into())
}

async fn boundary(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, HttpError> {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    if !host.is_some_and(|host| host.eq_ignore_ascii_case(&state.authority)) {
        return Err(HttpError(
            StatusCode::MISDIRECTED_REQUEST,
            "Unrecognized host".into(),
        ));
    }
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        != Some(state.origin.as_str())
    {
        return Err(HttpError(
            StatusCode::FORBIDDEN,
            "Requests must come from the configured WeTube origin".into(),
        ));
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert("x-frame-options", HeaderValue::from_static("DENY"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("same-origin"));
    Ok(response)
}

fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    let mut tokens = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|cookie| cookie.trim().split_once('='))
        .filter_map(|(name, token)| (name == COOKIE_NAME).then_some(token));
    let token = tokens.next()?;
    if tokens.next().is_some()
        || token.len() != 64
        || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(token)
}

async fn session(state: &AppState, headers: &HeaderMap) -> Result<Session, HttpError> {
    let token = session_cookie(headers).ok_or_else(unauthorized)?;
    let row = sqlx::query(
        "SELECT u.id, u.username, u.role, s.csrf_token FROM sessions s
         JOIN users u ON u.id = s.user_id WHERE s.token_hash = ? AND s.expires_at > ?",
    )
    .bind(accounts::token_hash(token))
    .bind(now())
    .fetch_optional(&state.pool)
    .await
    .map_err(internal)?
    .ok_or_else(unauthorized)?;
    Ok(Session {
        user: accounts::user_from_row(&row).map_err(internal)?,
        csrf_token: row.get("csrf_token"),
    })
}

async fn admin(state: &AppState, headers: &HeaderMap) -> Result<Session, HttpError> {
    let session = session(state, headers).await?;
    if session.user.role != Role::Admin {
        return Err(HttpError(
            StatusCode::FORBIDDEN,
            "Administrator access required".into(),
        ));
    }
    Ok(session)
}

fn check_csrf(session: &Session, headers: &HeaderMap) -> Result<(), HttpError> {
    let supplied = headers
        .get("x-csrf-token")
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    if !bool::from(session.csrf_token.as_bytes().ct_eq(supplied)) {
        return Err(HttpError(
            StatusCode::FORBIDDEN,
            "Invalid CSRF token".into(),
        ));
    }
    Ok(())
}

async fn login(
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> Result<Response, HttpError> {
    {
        let mut window = state.login_window.lock().map_err(internal)?;
        if window.0.elapsed() >= Duration::from_secs(60) {
            *window = (Instant::now(), 0);
        }
        if window.1 >= 10 {
            return Err(HttpError(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many sign-in attempts; try again in a minute".into(),
            ));
        }
        window.1 += 1;
    }
    if request.password.len() > 1024 || request.username.len() > 256 {
        return Err(unauthorized());
    }
    let username = wetube_core::normalize_username(&request.username).unwrap_or_default();
    let row = sqlx::query("SELECT id, username, role, password_hash FROM users WHERE username = ?")
        .bind(username)
        .fetch_optional(&state.pool)
        .await
        .map_err(internal)?;
    let hash: String = row
        .as_ref()
        .map(|row| row.get("password_hash"))
        .unwrap_or_else(|| state.dummy_hash.as_ref().clone());
    let permit = state
        .password_slots
        .clone()
        .acquire_owned()
        .await
        .map_err(internal)?;
    let verification_hash = hash.clone();
    let verified = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        accounts::verify_password(&request.password, &verification_hash)
    })
    .await
    .map_err(internal)?;
    let row = row.filter(|_| verified).ok_or_else(unauthorized)?;
    let user = accounts::user_from_row(&row).map_err(internal)?;
    let token = accounts::random_token();
    let csrf_token = accounts::random_token();
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now())
        .execute(&state.pool)
        .await
        .map_err(internal)?;
    let inserted = sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, csrf_token, expires_at)
         SELECT ?, id, ?, ? FROM users WHERE id = ? AND password_hash = ?",
    )
    .bind(accounts::token_hash(&token))
    .bind(&csrf_token)
    .bind(now() + SESSION_SECONDS)
    .bind(&user.id)
    .bind(hash)
    .execute(&state.pool)
    .await
    .map_err(internal)?;
    if inserted.rows_affected() != 1 {
        return Err(unauthorized());
    }
    let secure = if state.secure_cookie { "; Secure" } else { "" };
    let cookie = format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_SECONDS}{secure}"
    );
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(Session { user, csrf_token }),
    )
        .into_response())
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, HttpError> {
    let current = session(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(accounts::token_hash(session_cookie(&headers).unwrap()))
        .execute(&state.pool)
        .await
        .map_err(internal)?;
    let secure = if state.secure_cookie { "; Secure" } else { "" };
    Ok((
        [(
            header::SET_COOKIE,
            format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}"),
        )],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Session>, HttpError> {
    Ok(Json(session(&state, &headers).await?))
}

async fn movies(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Library>, HttpError> {
    session(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id, title, year, description FROM movies WHERE status = 'ready'
         ORDER BY title COLLATE NOCASE",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;
    Ok(Json(Library {
        movies: rows
            .into_iter()
            .map(|row| Movie {
                id: row.get("id"),
                title: row.get("title"),
                year: row.get("year"),
                description: row.get("description"),
            })
            .collect(),
    }))
}

async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SystemStatus>, HttpError> {
    session(&state, &headers).await?;
    let worker_last_seen =
        sqlx::query_scalar("SELECT last_seen FROM worker_status WHERE singleton = 1")
            .fetch_optional(&state.pool)
            .await
            .map_err(internal)?;
    Ok(Json(SystemStatus {
        version: APP_VERSION.into(),
        worker_last_seen,
        conversion_enabled: false,
        iso_deletion_enabled: false,
    }))
}

async fn queue_diagnostics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<DiagnosticJob>), HttpError> {
    let current = admin(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    let id = Uuid::new_v4().to_string();
    let created_at = now();
    let inserted = sqlx::query(
        "INSERT INTO diagnostic_jobs (id, requested_by, state, created_at)
         SELECT ?, ?, 'queued', ? WHERE NOT EXISTS
         (SELECT 1 FROM diagnostic_jobs WHERE state IN ('queued', 'running'))",
    )
    .bind(&id)
    .bind(&current.user.id)
    .bind(created_at)
    .execute(&state.pool)
    .await
    .map_err(internal)?;
    if inserted.rows_affected() != 1 {
        return Err(HttpError(
            StatusCode::CONFLICT,
            "A diagnostic run is already queued or running".into(),
        ));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(DiagnosticJob {
            id,
            state: JobState::Queued,
            created_at,
            report: None,
            error: None,
        }),
    ))
}

async fn jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<DiagnosticJob>>, HttpError> {
    admin(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id, state, created_at, report, error FROM diagnostic_jobs ORDER BY created_at DESC, rowid DESC LIMIT 20",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(internal)?;
    let mut jobs = Vec::with_capacity(rows.len());
    for row in rows {
        let job_state = match row.get::<&str, _>("state") {
            "queued" => JobState::Queued,
            "running" => JobState::Running,
            "succeeded" => JobState::Succeeded,
            "failed" => JobState::Failed,
            _ => return Err(internal("invalid diagnostic state")),
        };
        let report = row
            .get::<Option<String>, _>("report")
            .map(|report| serde_json::from_str(&report))
            .transpose()
            .map_err(internal)?;
        jobs.push(DiagnosticJob {
            id: row.get("id"),
            state: job_state,
            created_at: row.get("created_at"),
            report,
            error: row.get("error"),
        });
    }
    Ok(Json(jobs))
}

async fn ready(State(state): State<AppState>) -> Result<&'static str, HttpError> {
    db::check_version(&state.pool).await.map_err(internal)?;
    Ok("ready")
}
