//! New releases and updates in the Administration page. Admins only; an
//! API key may look but not change anything.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use starbooru_core::{UpdateInstall, UpdateSettings, UpdateStatus};

use super::admin::require_admin_session;
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::settings;
use crate::state::AppState;
use crate::update;

/// `GET /admin/updates`.
pub async fn status(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<UpdateStatus>> {
    user.require_admin()?;
    update::status(&state).await.map(Json)
}

/// `POST /admin/updates/check`: asks now, at most once a minute.
pub async fn check(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<UpdateStatus>> {
    require_admin_session(&user)?;
    update::check_now(&state).await?;
    update::status(&state).await.map(Json)
}

/// `PUT /admin/updates/settings`.
pub async fn set_settings(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<UpdateSettings>,
) -> AppResult<Json<UpdateStatus>> {
    require_admin_session(&user)?;
    req.check()
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    settings::save_updates(&state, req).await?;
    state.updates.settings_changed();
    update::status(&state).await.map(Json)
}

/// `POST /admin/updates/install`: starts installing the newest release and
/// answers at once; `run` in the answer follows it.
pub async fn install(
    State(state): State<AppState>,
    user: AuthUser,
    req: Option<Json<UpdateInstall>>,
) -> AppResult<(StatusCode, Json<UpdateStatus>)> {
    require_admin_session(&user)?;
    let req = req.map(|Json(r)| r).unwrap_or_default();
    update::install(&state, false, req.backup).await?;
    let status = update::status(&state).await?;
    Ok((StatusCode::ACCEPTED, Json(status)))
}
