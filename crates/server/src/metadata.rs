use crate::{AppState, HttpError, admin, check_csrf, internal};
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
};
use serde_json::Value;
use sqlx::Row;
use std::{
    io::Read,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path as FilePath,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use wetube_backend::{metadata, now};
use wetube_core::{MetadataMatch, MetadataResults, MetadataSearch, MetadataStatus};

#[derive(Clone)]
pub struct Tmdb {
    client: reqwest::Client,
    base: String,
    slots: Arc<Semaphore>,
    rate: Arc<Mutex<(Instant, usize)>>,
}

impl Tmdb {
    pub fn from_token_file(path: &FilePath) -> anyhow::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| anyhow::anyhow!("Cannot open TMDB token file"))?;
        let attributes = file
            .metadata()
            .map_err(|_| anyhow::anyhow!("Cannot inspect TMDB token file"))?;
        anyhow::ensure!(
            attributes.is_file()
                && attributes.permissions().mode() & 0o077 == 0
                && attributes.len() <= 4096,
            "TMDB token file must be a regular, owner-only file (mode 0600)"
        );
        let mut token = String::new();
        file.take(4097)
            .read_to_string(&mut token)
            .map_err(|_| anyhow::anyhow!("Cannot read TMDB token file"))?;
        anyhow::ensure!(token.len() <= 4096, "Invalid TMDB token file");
        Self::new(token.trim(), "https://api.themoviedb.org/3")
    }

    fn new(token: &str, base: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
            "Invalid TMDB token file"
        );
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow::anyhow!("Invalid TMDB token file"))?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, authorization);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(8))
            .build()
            .map_err(|_| anyhow::anyhow!("Cannot initialize TMDB client"))?;
        Ok(Self {
            client,
            base: base.to_owned(),
            slots: Arc::new(Semaphore::new(2)),
            rate: Arc::new(Mutex::new((Instant::now(), 0))),
        })
    }

    async fn fetch(&self, path: &str, parameters: &[(&str, String)]) -> Result<Value, HttpError> {
        let _slot = self.slots.try_acquire().map_err(|_| limited())?;
        {
            let mut rate = self.rate.lock().map_err(|_| unavailable())?;
            if rate.0.elapsed() >= Duration::from_secs(60) {
                *rate = (Instant::now(), 0);
            }
            if rate.1 >= 30 {
                return Err(limited());
            }
            rate.1 += 1;
        }
        let mut response = self
            .client
            .get(format!("{}{path}", self.base))
            .query(parameters)
            .send()
            .await
            .map_err(|_| unavailable())?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|size| size > 512 * 1024)
        {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if bytes.len() + chunk.len() > 512 * 1024 {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| unavailable())
    }
}

fn unavailable() -> HttpError {
    HttpError(
        StatusCode::BAD_GATEWAY,
        "TMDB is unavailable. Try again later or enter metadata manually.".into(),
    )
}

fn limited() -> HttpError {
    HttpError(
        StatusCode::TOO_MANY_REQUESTS,
        "Metadata lookup busy; try again later.".into(),
    )
}

fn provider(extension: Option<Extension<Tmdb>>) -> Result<Tmdb, HttpError> {
    extension.map(|Extension(client)| client).ok_or_else(|| {
        HttpError(
            StatusCode::SERVICE_UNAVAILABLE,
            "TMDB lookup is disabled; enter metadata manually.".into(),
        )
    })
}

fn movie(value: &Value, fetched_at: i64) -> Option<MetadataMatch> {
    let id = value["id"].as_i64().filter(|id| *id > 0)?;
    let title = value["title"].as_str()?.trim();
    if title.is_empty() || title.len() > 512 {
        return None;
    }
    let description = value["overview"].as_str().unwrap_or("");
    if description.len() > 16384 {
        return None;
    }
    let year = value["release_date"].as_str().and_then(|date| {
        let bytes = date.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return None;
        }
        date.get(..4)?
            .parse::<i32>()
            .ok()
            .filter(|y| (1888..=2100).contains(y))
    });
    Some(MetadataMatch {
        id,
        title: title.into(),
        year,
        description: description.into(),
        fetched_at,
    })
}

pub(crate) async fn status(
    State(state): State<AppState>,
    extension: Option<Extension<Tmdb>>,
    headers: HeaderMap,
) -> Result<Json<MetadataStatus>, HttpError> {
    admin(&state, &headers).await?;
    Ok(Json(MetadataStatus {
        configured: extension.is_some(),
    }))
}

pub(crate) async fn search(
    State(state): State<AppState>,
    extension: Option<Extension<Tmdb>>,
    headers: HeaderMap,
    Json(request): Json<MetadataSearch>,
) -> Result<Json<MetadataResults>, HttpError> {
    let session = admin(&state, &headers).await?;
    check_csrf(&session, &headers)?;
    let query = request.query.trim();
    if query.is_empty()
        || query.len() > 256
        || query.chars().any(char::is_control)
        || request
            .year
            .is_some_and(|year| !(1888..=2100).contains(&year))
    {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "Enter a search title (1–256 bytes) and optional year (1888–2100).".into(),
        ));
    }
    let client = provider(extension)?;
    let mut parameters = vec![
        ("query", query.into()),
        ("language", "en-US".into()),
        ("include_adult", "false".into()),
        ("page", "1".into()),
    ];
    if let Some(year) = request.year {
        parameters.push(("year", year.to_string()));
    }
    let value = client.fetch("/search/movie", &parameters).await?;
    let rows = value["results"].as_array().ok_or_else(unavailable)?;
    let fetched_at = now();
    Ok(Json(MetadataResults {
        movies: rows
            .iter()
            .take(20)
            .filter_map(|row| movie(row, fetched_at))
            .collect(),
    }))
}

pub(crate) async fn refresh(
    State(state): State<AppState>,
    Path(id): Path<String>,
    extension: Option<Extension<Tmdb>>,
    headers: HeaderMap,
) -> Result<Json<MetadataMatch>, HttpError> {
    let session = admin(&state, &headers).await?;
    check_csrf(&session, &headers)?;
    let client = provider(extension)?;
    let tmdb_id: i64 = sqlx::query_scalar(
        "SELECT t.tmdb_id FROM tmdb_metadata t JOIN movies m ON m.id=t.movie_id WHERE m.id=? AND m.status='ready'"
    ).bind(&id).fetch_optional(&state.pool).await.map_err(internal)?
        .ok_or_else(|| HttpError(StatusCode::NOT_FOUND, "Linked movie not found".into()))?;
    let value = client
        .fetch(
            &format!("/movie/{tmdb_id}"),
            &[("language", "en-US".into())],
        )
        .await?;
    let metadata = movie(&value, now())
        .filter(|m| m.id == tmdb_id)
        .ok_or_else(unavailable)?;
    let mut transaction = state.pool.begin().await.map_err(internal)?;
    let current = sqlx::query("SELECT t.tmdb_id FROM tmdb_metadata t JOIN movies m ON m.id=t.movie_id WHERE m.id=? AND m.status='ready'")
        .bind(&id).fetch_optional(&mut *transaction).await.map_err(internal)?;
    if current.is_none_or(|row| row.get::<i64, _>("tmdb_id") != tmdb_id) {
        return Err(HttpError(
            StatusCode::CONFLICT,
            "Movie link changed; reload the library.".into(),
        ));
    }
    metadata::store(&mut transaction, &id, &metadata)
        .await
        .map_err(internal)?;
    transaction.commit().await.map_err(internal)?;
    Ok(Json(metadata))
}

pub(crate) async fn genres(
    State(state): State<AppState>,
    Path(id): Path<String>,
    extension: Option<Extension<Tmdb>>,
    headers: HeaderMap,
) -> Result<Json<wetube_core::GenreSuggestions>, HttpError> {
    let current = admin(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    let tmdb_id: i64 = sqlx::query_scalar(
        "SELECT t.tmdb_id FROM tmdb_metadata t JOIN movies m ON m.id=t.movie_id WHERE m.id=? AND m.status='ready'"
    ).bind(id).fetch_optional(&state.pool).await.map_err(internal)?
        .ok_or_else(|| HttpError(StatusCode::NOT_FOUND, "Select TMDB metadata for this movie first.".into()))?;
    let value = provider(extension)?
        .fetch(
            &format!("/movie/{tmdb_id}"),
            &[("language", "en-US".into())],
        )
        .await?;
    if value["id"].as_i64() != Some(tmdb_id) {
        return Err(unavailable());
    }
    let genres = value["genres"]
        .as_array()
        .filter(|genres| genres.len() <= 32)
        .ok_or_else(unavailable)?;
    let names = genres
        .iter()
        .map(|genre| genre["name"].as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(unavailable)?;
    let categories = wetube_backend::labels::normalize(&names).map_err(|_| unavailable())?;
    Ok(Json(wetube_core::GenreSuggestions { categories }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::Query,
        http::{Request, Uri},
        response::IntoResponse,
        routing::get,
    };
    use serde_json::json;
    use tower::ServiceExt;

    type Calls = Arc<Mutex<Vec<(String, String)>>>;
    const SESSION_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn token_file_requires_private_regular_file_and_errors_never_echo_credentials()
    -> anyhow::Result<()> {
        // Windows-backed mounts can acknowledge chmod without enforcing it.
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        let path = directory.path().join("fake-token");
        std::fs::write(&path, "synthetic-token\n")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        assert!(Tmdb::from_token_file(&path).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        assert!(Tmdb::from_token_file(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        std::fs::write(&path, "synthetic-token\ninvalid")?;
        let error = Tmdb::from_token_file(&path).err().unwrap().to_string();
        assert_eq!(error, "Invalid TMDB token file");
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&path, &link)?;
        assert!(Tmdb::from_token_file(&link).is_err());
        Ok(())
    }

    async fn mock(
        State(calls): State<Calls>,
        uri: Uri,
        headers: HeaderMap,
        Query(query): Query<std::collections::HashMap<String, String>>,
    ) -> axum::response::Response {
        calls.lock().unwrap().push((
            uri.to_string(),
            headers[header::AUTHORIZATION].to_str().unwrap().into(),
        ));
        if uri.path() == "/movie/8" {
            return Json(json!({"id":8,"genres":[{"name":"bad\nlabel"}]})).into_response();
        }
        if uri.path() == "/movie/9" {
            return (StatusCode::UNAUTHORIZED, "private provider detail").into_response();
        }
        match query.get("query").map(String::as_str) {
            Some("failure") => {
                (StatusCode::UNAUTHORIZED, "private provider detail").into_response()
            }
            Some("redirect") => {
                (StatusCode::FOUND, [(header::LOCATION, "/must-not-follow")]).into_response()
            }
            Some("oversized") => "x".repeat(512 * 1024 + 1).into_response(),
            Some("slow") => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Json(json!({})).into_response()
            }
            Some("missing") => Json(json!({"unexpected":"private body"})).into_response(),
            Some("empty") => Json(json!({"results":[]})).into_response(),
            Some("many") => Json(json!({"results": (1..=25).map(|id| json!({"id":id,"title":"Bounded result"})).collect::<Vec<_>>()})).into_response(),
            _ => {
                let movie = json!({"id":7,"title":"Translated title","release_date":"2001-02-03","overview":"A synopsis",
                    "genres":[{"id":18,"name":"Drama"},{"id":1,"name":" Family "},{"id":2,"name":"family"}]});
                if uri.path().starts_with("/movie/") {
                    Json(movie).into_response()
                } else {
                    Json(json!({"results":[movie,{"id":0,"title":"Bad"},{"id":8},{"id":9,"title":"Unknown year","release_date":"","overview":null}]})).into_response()
                }
            }
        }
    }

    async fn send(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::HOST, "wetube.test")
                    .header(header::ORIGIN, "http://wetube.test")
                    .header(header::COOKIE, format!("wetube_session={SESSION_TOKEN}"))
                    .header("x-csrf-token", "csrf")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        (
            status,
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn mock_provider_search_refresh_privacy_and_failures() -> anyhow::Result<()> {
        let calls: Calls = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let mock = Router::new().fallback(get(mock)).with_state(calls.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });
        let client = Tmdb::new("not-a-real-token", &format!("http://{address}"))?;
        let directory = tempfile::tempdir()?;
        wetube_backend::db::initialize(directory.path()).await?;
        let pool = wetube_backend::db::connect(directory.path()).await?;
        sqlx::query("INSERT INTO users VALUES('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO sessions VALUES(?,'admin','csrf',?)")
            .bind(wetube_backend::accounts::token_hash(SESSION_TOKEN))
            .bind(now() + 600)
            .execute(&pool)
            .await?;
        let app = crate::router(pool.clone(), "http://wetube.test", directory.path())
            .await?
            .layer(Extension(client.clone()));
        let (status, body) = send(&app, "GET", "/api/v1/admin/metadata", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"configured":true}));
        assert!(calls.lock().unwrap().is_empty());
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/admin/metadata/search",
            json!({"query":" Amélie & me ","year":2001}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let results: MetadataResults = serde_json::from_value(body)?;
        assert_eq!(results.movies.len(), 2);
        assert_eq!(results.movies[0].title, "Translated title");
        assert_eq!(results.movies[0].year, Some(2001));
        assert_eq!(results.movies[1].year, None);
        assert_eq!(results.movies[1].description, "");
        assert!((now() - 2..=now()).contains(&results.movies[0].fetched_at));
        let (uri, authorization) = calls.lock().unwrap()[0].clone();
        assert_eq!(authorization, "Bearer not-a-real-token");
        let url = url::Url::parse(&format!("http://provider{uri}"))?;
        let parameters: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(url.path(), "/search/movie");
        assert_eq!(parameters.len(), 5);
        for (key, value) in [
            ("query", "Amélie & me"),
            ("year", "2001"),
            ("language", "en-US"),
            ("include_adult", "false"),
            ("page", "1"),
        ] {
            assert_eq!(parameters.get(key).unwrap(), value);
        }
        for query in ["failure", "redirect", "oversized", "missing"] {
            let (status, body) = send(
                &app,
                "POST",
                "/api/v1/admin/metadata/search",
                json!({"query":query,"year":null}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(
                body,
                json!({"error":"TMDB is unavailable. Try again later or enter metadata manually."})
            );
        }
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/admin/metadata/search",
            json!({"query":"empty","year":null}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"movies":[]}));
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/admin/metadata/search",
            json!({"query":"many","year":null}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["movies"].as_array().unwrap().len(), 20);
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|(uri, _)| uri.contains("must-not-follow"))
        );
        sqlx::query("INSERT INTO movies VALUES('movie','local.iso',NULL,'','ready')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO tmdb_metadata VALUES('movie',7,NULL,NULL,NULL,1,1)")
            .execute(&pool)
            .await?;
        let before = calls.lock().unwrap().len();
        let (status, _) = send(&app, "GET", "/api/v1/movies", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(calls.lock().unwrap().len(), before);
        let (status, body) = send(
            &app,
            "POST",
            "/api/v1/admin/movies/movie/genre-suggestions",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"categories":["Drama","Family"]}));
        assert_eq!(
            wetube_backend::labels::for_movie(&pool, "movie").await?,
            wetube_core::MovieLabels::default()
        );
        assert!(
            sqlx::query_scalar::<_, Option<String>>("SELECT title FROM tmdb_metadata")
                .fetch_one(&pool)
                .await?
                .is_none()
        );
        for id in [8, 9] {
            sqlx::query("UPDATE tmdb_metadata SET tmdb_id=?")
                .bind(id)
                .execute(&pool)
                .await?;
            let (status, body) = send(
                &app,
                "POST",
                "/api/v1/admin/movies/movie/genre-suggestions",
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(
                body,
                json!({"error":"TMDB is unavailable. Try again later or enter metadata manually."})
            );
            assert_eq!(
                wetube_backend::labels::for_movie(&pool, "movie").await?,
                wetube_core::MovieLabels::default()
            );
        }
        sqlx::query("UPDATE tmdb_metadata SET tmdb_id=7")
            .execute(&pool)
            .await?;
        let (status, _) = send(
            &app,
            "POST",
            "/api/v1/admin/movies/absent/genre-suggestions",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(
            &app,
            "POST",
            "/api/v1/admin/movies/movie/metadata/refresh",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let row = sqlx::query("SELECT t.title,t.expires_at,m.title fallback FROM tmdb_metadata t JOIN movies m ON m.id=t.movie_id").fetch_one(&pool).await?;
        assert_eq!(row.get::<String, _>("title"), "Translated title");
        assert_eq!(row.get::<String, _>("fallback"), "local.iso");
        assert!(row.get::<i64, _>("expires_at") >= now() + metadata::CACHE_SECONDS - 2);
        assert_eq!(
            calls.lock().unwrap().last().unwrap().0,
            "/movie/7?language=en-US"
        );
        let slot = client.slots.acquire_many(2).await?;
        assert!(matches!(
            client.fetch("/search/movie", &[]).await,
            Err(HttpError(StatusCode::TOO_MANY_REQUESTS, _))
        ));
        drop(slot);
        client.rate.lock().unwrap().1 = 30;
        assert!(matches!(
            client.fetch("/search/movie", &[]).await,
            Err(HttpError(StatusCode::TOO_MANY_REQUESTS, _))
        ));
        let mut timeout_client = Tmdb::new("fake", &format!("http://{address}"))?;
        timeout_client.client = reqwest::Client::builder()
            .default_headers(
                [(
                    header::AUTHORIZATION,
                    HeaderValue::from_static("Bearer fake"),
                )]
                .into_iter()
                .collect(),
            )
            .timeout(Duration::from_millis(20))
            .build()?;
        assert!(matches!(
            timeout_client
                .fetch("/search/movie", &[("query", "slow".into())])
                .await,
            Err(HttpError(StatusCode::BAD_GATEWAY, _))
        ));
        pool.close().await;
        server.abort();
        let _ = server.await;
        Ok(())
    }
}
