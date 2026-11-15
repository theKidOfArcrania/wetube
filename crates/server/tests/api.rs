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
async fn processing_queue_admin_origin_csrf_controls_and_conflicts() {
    let fixture = Fixture::new().await;
    let (admin, session) = fixture.login("admin").await;
    let (viewer, viewer_session) = fixture.login("viewer").await;
    sqlx::query("INSERT INTO diagnostic_jobs(id,requested_by,state,created_at) SELECT 'waiting',id,'queued',1 FROM users WHERE username='admin'").execute(&fixture.pool).await.unwrap();
    let initial = wetube_backend::queue::snapshot(&fixture.pool)
        .await
        .unwrap();
    let key = initial.jobs[0].key;
    for (path, payload) in [
        (
            "/api/v1/admin/queue/settings".to_owned(),
            serde_json::json!({"revision":initial.revision,"paused":true,"maximum":2}),
        ),
        (
            "/api/v1/admin/queue/order".to_owned(),
            serde_json::json!({"revision":initial.revision,"waiting":[key]}),
        ),
        (
            format!("/api/v1/admin/queue/{key}/cancel"),
            serde_json::Value::Null,
        ),
    ] {
        let body = payload.to_string();
        for (cookie, csrf, status) in [
            (None, None, StatusCode::UNAUTHORIZED),
            (
                Some(viewer.as_str()),
                Some(viewer_session.csrf_token.as_str()),
                StatusCode::FORBIDDEN,
            ),
            (Some(admin.as_str()), None, StatusCode::FORBIDDEN),
            (Some(admin.as_str()), Some("wrong"), StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                fixture
                    .send("POST", &path, &body, cookie, csrf)
                    .await
                    .status(),
                status
            );
        }
        for origin in [None, Some("http://elsewhere.test")] {
            let mut request = Request::builder()
                .method("POST")
                .uri(&path)
                .header(header::HOST, "wetube.test")
                .header(header::COOKIE, &admin)
                .header("x-csrf-token", &session.csrf_token)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            assert_eq!(
                fixture
                    .app
                    .clone()
                    .oneshot(request.body(Body::from(body.clone())).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
    }
    assert_eq!(
        fixture
            .send("GET", "/api/v1/admin/queue", "", Some(&viewer), None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let settings =
        serde_json::json!({"revision":initial.revision,"paused":true,"maximum":2}).to_string();
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/admin/queue/settings",
                &settings,
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert!(
        wetube_backend::queue::claim(&fixture.pool, key)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/admin/queue/settings",
                &settings,
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let current = wetube_backend::queue::snapshot(&fixture.pool)
        .await
        .unwrap();
    for maximum in [0, 17] {
        let body =
            serde_json::json!({"revision":current.revision,"paused":false,"maximum":maximum})
                .to_string();
        assert_eq!(
            fixture
                .send(
                    "POST",
                    "/api/v1/admin/queue/settings",
                    &body,
                    Some(&admin),
                    Some(&session.csrf_token)
                )
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let duplicate =
        serde_json::json!({"revision":current.revision,"waiting":[key,key]}).to_string();
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/admin/queue/order",
                &duplicate,
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let path = format!("/api/v1/admin/queue/{key}/cancel");
    assert_eq!(
        fixture
            .send(
                "POST",
                &path,
                "null",
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                &path,
                "null",
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let response = fixture
        .send("GET", "/api/v1/jobs", "", Some(&admin), None)
        .await;
    let jobs: Vec<DiagnosticJob> =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(jobs[0].state, wetube_core::JobState::Cancelled);
}

#[tokio::test]
async fn movie_labels_normalize_unicode_updates_catalog_reuse_and_assignment_filters() {
    use serde_json::json;
    let fixture = Fixture::new().await;
    let (admin, session) = fixture.login("admin").await;
    let (viewer, _) = fixture.login("viewer").await;
    sqlx::query(
        "INSERT INTO movies VALUES('one','First',NULL,'','ready'),('two','Second',NULL,'','ready')",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    for (id, categories, tags) in [
        (
            "one",
            vec!["Café", "Cafe\u{301}"],
            vec!["Déjà", "De\u{301}ja\u{300}"],
        ),
        ("two", vec!["CAFE\u{301}"], vec!["DE\u{301}JA\u{300}"]),
    ] {
        let payload = json!({"expected":{"categories":[],"tags":[]}, "labels":{ "categories": categories, "tags": tags }});
        assert_eq!(
            fixture
                .send(
                    "POST",
                    &format!("/api/v1/admin/movies/{id}/labels"),
                    &payload.to_string(),
                    Some(&admin),
                    Some(&session.csrf_token)
                )
                .await
                .status(),
            StatusCode::OK
        );
    }
    let labels = wetube_core::MovieLabels {
        categories: vec!["Café".into()],
        tags: vec!["Déjà".into()],
    };
    assert_eq!(
        wetube_backend::labels::catalog(&fixture.pool)
            .await
            .unwrap(),
        labels
    );
    let response = fixture
        .send("GET", "/api/v1/movies/two/labels", "", Some(&viewer), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<wetube_core::MovieLabels>(
            &to_bytes(response.into_body(), 4096).await.unwrap()
        )
        .unwrap(),
        labels
    );
    let payload = json!({"expected":{"categories":["Cafe\u{301}"],"tags":["De\u{301}ja\u{300}"]},
        "labels":{"categories":["Cafe\u{301}"],"tags":["De\u{301}ja\u{300}"]}});
    assert_eq!(
        fixture
            .send(
                "POST",
                "/api/v1/admin/movies/one/labels",
                &payload.to_string(),
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::OK
    );
    for (key, value, count) in [
        ("q", "Cafe\u{301}", 2),
        ("category", "Cafe\u{301}", 2),
        ("category", "CAFÉ", 2),
        ("tag", "De\u{301}ja\u{300}", 2),
        ("category", "Cafe", 0),
    ] {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair(key, value)
            .finish();
        let response = fixture
            .send(
                "GET",
                &format!("/api/v1/movies?{query}"),
                "",
                Some(&viewer),
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let library: wetube_core::Library =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(library.movies.len(), count, "{key}={value}");
    }
}

#[tokio::test]
async fn movie_labels_require_admin_csrf_and_filter_real_assignments_without_losing_unlabeled_movies()
 {
    use serde_json::json;
    let fixture = Fixture::new().await;
    let (admin, session) = fixture.login("admin").await;
    let (viewer, viewer_session) = fixture.login("viewer").await;
    sqlx::query("INSERT INTO movies VALUES('one','Local',2000,'Original','ready'),('two','Drama title',NULL,'Favorite text','ready'),('three','Unlabeled',NULL,'','ready')")
        .execute(&fixture.pool).await.unwrap();
    sqlx::query("INSERT INTO tmdb_metadata VALUES('one',7,NULL,NULL,NULL,1,1)")
        .execute(&fixture.pool)
        .await
        .unwrap();
    let path = "/api/v1/admin/movies/one/labels";
    let payload = json!({"expected":{"categories":[],"tags":[]},
        "labels":{"categories":["Drama","Family"],"tags":["Favorite"]}})
    .to_string();
    for (cookie, csrf, expected) in [
        (None, None, StatusCode::UNAUTHORIZED),
        (
            Some(viewer.as_str()),
            Some(viewer_session.csrf_token.as_str()),
            StatusCode::FORBIDDEN,
        ),
        (Some(admin.as_str()), None, StatusCode::FORBIDDEN),
        (
            Some(admin.as_str()),
            Some(session.csrf_token.as_str()),
            StatusCode::OK,
        ),
    ] {
        assert_eq!(
            fixture
                .send("POST", path, &payload, cookie, csrf)
                .await
                .status(),
            expected
        );
    }
    for protected in [path, "/api/v1/admin/movies/one/genre-suggestions"] {
        let response = fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(protected)
                    .header(header::HOST, "wetube.test")
                    .header(header::ORIGIN, "http://evil.test")
                    .header(header::COOKIE, &admin)
                    .header("x-csrf-token", &session.csrf_token)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(
        fixture
            .send(
                "POST",
                path,
                &payload,
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
                "/api/v1/admin/movies/absent/labels",
                &payload,
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    for (query, expected) in [
        ("", vec!["one", "two", "three"]),
        ("?q=favorite", vec!["one", "two"]),
        ("?q=family", vec!["one"]),
        ("?category=Drama", vec!["one"]),
        ("?category=Dram", vec![]),
        ("?tag=Favorite", vec!["one"]),
        ("?category=Family&tag=Favorite&q=local", vec!["one"]),
        ("?category=Family&tag=Missing", vec![]),
        ("?q=unlabeled", vec!["three"]),
    ] {
        let response = fixture
            .send(
                "GET",
                &format!("/api/v1/movies{query}"),
                "",
                Some(&viewer),
                None,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let library: wetube_core::Library =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        let ids = library
            .movies
            .iter()
            .map(|movie| movie.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids, expected.into_iter().collect(), "{query}");
        if let Some(movie) = library.movies.iter().find(|movie| movie.id == "one") {
            assert_eq!(movie.title, "Local");
            assert_eq!(movie.tmdb_id, Some(7));
            assert_eq!(movie.tags, ["Favorite"]);
        }
    }
    let response = fixture
        .send("GET", "/api/v1/labels", "", Some(&viewer), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fixture
            .send("GET", "/api/v1/labels", "", None, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture
            .send("GET", "/api/v1/movies/one/labels", "", None, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        fixture
            .send(
                "GET",
                "/api/v1/movies/absent/labels",
                "",
                Some(&viewer),
                None
            )
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    for (cookie, csrf, expected) in [
        (None, None, StatusCode::UNAUTHORIZED),
        (
            Some(viewer.as_str()),
            Some(viewer_session.csrf_token.as_str()),
            StatusCode::FORBIDDEN,
        ),
        (Some(admin.as_str()), None, StatusCode::FORBIDDEN),
        (
            Some(admin.as_str()),
            Some(session.csrf_token.as_str()),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        assert_eq!(
            fixture
                .send(
                    "POST",
                    "/api/v1/admin/movies/one/genre-suggestions",
                    "null",
                    cookie,
                    csrf
                )
                .await
                .status(),
            expected
        );
    }
    assert_eq!(
        wetube_backend::labels::for_movie(&fixture.pool, "one")
            .await
            .unwrap()
            .categories,
        ["Drama", "Family"]
    );
}

#[tokio::test]
async fn metadata_routes_require_admin_origin_csrf_and_allow_disabled_manual_fallback() {
    let fixture = Fixture::new().await;
    let (admin, session) = fixture.login("admin").await;
    let (viewer, viewer_session) = fixture.login("viewer").await;
    let search = "/api/v1/admin/metadata/search";
    let refresh = "/api/v1/admin/movies/unknown/metadata/refresh";
    for path in [search, refresh] {
        for (cookie, csrf, expected) in [
            (None, None, StatusCode::UNAUTHORIZED),
            (
                Some(viewer.as_str()),
                Some(viewer_session.csrf_token.as_str()),
                StatusCode::FORBIDDEN,
            ),
            (Some(admin.as_str()), None, StatusCode::FORBIDDEN),
            (
                Some(admin.as_str()),
                Some(session.csrf_token.as_str()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            assert_eq!(
                fixture
                    .send(
                        "POST",
                        path,
                        r#"{"query":"Deliberate title","year":2001}"#,
                        cookie,
                        csrf
                    )
                    .await
                    .status(),
                expected
            );
        }
        let response = fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::HOST, "wetube.test")
                    .header(header::ORIGIN, "http://evil.test")
                    .header(header::COOKIE, &admin)
                    .header("x-csrf-token", &session.csrf_token)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"query":"Title","year":null}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    for query in [
        serde_json::json!({"query":"","year":null}),
        serde_json::json!({"query":"x".repeat(257),"year":null}),
        serde_json::json!({"query":"x\nbad","year":null}),
        serde_json::json!({"query":"Title","year":1600}),
    ] {
        assert_eq!(
            fixture
                .send(
                    "POST",
                    search,
                    &query.to_string(),
                    Some(&admin),
                    Some(&session.csrf_token)
                )
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    for (cookie, status) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(viewer.as_str()), StatusCode::FORBIDDEN),
    ] {
        assert_eq!(
            fixture
                .send("GET", "/api/v1/admin/metadata", "", cookie, None)
                .await
                .status(),
            status
        );
    }
    let response = fixture
        .send("GET", "/api/v1/admin/metadata", "", Some(&admin), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
    assert_eq!(body, serde_json::json!({"configured":false}));
}

#[tokio::test]
async fn metadata_expiry_falls_back_on_reads_even_before_cleanup_without_affecting_movies() {
    let fixture = Fixture::new().await;
    let (viewer, _) = fixture.login("viewer").await;
    let now = wetube_backend::now();
    sqlx::query("INSERT INTO movies VALUES('movie','local.iso',NULL,'','ready')")
        .execute(&fixture.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO tmdb_metadata VALUES('movie',7,'Provider title',2001,'Provider text',?,?)",
    )
    .bind(now)
    .bind(now + 60)
    .execute(&fixture.pool)
    .await
    .unwrap();
    for (expires, title, year, description) in [
        (now + 60, "Provider title", Some(2001), "Provider text"),
        (now, "local.iso", None, ""),
    ] {
        sqlx::query("UPDATE tmdb_metadata SET expires_at=?")
            .bind(expires)
            .execute(&fixture.pool)
            .await
            .unwrap();
        let response = fixture
            .send("GET", "/api/v1/movies", "", Some(&viewer), None)
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let library: wetube_core::Library =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        let movie = &library.movies[0];
        assert_eq!(movie.title, title);
        assert_eq!(movie.year, year);
        assert_eq!(movie.description, description);
        assert_eq!(movie.tmdb_id, Some(7));
    }
}

#[tokio::test]
async fn comparison_jobs_are_private_deduplicated_snapshot_bound_and_range_pinned() {
    use serde_json::json;
    use wetube_core::{ComparisonJob, JobState};
    let mut fixture = Fixture::new().await;
    let scratch = tempfile::tempdir().unwrap();
    fixture.app = wetube_server::router_with_conversions(
        fixture.pool.clone(),
        ORIGIN,
        fixture._directory.path(),
        None,
        None,
        Some(scratch.path()),
    )
    .await
    .unwrap();
    let (admin, session) = fixture.login("admin").await;
    let (viewer, viewer_session) = fixture.login("viewer").await;
    let probe = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report)
        VALUES (?,?,'retained.iso','identity',1,1,'failed',1,?,'{}')")
        .bind(&probe).bind(&session.user.id).bind(uuid::Uuid::new_v4().to_string())
        .execute(&fixture.pool).await.unwrap();
    let path = format!("/api/v1/admin/probes/{probe}/comparison");
    for (cookie, csrf, expected) in [
        (None, None, StatusCode::UNAUTHORIZED),
        (
            Some(viewer.as_str()),
            Some(viewer_session.csrf_token.as_str()),
            StatusCode::FORBIDDEN,
        ),
        (Some(admin.as_str()), None, StatusCode::FORBIDDEN),
    ] {
        assert_eq!(
            fixture.send("POST", &path, "", cookie, csrf).await.status(),
            expected
        );
    }
    for cookie in [None, Some(viewer.as_str())] {
        let expected = if cookie.is_some() {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::UNAUTHORIZED
        };
        assert_eq!(
            fixture.send("GET", &path, "", cookie, None).await.status(),
            expected
        );
    }
    let wrong_origin = Request::builder()
        .method("POST")
        .uri(&path)
        .header(header::HOST, "wetube.test")
        .header(header::ORIGIN, "http://evil.test")
        .header(header::COOKIE, &admin)
        .header("x-csrf-token", &session.csrf_token)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        fixture
            .app
            .clone()
            .oneshot(wrong_origin)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .send("POST", &path, "", Some(&admin), Some(&session.csrf_token))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE probe_jobs SET state='needs_review' WHERE id=?")
        .bind(&probe)
        .execute(&fixture.pool)
        .await
        .unwrap();
    let response = fixture
        .send("POST", &path, "", Some(&admin), Some(&session.csrf_token))
        .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job: ComparisonJob =
        serde_json::from_slice(&to_bytes(response.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(job.state, JobState::Queued);
    assert!(job.progress.is_none());
    let mut legacy = serde_json::to_value(&job).unwrap();
    legacy.as_object_mut().unwrap().remove("progress");
    assert!(
        serde_json::from_value::<ComparisonJob>(legacy)
            .unwrap()
            .progress
            .is_none()
    );
    let progress = wetube_core::MediaProgress {
        workflow: wetube_core::MediaWorkflow::Comparison,
        stage: 3,
        phase: "Encoding Preserve".into(),
        elapsed_seconds: 4.0,
        completed_units: Some(2.0),
        total_units: Some(10.0),
        unit: "media seconds".into(),
        updated_at: wetube_backend::now(),
        measured_elapsed_seconds: None,
        stage_portion: None,
    };
    sqlx::query("UPDATE comparison_jobs SET progress=? WHERE id=?")
        .bind(serde_json::to_string(&progress).unwrap())
        .bind(&job.id)
        .execute(&fixture.pool)
        .await
        .unwrap();
    let response = fixture.send("GET", &path, "", Some(&admin), None).await;
    let observed: ComparisonJob =
        serde_json::from_slice(&to_bytes(response.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(observed.progress, Some(progress));
    assert_eq!(
        observed.state,
        JobState::Queued,
        "telemetry cannot make media ready"
    );
    sqlx::query("UPDATE comparison_jobs SET progress='invalid optional snapshot' WHERE id=?")
        .bind(&job.id)
        .execute(&fixture.pool)
        .await
        .unwrap();
    let response = fixture.send("GET", &path, "", Some(&admin), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let observed: ComparisonJob =
        serde_json::from_slice(&to_bytes(response.into_body(), 32768).await.unwrap()).unwrap();
    assert!(observed.progress.is_none());
    assert_eq!(
        fixture
            .send("POST", &path, "", Some(&admin), Some(&session.csrf_token))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let video = format!("/api/v1/admin/comparisons/{}/preserve/video", job.id);
    assert_eq!(
        fixture
            .send("GET", &video, "", Some(&admin), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    // Inject terminal metadata only to isolate the HTTP/file boundary. Actual media is
    // generated and independently decoded by the worker's authored-DVD test.
    let attempt = uuid::Uuid::new_v4().to_string();
    let directory = scratch.path().join(&attempt);
    std::fs::create_dir(&directory).unwrap();
    let bytes = b"private pinned comparison bytes";
    let mut report = json!({"version":1,"status":"succeeded","preserve":{"filter":null},
        "yadif":{"filter":wetube_core::YADIF_FILTER}});
    for treatment in ["preserve", "yadif"] {
        let clip = directory.join(format!("{treatment}.mp4"));
        std::fs::write(&clip, bytes).unwrap();
        let metadata = std::fs::metadata(clip).unwrap();
        report[treatment]["file_identity"] = json!(wetube_backend::incoming::identity(&metadata));
        report[treatment]["size_bytes"] = json!(bytes.len());
    }
    sqlx::query("UPDATE comparison_jobs SET state='succeeded',attempt_id=?,report=? WHERE id=?")
        .bind(&attempt)
        .bind(report.to_string())
        .bind(&job.id)
        .execute(&fixture.pool)
        .await
        .unwrap();
    for treatment in ["preserve", "yadif"] {
        let video = format!("/api/v1/admin/comparisons/{}/{treatment}/video", job.id);
        for (cookie, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(viewer.as_str()), StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                fixture.send("GET", &video, "", cookie, None).await.status(),
                expected
            );
        }
        for method in ["GET", "HEAD"] {
            let request = Request::builder()
                .method(method)
                .uri(&video)
                .header(header::HOST, "wetube.test")
                .header(header::COOKIE, &admin)
                .header(header::RANGE, "bytes=2-6")
                .body(Body::empty())
                .unwrap();
            let response = fixture.app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers()[header::CONTENT_LENGTH], "5");
            let payload = to_bytes(response.into_body(), 1024).await.unwrap();
            assert_eq!(
                &payload[..],
                if method == "HEAD" { b"" } else { &bytes[2..7] }
            );
        }
        let request = Request::builder()
            .uri(&video)
            .header(header::HOST, "wetube.test")
            .header(header::COOKIE, &admin)
            .header(header::RANGE, "bytes=9999-")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            fixture.app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::RANGE_NOT_SATISFIABLE
        );
    }
    sqlx::query("UPDATE probe_jobs SET title=2 WHERE id=?")
        .bind(&probe)
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .send("GET", &video, "", Some(&admin), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let response = fixture.send("GET", &path, "", Some(&admin), None).await;
    assert_eq!(
        &to_bytes(response.into_body(), 1024).await.unwrap()[..],
        b"null"
    );
    sqlx::query("UPDATE probe_jobs SET title=1,report='{\"changed\":true}' WHERE id=?")
        .bind(&probe)
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .send("GET", &video, "", Some(&admin), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE probe_jobs SET report='{}' WHERE id=?")
        .bind(&probe)
        .execute(&fixture.pool)
        .await
        .unwrap();
    let clip = directory.join("preserve.mp4");
    let saved = directory.join("saved.mp4");
    std::fs::rename(&clip, &saved).unwrap();
    std::os::unix::fs::symlink(&saved, &clip).unwrap();
    assert_eq!(
        fixture
            .send("GET", &video, "", Some(&admin), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    std::fs::remove_file(&clip).unwrap();
    std::fs::write(&clip, bytes).unwrap();
    assert_eq!(
        fixture
            .send("GET", &video, "", Some(&admin), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM movies")
            .fetch_one(&fixture.pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn personal_import_history_validates_lifecycle_permissions_pagination_and_persistence() {
    use wetube_core::{ImportJob, ImportPage};
    let fixture = Fixture::new().await;
    accounts::create_user(&fixture.pool, "other-admin", PASSWORD.into(), Role::Admin)
        .await
        .unwrap();
    let (admin, session) = fixture.login("admin").await;
    let (other, _) = fixture.login("other-admin").await;
    let (viewer, viewer_session) = fixture.login("viewer").await;
    let mut scans = Vec::new();
    for (state, time) in [
        ("queued", 10),
        ("needs_review", 10),
        ("needs_review", 10),
        ("failed", 10),
    ] {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at,attempt_id,error) VALUES (?,?,'retained.iso','bound',?,?,?,'retained error')")
            .bind(&id).bind(&session.user.id).bind(state).bind(time).bind(format!("attempt-{id}"))
            .execute(&fixture.pool).await.unwrap();
        scans.push(id);
    }
    let mut probes = Vec::new();
    for (state, scan) in [
        ("probing", None),
        ("needs_review", Some(&scans[2])),
        ("needs_review", None),
        ("needs_review", None),
        ("failed", None),
    ] {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,scan_job_id) VALUES (?,?,'retained.iso','bound',1,1,?,10,?)")
            .bind(&id).bind(&session.user.id).bind(state).bind(scan).execute(&fixture.pool).await.unwrap();
        probes.push(id);
    }
    let mut conversions = Vec::new();
    for (state, probe) in [("needs_review", &probes[2]), ("published", &probes[3])] {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO conversion_jobs(id,requested_by,probe_job_id,state,created_at,video_treatment) VALUES (?,?,?,?,10,'preserve')")
            .bind(&id).bind(&session.user.id).bind(probe).bind(state).execute(&fixture.pool).await.unwrap();
        conversions.push(id);
    }
    let archive_path = |kind: &str, id: &str| format!("/api/v1/admin/imports/{kind}/{id}/archive");
    let failed_path = archive_path("scan", &scans[3]);
    for path in [
        "/api/v1/admin/imports",
        "/api/v1/admin/imports?history=true",
    ] {
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
    for archived in [true, false] {
        let body = serde_json::json!({"archived":archived}).to_string();
        assert_eq!(
            fixture
                .send("POST", &failed_path, &body, None, None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            fixture
                .send(
                    "POST",
                    &failed_path,
                    &body,
                    Some(&viewer),
                    Some(&viewer_session.csrf_token)
                )
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            fixture
                .send("POST", &failed_path, &body, Some(&admin), None)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        for origin in [
            None,
            Some("http://evil.test"),
            Some("null"),
            Some("http://wetube.test/"),
        ] {
            let mut request = Request::builder()
                .method("POST")
                .uri(&failed_path)
                .header(header::HOST, "wetube.test")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &admin)
                .header("x-csrf-token", &session.csrf_token);
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            assert_eq!(
                fixture
                    .app
                    .clone()
                    .oneshot(request.body(Body::from(body.clone())).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        for (kind, id) in [
            ("scan", &scans[0]),
            ("scan", &scans[1]),
            ("probe", &probes[0]),
            ("probe", &probes[1]),
            ("conversion", &conversions[0]),
        ] {
            assert_eq!(
                fixture
                    .send(
                        "POST",
                        &archive_path(kind, id),
                        &body,
                        Some(&admin),
                        Some(&session.csrf_token)
                    )
                    .await
                    .status(),
                StatusCode::CONFLICT
            );
        }
    }
    for query in [
        "limit=0",
        "limit=51",
        "before=bad",
        "before=scan:not-a-uuid",
        "before=unknown:00000000-0000-0000-0000-000000000000",
        "limit=4294967296",
        "history=invalid",
    ] {
        assert!(
            fixture
                .send(
                    "GET",
                    &format!("/api/v1/admin/imports?{query}"),
                    "",
                    Some(&admin),
                    None
                )
                .await
                .status()
                .is_client_error()
        );
    }
    for state in [
        "queued",
        "converting",
        "needs_review",
        "queued_publication",
        "publishing",
    ] {
        sqlx::query("UPDATE conversion_jobs SET state=? WHERE id=?")
            .bind(state)
            .bind(&conversions[0])
            .execute(&fixture.pool)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .send(
                    "POST",
                    &archive_path("conversion", &conversions[0]),
                    "{\"archived\":true}",
                    Some(&admin),
                    Some(&session.csrf_token)
                )
                .await
                .status(),
            StatusCode::CONFLICT
        );
    }
    sqlx::query("UPDATE conversion_jobs SET state='needs_review' WHERE id=?")
        .bind(&conversions[0])
        .execute(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .send(
                "POST",
                &archive_path("unknown", &scans[3]),
                "{\"archived\":true}",
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                &archive_path("scan", &uuid::Uuid::new_v4().to_string()),
                "{\"archived\":true}",
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let read = |response: Response| async {
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice::<ImportPage>(
            &to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
        )
        .unwrap()
    };
    let initial = read(
        fixture
            .send("GET", "/api/v1/admin/imports", "", Some(&admin), None)
            .await,
    )
    .await;
    assert_eq!(
        initial.history_count, 4,
        "consumed scan/probes and published conversion collapse by default"
    );
    assert_eq!(initial.total, 7);
    assert!(initial.entries.iter().any(|entry| matches!(&entry.item, ImportJob::Scan(job) if job.id==scans[3] && job.error.as_deref()==Some("retained error"))));
    assert_eq!(
        fixture
            .send(
                "POST",
                &failed_path,
                "{\"archived\":true}",
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::OK
    );
    let (reloaded_cookie, _) = fixture.login("admin").await;
    let reloaded = read(
        fixture
            .send(
                "GET",
                "/api/v1/admin/imports",
                "",
                Some(&reloaded_cookie),
                None,
            )
            .await,
    )
    .await;
    assert_eq!(reloaded.history_count, 5);
    assert_eq!(reloaded.total, 6);
    let isolated = read(
        fixture
            .send("GET", "/api/v1/admin/imports", "", Some(&other), None)
            .await,
    )
    .await;
    assert_eq!(isolated.history_count, 4);
    assert_eq!(isolated.total, 7);
    let first = read(
        fixture
            .send(
                "GET",
                "/api/v1/admin/imports?history=true&limit=2",
                "",
                Some(&admin),
                None,
            )
            .await,
    )
    .await;
    let older = read(
        fixture
            .send(
                "GET",
                &format!(
                    "/api/v1/admin/imports?history=true&limit=2&before={}",
                    first.next.as_ref().unwrap()
                ),
                "",
                Some(&admin),
                None,
            )
            .await,
    )
    .await;
    assert_eq!(first.total, 5);
    assert_eq!(first.entries.len(), 2);
    assert_eq!(older.entries.len(), 2);
    let key = |entry: &wetube_core::ImportEntry| serde_json::to_string(&entry.item).unwrap();
    assert!(
        first
            .entries
            .iter()
            .all(|entry| !older.entries.iter().any(|old| key(entry) == key(old)))
    );
    let last = read(
        fixture
            .send(
                "GET",
                &format!(
                    "/api/v1/admin/imports?history=true&limit=2&before={}",
                    older.next.as_ref().unwrap()
                ),
                "",
                Some(&admin),
                None,
            )
            .await,
    )
    .await;
    assert_eq!(last.entries.len(), 1);
    assert!(last.next.is_none());
    let missing_cursor = read(
        fixture
            .send(
                "GET",
                &format!("/api/v1/admin/imports?before=scan:{}", uuid::Uuid::new_v4()),
                "",
                Some(&admin),
                None,
            )
            .await,
    )
    .await;
    assert!(missing_cursor.entries.is_empty());
    assert!(missing_cursor.next.is_none());
    assert_eq!(
        fixture
            .send(
                "POST",
                &failed_path,
                "{\"archived\":false}",
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::OK
    );
    let published_path = archive_path("conversion", &conversions[1]);
    assert_eq!(
        fixture
            .send(
                "POST",
                &published_path,
                "{\"archived\":false}",
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::OK
    );
    let restored = read(
        fixture
            .send("GET", "/api/v1/admin/imports", "", Some(&admin), None)
            .await,
    )
    .await;
    assert_eq!(restored.history_count, 3);
    assert_eq!(restored.total, 8);
    // An archived consumed inspection must return to attention when its child fails.
    let consumed_path = archive_path("probe", &probes[2]);
    assert_eq!(
        fixture
            .send(
                "POST",
                &consumed_path,
                "{\"archived\":true}",
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::OK
    );
    sqlx::query("UPDATE conversion_jobs SET state='failed' WHERE id=?")
        .bind(&conversions[0])
        .execute(&fixture.pool)
        .await
        .unwrap();
    let attention = read(
        fixture
            .send("GET", "/api/v1/admin/imports", "", Some(&admin), None)
            .await,
    )
    .await;
    assert!(attention.entries.iter().any(|entry| !entry.can_archive
        && matches!(&entry.item,ImportJob::Probe(job) if job.id==probes[2])));
    assert_eq!(
        fixture
            .send(
                "POST",
                &consumed_path,
                "{\"archived\":true}",
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let retained: (String, String) =
        sqlx::query_as("SELECT attempt_id,error FROM scan_jobs WHERE id=?")
            .bind(&scans[3])
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(
        retained,
        (format!("attempt-{}", scans[3]), "retained error".into())
    );
    assert_eq!(
        wetube_backend::scans::list(&fixture.pool)
            .await
            .unwrap()
            .len(),
        4,
        "legacy list remains unfiltered"
    );
    let consumed_scan = archive_path("scan", &scans[2]);
    assert_eq!(
        fixture
            .send(
                "POST",
                &consumed_scan,
                "{\"archived\":true}",
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::OK
    );
    sqlx::query("UPDATE probe_jobs SET state='failed' WHERE id=?")
        .bind(&probes[1])
        .execute(&fixture.pool)
        .await
        .unwrap();
    let attention = read(
        fixture
            .send("GET", "/api/v1/admin/imports", "", Some(&admin), None)
            .await,
    )
    .await;
    assert!(attention.entries.iter().any(|entry| !entry.can_archive
        && matches!(&entry.item,
        ImportJob::Scan(job) if job.id==scans[2])));
}

#[tokio::test]
async fn simple_import_requires_current_admin_origin_csrf_identity_and_explicit_consent() {
    let mut fixture = Fixture::new().await;
    let artifacts = tempfile::tempdir().unwrap();
    let incoming = artifacts.path().join("incoming");
    let scratch = artifacts.path().join("scratch");
    let media = artifacts.path().join("media");
    std::fs::create_dir(&incoming).unwrap();
    std::fs::create_dir(&scratch).unwrap();
    std::fs::create_dir(&media).unwrap();
    std::fs::write(incoming.join("copy.iso"), b"completed test copy").unwrap();
    std::fs::File::open(incoming.join("copy.iso"))
        .unwrap()
        .sync_all()
        .unwrap();
    fixture.app = wetube_server::router_with_conversions(
        fixture.pool.clone(),
        ORIGIN,
        fixture._directory.path(),
        Some(wetube_backend::incoming::IncomingRoot::open(&incoming).unwrap()),
        Some(wetube_backend::preview::PreviewRoot::open(&media).unwrap()),
        Some(&scratch),
    )
    .await
    .unwrap();
    let (admin, session) = fixture.login("admin").await;
    let (viewer, viewer_session) = fixture.login("viewer").await;
    let identity = wetube_backend::incoming::IncomingRoot::open(&incoming)
        .unwrap()
        .list()
        .unwrap()
        .remove(0)
        .identity;
    let body = serde_json::json!({"name":"copy.iso","identity":identity,"copy_complete":true,"accept_preserve":true});
    let endpoint = "/api/v1/admin/simple-imports";
    for (cookie, csrf, expected) in [
        (None, None, StatusCode::UNAUTHORIZED),
        (
            Some(viewer.as_str()),
            Some(viewer_session.csrf_token.as_str()),
            StatusCode::FORBIDDEN,
        ),
        (Some(admin.as_str()), None, StatusCode::FORBIDDEN),
    ] {
        assert_eq!(
            fixture
                .send("POST", endpoint, &body.to_string(), cookie, csrf)
                .await
                .status(),
            expected
        );
    }
    let cross_origin = Request::builder()
        .method("POST")
        .uri(endpoint)
        .header(header::HOST, "wetube.test")
        .header(header::ORIGIN, "http://other.test")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &admin)
        .header("x-csrf-token", &session.csrf_token)
        .body(Body::from(body.to_string()))
        .unwrap();
    assert_eq!(
        fixture
            .app
            .clone()
            .oneshot(cross_origin)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    for (key, value, expected) in [
        (
            "copy_complete",
            serde_json::json!(false),
            StatusCode::BAD_REQUEST,
        ),
        (
            "accept_preserve",
            serde_json::json!(false),
            StatusCode::BAD_REQUEST,
        ),
        ("identity", serde_json::json!("stale"), StatusCode::CONFLICT),
    ] {
        let mut invalid = body.clone();
        invalid[key] = value;
        assert_eq!(
            fixture
                .send(
                    "POST",
                    endpoint,
                    &invalid.to_string(),
                    Some(&admin),
                    Some(&session.csrf_token)
                )
                .await
                .status(),
            expected
        );
    }
    let mut forged = body.clone();
    forged["inspection"] = serde_json::json!({"title":1});
    assert_eq!(
        fixture
            .send(
                "POST",
                endpoint,
                &forged.to_string(),
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let first = fixture
        .send(
            "POST",
            endpoint,
            &body.to_string(),
            Some(&admin),
            Some(&session.csrf_token),
        )
        .await;
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first: wetube_core::ScanJob =
        serde_json::from_slice(&to_bytes(first.into_body(), 32768).await.unwrap()).unwrap();
    let second = fixture
        .send(
            "POST",
            endpoint,
            &body.to_string(),
            Some(&admin),
            Some(&session.csrf_token),
        )
        .await;
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    let second: wetube_core::ScanJob =
        serde_json::from_slice(&to_bytes(second.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(first.state, wetube_core::ScanState::Queued);
    assert!(
        wetube_backend::probe_jobs::list(&fixture.pool)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        wetube_backend::conversions::list(&fixture.pool)
            .await
            .unwrap()
            .is_empty()
    );
    let advanced = format!("/api/v1/admin/scans/{}/advanced", first.id);
    assert_eq!(
        fixture
            .send("POST", &advanced, "{}", Some(&admin), None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .send(
                "POST",
                &advanced,
                "{}",
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
                &advanced,
                "{}",
                Some(&admin),
                Some(&session.csrf_token)
            )
            .await
            .status(),
        StatusCode::OK
    );
    wetube_backend::simple_imports::advance(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(
        wetube_backend::scans::list(&fixture.pool).await.unwrap()[0]
            .simple_state
            .as_deref(),
        Some("advanced")
    );
}

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
        measured_elapsed_seconds: None,
        stage_portion: None,
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
    for fetched_at in [wetube_backend::now() - 86401, wetube_backend::now() + 3600] {
        let mut stale = publication.clone();
        stale["tmdb"] = serde_json::json!({"id":7,"fetched_at":fetched_at});
        let response = fixture
            .send(
                "POST",
                &publish_path,
                &stale.to_string(),
                Some(&admin),
                Some(&session.csrf_token),
            )
            .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: wetube_core::ApiError =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body.error, wetube_core::TMDB_SELECTION_EXPIRED);
        let stored: (String, Option<String>) =
            sqlx::query_as("SELECT state,publication FROM conversion_jobs WHERE id=?")
                .bind(&job.id)
                .fetch_one(&fixture.pool)
                .await
                .unwrap();
        assert_eq!(stored, ("needs_review".into(), None));
    }
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
    assert!(job.finished_at.is_none());
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
    let completed = job.created_at + 3;
    sqlx::query("UPDATE diagnostic_jobs SET state='succeeded',finished_at=? WHERE id=?")
        .bind(completed)
        .bind(&job.id)
        .execute(&fixture.pool)
        .await
        .unwrap();
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
    assert_eq!(jobs[0].finished_at, Some(completed));
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
