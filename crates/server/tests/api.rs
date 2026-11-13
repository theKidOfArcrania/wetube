use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use sqlx::SqlitePool;
use tower::ServiceExt;
use wetube_backend::{accounts, db};
use wetube_core::{DiagnosticJob, IncomingFiles, ProbeJob, Role, Session};

const ORIGIN: &str = "http://wetube.test";
const PASSWORD: &str = "a household test password";

struct Fixture {
    _directory: tempfile::TempDir,
    pool: SqlitePool,
    app: Router,
}

#[tokio::test]
async fn incoming_probes_require_admin_confirmation_and_exclude_concurrent_jobs() {
    let mut fixture = Fixture::new().await;
    let (cookie, session) = fixture.login("admin").await;
    let disabled = fixture
        .send("GET", "/api/v1/admin/incoming", "", Some(&cookie), None)
        .await;
    let disabled: IncomingFiles =
        serde_json::from_slice(&to_bytes(disabled.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert!(!disabled.configured);
    let incoming = tempfile::tempdir().unwrap();
    std::fs::write(incoming.path().join("movie.iso"), b"a completed copy").unwrap();
    let root = wetube_backend::incoming::IncomingRoot::open(incoming.path()).unwrap();
    let file = root.list().unwrap().remove(0);
    let mut request = serde_json::json!({
        "name": file.name, "identity": file.identity, "title": 1, "angle": 1, "copy_complete": true
    });
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/admin/probes",
                &request.to_string(),
                Some(&cookie),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    fixture.app = wetube_server::router_with_incoming(
        fixture.pool.clone(),
        ORIGIN,
        fixture._directory.path(),
        Some(root),
    )
    .await
    .unwrap();
    let (viewer, viewer_session) = fixture.login("viewer").await;
    for path in ["/api/v1/admin/incoming", "/api/v1/admin/probes"] {
        assert_eq!(
            fixture.send("GET", path, "", None, None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            fixture
                .send("GET", path, "", Some(&viewer), None)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let listing = fixture
        .send("GET", "/api/v1/admin/incoming", "", Some(&cookie), None)
        .await;
    let listing: IncomingFiles =
        serde_json::from_slice(&to_bytes(listing.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert!(listing.configured);
    assert_eq!(listing.files.len(), 1);
    assert_eq!(listing.files[0].name, file.name);
    assert_eq!(listing.files[0].identity, file.identity);
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/admin/probes",
                &request.to_string(),
                Some(&viewer),
                Some(&viewer_session.csrf_token)
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/admin/probes",
                &request.to_string(),
                Some(&cookie),
                None
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    for (field, invalid) in [
        ("copy_complete", serde_json::json!(false)),
        ("title", serde_json::json!(0)),
        ("angle", serde_json::json!(10)),
        ("name", serde_json::json!("../movie.iso")),
    ] {
        let mut bad = request.clone();
        bad[field] = invalid;
        assert_eq!(
            fixture
                .send(
                    "POST",
                    "/api/v1/admin/probes",
                    &bad.to_string(),
                    Some(&cookie),
                    Some(&session.csrf_token)
                )
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    request["identity"] = serde_json::json!("stale identity");
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/admin/probes",
                &request.to_string(),
                Some(&cookie),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    request["identity"] = serde_json::json!(listing.files[0].identity);
    let body = request.to_string();
    let (first, second) = tokio::join!(
        fixture.send(
            "POST",
            "/api/v1/admin/probes",
            &body,
            Some(&cookie),
            Some(&session.csrf_token)
        ),
        fixture.send(
            "POST",
            "/api/v1/admin/probes",
            &body,
            Some(&cookie),
            Some(&session.csrf_token)
        ),
    );
    let mut statuses = [first.status(), second.status()];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::ACCEPTED, StatusCode::CONFLICT]);
    let history = fixture
        .send("GET", "/api/v1/admin/probes", "", Some(&cookie), None)
        .await;
    let history: Vec<ProbeJob> =
        serde_json::from_slice(&to_bytes(history.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].name, "movie.iso");
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        db::initialize(directory.path()).await.unwrap();
        let pool = db::connect(directory.path()).await.unwrap();
        for (username, role) in [("admin", Role::Admin), ("viewer", Role::Viewer)] {
            accounts::create_user(&pool, username, PASSWORD.into(), role)
                .await
                .unwrap();
        }
        let app = wetube_server::router(pool.clone(), ORIGIN, directory.path())
            .await
            .unwrap();
        Self {
            _directory: directory,
            pool,
            app,
        }
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        body: &str,
        cookie: Option<&str>,
        csrf: Option<&str>,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "wetube.test")
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(csrf) = csrf {
            request = request.header("x-csrf-token", csrf);
        }
        self.app
            .clone()
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    async fn login(&self, username: &str) -> (String, Session) {
        let response = self
            .send(
                "POST",
                "/api/v1/auth/login",
                &serde_json::json!({"username": username, "password": PASSWORD}).to_string(),
                None,
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Lax"));
        assert!(!set_cookie.contains("Domain="));
        let cookie = set_cookie.split(';').next().unwrap().to_owned();
        let session =
            serde_json::from_slice(&to_bytes(response.into_body(), 16 * 1024).await.unwrap())
                .unwrap();
        (cookie, session)
    }
}

#[tokio::test]
async fn host_and_origin_checks_apply_before_login() {
    let fixture = Fixture::new().await;
    for (host, origin, expected) in [
        ("evil.test", ORIGIN, StatusCode::MISDIRECTED_REQUEST),
        ("wetube.test", "http://evil.test", StatusCode::FORBIDDEN),
        ("wetube.test", "null", StatusCode::FORBIDDEN),
    ] {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/login")
            .header(header::HOST, host)
            .header(header::ORIGIN, origin)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(
            fixture.app.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
    assert_eq!(
        fixture
            .send("GET", "/api/v1/movies", "", None, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn sessions_are_hashed_expire_and_require_csrf_for_logout() {
    let fixture = Fixture::new().await;
    let (cookie, session) = fixture.login("ADMIN").await;
    let stored: String = sqlx::query_scalar("SELECT token_hash FROM sessions")
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    let raw_token = cookie.split_once('=').unwrap().1;
    assert_ne!(stored, raw_token);
    assert_eq!(stored, accounts::token_hash(raw_token));
    assert_eq!(
        fixture
            .send("GET", "/api/v1/movies", "", Some(&cookie), None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/auth/logout",
                "",
                Some(&cookie),
                Some("bad")
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/auth/logout",
                "",
                Some(&cookie),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        fixture
            .send("GET", "/api/v1/auth/me", "", Some(&cookie), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let (cookie, _) = fixture.login("admin").await;
    sqlx::query("UPDATE sessions SET expires_at = 0")
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .send("GET", "/api/v1/auth/me", "", Some(&cookie), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn diagnostic_jobs_require_admin_csrf_and_survive_router_recreation() {
    let fixture = Fixture::new().await;
    let (viewer, viewer_session) = fixture.login("viewer").await;
    assert_eq!(
        fixture
            .send("GET", "/api/v1/jobs", "", Some(&viewer), None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/jobs/diagnostics",
                "",
                Some(&viewer),
                Some(&viewer_session.csrf_token)
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let (cookie, session) = fixture.login("admin").await;
    assert_eq!(
        fixture
            .send("POST", "/api/v1/jobs/diagnostics", "", Some(&cookie), None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = fixture
        .send(
            "POST",
            "/api/v1/jobs/diagnostics",
            "",
            Some(&cookie),
            Some(&session.csrf_token),
        )
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job: DiagnosticJob =
        serde_json::from_slice(&to_bytes(response.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/jobs/diagnostics",
                "",
                Some(&cookie),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let app = wetube_server::router(fixture.pool.clone(), ORIGIN, fixture._directory.path())
        .await
        .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/jobs")
                .header(header::HOST, "wetube.test")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let jobs: Vec<DiagnosticJob> =
        serde_json::from_slice(&to_bytes(response.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, job.id);
}

#[tokio::test]
async fn login_rate_limit_also_counts_invalid_payloads() {
    let fixture = Fixture::new().await;
    let payload =
        serde_json::json!({"username": "admin", "password": "x".repeat(1025)}).to_string();
    for _ in 0..10 {
        assert_eq!(
            fixture
                .send("POST", "/api/v1/auth/login", &payload, None, None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        fixture
            .send("POST", "/api/v1/auth/login", &payload, None, None)
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn library_hides_unprepared_and_trashed_movies() {
    let fixture = Fixture::new().await;
    for (id, status) in [
        ("ready", "ready"),
        ("missing", "needs_reimport"),
        ("trash", "trashed"),
    ] {
        sqlx::query("INSERT INTO movies (id, title, status) VALUES (?, ?, ?)")
            .bind(id)
            .bind(id)
            .bind(status)
            .execute(&fixture.pool)
            .await
            .unwrap();
    }
    let (cookie, _) = fixture.login("viewer").await;
    let response = fixture
        .send("GET", "/api/v1/movies", "", Some(&cookie), None)
        .await;
    let library: wetube_core::Library =
        serde_json::from_slice(&to_bytes(response.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert_eq!(library.movies.len(), 1);
    assert_eq!(library.movies[0].id, "ready");
}
