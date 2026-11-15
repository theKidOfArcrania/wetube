use super::{AppState, HttpError, admin, check_csrf, internal};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use wetube_core::{ProcessingQueue, QueueOrder, QueueSettings};

pub(super) async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ProcessingQueue>, HttpError> {
    admin(&state, &headers).await?;
    Ok(Json(
        wetube_backend::queue::snapshot(&state.pool)
            .await
            .map_err(internal)?,
    ))
}

fn stale() -> HttpError {
    HttpError(
        StatusCode::CONFLICT,
        "Queue changed. Refresh and try again.".into(),
    )
}

pub(super) async fn settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<QueueSettings>,
) -> Result<Json<ProcessingQueue>, HttpError> {
    let session = admin(&state, &headers).await?;
    check_csrf(&session, &headers)?;
    if !(1..=16).contains(&request.maximum) {
        return Err(HttpError(
            StatusCode::BAD_REQUEST,
            "Simultaneous jobs must be between 1 and 16.".into(),
        ));
    }
    if !wetube_backend::queue::settings(&state.pool, &request)
        .await
        .map_err(internal)?
    {
        return Err(stale());
    }
    get(State(state), headers).await
}

pub(super) async fn order(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<QueueOrder>,
) -> Result<Json<ProcessingQueue>, HttpError> {
    let session = admin(&state, &headers).await?;
    check_csrf(&session, &headers)?;
    if !wetube_backend::queue::reorder(&state.pool, &request)
        .await
        .map_err(internal)?
    {
        return Err(stale());
    }
    get(State(state), headers).await
}

pub(super) async fn cancel(
    State(state): State<AppState>,
    Path(key): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<ProcessingQueue>, HttpError> {
    let session = admin(&state, &headers).await?;
    check_csrf(&session, &headers)?;
    if !wetube_backend::queue::cancel(&state.pool, key)
        .await
        .map_err(internal)?
    {
        return Err(HttpError(StatusCode::CONFLICT,"Stage already finished or is unavailable. Published movies are unchanged; refresh its status.".into()));
    }
    get(State(state), headers).await
}
