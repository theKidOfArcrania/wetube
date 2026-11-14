use crate::{AppState, HttpError, admin, check_csrf, internal, session};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use wetube_backend::labels::{self, UpdateResult};
use wetube_core::{MovieLabels, UpdateMovieLabels};

pub(crate) async fn catalog(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MovieLabels>, HttpError> {
    session(&state, &headers).await?;
    Ok(Json(labels::catalog(&state.pool).await.map_err(internal)?))
}

pub(crate) async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<MovieLabels>, HttpError> {
    session(&state, &headers).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM movies WHERE id=? AND status='ready')")
            .bind(&id)
            .fetch_one(&state.pool)
            .await
            .map_err(internal)?;
    if !exists {
        return Err(HttpError(StatusCode::NOT_FOUND, "Movie not found".into()));
    }
    Ok(Json(
        labels::for_movie(&state.pool, &id)
            .await
            .map_err(internal)?,
    ))
}

pub(crate) async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<UpdateMovieLabels>,
) -> Result<Json<MovieLabels>, HttpError> {
    let current = admin(&state, &headers).await?;
    check_csrf(&current, &headers)?;
    labels::validate(&request.labels)
        .and_then(|_| labels::validate(&request.expected))
        .map_err(|error| HttpError(StatusCode::BAD_REQUEST, error.to_string()))?;
    match labels::update(&state.pool, &id, &request)
        .await
        .map_err(internal)?
    {
        UpdateResult::Saved(labels) => Ok(Json(labels)),
        UpdateResult::Missing => Err(HttpError(StatusCode::NOT_FOUND, "Movie not found".into())),
        UpdateResult::Conflict => Err(HttpError(
            StatusCode::CONFLICT,
            "Labels changed elsewhere. Review the latest labels and save again.".into(),
        )),
    }
}
