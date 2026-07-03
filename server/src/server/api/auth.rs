// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{extract::State, http::StatusCode, response::IntoResponse, Extension, Json};
use serde::{Deserialize, Serialize};

use std::sync::Arc;

use crate::server::auth::{self, JwtSecretState};
use crate::server::AppState;
use depot_core::auth::AuthenticatedUser;
use depot_core::error::{DepotError, Retryability};
use depot_core::service;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct LoginResponse {
    pub token: String,
    pub must_change_password: bool,
}

/// Authenticate with username and password, returning a JWT token.
#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Login successful", body = LoginResponse),
        (status = 401, description = "Invalid credentials")
    ),
    tag = "auth"
)]
pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    let user = match state
        .auth
        .backend
        .authenticate(&req.username, &req.password)
        .await
    {
        Ok(Some(u)) => u,
        Ok(None) => {
            tracing::warn!(username = %req.username, "audit: login failed");
            return Err(DepotError::Unauthorized("invalid credentials".into()));
        }
        Err(_) => {
            tracing::warn!(username = %req.username, "audit: login failed");
            return Err(DepotError::Unauthorized("invalid credentials".into()));
        }
    };

    let jwt = state.auth.jwt_secret.load();
    let expiry_secs = state.settings.load().jwt_expiry_secs;
    let token = auth::create_user_token(
        &user.username,
        &jwt.current,
        &user.token_secret,
        expiry_secs,
        user.roles.clone(),
    )
    .map_err(|e| DepotError::Storage(Box::new(e), Retryability::Permanent))?;

    Ok(Json(LoginResponse {
        token,
        must_change_password: user.must_change_password,
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ChangePasswordRequest {
    pub username: String,
    #[serde(default)]
    pub current_password: Option<String>,
    pub new_password: String,
}

/// Change a user's password. Requires authentication. Any authenticated user
/// can change their own password (must provide current password); admins can
/// change any user's password without providing the current password.
#[utoipa::path(
    post,
    path = "/api/v1/auth/change-password",
    request_body = ChangePasswordRequest,
    responses(
        (status = 200, description = "Password changed (own account), returns new token", body = LoginResponse),
        (status = 204, description = "Password changed (admin changing another user)"),
        (status = 403, description = "Current password incorrect or not permitted")
    ),
    tag = "auth"
)]
pub async fn change_password(
    State(state): State<AppState>,
    Extension(caller): Extension<AuthenticatedUser>,
    Json(req): Json<ChangePasswordRequest>,
) -> impl IntoResponse {
    // Must be the same user or an admin.
    let is_admin_changing_other = if caller.0 != req.username {
        if let Err(e) = state.auth.backend.require_admin(&caller.0).await {
            tracing::warn!(target_user = %req.username, "audit: password change denied");
            return e.into_response();
        }
        true
    } else {
        false
    };

    let username = req.username.clone();
    let (record, hash) = match service::get_user(state.repo.kv.as_ref(), &username).await {
        Ok(Some(u)) => {
            let h = u.password_hash.clone();
            (Some(u), h)
        }
        _ => (None, auth::dummy_hash().to_string()),
    };

    // Admins changing another user's password skip current-password verification.
    let record = if is_admin_changing_other {
        match record {
            Some(r) => r,
            None => {
                // Run a dummy verify so timing is consistent.
                auth::verify_password(String::new(), hash).await;
                return DepotError::Forbidden("current password is incorrect".into())
                    .into_response();
            }
        }
    } else {
        let current = req.current_password.clone().unwrap_or_default();
        match record {
            Some(r) if auth::verify_password(current, hash).await => r,
            _ => {
                return DepotError::Forbidden("current password is incorrect".into())
                    .into_response()
            }
        }
    };

    if record.auth_source != "builtin" {
        return DepotError::BadRequest(
            "cannot change password for externally authenticated user".into(),
        )
        .into_response();
    }
    let new_hash = match auth::hash_password(req.new_password.clone()).await {
        Ok(h) => h,
        Err(e) => return e.into_response(),
    };
    let now = crate::server::repo::now_utc();
    let username_for_txn = req.username.clone();
    {
        let mut record = record;
        record.password_hash = new_hash;
        record.token_secret = auth::generate_token_secret();
        record.must_change_password = false;
        record.updated_at = now;
        let new_token_secret = record.token_secret.clone();
        match service::put_user(state.repo.kv.as_ref(), &record).await {
            Ok(_) => {
                tracing::warn!(target_user = %username_for_txn, changed_by = %caller.0, "audit: password changed");
                state.auth.backend.invalidate_user(&username_for_txn);
                // When changing your own password, return a fresh token so the
                // caller stays logged in despite the token_secret rotation.
                if caller.0 == username_for_txn {
                    let jwt = state.auth.jwt_secret.load();
                    let expiry_secs = state.settings.load().jwt_expiry_secs;
                    match auth::create_user_token(
                        &username_for_txn,
                        &jwt.current,
                        &new_token_secret,
                        expiry_secs,
                        record.roles.clone(),
                    ) {
                        Ok(token) => Json(LoginResponse {
                            token,
                            must_change_password: false,
                        })
                        .into_response(),
                        Err(e) => DepotError::Storage(Box::new(e), Retryability::Permanent)
                            .into_response(),
                    }
                } else {
                    StatusCode::NO_CONTENT.into_response()
                }
            }
            Err(e) => e.into_response(),
        }
    }
}

/// Revoke all outstanding JWT tokens by replacing the global signing secret.
/// The previous secret is discarded so no existing tokens will validate.
#[utoipa::path(
    post,
    path = "/api/v1/auth/revoke-all",
    responses(
        (status = 204, description = "All tokens revoked"),
        (status = 403, description = "Admin access required")
    ),
    tag = "auth"
)]
pub async fn revoke_all_tokens(
    State(state): State<AppState>,
    Extension(caller): Extension<AuthenticatedUser>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&caller.0).await {
        return e.into_response();
    }

    let new_state = JwtSecretState {
        current: auth::generate_token_secret(),
        previous: None,
        rotated_at: chrono::Utc::now().timestamp(),
    };

    let serialized = match rmp_serde::to_vec(&new_state) {
        Ok(v) => v,
        Err(e) => {
            return DepotError::Internal(format!("failed to serialize JWT state: {e}"))
                .into_response()
        }
    };

    if let Err(e) = service::put_meta(state.repo.kv.as_ref(), "jwt_secret", &serialized).await {
        return e.into_response();
    }

    state.auth.jwt_secret.store(Arc::new(new_state));
    tracing::warn!(admin = %caller.0, "audit: all JWT tokens revoked");
    StatusCode::NO_CONTENT.into_response()
}
