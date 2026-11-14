use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;
use wetube_backend::{accounts, db};
use wetube_core::{DiagnosticJob, IncomingFiles, ProbeJob, ProbeState, Role, Session};
use wetube_worker::imports::{ImportConfig, tick};

const ORIGIN: &str = "http://wetube.test";
const PASSWORD: &str = "a household test password";

#[tokio::test]
async fn conversion_publication_and_scan_selection_require_admin_csrf_and_bound_review() {
    let mut fixture = Fixture::new().await;
    let artifacts = tempfile::tempdir().unwrap();
    let scratch = artifacts.path().join("scratch");
    let media = artifacts.path().join("media");
    std::fs::create_dir(&scratch).unwrap();
    std::fs::create_dir(&media).unwrap();
    fixture.app = wetube_server::router_with_conversions(
        fixture.pool.clone(),
        ORIGIN,
        fixture._directory.path(),
        None,
        Some(wetube_backend::preview::PreviewRoot::open(&media).unwrap()),
        Some(&scratch),
    )
    .await
    .unwrap();
    let (admin, session) = fixture.login("admin").await;
    let (viewer, viewer_session) = fixture.login("viewer").await;
    let probe = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report) VALUES(?,?,'a.iso','identity',1,1,'failed',1,?,'{}')")
        .bind(&probe).bind(&session.user.id).bind(uuid::Uuid::new_v4().to_string()).execute(&fixture.pool).await.unwrap();
    let conversion = serde_json::json!({"probe_job_id":probe,"video_treatment":"preserve"});
    for path in ["/api/v1/admin/scans", "/api/v1/admin/conversions"] {
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
    let publish_path = format!("/api/v1/admin/conversions/{probe}/publish");
    let inspect_path = format!("/api/v1/admin/scans/{probe}/inspect");
    for (path,body) in [
        ("/api/v1/admin/conversions",conversion.to_string()),
        ("/api/v1/admin/scans",serde_json::json!({"name":"a.iso","identity":"id","copy_complete":true}).to_string()),
        (&inspect_path,serde_json::json!({"title":1,"angle":1}).to_string()),
        (&publish_path,serde_json::json!({"title":"Movie","year":2026,"description":"","movie_sha256":"a".repeat(64),"confirm":true}).to_string()),
    ] {
        assert_eq!(fixture.send("POST",path,&body,Some(&viewer),Some(&viewer_session.csrf_token)).await.status(),StatusCode::FORBIDDEN);
        assert_eq!(fixture.send("POST",path,&body,Some(&admin),None).await.status(),StatusCode::FORBIDDEN);
        let wrong_origin = Request::builder().method("POST").uri(path)
            .header(header::HOST,"wetube.test").header(header::ORIGIN,"http://evil.test")
            .header(header::CONTENT_TYPE,"application/json").header(header::COOKIE,&admin)
            .header("x-csrf-token",&session.csrf_token).body(Body::from(body)).unwrap();
        assert_eq!(fixture.app.clone().oneshot(wrong_origin).await.unwrap().status(),StatusCode::FORBIDDEN);
    }
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/admin/conversions",
                &conversion.to_string(),
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE probe_jobs SET state='needs_review'")
        .execute(&fixture.pool)
        .await
        .unwrap();
    let response = fixture
        .send(
            "POST",
            "/api/v1/admin/conversions",
            &conversion.to_string(),
            Some(&admin),
            Some(&session.csrf_token),
        )
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job: wetube_core::ConversionJob =
        serde_json::from_slice(&to_bytes(response.into_body(), 32768).await.unwrap()).unwrap();
    assert!(job.progress.is_none());
    let progress = wetube_core::MediaProgress {
        workflow: wetube_core::MediaWorkflow::Conversion,
        stage: 2,
        phase: "Encoding".into(),
        elapsed_seconds: 10.0,
        completed_units: Some(20.0),
        total_units: Some(80.0),
        unit: "media seconds".into(),
        updated_at: wetube_backend::now(),
    };
    sqlx::query("UPDATE conversion_jobs SET progress=? WHERE id=?")
        .bind(serde_json::to_string(&progress).unwrap())
        .bind(&job.id)
        .execute(&fixture.pool)
        .await
        .unwrap();
    let response = fixture
        .send("GET", "/api/v1/admin/conversions", "", Some(&admin), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let jobs: wetube_core::ConversionJobs =
        serde_json::from_slice(&to_bytes(response.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(jobs.jobs[0].progress.as_ref(), Some(&progress));
    assert_eq!(
        jobs.jobs[0].state,
        wetube_core::ConversionState::Queued,
        "observational telemetry cannot advance durable job state"
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/admin/conversions",
                &conversion.to_string(),
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let publish_path = format!("/api/v1/admin/conversions/{}/publish", job.id);
    let publication = serde_json::json!({"title":"Movie","year":2026,"description":"","movie_sha256":"a".repeat(64),"confirm":true});
    assert_eq!(
        fixture
            .send(
                "POST",
                &publish_path,
                &publication.to_string(),
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE conversion_jobs SET state='needs_review',movie_sha256=?")
        .bind("a".repeat(64))
        .execute(&fixture.pool)
        .await
        .unwrap();
    for (field, value) in [
        ("confirm", serde_json::json!(false)),
        ("title", serde_json::json!(" ")),
        ("year", serde_json::json!(1700)),
        ("movie_sha256", serde_json::json!("invalid")),
    ] {
        let mut bad = publication.clone();
        bad[field] = value;
        assert_eq!(
            fixture
                .send(
                    "POST",
                    &publish_path,
                    &bad.to_string(),
                    Some(&admin),
                    Some(&session.csrf_token)
                )
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let mut wrong_hash = publication.clone();
    wrong_hash["movie_sha256"] = serde_json::json!("b".repeat(64));
    assert_eq!(
        fixture
            .send(
                "POST",
                &publish_path,
                &wrong_hash.to_string(),
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                &publish_path,
                &publication.to_string(),
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                &publish_path,
                &publication.to_string(),
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM movies")
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "enqueue never synchronously publishes");
    let scan_id = uuid::Uuid::new_v4().to_string();
    let scan_attempt = uuid::Uuid::new_v4().to_string();
    let report = serde_json::json!({"titles":[{"title":4,"duration_seconds":1800.0,"chapter_count":2,"audio_count":1,"subtitle_count":0,"audio_languages":["en"],"subtitle_languages":[],"angles":[1,2],"main_feature_candidate":true}]});
    sqlx::query("INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at,attempt_id,sha256,size_bytes,report) VALUES(?,?,'a.iso','identity','needs_review',1,?,'hash',10,?)")
        .bind(&scan_id).bind(&session.user.id).bind(&scan_attempt).bind(report.to_string()).execute(&fixture.pool).await.unwrap();
    let inspect = format!("/api/v1/admin/scans/{scan_id}/inspect");
    for request in [
        serde_json::json!({"title":1,"angle":1}),
        serde_json::json!({"title":4,"angle":3}),
    ] {
        assert_eq!(
            fixture
                .send(
                    "POST",
                    &inspect,
                    &request.to_string(),
                    Some(&admin),
                    Some(&session.csrf_token)
                )
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let selected = fixture
        .send(
            "POST",
            &inspect,
            r#"{"title":4,"angle":2}"#,
            Some(&admin),
            Some(&session.csrf_token),
        )
        .await;
    assert_eq!(selected.status(), StatusCode::ACCEPTED);
    let selected: ProbeJob =
        serde_json::from_slice(&to_bytes(selected.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!((selected.title, selected.angle), (4, 2));
    assert_eq!(selected.attempt_id.as_deref(), Some(scan_attempt.as_str()));
    assert_eq!(selected.report.unwrap()["sha256"], "hash");
    sqlx::query("UPDATE probe_jobs SET state='needs_review' WHERE id=?")
        .bind(&selected.id)
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .send(
                "POST",
                &inspect,
                r#"{"title":4,"angle":2}"#,
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let other = fixture
        .send(
            "POST",
            &inspect,
            r#"{"title":4,"angle":1}"#,
            Some(&admin),
            Some(&session.csrf_token),
        )
        .await;
    assert_eq!(other.status(), StatusCode::ACCEPTED);
    let other: ProbeJob =
        serde_json::from_slice(&to_bytes(other.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(
        other.attempt_id.as_deref(),
        Some(scan_attempt.as_str()),
        "distinct selections share the verified staged ISO"
    );
}

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
    // DrvFS may finalize ctime after creation; confirmations use the refreshed identity.
    assert_eq!(listing.files[0].size_bytes, file.size_bytes);
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

#[tokio::test]
async fn admin_selection_is_staged_inspected_and_retained_in_http_history() {
    let mut fixture = Fixture::new().await;
    let incoming = fixture._directory.path().join("incoming");
    let scratch = fixture._directory.path().join("scratch");
    std::fs::create_dir(&incoming).unwrap();
    let source = incoming.join("selected.iso");
    let input = b"synthetic invalid DVD: preserve this exact selected copy";
    std::fs::write(&source, input).unwrap();
    std::fs::write(incoming.join("decoy.iso"), b"not the selected input").unwrap();
    fixture.app = wetube_server::router_with_incoming(
        fixture.pool.clone(),
        ORIGIN,
        fixture._directory.path(),
        Some(wetube_backend::incoming::IncomingRoot::open(&incoming).unwrap()),
    )
    .await
    .unwrap();
    let (cookie, session) = fixture.login("admin").await;
    let response = fixture
        .send("GET", "/api/v1/admin/incoming", "", Some(&cookie), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let listing: IncomingFiles =
        serde_json::from_slice(&to_bytes(response.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert!(listing.configured);
    assert_eq!(listing.files.len(), 2);
    let selected = listing
        .files
        .iter()
        .find(|file| file.name == "selected.iso")
        .unwrap();
    let response = fixture
        .send(
            "POST",
            "/api/v1/admin/probes",
            &serde_json::json!({
                "name": selected.name,
                "identity": selected.identity,
                "title": 7,
                "angle": 2,
                "copy_complete": true
            })
            .to_string(),
            Some(&cookie),
            Some(&session.csrf_token),
        )
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let queued: ProbeJob =
        serde_json::from_slice(&to_bytes(response.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert_eq!(queued.state, ProbeState::Queued);
    assert_eq!(queued.name, selected.name);
    assert_eq!((queued.title, queued.angle), (7, 2));
    let config = Arc::new(
        ImportConfig::open(&incoming, &scratch, 1024, 4096, Duration::from_secs(30)).unwrap(),
    );
    assert!(tick(&fixture.pool, config.clone()).await.unwrap());
    assert!(!tick(&fixture.pool, config).await.unwrap());

    fixture.app = wetube_server::router_with_incoming(
        fixture.pool.clone(),
        ORIGIN,
        fixture._directory.path(),
        Some(wetube_backend::incoming::IncomingRoot::open(&incoming).unwrap()),
    )
    .await
    .unwrap();
    let response = fixture
        .send("GET", "/api/v1/admin/probes", "", Some(&cookie), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let history: Vec<ProbeJob> =
        serde_json::from_slice(&to_bytes(response.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert_eq!(history.len(), 1);
    let result = &history[0];
    assert_eq!(result.id, queued.id);
    assert_eq!(result.name, selected.name);
    assert_eq!((result.title, result.angle), (7, 2));
    assert_eq!(result.state, ProbeState::Failed);
    assert!(
        result
            .error
            .as_deref()
            .unwrap()
            .starts_with("DVD inspection failed:"),
        "expected ffprobe to reject the staged synthetic DVD: {:?}",
        result.error
    );
    let staged = scratch
        .join(result.attempt_id.as_ref().unwrap())
        .join("input.iso");
    let staged_bytes = std::fs::read(staged).unwrap();
    assert_eq!(staged_bytes, input);
    let report = result.report.as_ref().unwrap();
    let expected_hash = format!("{:x}", Sha256::digest(input));
    assert_eq!(report["sha256"], expected_hash);
    assert_eq!(
        report["sha256"],
        format!("{:x}", Sha256::digest(&staged_bytes))
    );
    assert_eq!(report["size_bytes"], input.len() as u64);
    assert!(report.get("inspection").is_none());
    assert_eq!(std::fs::read(&source).unwrap(), input);
    assert_eq!(
        wetube_backend::incoming::IncomingRoot::open(&incoming)
            .unwrap()
            .list()
            .unwrap()
            .iter()
            .find(|file| file.name == selected.name)
            .unwrap()
            .identity,
        selected.identity
    );
    assert_eq!(
        std::fs::read(incoming.join("decoy.iso")).unwrap(),
        b"not the selected input"
    );
    let response = fixture
        .send("GET", "/api/v1/movies", "", Some(&cookie), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let library: wetube_core::Library =
        serde_json::from_slice(&to_bytes(response.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert!(library.movies.is_empty());
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
