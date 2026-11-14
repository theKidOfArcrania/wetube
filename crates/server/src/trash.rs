use crate::{AppState, HttpError, admin, check_csrf, internal, now, session, session_cookie};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use wetube_backend::{accounts, trash};
use wetube_core::{PlaybackLease, TrashMovie};

#[derive(Default, serde::Deserialize)]
pub(crate) struct PlaybackQuery {
    pub playback: Option<String>,
}

pub(crate) fn session_hash(headers: &HeaderMap) -> Result<String, HttpError> {
    Ok(accounts::token_hash(
        session_cookie(headers).ok_or_else(crate::unauthorized)?,
    ))
}

pub(crate) async fn has_lease(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    query: &PlaybackQuery,
) -> Result<bool, HttpError> {
    match query.playback.as_deref() {
        Some(lease) => trash::valid(&state.pool, id, &session_hash(headers)?, lease, now())
            .await
            .map_err(internal),
        None => Ok(false),
    }
}

fn unavailable() -> HttpError {
    HttpError(
        StatusCode::NOT_FOUND,
        "Movie is unavailable or its recovery period has ended".into(),
    )
}

pub(crate) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<TrashMovie>>, HttpError> {
    admin(&state, &headers).await?;
    Ok(Json(trash::list(&state.pool).await.map_err(internal)?))
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<bool>, HttpError> {
    let user = admin(&state, &headers).await?;
    check_csrf(&user, &headers)?;
    if !trash::delete(&state.pool, &id, now())
        .await
        .map_err(internal)?
    {
        return Err(unavailable());
    }
    Ok(Json(true))
}

pub(crate) async fn restore(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<bool>, HttpError> {
    let user = admin(&state, &headers).await?;
    check_csrf(&user, &headers)?;
    if !trash::restore(&state.pool, &id, now())
        .await
        .map_err(internal)?
    {
        return Err(unavailable());
    }
    Ok(Json(true))
}

pub(crate) async fn start(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<PlaybackLease>, HttpError> {
    let user = session(&state, &headers).await?;
    check_csrf(&user, &headers)?;
    crate::preview(&state, &id).await?;
    Ok(Json(
        trash::authorize(&state.pool, &id, &session_hash(&headers)?, None, now())
            .await
            .map_err(internal)?
            .ok_or_else(unavailable)?,
    ))
}

pub(crate) async fn renew(
    State(state): State<AppState>,
    Path((id, lease)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<PlaybackLease>, HttpError> {
    let user = session(&state, &headers).await?;
    check_csrf(&user, &headers)?;
    Ok(Json(
        trash::authorize(
            &state.pool,
            &id,
            &session_hash(&headers)?,
            Some(&lease),
            now(),
        )
        .await
        .map_err(internal)?
        .ok_or_else(unavailable)?,
    ))
}
