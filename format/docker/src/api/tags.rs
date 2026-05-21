// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};

use depot_core::auth::{check_grants, AuthenticatedUser};
use depot_core::format_state::FormatState;
use depot_core::service;
use depot_core::store::kv::{ArtifactFormat, Capability, RepoKind};

use super::helpers::{
    check_docker_permission, docker_error, hosted_store, resolve_blob_store, validate_docker_repo,
};
use super::types::{CatalogResponse, DockerPaginationParams, TagListResponse};

pub async fn do_list_tags(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    pagination: &DockerPaginationParams,
    req_headers: &HeaderMap,
    uri_authority: Option<&str>,
) -> Response {
    if let Err(r) = check_docker_permission(
        state,
        username,
        repo_name,
        Capability::Read,
        req_headers,
        uri_authority,
    )
    .await
    {
        return r;
    }
    let config = match validate_docker_repo(state, repo_name).await {
        Ok(c) => c,
        Err(r) => return r,
    };

    let display_name = match image {
        Some(img) => format!("{}/{}", repo_name, img),
        None => repo_name.to_string(),
    };

    let all_tags = match config.kind {
        RepoKind::Hosted | RepoKind::Cache { .. } => {
            let blobs = match resolve_blob_store(state, &config).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let store = hosted_store(state, &config.name, image, blobs.as_ref(), &config.store);
            match store.list_tags().await {
                Ok(tags) => tags,
                Err(e) => {
                    return docker_error(
                        "UNKNOWN",
                        &e.to_string(),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    )
                }
            }
        }
        RepoKind::Proxy { ref members, .. } => {
            let mut tag_set = std::collections::BTreeSet::new();
            for member_name in members {
                let mn = member_name.clone();
                let member_config = match service::get_repo(state.kv.as_ref(), &mn).await {
                    Ok(Some(c)) => c,
                    _ => continue,
                };
                let member_blobs = match resolve_blob_store(state, &member_config).await {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let store = hosted_store(
                    state,
                    member_name,
                    image,
                    member_blobs.as_ref(),
                    &member_config.store,
                );
                if let Ok(tags) = store.list_tags().await {
                    tag_set.extend(tags);
                }
            }
            tag_set.into_iter().collect::<Vec<_>>()
        }
    };

    // Apply Docker V2 cursor pagination: filter tags > last, take n.
    let filtered: Vec<String> = match &pagination.last {
        Some(last) => all_tags
            .into_iter()
            .filter(|t| t.as_str() > last.as_str())
            .collect(),
        None => all_tags,
    };

    let n = pagination.n.unwrap_or(filtered.len());
    let page: Vec<String> = filtered.iter().take(n).cloned().collect();
    let has_more = filtered.len() > n;

    let mut resp = Json(TagListResponse {
        name: display_name.clone(),
        tags: page.clone(),
    })
    .into_response();

    if has_more {
        if let Some(last_tag) = page.last() {
            let link_path = match image {
                Some(img) => format!("/v2/{}/{}/tags/list", repo_name, img),
                None => format!("/v2/{}/tags/list", repo_name),
            };
            let link_value = format!("<{}?n={}&last={}>; rel=\"next\"", link_path, n, last_tag);
            if let Ok(hv) = link_value.parse() {
                resp.headers_mut().insert("Link", hv);
            }
        }
    }

    resp
}

/// `GET /v2/_catalog`
///
/// Returns the sorted set of image names hosted across all Docker-format
/// repositories the caller can read, as required by the Docker Registry HTTP
/// API V2 spec. Names are the `<name>` portion of `docker pull <host>/<name>:<tag>`.
pub async fn catalog(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(pagination): Query<DockerPaginationParams>,
) -> Response {
    let roles = match state.auth.resolve_user_roles(&user.0).await {
        Ok(Some((_, roles))) => roles,
        Ok(None) => {
            return Json(CatalogResponse {
                repositories: Vec::new(),
            })
            .into_response()
        }
        Err(_) => return docker_error("UNKNOWN", "auth error", StatusCode::INTERNAL_SERVER_ERROR),
    };

    let repos = match service::list_repos(state.kv.as_ref()).await {
        Ok(r) => r,
        Err(e) => {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
        }
    };
    let docker_repos: Vec<String> = repos
        .into_iter()
        .filter(|r| {
            r.format() == ArtifactFormat::Docker
                && check_grants(&roles, &r.name, Capability::Read).is_ok()
        })
        .map(|r| r.name)
        .collect();

    // Scan each docker repo in parallel and union the image-name sets.
    let scans = docker_repos.into_iter().map(|name| {
        let kv = std::sync::Arc::clone(&state.kv);
        async move { crate::store::list_image_names(kv.as_ref(), &name).await }
    });
    let results = futures::future::join_all(scans).await;
    let mut images: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for r in results {
        match r {
            Ok(set) => images.extend(set),
            Err(e) => {
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }

    paginate_catalog(images, &pagination, "/v2/_catalog")
}

/// Apply Docker V2 cursor pagination (`n`, `last`) to a sorted set of
/// names and emit a `Link: <link_path?n=…&last=…>; rel="next"` header
/// when the response is truncated.
pub(super) fn paginate_catalog(
    names: std::collections::BTreeSet<String>,
    pagination: &DockerPaginationParams,
    link_path: &str,
) -> Response {
    let filtered: Vec<String> = match &pagination.last {
        Some(last) => names
            .into_iter()
            .filter(|n| n.as_str() > last.as_str())
            .collect(),
        None => names.into_iter().collect(),
    };
    let n = pagination.n.unwrap_or(filtered.len());
    let page: Vec<String> = filtered.iter().take(n).cloned().collect();
    let has_more = filtered.len() > n;

    let mut resp = Json(CatalogResponse {
        repositories: page.clone(),
    })
    .into_response();

    if has_more {
        if let Some(last_name) = page.last() {
            let link_value = format!("<{}?n={}&last={}>; rel=\"next\"", link_path, n, last_name);
            if let Ok(hv) = link_value.parse() {
                resp.headers_mut().insert("Link", hv);
            }
        }
    }

    resp
}
