use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as RoutePath, Query, Request, State},
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
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Semaphore;
use tokio_util::io::ReaderStream;
use tower_http::services::ServeDir;
use uuid::Uuid;
use wetube_backend::preview::PreviewRoot;
use wetube_backend::{
    accounts, comparisons, conversions, db, import_history, incoming::IncomingRoot, now,
    probe_jobs, scans,
};
use wetube_core::{
    APP_VERSION, ApiError, ArchiveImport, ConversionJob, ConversionJobs, ConversionRequest,
    DiagnosticJob, ImportPage, IncomingFiles, InspectRequest, JobState, Library, LoginRequest,
    Movie, MovieDetails, PlaybackProgress, ProbeJob, ProbeRequest, PublishRequest, Role, ScanJob,
    ScanJobs, ScanRequest, Session, SystemStatus, UpdatePlaybackProgress,
};

pub mod admission;
pub mod metadata;

const COOKIE_NAME: &str = "wetube_session";
const SESSION_SECONDS: i64 = 7 * 24 * 60 * 60;
const PLAYBACK_END_TOLERANCE_SECONDS: f64 = 0.25;

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
    origin: String,
    authority: String,
    secure_cookie: bool,
    dummy_hash: Arc<String>,
    login_window: Arc<Mutex<(Instant, usize)>>,
    password_slots: Arc<Semaphore>,
    incoming: Option<Arc<IncomingRoot>>,
    media: Option<Arc<PreviewRoot>>,
    conversion_enabled: bool,
    scratch: Option<Arc<PreviewRoot>>,
}

pub async fn router(pool: SqlitePool, origin: &str, static_dir: &Path) -> anyhow::Result<Router> {
    router_with_incoming(pool, origin, static_dir, None).await
}

pub async fn router_with_incoming(
    pool: SqlitePool,
    origin: &str,
    static_dir: &Path,
    incoming: Option<IncomingRoot>,
) -> anyhow::Result<Router> {
    router_with_media(pool, origin, static_dir, incoming, None).await
}

pub async fn router_with_media(
    pool: SqlitePool,
    origin: &str,
    static_dir: &Path,
    incoming: Option<IncomingRoot>,
    media: Option<PreviewRoot>,
) -> anyhow::Result<Router> {
    router_with_conversions(pool, origin, static_dir, incoming, media, None).await
}

pub async fn router_with_conversions(
    pool: SqlitePool,
    origin: &str,
    static_dir: &Path,
    incoming: Option<IncomingRoot>,
    media: Option<PreviewRoot>,
    scratch_dir: Option<&Path>,
) -> anyhow::Result<Router> {
    db::check_version(&pool).await?;
    let scratch = scratch_dir
        .map(|path| {
            anyhow::ensure!(
                path.is_absolute(),
                "scratch root must be an explicit absolute path"
            );
            PreviewRoot::open(path)
        })
        .transpose()?;
    if let Some(scratch) = &scratch {
        scratch.require_separate(static_dir)?;
        if let Some(incoming) = &incoming {
            scratch.require_separate(&incoming.path()?)?;
        }
        for (_, _, file) in sqlx::query_as::<_, (i64, String, String)>("PRAGMA database_list")
            .fetch_all(&pool)
            .await?
        {
            if !file.is_empty() {
                scratch.require_separate(
                    Path::new(&file)
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("invalid state database path"))?,
                )?;
            }
        }
    }
    if let Some(media) = &media {
        if let Some(scratch) = scratch_dir {
            media.require_separate(scratch)?;
        }
        media.require_separate(static_dir)?;
        let databases = sqlx::query("PRAGMA database_list").fetch_all(&pool).await?;
        for database in databases {
            let file: String = database.get("file");
            if !file.is_empty() {
                media.require_separate(
                    Path::new(&file)
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("invalid database path"))?,
                )?;
            }
        }
        if let Some(incoming) = &incoming {
            media.require_separate(&incoming.path()?)?;
        }
    }
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
        incoming: incoming.map(Arc::new),
        conversion_enabled: scratch_dir.is_some() && media.is_some(),
        media: media.map(Arc::new),
        scratch: scratch.map(Arc::new),
    };

    let api = Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/movies", get(movies))
        .route("/movies/{id}", get(movie_details))
        .route("/movies/{id}/video", get(video).head(video))
        .route(
            "/movies/{id}/progress",
            get(get_progress).put(update_progress),
        )
        .route("/status", get(status))
        .route("/jobs", get(jobs))
        .route("/jobs/diagnostics", post(queue_diagnostics))
        .route("/admin/incoming", get(incoming_files))
        .route("/admin/metadata", get(metadata::status))
        .route("/admin/metadata/search", post(metadata::search))
        .route(
            "/admin/movies/{id}/metadata/refresh",
            post(metadata::refresh),
        )
        .route("/admin/imports", get(imports))
        .route("/admin/imports/{kind}/{id}/archive", post(archive_import))
        .route("/admin/probes", get(probes).post(queue_probe))
        .route(
            "/admin/probes/{id}/comparison",
            get(comparison).post(queue_comparison),
        )
        .route(
            "/admin/comparisons/{id}/{treatment}/video",
            get(comparison_video).head(comparison_video),
        )
        .route(
            "/admin/conversions",
            get(conversion_jobs).post(queue_conversion),
        )
        .route("/admin/conversions/{id}/publish", post(queue_publication))
        .route("/admin/scans", get(scan_jobs).post(queue_scan))
        .route("/admin/scans/{id}/inspect", post(queue_scanned_inspection))
        .fallback(|| async { HttpError(StatusCode::NOT_FOUND, "Unknown API route".into()) });

    Ok(Router::new()
        .route("/health/live", get(|| async { "ok" }))
        .route("/health/ready", get(ready))
        .nest("/api/v1", api)
        .fallback_service(ServeDir::new(static_dir).append_index_html_on_directories(true))
        .layer(DefaultBodyLimit::max(32 * 1024))
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
        "SELECT m.id,COALESCE(t.title,m.title) title,
         CASE WHEN t.title IS NOT NULL THEN t.year ELSE m.year END year,
         COALESCE(t.description,m.description) description,
         s.tmdb_id,s.expires_at metadata_expires_at
         FROM movies m LEFT JOIN tmdb_metadata s ON s.movie_id=m.id
         LEFT JOIN tmdb_metadata t ON t.movie_id=m.id AND t.expires_at>?
         WHERE m.status = 'ready'
         ORDER BY title COLLATE NOCASE",
    )
    .bind(now())
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
                tmdb_id: row.get("tmdb_id"),
                metadata_expires_at: row.get("metadata_expires_at"),
            })
            .collect(),
    }))
}

struct ReadyPreview {
    details: MovieDetails,
    package: String,
    size: u64,
    file_identity: String,
}

async fn preview(state: &AppState, id: &str) -> Result<ReadyPreview, HttpError> {
    let row = sqlx::query(
        "SELECT m.id,COALESCE(t.title,m.title) title,
         CASE WHEN t.title IS NOT NULL THEN t.year ELSE m.year END year,
         COALESCE(t.description,m.description) description,s.tmdb_id,s.expires_at metadata_expires_at,
         p.package_id,p.size_bytes,p.duration_seconds,p.file_identity
         FROM movies m JOIN preview_media p ON p.movie_id=m.id
         LEFT JOIN tmdb_metadata s ON s.movie_id=m.id
         LEFT JOIN tmdb_metadata t ON t.movie_id=m.id AND t.expires_at>?
         WHERE m.id=? AND m.status='ready'",
    ).bind(now()).bind(id).fetch_optional(&state.pool).await.map_err(internal)?
        .ok_or_else(|| HttpError(StatusCode::NOT_FOUND, "Movie is not available for local preview".into()))?;
    Ok(ReadyPreview {
        details: MovieDetails {
            movie: Movie {
                id: row.get("id"),
                title: row.get("title"),
                year: row.get("year"),
                description: row.get("description"),
                tmdb_id: row.get("tmdb_id"),
                metadata_expires_at: row.get("metadata_expires_at"),
            },
            duration_seconds: row.get("duration_seconds"),
        },
        package: row.get("package_id"),
        size: row.get::<i64, _>("size_bytes") as u64,
        file_identity: row.get("file_identity"),
    })
}

async fn movie_details(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<MovieDetails>, HttpError> {
    session(&state, &headers).await?;
    Ok(Json(preview(&state, &id).await?.details))
}

async fn get_progress(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<PlaybackProgress>, HttpError> {
    let current = session(&state, &headers).await?;
    preview(&state, &id).await?;
    let position_seconds = sqlx::query_scalar(
        "SELECT position_seconds FROM playback_progress WHERE user_id=? AND movie_id=?",
    )
    .bind(current.user.id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(internal)?
    .unwrap_or(0.0);
    Ok(Json(PlaybackProgress { position_seconds }))
}

async fn update_progress(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Json(request): Json<UpdatePlaybackProgress>,
) -> Result<Json<PlaybackProgress>, HttpError> {
    let current = session(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    let movie = preview(&state, &id).await?;
    if !request.position_seconds.is_finite()
        || request.position_seconds < 0.0
        || request.position_seconds
            > movie.details.duration_seconds + PLAYBACK_END_TOLERANCE_SECONDS
    {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "Position must be within the movie duration".into(),
        ));
    }
    let position_seconds = request.position_seconds.min(movie.details.duration_seconds);
    let result = sqlx::query(
        "INSERT INTO playback_progress (user_id,movie_id,position_seconds,updated_at)
         SELECT ?,id,?,? FROM movies WHERE id=? AND status='ready'
         ON CONFLICT(user_id,movie_id) DO UPDATE SET position_seconds=excluded.position_seconds,updated_at=excluded.updated_at",
    ).bind(current.user.id).bind(position_seconds).bind(now()).bind(id)
        .execute(&state.pool).await.map_err(internal)?;
    if result.rows_affected() != 1 {
        return Err(HttpError(
            StatusCode::NOT_FOUND,
            "Movie is not available".into(),
        ));
    }
    Ok(Json(PlaybackProgress { position_seconds }))
}

// Reject malformed or multiple ranges rather than attempting unbounded multipart responses.
fn byte_range(headers: &HeaderMap, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let mut values = headers.get_all(header::RANGE).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let text = value.to_str().map_err(|_| ())?;
    if text.len() > 128 {
        return Err(());
    }
    let text = text.strip_prefix("bytes=").ok_or(())?;
    let (start, end) = text.split_once('-').ok_or(())?;
    let number = |text: &str| -> Result<u64, ()> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(());
        }
        text.parse().map_err(|_| ())
    };
    if size == 0 {
        return Err(());
    }
    let (start, end) = if start.is_empty() {
        let count = number(end)?;
        if count == 0 {
            return Err(());
        }
        (size.saturating_sub(count), size - 1)
    } else {
        let start = number(start)?;
        let end = if end.is_empty() {
            size - 1
        } else {
            number(end)?.min(size - 1)
        };
        if start >= size || end < start {
            return Err(());
        }
        (start, end)
    };
    Ok(Some((start, end)))
}

async fn video(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    session(&state, &headers).await?;
    let movie = preview(&state, &id).await?;
    let root = state.media.ok_or_else(|| {
        HttpError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Local preview media is not configured".into(),
        )
    })?;
    let file = root.open_movie(&movie.package).map_err(|error| {
        tracing::warn!(%error, "preview file unavailable");
        HttpError(StatusCode::NOT_FOUND, "Movie media is unavailable".into())
    })?;
    if wetube_backend::incoming::identity(&file.metadata().map_err(internal)?)
        != movie.file_identity
    {
        return Err(HttpError(
            StatusCode::NOT_FOUND,
            "Movie media has changed; re-admission is required".into(),
        ));
    }
    serve_video(file, movie.size, method, headers).await
}

async fn serve_video(
    file: std::fs::File,
    size: u64,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let range = match byte_range(&headers, size) {
        Ok(range) => range,
        Err(()) => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_LENGTH, "0")
                .body(Body::empty())
                .map_err(internal);
        }
    };
    let (start, end) = range.unwrap_or((0, size - 1));
    let length = end - start + 1;
    let mut response = Response::builder()
        .status(if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, length.to_string());
    if range.is_some() {
        response = response.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        let mut file = tokio::fs::File::from_std(file);
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(internal)?;
        Body::from_stream(ReaderStream::with_capacity(file.take(length), 64 * 1024))
    };
    response.body(body).map_err(internal)
}

async fn comparison(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<Option<wetube_core::ComparisonJob>>, HttpError> {
    admin(&state, &headers).await?;
    Ok(Json(
        comparisons::latest(&state.pool, &id)
            .await
            .map_err(internal)?,
    ))
}

async fn queue_comparison(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<wetube_core::ComparisonJob>), HttpError> {
    let current = admin(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    if state.scratch.is_none() {
        return Err(HttpError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Private scratch is not configured".into(),
        ));
    }
    if Uuid::parse_str(&id).is_err() {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "Invalid probe ID".into(),
        ));
    }
    let job = comparisons::enqueue(&state.pool, &current.user.id, &id).await.map_err(internal)?
        .ok_or_else(|| HttpError(StatusCode::CONFLICT,
            "Comparison already exists, queue is full, or selected inspection is not ready. Refresh its comparison.".into()))?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn comparison_video(
    State(state): State<AppState>,
    RoutePath((id, treatment)): RoutePath<(String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    admin(&state, &headers).await?;
    let unavailable = || {
        HttpError(
            StatusCode::NOT_FOUND,
            "Comparison is unavailable or stale".into(),
        )
    };
    if !matches!(treatment.as_str(), "preserve" | "yadif") {
        return Err(unavailable());
    }
    let row = sqlx::query(
        "SELECT c.* FROM comparison_jobs c JOIN probe_jobs p ON p.id=c.probe_job_id
        WHERE c.id=? AND c.state='succeeded' AND p.state='needs_review'
        AND c.probe_report=p.report AND c.title=p.title AND c.angle=p.angle",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(internal)?
    .ok_or_else(unavailable)?;
    let job = comparisons::from_row(&row).map_err(internal)?;
    let report = job.report.as_ref().ok_or_else(unavailable)?;
    if report["version"] != 1
        || report["status"] != "succeeded"
        || !report["preserve"]["filter"].is_null()
        || report["yadif"]["filter"] != wetube_core::YADIF_FILTER
    {
        return Err(unavailable());
    }
    let clip = &report[&treatment];
    let size = clip["size_bytes"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(unavailable)?;
    let root = state.scratch.as_ref().ok_or_else(unavailable)?;
    let file = root
        .open_comparison(
            job.attempt_id.as_deref().ok_or_else(unavailable)?,
            &treatment,
        )
        .map_err(|_| unavailable())?;
    let metadata = file.metadata().map_err(internal)?;
    if metadata.len() != size
        || Some(wetube_backend::incoming::identity(&metadata).as_str())
            != clip["file_identity"].as_str()
    {
        return Err(unavailable());
    }
    serve_video(file, size, method, headers).await
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
        conversion_enabled: state.conversion_enabled,
        iso_deletion_enabled: false,
    }))
}

async fn conversion_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ConversionJobs>, HttpError> {
    admin(&state, &headers).await?;
    Ok(Json(ConversionJobs {
        jobs: conversions::list(&state.pool).await.map_err(internal)?,
    }))
}

async fn scan_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ScanJobs>, HttpError> {
    admin(&state, &headers).await?;
    Ok(Json(ScanJobs {
        jobs: scans::list(&state.pool).await.map_err(internal)?,
    }))
}

async fn queue_scan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ScanRequest>,
) -> Result<(StatusCode, Json<ScanJob>), HttpError> {
    let current = admin(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    if !request.copy_complete {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "Confirm the ISO copy has finished".into(),
        ));
    }
    wetube_backend::incoming::validate_name(&request.name)
        .map_err(|error| HttpError(StatusCode::BAD_REQUEST, error.to_string()))?;
    let root = state.incoming.clone().ok_or_else(|| {
        HttpError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Incoming root is not configured".into(),
        )
    })?;
    let name = request.name.clone();
    let actual =
        tokio::task::spawn_blocking(move || root.open_file(&name).map(|(_, entry)| entry.identity))
            .await
            .map_err(internal)?
            .map_err(|_| {
                HttpError(
                    StatusCode::CONFLICT,
                    "Input unavailable; refresh incoming list".into(),
                )
            })?;
    if actual != request.identity {
        return Err(HttpError(
            StatusCode::CONFLICT,
            "Input changed; refresh and reconfirm completion".into(),
        ));
    }
    let job = scans::enqueue(&state.pool, &current.user.id, &request)
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            HttpError(
                StatusCode::CONFLICT,
                "A DVD scan is already queued or running".into(),
            )
        })?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn queue_scanned_inspection(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Json(request): Json<InspectRequest>,
) -> Result<(StatusCode, Json<ProbeJob>), HttpError> {
    let current = admin(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    if Uuid::parse_str(&id).is_err()
        || !(1..=99).contains(&request.title)
        || !(1..=9).contains(&request.angle)
    {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "Select a discovered title and angle".into(),
        ));
    }
    let job = scans::inspect(&state.pool, &current.user.id, &id, &request)
        .await
        .map_err(|error| HttpError(StatusCode::BAD_REQUEST, error.to_string()))?
        .ok_or_else(|| {
            HttpError(
                StatusCode::CONFLICT,
                "Select a successful scan; another inspection may already be active".into(),
            )
        })?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

fn conversions_enabled(state: &AppState) -> Result<(), HttpError> {
    if !state.conversion_enabled {
        return Err(HttpError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Conversion requires configured scratch and local media roots".into(),
        ));
    }
    Ok(())
}

async fn queue_conversion(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ConversionRequest>,
) -> Result<(StatusCode, Json<ConversionJob>), HttpError> {
    let current = admin(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    conversions_enabled(&state)?;
    if Uuid::parse_str(&request.probe_job_id).is_err() {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "Invalid probe job ID".into(),
        ));
    }
    let job = conversions::enqueue(&state.pool, &current.user.id, &request)
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            HttpError(
                StatusCode::CONFLICT,
                "Select a completed reviewed probe without an existing conversion".into(),
            )
        })?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn queue_publication(
    State(state): State<AppState>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Json(request): Json<PublishRequest>,
) -> Result<(StatusCode, Json<ConversionJob>), HttpError> {
    let current = admin(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    conversions_enabled(&state)?;
    if !request.confirm
        || request.title.trim().is_empty()
        || request.title.len() > 512
        || request.description.len() > 16384
        || request
            .year
            .is_some_and(|year| !(1888..=2100).contains(&year))
        || request.movie_sha256.len() != 64
        || !request
            .movie_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || Uuid::parse_str(&id).is_err()
    {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "Confirm reviewed output, hash, and valid movie metadata".into(),
        ));
    }
    let invalid_selection = || {
        HttpError(
            StatusCode::CONFLICT,
            wetube_core::TMDB_SELECTION_EXPIRED.into(),
        )
    };
    if let Some(source) = &request.tmdb {
        wetube_backend::metadata::validate_selection(source, now())
            .map_err(|_| invalid_selection())?;
    }
    let job = conversions::publish(&state.pool,&id,&request).await.map_err(|error| {
        // The authoritative backend check can cross the 24-hour boundary
        // after the HTTP precheck; that is still an actionable conflict.
        if request.tmdb.as_ref().is_some_and(|source|
            wetube_backend::metadata::validate_selection(source, now()).is_err()) {
            invalid_selection()
        } else {
            internal(error)
        }
    })?
        .ok_or_else(||HttpError(StatusCode::CONFLICT,"Publication requires an unpublished successful conversion and its exact validated hash".into()))?;
    Ok((StatusCode::ACCEPTED, Json(job)))
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
            finished_at: None,
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
        "SELECT id, state, created_at, finished_at, report, error FROM diagnostic_jobs ORDER BY created_at DESC, rowid DESC LIMIT 20",
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
            finished_at: row.get("finished_at"),
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

async fn incoming_files(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<IncomingFiles>, HttpError> {
    admin(&state, &headers).await?;
    let Some(root) = state.incoming else {
        return Ok(Json(IncomingFiles {
            configured: false,
            files: Vec::new(),
        }));
    };
    let files = tokio::task::spawn_blocking(move || root.list())
        .await
        .map_err(internal)?
        .map_err(internal)?;
    Ok(Json(IncomingFiles {
        configured: true,
        files,
    }))
}

async fn probes(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ProbeJob>>, HttpError> {
    admin(&state, &headers).await?;
    Ok(Json(probe_jobs::list(&state.pool).await.map_err(internal)?))
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportQuery {
    #[serde(default)]
    history: bool,
    before: Option<String>,
    limit: Option<u32>,
}

async fn imports(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ImportQuery>,
) -> Result<Json<ImportPage>, HttpError> {
    let current = admin(&state, &headers).await?;
    let limit = query.limit.unwrap_or(10);
    if !(1..=50).contains(&limit)
        || query
            .before
            .as_deref()
            .is_some_and(|cursor| !import_history::valid_cursor(cursor))
    {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "Use limit 1–50 and a cursor returned by the previous page".into(),
        ));
    }
    Ok(Json(
        import_history::page(
            &state.pool,
            &current.user.id,
            query.history,
            query.before.as_deref(),
            limit,
        )
        .await
        .map_err(internal)?,
    ))
}

async fn archive_import(
    State(state): State<AppState>,
    RoutePath((kind, id)): RoutePath<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<ArchiveImport>,
) -> Result<Json<ArchiveImport>, HttpError> {
    let current = admin(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    if !matches!(kind.as_str(), "scan" | "probe" | "conversion") || Uuid::parse_str(&id).is_err() {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "Unknown import kind or invalid job ID".into(),
        ));
    }
    if !import_history::set_archived(&state.pool, &current.user.id, &kind, &id, request.archived)
        .await
        .map_err(internal)?
    {
        return Err(HttpError(
            StatusCode::CONFLICT,
            "Job is missing, active, or still awaiting a decision. Refresh its status.".into(),
        ));
    }
    Ok(Json(request))
}

async fn queue_probe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ProbeRequest>,
) -> Result<(StatusCode, Json<ProbeJob>), HttpError> {
    let current = admin(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    if !request.copy_complete
        || !(1..=99).contains(&request.title)
        || !(1..=9).contains(&request.angle)
    {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "Confirm copy completion and select a valid title/angle".into(),
        ));
    }
    wetube_backend::incoming::validate_name(&request.name)
        .map_err(|error| HttpError(StatusCode::BAD_REQUEST, error.to_string()))?;
    let root = state.incoming.clone().ok_or_else(|| {
        HttpError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Incoming directory is not configured".into(),
        )
    })?;
    let name = request.name.clone();
    let identity =
        tokio::task::spawn_blocking(move || root.open_file(&name).map(|(_, file)| file.identity))
            .await
            .map_err(internal)?
            .map_err(|_| {
                HttpError(
                    StatusCode::CONFLICT,
                    "Input is unavailable; refresh the incoming list".into(),
                )
            })?;
    if identity != request.identity {
        return Err(HttpError(
            StatusCode::CONFLICT,
            "Input changed; refresh and reconfirm completion".into(),
        ));
    }
    let job = probe_jobs::enqueue(&state.pool, &current.user.id, &request)
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            HttpError(
                StatusCode::CONFLICT,
                "A DVD inspection is already queued or running".into(),
            )
        })?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}
