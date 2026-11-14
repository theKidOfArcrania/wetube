use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::{path::Path, time::Duration};
use tower::ServiceExt;
use wetube_backend::{accounts, db, preview::PreviewRoot};
use wetube_core::Role;
use wetube_server::admission::{Admission, admit};

const ORIGIN: &str = "http://preview.test";

struct Fixture {
    _root: tempfile::TempDir,
    state: std::path::PathBuf,
    media: std::path::PathBuf,
    attempt: std::path::PathBuf,
    static_dir: std::path::PathBuf,
    pool: SqlitePool,
    digest: String,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let media = root.path().join("media");
        let attempt = root
            .path()
            .join("attempts")
            .join(uuid::Uuid::new_v4().to_string());
        let static_dir = root.path().join("static");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&media).unwrap();
        std::fs::create_dir_all(&attempt).unwrap();
        std::fs::create_dir(&static_dir).unwrap();
        std::fs::write(static_dir.join("index.html"), "preview").unwrap();
        db::initialize(&state).await.unwrap();
        let pool = db::connect(&state).await.unwrap();
        // Small generated media only: no real-disc jobs.
        let mut child = tokio::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=160x90:r=24",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-threads",
                "1",
                "-c:a",
                "aac",
                "-ac",
                "2",
                "-movflags",
                "+faststart",
            ])
            .arg(attempt.join("movie.mp4"))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(20), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        let digest = hex::encode(Sha256::digest(
            std::fs::read(attempt.join("movie.mp4")).unwrap(),
        ));
        let fixture = Self {
            _root: root,
            state,
            media,
            attempt,
            static_dir,
            pool,
            digest,
        };
        fixture.report("verified_experimental_output", 24);
        fixture
    }

    fn report(&self, status: &str, frames: u64) {
        let report = json!({
            "experimental":true,"published":false,"source_deletion_enabled":false,
            "status":status,"attempt_id":self.attempt.file_name().unwrap().to_str().unwrap(),
            "validation":{"source_decoded_frames":frames,"output_decoded_frames":frames,
                "source_decoded_seconds":1.0,"output_decoded_seconds":1.0,
                "movie":{"format":{"duration":"1.000000"},"streams":[
                    {"codec_type":"video","codec_name":"h264","pix_fmt":"yuv420p","width":160,"height":90},
                    {"codec_type":"audio","codec_name":"aac","channels":2}
                ]}}
        });
        std::fs::write(
            self.attempt.join("report.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
    }

    async fn admit(&self, digest: &str) -> anyhow::Result<Value> {
        let root = PreviewRoot::open(&self.media)?;
        admit(
            &self.pool,
            &root,
            Admission {
                report: &self.attempt.join("report.json"),
                title: "Local test",
                year: Some(2026),
                description: "",
                movie_sha256: digest,
                max_bytes: 1024 * 1024,
                timeout: Duration::from_secs(20),
            },
        )
        .await
    }

    async fn app(&self) -> Router {
        wetube_server::router_with_media(
            self.pool.clone(),
            ORIGIN,
            &self.static_dir,
            None,
            Some(PreviewRoot::open(&self.media).unwrap()),
        )
        .await
        .unwrap()
    }
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    csrf: Option<&str>,
    range: Option<&str>,
    body: &str,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "preview.test")
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    if let Some(csrf) = csrf {
        builder = builder.header("x-csrf-token", csrf);
    }
    if let Some(range) = range {
        builder = builder.header(header::RANGE, range);
    }
    app.clone()
        .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}

async fn login(app: &Router, username: &str) -> (String, String) {
    let response = request(
        app,
        "POST",
        "/api/v1/auth/login",
        None,
        None,
        None,
        &json!({"username":username,"password":"a generated preview password"}).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
    (cookie, body["csrf_token"].as_str().unwrap().to_owned())
}

async fn assert_response_status(response: axum::response::Response, expected: StatusCode) {
    let actual = response.status();
    if actual != expected {
        let body = to_bytes(response.into_body(), 8192).await.unwrap();
        panic!(
            "expected {expected}, got {actual}: {}",
            String::from_utf8_lossy(&body)
        );
    }
}

#[tokio::test]
async fn trash_requires_admin_origin_csrf_hides_movies_and_preserves_authorized_ranges() {
    let fixture = Fixture::new().await;
    for (name, role) in [
        ("admin", Role::Admin),
        ("viewer", Role::Viewer),
        ("other", Role::Viewer),
    ] {
        accounts::create_user(
            &fixture.pool,
            name,
            "a generated preview password".into(),
            role,
        )
        .await
        .unwrap();
    }
    let app = fixture.app().await;
    let (admin, admin_csrf) = login(&app, "admin").await;
    let (viewer, viewer_csrf) = login(&app, "viewer").await;
    let (other, _) = login(&app, "other").await;
    let admitted = fixture.admit(&fixture.digest).await.unwrap();
    let id = admitted["movie_id"].as_str().unwrap();
    let trash = format!("/api/v1/admin/movies/{id}/trash");
    let restore = format!("/api/v1/admin/movies/{id}/restore");
    let start = format!("/api/v1/movies/{id}/playback");
    let video = format!("/api/v1/movies/{id}/video");
    for (path, method) in [
        ("/api/v1/admin/trash", "GET"),
        (trash.as_str(), "POST"),
        (restore.as_str(), "POST"),
    ] {
        assert_eq!(
            request(&app, method, path, None, None, None, "")
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(
                &app,
                method,
                path,
                Some(&viewer),
                Some(&viewer_csrf),
                None,
                ""
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    for path in [&trash, &restore, &start] {
        assert_eq!(
            request(&app, "POST", path, Some(&admin), None, None, "")
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::HOST, "preview.test")
                    .header(header::ORIGIN, "http://other.test")
                    .header(header::COOKIE, &admin)
                    .header("x-csrf-token", &admin_csrf)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let response = request(
        &app,
        "POST",
        &start,
        Some(&viewer),
        Some(&viewer_csrf),
        None,
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let lease: wetube_core::PlaybackLease =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    let leased = format!("{video}?playback={}", lease.id);
    let renew = format!("{start}/{}", lease.id);
    // Inactivity may outlast the activity row, but not the login/movie binding.
    sqlx::query("UPDATE playback_leases SET expires_at=?")
        .bind(wetube_backend::now() - 1)
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert_response_status(
        request(
            &app,
            "GET",
            &leased,
            Some(&viewer),
            None,
            Some("bytes=0-9"),
            "",
        )
        .await,
        StatusCode::PARTIAL_CONTENT,
    )
    .await;
    sqlx::query("DELETE FROM playback_leases")
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            &renew,
            Some(&viewer),
            Some(&viewer_csrf),
            None,
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
    sqlx::query("DELETE FROM playback_leases")
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(
        request(
            &app,
            "GET",
            &leased,
            Some(&other),
            None,
            Some("bytes=0-9"),
            ""
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &leased,
            Some(&viewer),
            None,
            Some("bytes=0-9"),
            ""
        )
        .await
        .status(),
        StatusCode::PARTIAL_CONTENT
    );
    // The response opened before Trash can finish even without the explicit browser protocol.
    let in_flight = request(&app, "GET", &video, Some(&viewer), None, None, "").await;
    assert_eq!(
        request(
            &app,
            "POST",
            &trash,
            Some(&admin),
            Some(&admin_csrf),
            None,
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert!(
        !to_bytes(in_flight.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .is_empty()
    );
    for suffix in ["", "/video", "/progress", "/labels", "/poster"] {
        assert_eq!(
            request(
                &app,
                "GET",
                &format!("/api/v1/movies/{id}{suffix}"),
                Some(&viewer),
                None,
                None,
                ""
            )
            .await
            .status(),
            StatusCode::NOT_FOUND,
            "{suffix}"
        );
    }
    let response = request(&app, "GET", "/api/v1/movies", Some(&viewer), None, None, "").await;
    let library: wetube_core::Library =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert!(library.movies.is_empty());
    assert_eq!(
        request(
            &app,
            "POST",
            &start,
            Some(&viewer),
            Some(&viewer_csrf),
            None,
            ""
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &app,
            "GET",
            &leased,
            Some(&other),
            None,
            Some("bytes=0-9"),
            ""
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let response = request(
        &app,
        "GET",
        &leased,
        Some(&viewer),
        None,
        Some("bytes=0-9"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap().len(),
        10
    );
    let progress = format!("/api/v1/movies/{id}/progress?playback={}", lease.id);
    assert_eq!(
        request(
            &app,
            "PUT",
            &progress,
            Some(&viewer),
            Some(&viewer_csrf),
            None,
            r#"{"position_seconds":0.5}"#
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "POST", &renew, Some(&viewer), None, None, "")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &renew,
            Some(&viewer),
            Some(&viewer_csrf),
            None,
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
    sqlx::query("UPDATE playback_leases SET expires_at=?")
        .bind(wetube_backend::now())
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", &leased, Some(&viewer), None, None, "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("DELETE FROM playback_leases")
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            &renew,
            Some(&viewer),
            Some(&viewer_csrf),
            None,
            ""
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&app, "GET", &leased, Some(&viewer), None, None, "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &app,
            "POST",
            &restore,
            Some(&admin),
            Some(&admin_csrf),
            None,
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
    let progress = request(
        &app,
        "GET",
        &format!("/api/v1/movies/{id}/progress"),
        Some(&viewer),
        None,
        None,
        "",
    )
    .await;
    assert_eq!(progress.status(), StatusCode::OK);
    let progress: wetube_core::PlaybackProgress =
        serde_json::from_slice(&to_bytes(progress.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(progress.position_seconds, 0.5);
    assert_eq!(
        request(
            &app,
            "GET",
            &video,
            Some(&viewer),
            None,
            Some("bytes=0-9"),
            ""
        )
        .await
        .status(),
        StatusCode::PARTIAL_CONTENT
    );
}

#[tokio::test]
async fn successful_real_media_is_copied_and_unauthorized_ranges_and_progress_are_bounded() {
    let fixture = Fixture::new().await;
    let original = std::fs::read(fixture.attempt.join("movie.mp4")).unwrap();
    std::fs::write(fixture.attempt.join("input.iso"), b"retained staged ISO").unwrap();
    let report = std::fs::read(fixture.attempt.join("report.json")).unwrap();
    let admitted = fixture.admit(&fixture.digest).await.unwrap();
    assert_eq!(
        std::fs::read(fixture.attempt.join("movie.mp4")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read(fixture.attempt.join("report.json")).unwrap(),
        report
    );
    assert_eq!(
        std::fs::read(fixture.attempt.join("input.iso")).unwrap(),
        b"retained staged ISO"
    );
    let id = admitted["movie_id"].as_str().unwrap();
    let package = admitted["package_id"].as_str().unwrap();
    let stored_identity: String =
        sqlx::query_scalar("SELECT file_identity FROM preview_media WHERE movie_id=?")
            .bind(id)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    let fresh = PreviewRoot::open(&fixture.media)
        .unwrap()
        .open_movie(package)
        .unwrap();
    assert_eq!(
        stored_identity,
        wetube_backend::incoming::identity(&fresh.metadata().unwrap())
    );
    let manifest = std::fs::read(fixture.media.join(package).join("manifest.json")).unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.media.join(package).join("manifest.sha256")).unwrap(),
        hex::encode(Sha256::digest(&manifest))
    );
    assert_eq!(
        std::fs::read(fixture.media.join(package).join("movie.mp4")).unwrap(),
        original
    );
    for username in ["alice", "bob"] {
        accounts::create_user(
            &fixture.pool,
            username,
            "a generated preview password".into(),
            Role::Viewer,
        )
        .await
        .unwrap();
    }
    let app = fixture.app().await;
    let (alice, csrf) = login(&app, "alice").await;
    let (bob, _) = login(&app, "bob").await;
    let labels = wetube_core::MovieLabels {
        categories: vec!["Family".into()],
        tags: vec!["Favorite".into()],
    };
    wetube_backend::labels::update(
        &fixture.pool,
        id,
        &wetube_core::UpdateMovieLabels {
            expected: wetube_core::MovieLabels::default(),
            labels: labels.clone(),
        },
    )
    .await
    .unwrap();
    let details = request(
        &app,
        "GET",
        &format!("/api/v1/movies/{id}"),
        Some(&alice),
        None,
        None,
        "",
    )
    .await;
    assert_eq!(details.status(), StatusCode::OK);
    let details: wetube_core::MovieDetails =
        serde_json::from_slice(&to_bytes(details.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(details.movie.categories, labels.categories);
    assert_eq!(details.movie.tags, labels.tags);
    assert!((details.duration_seconds - 1.0).abs() < 0.1);
    let video = format!("/api/v1/movies/{id}/video");
    for suffix in ["", "/video", "/progress"] {
        let path = format!("/api/v1/movies/{id}{suffix}");
        assert_eq!(
            request(&app, "GET", &path, None, None, None, "")
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let full = request(&app, "GET", &video, Some(&alice), None, None, "").await;
    assert_eq!(full.status(), StatusCode::OK);
    assert_eq!(full.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        &to_bytes(full.into_body(), original.len() + 1)
            .await
            .unwrap()[..],
        original
    );
    let size = original.len();
    for (range, start, end) in [
        ("bytes=0-9".to_owned(), 0, 9),
        ("bytes=-12".to_owned(), size - 12, size - 1),
        (format!("bytes={}-", size - 8), size - 8, size - 1),
        (format!("bytes=0-{}", size + 100), 0, size - 1),
    ] {
        let response = request(&app, "GET", &video, Some(&alice), None, Some(&range), "").await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers()[header::CONTENT_RANGE],
            format!("bytes {start}-{end}/{size}")
        );
        assert_eq!(
            &to_bytes(response.into_body(), size + 1).await.unwrap()[..],
            &original[start..=end]
        );
    }
    for range in [
        "bytes=-0",
        "bytes=2-1",
        "bytes=999999999-",
        "bytes=0-1,3-4",
        "bytes=bogus",
        "bytes=+1-2",
    ] {
        let response = request(&app, "GET", &video, Some(&alice), None, Some(range), "").await;
        assert_eq!(
            response.status(),
            StatusCode::RANGE_NOT_SATISFIABLE,
            "{range}"
        );
        assert_eq!(
            response.headers()[header::CONTENT_RANGE],
            format!("bytes */{size}")
        );
    }
    let duplicate_ranges = Request::builder()
        .method("GET")
        .uri(&video)
        .header(header::HOST, "preview.test")
        .header(header::COOKIE, &alice)
        .header(header::RANGE, "bytes=0-1")
        .header(header::RANGE, "bytes=3-4")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(duplicate_ranges)
            .await
            .unwrap()
            .status(),
        StatusCode::RANGE_NOT_SATISFIABLE
    );
    for range in [None, Some("bytes=0-9")] {
        let response = request(&app, "HEAD", &video, Some(&alice), None, range, "").await;
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            if range.is_some() {
                "10".to_owned()
            } else {
                size.to_string()
            }
        );
        assert!(to_bytes(response.into_body(), 1).await.unwrap().is_empty());
    }
    let progress = format!("/api/v1/movies/{id}/progress");
    let cross_origin = Request::builder()
        .method("PUT")
        .uri(&progress)
        .header(header::HOST, "preview.test")
        .header(header::COOKIE, &alice)
        .header("x-csrf-token", &csrf)
        .header(header::ORIGIN, "http://other.test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"position_seconds":0.5}"#))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(cross_origin).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            &app,
            "PUT",
            &progress,
            Some(&alice),
            None,
            None,
            r#"{"position_seconds":0.5}"#
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    for position in ["-1", "1.251", "2", "null", "\"NaN\""] {
        let response = request(
            &app,
            "PUT",
            &progress,
            Some(&alice),
            Some(&csrf),
            None,
            &format!("{{\"position_seconds\":{position}}}"),
        )
        .await;
        assert!(response.status().is_client_error());
    }
    assert_eq!(
        request(
            &app,
            "PUT",
            &progress,
            Some(&alice),
            Some(&csrf),
            None,
            r#"{"position_seconds":0.5}"#
        )
        .await
        .status(),
        StatusCode::OK
    );
    let reopened = db::connect(&fixture.state).await.unwrap();
    let persisted: f64 = sqlx::query_scalar("SELECT position_seconds FROM playback_progress")
        .fetch_one(&reopened)
        .await
        .unwrap();
    assert_eq!(persisted, 0.5);
    for (cookie, expected) in [(&alice, 0.5), (&bob, 0.0)] {
        let response = request(&app, "GET", &progress, Some(cookie), None, None, "").await;
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(body["position_seconds"], expected);
    }
    let response = request(
        &app,
        "PUT",
        &progress,
        Some(&alice),
        Some(&csrf),
        None,
        r#"{"position_seconds":1.001}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(body["position_seconds"], 1.0);
    let persisted: f64 = sqlx::query_scalar("SELECT position_seconds FROM playback_progress")
        .fetch_one(&reopened)
        .await
        .unwrap();
    assert_eq!(persisted, 1.0);
    sqlx::query("UPDATE movies SET status='trashed' WHERE id=?")
        .bind(id)
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(
        request(&app, "GET", &video, Some(&alice), None, None, "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE movies SET status='ready' WHERE id=?")
        .bind(id)
        .execute(&fixture.pool)
        .await
        .unwrap();
    std::fs::write(fixture.media.join(package).join("movie.mp4"), b"modified").unwrap();
    assert_eq!(
        request(&app, "GET", &video, Some(&alice), None, None, "")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn failed_forged_corrupt_and_modified_payloads_never_become_eligible() {
    let fixture = Fixture::new().await;
    fixture.report("failed", 24);
    assert!(fixture.admit(&fixture.digest).await.is_err());
    fixture.report("verified_experimental_output", 25);
    assert!(fixture.admit(&fixture.digest).await.is_err());
    fixture.report("verified_experimental_output", 24);
    assert!(fixture.admit(&"0".repeat(64)).await.is_err());
    std::fs::write(fixture.attempt.join("movie.mp4"), b"forged mp4").unwrap();
    let digest = hex::encode(Sha256::digest(b"forged mp4"));
    assert!(fixture.admit(&digest).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM preview_media")
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM movies")
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(std::fs::read_dir(&fixture.media).unwrap().all(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_str()
            .unwrap()
            .parse::<uuid::Uuid>()
            .is_ok()
    }));
}

#[tokio::test]
async fn preview_roots_cannot_overlap_static_or_state() {
    let fixture = Fixture::new().await;
    for path in [&fixture.state, &fixture.static_dir] {
        assert!(
            wetube_server::router_with_media(
                fixture.pool.clone(),
                ORIGIN,
                &fixture.static_dir,
                None,
                Some(PreviewRoot::open(path).unwrap())
            )
            .await
            .is_err()
        );
    }
    let root = PreviewRoot::open(&fixture.media).unwrap();
    assert!(root.open_movie("../source.iso").is_err());
    let package = uuid::Uuid::new_v4().to_string();
    std::os::unix::fs::symlink(Path::new("../attempts"), fixture.media.join(&package)).unwrap();
    assert!(root.open_movie(&package).is_err());
    assert!(
        root.reserve_package(&package).is_err(),
        "never overwrite an existing package"
    );
    let package = uuid::Uuid::new_v4().to_string();
    let directory = fixture.media.join(&package);
    std::fs::create_dir(&directory).unwrap();
    std::os::unix::fs::symlink(
        fixture.attempt.join("movie.mp4"),
        directory.join("movie.mp4"),
    )
    .unwrap();
    assert!(root.open_movie(&package).is_err());
    let source = format!(".admitting-{package}");
    std::fs::create_dir(fixture.media.join(&source)).unwrap();
    assert!(root.finalize(&source, &package).is_err());
    assert!(fixture.media.join(source).is_dir());
}

#[tokio::test]
async fn preserved_mp4_chapter_data_is_supported_without_exposing_other_tracks() {
    let fixture = Fixture::new().await;
    let chapters = fixture.attempt.join("chapters.txt");
    std::fs::write(
        &chapters,
        ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\ntitle=Opening\n",
    )
    .unwrap();
    let movie = fixture.attempt.join("movie.mp4");
    let remuxed = fixture.attempt.join("chaptered.mp4");
    let mut child = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-v", "error", "-i"])
        .arg(&movie)
        .arg("-i")
        .arg(&chapters)
        .args([
            "-map",
            "0",
            "-map_metadata",
            "1",
            "-map_chapters",
            "1",
            "-c",
            "copy",
        ])
        .arg(&remuxed)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    std::fs::rename(&remuxed, &movie).unwrap();
    let mut report: Value =
        serde_json::from_slice(&std::fs::read(fixture.attempt.join("report.json")).unwrap())
            .unwrap();
    report["validation"]["movie"]["streams"]
        .as_array_mut()
        .unwrap()
        .push(json!({"codec_type":"data","codec_name":"bin_data"}));
    report["validation"]["movie"]["chapters"] =
        json!([{"start_time":"0.000000","end_time":"1.000000"}]);
    std::fs::write(
        fixture.attempt.join("report.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    let digest = hex::encode(Sha256::digest(std::fs::read(&movie).unwrap()));
    fixture.admit(&digest).await.unwrap();
}

#[tokio::test]
async fn sub_frame_timestamp_precision_survives_full_preview_decode() {
    let fixture = Fixture::new().await;
    let movie = fixture.attempt.join("movie.mp4");
    let precise = fixture.attempt.join("precise.mp4");
    // A sparse short VFR interval rounds onto its neighbor with the inferred output time base.
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostdin",
            "-v",
            "error",
            "-stream_loop",
            "7",
            "-i",
        ])
        .arg(&movie)
        .args([
            "-vf",
            "setpts=PTS+0.03/TB*eq(N\\,150),settb=1/1000,setpts=0.8*PTS",
            "-enc_time_base:v",
            "1/1000",
            "-fps_mode",
            "passthrough",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-c:a",
            "copy",
        ])
        .arg(&precise)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    std::fs::rename(&precise, &movie).unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_format",
                "-show_streams",
                "-count_frames",
                "-of",
                "json",
            ])
            .arg(&movie)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.status.success() && output.stderr.is_empty());
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
    let frames: u64 = metadata["streams"][0]["nb_read_frames"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut report: Value =
        serde_json::from_slice(&std::fs::read(fixture.attempt.join("report.json")).unwrap())
            .unwrap();
    report["validation"]["source_decoded_frames"] = json!(frames);
    report["validation"]["output_decoded_frames"] = json!(frames);
    report["validation"]["source_decoded_seconds"] = json!(8.0);
    report["validation"]["output_decoded_seconds"] = json!(8.0);
    report["validation"]["movie"] = metadata;
    std::fs::write(
        fixture.attempt.join("report.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();
    let digest = hex::encode(Sha256::digest(std::fs::read(&movie).unwrap()));
    fixture.admit(&digest).await.unwrap();
}
