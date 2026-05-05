// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};

use depot_core::auth::AuthenticatedUser;
use depot_core::format_state::FormatState;

use super::blobs::{do_get_blob, do_head_blob};
use super::helpers::docker_unauthorized_response;
use super::manifests::{do_delete_manifest, do_get_manifest, do_head_manifest, do_put_manifest};
use super::tags::do_list_tags;
use super::types::{
    CompleteUploadParams, DockerPaginationParams, DockerPath, TokenFormParams, TokenParams,
    UploadInitParams,
};
use super::uploads::{do_complete_upload, do_patch_upload, do_start_upload};

/// `GET /v2/` — Docker V2 API version check.
///
/// Returns 401 with Bearer challenge for anonymous users, 200 for authenticated users.
/// Always includes `Docker-Distribution-Api-Version: registry/2.0` header.
pub async fn v2_check(
    uri: axum::http::Uri,
    req_headers: HeaderMap,
    Extension(user): Extension<AuthenticatedUser>,
) -> impl IntoResponse {
    if user.0 == "anonymous" {
        return docker_unauthorized_response(&req_headers, uri.authority().map(|a| a.as_str()));
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        "Docker-Distribution-Api-Version",
        HeaderValue::from_static("registry/2.0"),
    );
    (StatusCode::OK, headers, "{}").into_response()
}

/// `GET /v2/token` — Docker V2 token endpoint.
///
/// Accepts Basic auth credentials and returns a JWT token.
/// This endpoint handles its own authentication and does not go through
/// the `resolve_identity` middleware.
pub async fn v2_token(
    State(state): State<FormatState>,
    headers: HeaderMap,
    Query(_params): Query<TokenParams>,
) -> impl IntoResponse {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let (username, password) = match auth_header.and_then(depot_core::auth::decode_basic_auth) {
        Some(creds) => creds,
        None => {
            return issue_anonymous_token(&state).await;
        }
    };

    issue_token(&state, &username, &password).await
}

/// `POST /v2/token` — OAuth2 / Docker V2 token endpoint.
///
/// Containerd and other OCI clients send credentials as
/// `application/x-www-form-urlencoded` body with `grant_type=password`.
/// Also falls back to Basic auth header if the form body has no credentials.
pub async fn v2_token_post(
    State(state): State<FormatState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Try to parse form-encoded credentials from the body.
    if let Ok(form) = serde_urlencoded::from_bytes::<TokenFormParams>(&body) {
        if let (Some(username), Some(password)) = (form.username, form.password) {
            return issue_token(&state, &username, &password).await;
        }
    }

    // Fall back to Basic auth header.
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    if let Some((username, password)) = auth_header.and_then(depot_core::auth::decode_basic_auth) {
        return issue_token(&state, &username, &password).await;
    }

    issue_anonymous_token(&state).await
}

/// Authenticate credentials and return a JWT token response.
async fn issue_token(state: &FormatState, username: &str, password: &str) -> Response {
    match state.auth.authenticate(username, password).await {
        Ok(Some(user)) => match state.auth.create_user_token(&user).await {
            Ok(token) => token_response(&token),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "token creation failed").into_response(),
        },
        _ => depot_core::auth::unauthorized_response(false),
    }
}

/// Issue an anonymous token if the anonymous user exists.
async fn issue_anonymous_token(state: &FormatState) -> Response {
    if let Ok(Some(user)) = depot_core::service::get_user(state.kv.as_ref(), "anonymous").await {
        if let Ok(token) = state.auth.create_user_token(&user).await {
            return token_response(&token);
        }
    }
    depot_core::auth::unauthorized_response(false)
}

/// Build a standard Docker token JSON response.
fn token_response(token: &str) -> Response {
    let issued_at = chrono::Utc::now().to_rfc3339();
    Json(serde_json::json!({
        "token": token,
        "access_token": token,
        "expires_in": 86400,
        "issued_at": issued_at,
    }))
    .into_response()
}

pub async fn head_manifest(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    dp: DockerPath,
    headers: HeaderMap,
) -> Response {
    do_head_manifest(
        &state,
        &user.0,
        &dp.repo,
        dp.image.as_deref(),
        dp.param("reference"),
        &headers,
        None,
    )
    .await
}

pub async fn get_manifest(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    dp: DockerPath,
    headers: HeaderMap,
) -> Response {
    do_get_manifest(
        &state,
        &user.0,
        &dp.repo,
        dp.image.as_deref(),
        dp.param("reference"),
        &headers,
        None,
    )
    .await
}

pub async fn put_manifest(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    dp: DockerPath,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    do_put_manifest(
        &state,
        &user.0,
        &dp.repo,
        dp.image.as_deref(),
        dp.param("reference"),
        &headers,
        &body,
        None,
    )
    .await
}

pub async fn delete_manifest(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    dp: DockerPath,
    headers: HeaderMap,
) -> Response {
    do_delete_manifest(
        &state,
        &user.0,
        &dp.repo,
        dp.image.as_deref(),
        dp.param("reference"),
        &headers,
        None,
    )
    .await
}

/// `GET /v2/{name}/referrers/{digest}` — OCI Distribution Spec v1.1 referrers API.
///
/// Depot does not yet track referrers (signatures/attestations), so we always
/// return a spec-compliant empty image index. Modern Docker clients (24+) call
/// this on every pull; without a real handler the SPA fallback would return
/// HTML and the client fails with a JSON decode error.
pub async fn get_referrers(_dp: DockerPath) -> Response {
    let body = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [],
    }))
    .expect("static JSON serializes");
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "application/vnd.oci.image.index.v1+json",
        )],
        body,
    )
        .into_response()
}

pub async fn head_blob(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    dp: DockerPath,
    headers: HeaderMap,
) -> Response {
    do_head_blob(
        &state,
        &user.0,
        &dp.repo,
        dp.image.as_deref(),
        dp.param("digest"),
        &headers,
        None,
    )
    .await
}

pub async fn get_blob(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    dp: DockerPath,
    headers: HeaderMap,
) -> Response {
    do_get_blob(
        &state,
        &user.0,
        &dp.repo,
        dp.image.as_deref(),
        dp.param("digest"),
        &headers,
        None,
    )
    .await
}

pub async fn start_upload(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    dp: DockerPath,
    Query(params): Query<UploadInitParams>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    do_start_upload(
        &state,
        &user.0,
        &dp.repo,
        dp.image.as_deref(),
        &params,
        body,
        &headers,
        None,
    )
    .await
}

pub async fn patch_upload(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    dp: DockerPath,
    headers: HeaderMap,
    body: Body,
) -> Response {
    do_patch_upload(
        &state,
        &user.0,
        &dp.repo,
        dp.image.as_deref(),
        dp.param("uuid"),
        body,
        &headers,
        None,
    )
    .await
}

pub async fn complete_upload(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    dp: DockerPath,
    Query(params): Query<CompleteUploadParams>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    do_complete_upload(
        &state,
        &user.0,
        &dp.repo,
        dp.image.as_deref(),
        dp.param("uuid"),
        &params.digest,
        body,
        &headers,
        None,
    )
    .await
}

pub async fn list_tags(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    dp: DockerPath,
    Query(pagination): Query<DockerPaginationParams>,
    headers: HeaderMap,
) -> Response {
    do_list_tags(
        &state,
        &user.0,
        &dp.repo,
        dp.image.as_deref(),
        &pagination,
        &headers,
        None,
    )
    .await
}
