// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Settings API — GET/PUT cluster-wide operational settings.

use axum::{extract::State, response::IntoResponse, Extension, Json};

use crate::server::config::settings::{self, Settings};
use crate::server::AppState;
use depot_core::auth::AuthenticatedUser;
use depot_core::error::DepotError;

/// GET /api/v1/settings — returns current settings JSON (admin only).
#[utoipa::path(
    get,
    path = "/api/v1/settings",
    responses(
        (status = 200, description = "Current settings", body = Settings),
        (status = 403, description = "Admin access required")
    ),
    tag = "settings"
)]
pub async fn get_settings(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }
    let current = (**state.settings.load()).clone();
    Json(current).into_response()
}

/// PUT /api/v1/settings — update settings (admin only).
///
/// The body is merged over the current settings document: fields present in
/// the request (including explicit `null`) replace the stored value; absent
/// fields keep theirs. A partial body therefore updates exactly the fields it
/// names — it can no longer silently reset everything it omits to defaults
/// (a partial PUT once nulled `default_docker_repo` in production and broke
/// every bare `/v2` docker pull until diagnosed).
#[utoipa::path(
    put,
    path = "/api/v1/settings",
    request_body = Settings,
    responses(
        (status = 200, description = "Settings updated", body = Settings),
        (status = 400, description = "Invalid settings"),
        (status = 403, description = "Admin access required")
    ),
    tag = "settings"
)]
pub async fn put_settings(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }

    let Some(patch) = body.as_object() else {
        return DepotError::BadRequest("settings body must be a JSON object".into())
            .into_response();
    };

    // Merge over the current document, then re-validate the whole thing.
    let current = (**state.settings.load()).clone();
    let mut merged = match serde_json::to_value(&current) {
        Ok(serde_json::Value::Object(obj)) => obj,
        _ => {
            return DepotError::Internal("failed to serialize current settings".into())
                .into_response();
        }
    };
    for (k, v) in patch {
        merged.insert(k.clone(), v.clone());
    }
    let new_settings: Settings = match serde_json::from_value(serde_json::Value::Object(merged)) {
        Ok(s) => s,
        Err(e) => {
            return DepotError::BadRequest(format!("invalid settings: {e}")).into_response();
        }
    };

    if let Err(errors) = new_settings.validate() {
        return DepotError::BadRequest(format!("invalid settings: {}", errors.join("; ")))
            .into_response();
    }

    match settings::save_settings_to_kv(state.repo.kv.as_ref(), &new_settings).await {
        Ok(()) => {
            state.rate_limiter.update(new_settings.rate_limit.as_ref());
            state
                .settings
                .store(std::sync::Arc::new(new_settings.clone()));
            state.bg.event_bus.publish(
                crate::server::infra::event_bus::Topic::Settings,
                crate::server::infra::event_bus::ModelEvent::SettingsUpdated {
                    settings: new_settings.clone(),
                },
            );
            Json(new_settings).into_response()
        }
        Err(e) => DepotError::Internal(format!("failed to save settings: {}", e)).into_response(),
    }
}

/// GET /api/v1/settings/ldap — returns current LDAP config (admin only).
#[cfg(feature = "ldap")]
#[utoipa::path(
    get,
    path = "/api/v1/settings/ldap",
    responses(
        (status = 200, description = "Current LDAP configuration"),
        (status = 403, description = "Admin access required")
    ),
    tag = "settings"
)]
pub async fn get_ldap_config(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }
    let config = (**state.auth.ldap_config.load()).clone();
    Json(config).into_response()
}

/// PUT /api/v1/settings/ldap — set LDAP config (admin only).
#[cfg(feature = "ldap")]
#[utoipa::path(
    put,
    path = "/api/v1/settings/ldap",
    responses(
        (status = 200, description = "LDAP configuration updated"),
        (status = 400, description = "Invalid LDAP configuration"),
        (status = 403, description = "Admin access required")
    ),
    tag = "settings"
)]
pub async fn put_ldap_config(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(new_config): Json<crate::server::config::LdapConfig>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }

    if let Err(errors) = settings::validate_ldap_config(&new_config) {
        return DepotError::BadRequest(format!("invalid LDAP config: {}", errors.join("; ")))
            .into_response();
    }

    match settings::save_ldap_config(state.repo.kv.as_ref(), &new_config).await {
        Ok(()) => {
            state.auth.ldap_config.store(Some(new_config.clone()));
            state.auth.backend.invalidate_all();
            Json(new_config).into_response()
        }
        Err(e) => {
            DepotError::Internal(format!("failed to save LDAP config: {}", e)).into_response()
        }
    }
}

/// DELETE /api/v1/settings/ldap — remove LDAP config, revert to builtin auth (admin only).
#[cfg(feature = "ldap")]
#[utoipa::path(
    delete,
    path = "/api/v1/settings/ldap",
    responses(
        (status = 204, description = "LDAP configuration removed"),
        (status = 403, description = "Admin access required")
    ),
    tag = "settings"
)]
pub async fn delete_ldap_config(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }

    match settings::delete_ldap_config(state.repo.kv.as_ref()).await {
        Ok(()) => {
            state.auth.ldap_config.store(None);
            state.auth.backend.invalidate_all();
            axum::http::StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            DepotError::Internal(format!("failed to delete LDAP config: {}", e)).into_response()
        }
    }
}
