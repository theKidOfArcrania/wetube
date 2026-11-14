use crate::{AppState, HttpError, internal, metadata::Tmdb, session};
use anyhow::{Result, ensure};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use sqlx::{Row, SqlitePool};
use std::{io::Cursor, time::Duration};
use wetube_backend::{metadata::CACHE_SECONDS, now};

const MAX_BYTES: usize = 1024 * 1024;

fn valid_path(path: &str) -> bool {
    let Some(name) = path.strip_prefix('/') else {
        return false;
    };
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && stem.len() <= 128
        && stem.bytes().all(|b| b.is_ascii_alphanumeric())
        && matches!(extension, "jpg" | "png")
}

fn validate(bytes: &[u8], mime: &str) -> Result<()> {
    ensure!(bytes.len() <= MAX_BYTES, "poster too large");
    let format = match mime {
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/png" => image::ImageFormat::Png,
        _ => anyhow::bail!("unsupported poster type"),
    };
    ensure!(
        image::guess_format(bytes)? == format,
        "poster type mismatch"
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(342);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(8 * 1024 * 1024);
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits.clone());
    let (width, height) = reader.into_dimensions()?;
    ensure!(
        width > 0 && width <= 342 && height > 0 && height <= 2048,
        "poster dimensions out of bounds"
    );
    // Header checks alone would accept truncated or otherwise invalid images.
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    reader.decode()?;
    Ok(())
}

pub(crate) async fn acquire(
    client: &Tmdb,
    pool: &SqlitePool,
    id: i64,
    details: Option<&serde_json::Value>,
) -> Result<()> {
    let _slot = client.poster_slots.try_acquire()?;
    // One bounded operation: no background retries or provider calls on reads.
    tokio::time::timeout(Duration::from_secs(6), async {
        let fetched;
        let value = match details {
            Some(value) => value,
            None => {
                fetched = client
                    .fetch(&format!("/movie/{id}"), &[])
                    .await
                    .map_err(|_| anyhow::anyhow!("poster metadata unavailable"))?;
                &fetched
            }
        };
        ensure!(value["id"].as_i64() == Some(id), "poster movie mismatch");
        let Some(path) = value["poster_path"].as_str() else {
            sqlx::query("DELETE FROM tmdb_posters WHERE tmdb_id=?")
                .bind(id)
                .execute(pool)
                .await?;
            return Ok(());
        };
        ensure!(valid_path(path), "invalid poster path");
        let mut response = client
            .images
            .get(format!("{}{path}", client.image_base))
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK, "poster unavailable");
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= MAX_BYTES as u64),
            "poster too large"
        );
        let mime = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(bytes.len() + chunk.len() <= MAX_BYTES, "poster too large");
            bytes.extend_from_slice(&chunk);
        }
        validate(&bytes, &mime)?;
        let time = now();
        sqlx::query(
            "INSERT INTO tmdb_posters VALUES(?,?,?,?,?)
            ON CONFLICT(tmdb_id) DO UPDATE SET image=excluded.image,mime=excluded.mime,
            fetched_at=excluded.fetched_at,expires_at=excluded.expires_at",
        )
        .bind(id)
        .bind(bytes)
        .bind(mime)
        .bind(time)
        .bind(time + CACHE_SECONDS)
        .execute(pool)
        .await?;
        Ok(())
    })
    .await?
}

pub(crate) async fn read(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    session(&state, &headers).await?;
    let row = sqlx::query(
        "SELECT p.image,p.mime FROM movies m JOIN tmdb_metadata t ON t.movie_id=m.id
         JOIN tmdb_posters p ON p.tmdb_id=t.tmdb_id
         WHERE m.id=? AND m.status='ready' AND t.expires_at>? AND p.expires_at>?",
    )
    .bind(id)
    .bind(now())
    .bind(now())
    .fetch_optional(&state.pool)
    .await
    .map_err(internal)?;
    let mut response = if let Some(row) = row {
        let mut response = Response::new(Body::from(row.get::<Vec<u8>, _>("image")));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            row.get::<String, _>("mime").parse().map_err(internal)?,
        );
        response
    } else {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_FOUND;
        response
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Extension, Router, body::to_bytes, http::Request, response::IntoResponse, routing::get,
    };
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;
    use wetube_core::MetadataMatch;

    type Calls = Arc<Mutex<Vec<String>>>;
    const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const VIEWER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(width, height)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    async fn mock(
        State(calls): State<Calls>,
        Path(path): Path<String>,
        headers: HeaderMap,
    ) -> Response {
        calls.lock().unwrap().push(path.clone());
        if let Some(id) = path.strip_prefix("movie/") {
            assert_eq!(headers[header::AUTHORIZATION], "Bearer synthetic");
            let id: i64 = id.parse().unwrap();
            let poster = match id {
                2 => Some("/redirect.png"),
                3 => Some("/html.png"),
                4 => Some("/oversize.png"),
                5 => Some("/../secret.png"),
                6 => None,
                8 => Some("/pixels.png"),
                _ => Some("/valid.png"),
            };
            return axum::Json(json!({
                "id": if id == 7 { 99 } else { id },
                "title": "Provider title", "overview": "Description", "poster_path": poster
            }))
            .into_response();
        }
        assert!(!headers.contains_key(header::AUTHORIZATION));
        match path.as_str() {
            "w342/redirect.png" => {
                (StatusCode::FOUND, [(header::LOCATION, "/must-not-follow")]).into_response()
            }
            "w342/html.png" => (
                [(header::CONTENT_TYPE, "text/html")],
                "<html>not an image</html>",
            )
                .into_response(),
            "w342/oversize.png" => (
                [(header::CONTENT_TYPE, "image/png")],
                vec![0_u8; MAX_BYTES + 1],
            )
                .into_response(),
            "w342/pixels.png" => {
                ([(header::CONTENT_TYPE, "image/png")], png(343, 10)).into_response()
            }
            "w342/valid.png" => ([(header::CONTENT_TYPE, "image/png")], png(2, 3)).into_response(),
            _ => panic!("unexpected poster request"),
        }
    }

    async fn request(
        app: &Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        csrf: bool,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "wetube.test")
            .header(header::ORIGIN, "http://wetube.test");
        if let Some(token) = token {
            request = request.header(header::COOKIE, format!("wetube_session={token}"));
        }
        if csrf {
            request = request.header("x-csrf-token", "csrf");
        }
        app.clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[test]
    fn poster_paths_and_decoded_images_are_strictly_bounded() {
        for path in [
            "https://evil/a.jpg",
            "//evil/a.jpg",
            "/../a.jpg",
            "/a%2fb.jpg",
            "/a/b.jpg",
            "/a.svg",
            "/a.jpg?x=y",
            "/a-b.jpg",
            "/.png",
        ] {
            assert!(!valid_path(path), "{path}");
        }
        assert!(valid_path("/ABC012xyz.jpg"));
        let bytes = png(2, 3);
        assert!(validate(&bytes, "image/png").is_ok());
        assert!(validate(&bytes, "image/jpeg").is_err());
        assert!(validate(b"<svg/>", "image/png").is_err());
        assert!(validate(&bytes[..bytes.len() / 2], "image/png").is_err());
        assert!(validate(&png(343, 1), "image/png").is_err());
        assert!(validate(&png(1, 2049), "image/png").is_err());
    }

    #[tokio::test]
    async fn posters_follow_only_accepted_linked_publication_and_never_block_it() -> Result<()> {
        let calls: Calls = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let mock = Router::new()
            .route("/{*path}", get(mock))
            .with_state(calls.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });
        let mut client = Tmdb::new("synthetic", &format!("http://{address}"))?;
        client.image_base = format!("http://{address}/w342");
        let directory = tempfile::tempdir()?;
        let assets = tempfile::tempdir()?;
        let media = assets.path().join("media");
        let scratch = assets.path().join("scratch");
        std::fs::create_dir(&media)?;
        std::fs::create_dir(&scratch)?;
        wetube_backend::db::initialize(directory.path()).await?;
        let pool = wetube_backend::db::connect(directory.path()).await?;
        sqlx::query("INSERT INTO users VALUES('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO sessions VALUES(?,'admin','csrf',?)")
            .bind(wetube_backend::accounts::token_hash(ADMIN))
            .bind(now() + 600)
            .execute(&pool)
            .await?;
        let plain = crate::router_with_conversions(
            pool.clone(),
            "http://wetube.test",
            directory.path(),
            None,
            Some(wetube_backend::preview::PreviewRoot::open(&media)?),
            Some(&scratch),
        )
        .await?;
        let configured = plain.clone().layer(Extension(client.clone()));
        for id in 1..=4 {
            let job = uuid::Uuid::new_v4().to_string();
            sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at)
                VALUES(?,'admin','source.iso','bound',1,1,'needs_review',1)").bind(&job).execute(&pool).await?;
            sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment,movie_sha256)
                VALUES(?,?,'admin','needs_review',1,'preserve',?)").bind(&job).bind(&job).bind("a".repeat(64)).execute(&pool).await?;
            let before = calls.lock().unwrap().len();
            let app = if id == 3 { &plain } else { &configured };
            for valid in [false, true] {
                let body = json!({
                    "title": "Reviewed", "description": "", "year": null, "confirm": true,
                    "movie_sha256": if valid { "a".repeat(64) } else { "b".repeat(64) },
                    "tmdb": if id == 4 { serde_json::Value::Null } else { json!({"id": id, "fetched_at": now()}) }
                });
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(format!("/api/v1/admin/conversions/{job}/publish"))
                            .header(header::HOST, "wetube.test")
                            .header(header::ORIGIN, "http://wetube.test")
                            .header(header::COOKIE, format!("wetube_session={ADMIN}"))
                            .header(header::CONTENT_TYPE, "application/json")
                            .header("x-csrf-token", "csrf")
                            .body(Body::from(body.to_string()))?,
                    )
                    .await?;
                assert_eq!(
                    response.status(),
                    if valid {
                        StatusCode::ACCEPTED
                    } else {
                        StatusCode::CONFLICT
                    }
                );
                if !valid {
                    assert_eq!(calls.lock().unwrap().len(), before);
                }
            }
            assert_eq!(
                sqlx::query_scalar::<_, String>("SELECT state FROM conversion_jobs WHERE id=?")
                    .bind(&job)
                    .fetch_one(&pool)
                    .await?,
                "queued_publication"
            );
            if id >= 3 {
                assert_eq!(calls.lock().unwrap().len(), before);
            }
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT tmdb_id FROM tmdb_posters")
                .fetch_all(&pool)
                .await?,
            vec![1]
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tmdb_metadata")
                .fetch_one(&pool)
                .await?,
            0
        );
        pool.close().await;
        server.abort();
        let _ = server.await;
        Ok(())
    }

    #[tokio::test]
    async fn posters_explicit_refresh_private_reads_failures_and_retention() -> Result<()> {
        let calls: Calls = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let mock = Router::new()
            .route("/{*path}", get(mock))
            .with_state(calls.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });
        let mut client = Tmdb::new("synthetic", &format!("http://{address}"))?;
        client.image_base = format!("http://{address}/w342");
        let directory = tempfile::tempdir()?;
        wetube_backend::db::initialize(directory.path()).await?;
        let pool = wetube_backend::db::connect(directory.path()).await?;
        for (id, role, token) in [("admin", "admin", ADMIN), ("viewer", "viewer", VIEWER)] {
            sqlx::query("INSERT INTO users VALUES(?,?, 'hash',?)")
                .bind(id)
                .bind(id)
                .bind(role)
                .execute(&pool)
                .await?;
            sqlx::query("INSERT INTO sessions VALUES(?,?,'csrf',?)")
                .bind(wetube_backend::accounts::token_hash(token))
                .bind(id)
                .bind(now() + 600)
                .execute(&pool)
                .await?;
        }
        for id in ["movie", "unlinked", "shared"] {
            sqlx::query("INSERT INTO movies VALUES(?,'Local',NULL,'','ready')")
                .bind(id)
                .execute(&pool)
                .await?;
        }
        let metadata = MetadataMatch {
            id: 1,
            title: "Provider".into(),
            year: None,
            description: "".into(),
            fetched_at: now(),
        };
        for id in ["movie", "shared"] {
            wetube_backend::metadata::store(&mut *pool.acquire().await?, id, &metadata).await?;
        }
        let app = crate::router(pool.clone(), "http://wetube.test", directory.path())
            .await?
            .layer(Extension(client.clone()));
        for (token, csrf, expected) in [
            (None, true, StatusCode::UNAUTHORIZED),
            (Some(VIEWER), true, StatusCode::FORBIDDEN),
            (Some(ADMIN), false, StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                request(
                    &app,
                    "POST",
                    "/api/v1/admin/movies/movie/metadata/refresh",
                    token,
                    csrf
                )
                .await
                .status(),
                expected
            );
        }
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(
            request(&app, "GET", "/api/v1/movies/movie/poster", None, false)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(
                &app,
                "GET",
                "/api/v1/movies/movie/poster",
                Some(VIEWER),
                false
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        let response = request(&app, "GET", "/api/v1/movies", Some(VIEWER), false).await;
        let library: wetube_core::Library =
            serde_json::from_slice(&to_bytes(response.into_body(), MAX_BYTES).await?)?;
        assert!(
            library
                .movies
                .iter()
                .all(|movie| movie.poster_expires_at.is_none())
        );
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/v1/admin/movies/movie/metadata/refresh",
                Some(ADMIN),
                true
            )
            .await
            .status(),
            StatusCode::OK
        );
        let expiry: i64 = sqlx::query_scalar("SELECT expires_at FROM tmdb_posters")
            .fetch_one(&pool)
            .await?;
        let count = calls.lock().unwrap().len();
        let response = request(&app, "GET", "/api/v1/movies", Some(VIEWER), false).await;
        let library: wetube_core::Library =
            serde_json::from_slice(&to_bytes(response.into_body(), MAX_BYTES).await?)?;
        assert_eq!(
            library
                .movies
                .iter()
                .find(|movie| movie.id == "movie")
                .unwrap()
                .poster_expires_at,
            Some(expiry)
        );
        for token in [ADMIN, VIEWER] {
            let response = request(
                &app,
                "GET",
                "/api/v1/movies/movie/poster",
                Some(token),
                false,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            assert_eq!(
                to_bytes(response.into_body(), MAX_BYTES).await?.as_ref(),
                png(2, 3)
            );
        }
        assert_eq!(
            request(
                &app,
                "GET",
                "/api/v1/movies/unlinked/poster",
                Some(VIEWER),
                false
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(calls.lock().unwrap().len(), count);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT expires_at FROM tmdb_posters")
                .fetch_one(&pool)
                .await?,
            expiry
        );
        sqlx::query("UPDATE tmdb_metadata SET expires_at=? WHERE movie_id='movie'")
            .bind(now())
            .execute(&pool)
            .await?;
        assert_eq!(
            request(
                &app,
                "GET",
                "/api/v1/movies/movie/poster",
                Some(VIEWER),
                false
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(
                &app,
                "GET",
                "/api/v1/movies/shared/poster",
                Some(VIEWER),
                false
            )
            .await
            .status(),
            StatusCode::OK
        );
        // A shared cache entry follows the current atomic metadata association, not the movie URL.
        sqlx::query("UPDATE tmdb_metadata SET tmdb_id=2 WHERE movie_id='movie'")
            .execute(&pool)
            .await?;
        assert_eq!(
            request(
                &app,
                "GET",
                "/api/v1/movies/movie/poster",
                Some(VIEWER),
                false
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(
                &app,
                "GET",
                "/api/v1/movies/shared/poster",
                Some(VIEWER),
                false
            )
            .await
            .status(),
            StatusCode::OK
        );
        // Optional CDN failures do not undo a successful metadata refresh.
        for id in [2, 3, 4, 5, 7, 8] {
            assert!(acquire(&client, &pool, id, None).await.is_err());
        }
        assert_eq!(
            request(
                &app,
                "POST",
                "/api/v1/admin/movies/movie/metadata/refresh",
                Some(ADMIN),
                true
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT title FROM tmdb_metadata WHERE movie_id='movie'"
            )
            .fetch_one(&pool)
            .await?,
            "Provider title"
        );
        assert!(acquire(&client, &pool, 6, None).await.is_ok());
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|path| path.contains("must-not-follow") || path.contains("secret"))
        );
        sqlx::query("UPDATE tmdb_posters SET expires_at=?")
            .bind(now())
            .execute(&pool)
            .await?;
        let response = request(&app, "GET", "/api/v1/movies", Some(VIEWER), false).await;
        let library: wetube_core::Library =
            serde_json::from_slice(&to_bytes(response.into_body(), MAX_BYTES).await?)?;
        assert!(
            library
                .movies
                .iter()
                .all(|movie| movie.poster_expires_at.is_none())
        );
        assert_eq!(
            request(
                &app,
                "GET",
                "/api/v1/movies/shared/poster",
                Some(VIEWER),
                false
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        wetube_backend::metadata::expire(&pool, now()).await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tmdb_posters")
                .fetch_one(&pool)
                .await?,
            0
        );
        pool.close().await;
        server.abort();
        let _ = server.await;
        Ok(())
    }
}
