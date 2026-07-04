// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::sync::Arc;

use depot_core::error::DepotError;
use depot_core::format_state::FormatState;
use depot_core::store::kv::{ArtifactFormat, Capability, RepoConfig};

use crate::store::{DockerStore, NO_ATIME_HEADER};

/// Whether a read should refresh `last_accessed_at`. A request opts out by
/// sending [`NO_ATIME_HEADER`] (used by read-only tooling like the reorg CLI),
/// in which case the serve path builds a [`DockerStore::without_atime`].
pub fn wants_atime(req_headers: &HeaderMap) -> bool {
    !req_headers.contains_key(NO_ATIME_HEADER)
}

pub fn docker_error(code: &str, message: &str, status: StatusCode) -> Response {
    let body = serde_json::json!({
        "errors": [{
            "code": code,
            "message": message,
            "detail": {}
        }]
    });
    (status, Json(body)).into_response()
}

/// Build the `WWW-Authenticate: Bearer` challenge value from request headers.
///
/// The realm origin honors (in priority) the configured `base_url` setting,
/// `X-Forwarded-Proto`/`Host` reverse-proxy headers, then the accepting
/// listener's scheme — so a client talking to the plain-HTTP listener gets an
/// `http://` realm rather than an unconditional `https://`.
pub fn docker_bearer_challenge(
    state: &FormatState,
    req_headers: &HeaderMap,
    uri_authority: Option<&str>,
) -> String {
    let origin = state.external_origin(req_headers, uri_authority);
    format!("Bearer realm=\"{origin}/v2/token\",service=\"Artifact Depot\"")
}

/// Build a 401 UNAUTHORIZED response with a Docker Bearer challenge.
///
/// Used when an anonymous (unauthenticated) client must authenticate before
/// accessing a Docker resource.
pub fn docker_unauthorized_response(
    state: &FormatState,
    req_headers: &HeaderMap,
    uri_authority: Option<&str>,
) -> Response {
    let challenge = docker_bearer_challenge(state, req_headers, uri_authority);
    let body = serde_json::json!({
        "errors": [{
            "code": "UNAUTHORIZED",
            "message": "authentication required",
            "detail": {}
        }]
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        "Docker-Distribution-Api-Version",
        HeaderValue::from_static("registry/2.0"),
    );
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        headers.insert(header::WWW_AUTHENTICATE, value);
    }
    (StatusCode::UNAUTHORIZED, headers, Json(body)).into_response()
}

/// Build the URL path prefix for Docker V2 Location headers.
///
/// When `image` is Some (default-docker-repo / dedicated-port mode), the
/// client's path uses the image name, not the internal repo name.
/// When `image` is None (main-port, repo-in-path mode), the client's path
/// uses the repo name directly.
pub fn location_prefix(repo_name: &str, image: Option<&str>) -> String {
    match image {
        Some(img) => format!("/v2/{}", img),
        None => format!("/v2/{}", repo_name),
    }
}

/// Check Docker permission; returns a DENIED error on failure.
///
/// For anonymous (unauthenticated) users, returns 401 UNAUTHORIZED with a
/// Bearer challenge so that Docker clients know to authenticate via the
/// `/v2/token` endpoint. For authenticated users, returns 403 FORBIDDEN.
pub async fn check_docker_permission(
    state: &FormatState,
    username: &str,
    repo: &str,
    cap: Capability,
    req_headers: &HeaderMap,
    uri_authority: Option<&str>,
) -> Result<(), Response> {
    state
        .auth
        .check_permission(username, repo, cap)
        .await
        .map_err(|_| {
            if username == "anonymous" {
                docker_unauthorized_response(state, req_headers, uri_authority)
            } else {
                docker_error(
                    "DENIED",
                    "requested access to the resource is denied",
                    StatusCode::FORBIDDEN,
                )
            }
        })
}

/// Validate that a repo exists and has Docker format, mapping errors to Docker V2 format.
pub async fn validate_docker_repo(
    state: &FormatState,
    repo_name: &str,
) -> Result<RepoConfig, Response> {
    let name = repo_name.to_string();
    let result: Result<RepoConfig, depot_core::error::DepotError> = async {
        let config = depot_core::service::get_repo(state.kv.as_ref(), &name)
            .await?
            .ok_or_else(|| {
                depot_core::error::DepotError::NotFound(format!(
                    "repository '{}' not found",
                    repo_name
                ))
            })?;
        if config.format() != ArtifactFormat::Docker {
            return Err(depot_core::error::DepotError::BadRequest(
                "repository is not a Docker repository".to_string(),
            ));
        }
        Ok(config)
    }
    .await;
    result.map_err(|e| match e {
        DepotError::NotFound(_) => docker_error(
            "NAME_UNKNOWN",
            "repository not found",
            StatusCode::NOT_FOUND,
        ),
        DepotError::BadRequest(_) => docker_error(
            "UNSUPPORTED",
            "repository is not a Docker repository",
            StatusCode::BAD_REQUEST,
        ),
        _ => docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    })
}

/// Build a DockerStore for a hosted repo.
pub fn hosted_store<'a>(
    state: &'a FormatState,
    repo_name: &'a str,
    image: Option<&'a str>,
    blobs: &'a dyn depot_core::store::blob::BlobStore,
    store: &'a str,
) -> DockerStore<'a> {
    DockerStore {
        repo: repo_name,
        image,
        kv: state.kv.as_ref(),
        blobs,
        updater: &state.updater,
        store,
        track_access: true,
    }
}

/// Resolve blob store for a repo config, mapping errors to Docker V2 error responses.
pub async fn resolve_blob_store(
    state: &FormatState,
    config: &RepoConfig,
) -> Result<Arc<dyn depot_core::store::blob::BlobStore>, Response> {
    state.blob_store(&config.store).await.map_err(|e| {
        docker_error(
            "UNKNOWN",
            &format!("failed to resolve blob store: {}", e),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
    })
}

/// Build Docker-standard headers for blob responses.
pub fn docker_blob_headers(size: u64, digest: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_LENGTH,
        size.to_string()
            .parse()
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        "Docker-Content-Digest",
        digest
            .parse()
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers
}

/// Build the KV artifact path for a tag record.
pub fn store_tag_path(image: Option<&str>, tag: &str) -> String {
    match image {
        Some(img) => format!("{img}/_tags/{tag}"),
        None => format!("_tags/{tag}"),
    }
}
