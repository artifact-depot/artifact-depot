// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{
    extract::{FromRef, FromRequestParts, Path},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use depot_core::format_state::FormatState;

use super::helpers::docker_error;

/// Query parameters for the Docker V2 token endpoint.
#[derive(Deserialize)]
pub struct TokenParams {
    #[allow(dead_code)]
    pub service: Option<String>,
    #[allow(dead_code)]
    pub scope: Option<String>,
}

/// Form body for the OAuth2 / Docker V2 token POST endpoint.
///
/// Containerd and other OCI clients may POST credentials as
/// `application/x-www-form-urlencoded` instead of sending Basic auth
/// in a GET request (RFC 6749 "Resource Owner Password Credentials" grant).
#[derive(Deserialize)]
pub struct TokenFormParams {
    #[allow(dead_code)]
    pub grant_type: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    #[allow(dead_code)]
    pub service: Option<String>,
    #[allow(dead_code)]
    pub scope: Option<String>,
    #[allow(dead_code)]
    pub client_id: Option<String>,
    #[allow(dead_code)]
    pub access_type: Option<String>,
}

#[derive(Default, Deserialize)]
pub struct DockerPaginationParams {
    pub n: Option<usize>,
    pub last: Option<String>,
}

#[derive(Default, Deserialize)]
pub struct UploadInitParams {
    pub mount: Option<String>,
    pub from: Option<String>,
    pub digest: Option<String>,
}

#[derive(Deserialize)]
pub struct CompleteUploadParams {
    pub digest: String,
}

#[derive(Serialize)]
pub struct CatalogResponse {
    pub repositories: Vec<String>,
}

#[derive(Serialize)]
pub struct TagListResponse {
    pub name: String,
    pub tags: Vec<String>,
}

/// When present in request extensions, overrides path-based repo resolution.
/// Used by dedicated Docker port routes where the repo is fixed.
#[derive(Clone)]
pub struct FixedDockerRepo(pub String);

/// Resolved Docker path components extracted from either single-segment
/// (`/v2/{name}/...`) or two-segment (`/v2/{repo}/{image}/...`) routes.
///
/// Also handles dedicated Docker port routes where the repo is fixed via
/// [`FixedDockerRepo`] extension and route params use `{ns}/{img}`.
pub struct DockerPath {
    pub repo: String,
    pub image: Option<String>,
    params: HashMap<String, String>,
}

impl DockerPath {
    /// Get a required path parameter by name. Route definitions guarantee
    /// the parameter exists; returns `""` if absent (should never happen).
    pub fn param(&self, key: &str) -> &str {
        self.params.get(key).map_or("", String::as_str)
    }
}

/// Resolve Docker repo and image based on `default_docker_repo` setting.
///
/// When `default_docker_repo` is set, single-segment `{name}` paths treat the
/// name as an image within the default repo. When not set, `{name}` is the
/// repo name with no image prefix (original behavior).
fn resolve_docker_repo(
    state: &FormatState,
    path_name: &str,
    path_image: Option<&str>,
) -> (String, Option<String>) {
    if let Some(default_repo) = state.settings.default_docker_repo() {
        match path_image {
            Some(img) => (default_repo, Some(format!("{}/{}", path_name, img))),
            None => (default_repo, Some(path_name.to_string())),
        }
    } else {
        (path_name.to_string(), path_image.map(|s| s.to_string()))
    }
}

impl<S> FromRequestParts<S> for DockerPath
where
    S: Send + Sync,
    FormatState: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(mut map) = Path::<HashMap<String, String>>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;

        // Dedicated Docker port: repo is fixed, path segments are the image.
        if let Some(fixed) = parts.extensions.get::<FixedDockerRepo>() {
            let repo = fixed.0.clone();
            let image = match (map.remove("ns"), map.remove("img")) {
                (Some(ns), Some(img)) => Some(format!("{ns}/{img}")),
                _ => map.remove("name"),
            };
            return Ok(DockerPath {
                repo,
                image,
                params: map,
            });
        }

        // Main router: detect single-segment vs two-segment from param keys.
        let (raw_name, raw_image) = if let Some(name) = map.remove("name") {
            (name, None)
        } else if let Some(repo) = map.remove("repo") {
            // The image under a named repo may be one segment ({image}) or two
            // ({image}/{image2}), e.g. a repo serving `library/nginx`.
            let image = match (map.remove("image"), map.remove("image2")) {
                (Some(a), Some(b)) => Some(format!("{a}/{b}")),
                (a, _) => a,
            };
            (repo, image)
        } else {
            return Err(docker_error(
                "NAME_UNKNOWN",
                "missing path params",
                StatusCode::BAD_REQUEST,
            ));
        };

        // Pull out the subset of the outer state that we need.
        let format_state = FormatState::from_ref(state);
        let (repo, image) = resolve_docker_repo(&format_state, &raw_name, raw_image.as_deref());
        Ok(DockerPath {
            repo,
            image,
            params: map,
        })
    }
}
