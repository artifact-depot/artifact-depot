// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

use depot_core::format_state::FormatState;
use depot_core::repo;
use depot_core::repo::proxy::MAX_PROXY_DEPTH;
use depot_core::service;
use depot_core::store::kv::{ArtifactFormat, Capability, RepoKind, RepoType};

use crate::store::{self as docker, DockerStore, MANIFEST_TYPES};

use super::helpers::{
    check_docker_permission, docker_error, hosted_store, location_prefix, resolve_blob_store,
    store_tag_path, validate_docker_repo,
};

// =============================================================================
// Manifest GET/HEAD — shared implementation
// =============================================================================

/// Check if the stored content type is acceptable per the request's Accept header(s).
///
/// Per RFC 7230 §3.2.2, multiple headers with the same name are equivalent to a
/// single comma-joined value, so we must inspect all `Accept` values — skopeo
/// (and other OCI clients) sends them as separate header lines.
fn is_manifest_acceptable(headers: &HeaderMap, content_type: &str) -> bool {
    let mut any = false;
    for v in headers.get_all(header::ACCEPT) {
        any = true;
        if let Ok(s) = v.to_str() {
            if s.contains("*/*") || s.contains(content_type) {
                return true;
            }
        }
    }
    !any
}

/// Wrap a manifest response with Accept header checking.
fn manifest_response_with_accept(
    result: depot_core::error::Result<Option<(Vec<u8>, String, String)>>,
    headers: &HeaderMap,
) -> Response {
    if let Ok(Some((_data, content_type, _digest))) = &result {
        if !is_manifest_acceptable(headers, content_type) {
            return docker_error(
                "MANIFEST_UNKNOWN",
                "manifest content type not acceptable",
                StatusCode::NOT_FOUND,
            );
        }
    }
    manifest_response(result)
}

pub async fn do_get_manifest(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    reference: &str,
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
    match config.repo_type() {
        RepoType::Hosted => {
            let blobs = match resolve_blob_store(state, &config).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let store = hosted_store(state, repo_name, image, blobs.as_ref(), &config.store);
            // When the client fetches by digest, it already identified the
            // exact content — skip Accept-header content-type negotiation.
            // Docker daemon does HEAD-by-tag → GET-by-digest, and the GET
            // may use a narrower Accept set than the HEAD.
            if reference.starts_with("sha256:") {
                manifest_response(store.get_manifest(reference).await)
            } else {
                manifest_response_with_accept(store.get_manifest(reference).await, req_headers)
            }
        }
        RepoType::Cache => cache_get_manifest(state, &config, image, reference).await,
        RepoType::Proxy => proxy_get_manifest(state, &config, image, reference, 0).await,
    }
}

pub async fn do_head_manifest(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    reference: &str,
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
    let _ = req_headers; // Accept check on HEAD is not typically enforced

    match config.repo_type() {
        RepoType::Hosted => {
            let blobs = match resolve_blob_store(state, &config).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let store = hosted_store(state, repo_name, image, blobs.as_ref(), &config.store);
            head_manifest_response(&store, reference).await
        }
        RepoType::Cache => {
            // For HEAD, do a full GET from cache (upstream or local) and return headers only.
            cache_head_manifest(state, &config, image, reference).await
        }
        RepoType::Proxy => proxy_head_manifest(state, &config, image, reference, 0).await,
    }
}

/// Build Docker-standard headers for manifest responses.
fn docker_manifest_headers(content_type: &str, size: usize, digest: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        content_type
            .parse()
            .unwrap_or(HeaderValue::from_static(docker::DOCKER_MANIFEST_V2)),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        size.to_string()
            .parse()
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        "Docker-Content-Digest",
        digest
            .parse()
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers
}

fn manifest_response(
    result: depot_core::error::Result<Option<(Vec<u8>, String, String)>>,
) -> Response {
    match result {
        Ok(Some((data, content_type, digest))) => {
            let headers = docker_manifest_headers(&content_type, data.len(), &digest);
            (StatusCode::OK, headers, Body::from(data)).into_response()
        }
        Ok(None) => docker_error(
            "MANIFEST_UNKNOWN",
            "manifest not found",
            StatusCode::NOT_FOUND,
        ),
        Err(e) => docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn head_manifest_response(store: &DockerStore<'_>, reference: &str) -> Response {
    // Resolve the reference to get the actual manifest data for accurate headers.
    let result = store.get_manifest(reference).await;
    match result {
        Ok(Some((data, content_type, digest))) => {
            let headers = docker_manifest_headers(&content_type, data.len(), &digest);
            (StatusCode::OK, headers).into_response()
        }
        Ok(None) => docker_error(
            "MANIFEST_UNKNOWN",
            "manifest not found",
            StatusCode::NOT_FOUND,
        ),
        Err(e) => docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn manifest_data_response(data: Vec<u8>, content_type: &str, digest: &str) -> Response {
    let headers = docker_manifest_headers(content_type, data.len(), digest);
    (StatusCode::OK, headers, Body::from(data)).into_response()
}

// --- Cache manifest operations ---

async fn cache_get_manifest(
    state: &FormatState,
    config: &RepoConfig,
    image: Option<&str>,
    reference: &str,
) -> Response {
    let RepoKind::Cache {
        ref upstream_url,
        cache_ttl_secs,
        ref upstream_auth,
    } = config.kind
    else {
        return docker_error(
            "UNSUPPORTED",
            "expected cache repo kind",
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    };
    let image_name = image.unwrap_or(&config.name);
    let blobs = match resolve_blob_store(state, config).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let store = hosted_store(state, &config.name, image, blobs.as_ref(), &config.store);

    // Check local cache first.
    let cached = store.get_manifest(reference).await.ok().flatten();

    if let Some((ref data, ref ct, ref digest)) = cached {
        let ttl = cache_ttl_secs;
        let mut expired = false;

        if ttl > 0 && !reference.starts_with("sha256:") {
            // For tag references, check the tag record's updated_at for TTL.
            let tp = store_tag_path(image, reference);
            let cn = config.name.clone();
            let tp_owned = tp.clone();
            if let Ok(Some(tag_rec)) =
                service::get_artifact(state.kv.as_ref(), &cn, &tp_owned).await
            {
                let now = depot_core::repo::now_utc();
                if (now - tag_rec.updated_at).num_seconds() >= ttl as i64 {
                    expired = true;
                    tracing::debug!(
                        repo = config.name,
                        reference,
                        "cache expired, re-fetching manifest"
                    );
                }
            }
        }

        if !expired {
            return manifest_data_response(data.clone(), ct, digest);
        }
    }

    // Fetch from upstream.
    match docker::fetch_upstream_manifest(
        &state.http,
        upstream_url,
        image_name,
        reference,
        upstream_auth.as_ref(),
    )
    .await
    {
        Ok(Some((data, ct, digest))) => {
            // Cache locally.
            if let Err(e) = store.put_manifest(reference, &ct, &data).await {
                tracing::error!(error = %e, "failed to cache manifest locally");
            }
            manifest_data_response(data, &ct, &digest)
        }
        Ok(None) => docker_error(
            "MANIFEST_UNKNOWN",
            "manifest not found",
            StatusCode::NOT_FOUND,
        ),
        Err(e) => {
            // If upstream is unreachable but we have a stale cached copy, return it.
            if let Some((data, ct, digest)) = cached {
                tracing::warn!(error = %e, "upstream unreachable, serving stale cache");
                manifest_data_response(data, &ct, &digest)
            } else {
                tracing::error!(error = %e, "upstream manifest fetch failed");
                docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

async fn cache_head_manifest(
    state: &FormatState,
    config: &RepoConfig,
    image: Option<&str>,
    reference: &str,
) -> Response {
    let RepoKind::Cache {
        ref upstream_url,
        ref upstream_auth,
        ..
    } = config.kind
    else {
        return docker_error(
            "UNSUPPORTED",
            "expected cache repo kind",
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    };
    let image_name = image.unwrap_or(&config.name);
    let blobs = match resolve_blob_store(state, config).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let store = hosted_store(state, &config.name, image, blobs.as_ref(), &config.store);

    // Check local first.
    if let Ok(Some((data, ct, digest))) = store.get_manifest(reference).await {
        let headers = docker_manifest_headers(&ct, data.len(), &digest);
        return (StatusCode::OK, headers).into_response();
    }

    // Fetch full manifest from upstream (not just HEAD) so we can cache it.
    match docker::fetch_upstream_manifest(
        &state.http,
        upstream_url,
        image_name,
        reference,
        upstream_auth.as_ref(),
    )
    .await
    {
        Ok(Some((data, ct, digest))) => {
            // Cache locally for future requests.
            if let Err(e) = store.put_manifest(reference, &ct, &data).await {
                tracing::error!(error = %e, "failed to cache manifest on HEAD");
            }
            let headers = docker_manifest_headers(&ct, data.len(), &digest);
            (StatusCode::OK, headers).into_response()
        }
        Ok(None) => docker_error(
            "MANIFEST_UNKNOWN",
            "manifest not found",
            StatusCode::NOT_FOUND,
        ),
        Err(e) => docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

// --- Proxy manifest operations ---

async fn proxy_get_manifest(
    state: &FormatState,
    config: &RepoConfig,
    image: Option<&str>,
    reference: &str,
    depth: u8,
) -> Response {
    if depth > MAX_PROXY_DEPTH {
        tracing::warn!(
            proxy = config.name,
            depth,
            "docker proxy recursion depth exceeded"
        );
        return docker_error(
            "MANIFEST_UNKNOWN",
            "manifest not found",
            StatusCode::NOT_FOUND,
        );
    }
    let RepoKind::Proxy { ref members, .. } = config.kind else {
        return docker_error(
            "UNSUPPORTED",
            "expected proxy repo kind",
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    };
    for member_name in members {
        let mn = member_name.clone();
        let member_config = match service::get_repo(state.kv.as_ref(), &mn).await {
            Ok(Some(c)) if c.format() == ArtifactFormat::Docker => c,
            _ => continue,
        };

        match member_config.repo_type() {
            RepoType::Hosted => {
                let blobs = match resolve_blob_store(state, &member_config).await {
                    Ok(b) => b,
                    Err(r) => return r,
                };
                let store = hosted_store(
                    state,
                    member_name,
                    image,
                    blobs.as_ref(),
                    &member_config.store,
                );
                if let Ok(Some((data, ct, digest))) = store.get_manifest(reference).await {
                    return manifest_data_response(data, &ct, &digest);
                }
            }
            RepoType::Cache => {
                let resp = cache_get_manifest(state, &member_config, image, reference).await;
                if resp.status() != StatusCode::NOT_FOUND {
                    return resp;
                }
            }
            RepoType::Proxy => {
                let resp = Box::pin(proxy_get_manifest(
                    state,
                    &member_config,
                    image,
                    reference,
                    depth + 1,
                ))
                .await;
                if resp.status() != StatusCode::NOT_FOUND {
                    return resp;
                }
            }
        }
    }
    docker_error(
        "MANIFEST_UNKNOWN",
        "manifest not found",
        StatusCode::NOT_FOUND,
    )
}

async fn proxy_head_manifest(
    state: &FormatState,
    config: &RepoConfig,
    image: Option<&str>,
    reference: &str,
    depth: u8,
) -> Response {
    if depth > MAX_PROXY_DEPTH {
        tracing::warn!(
            proxy = config.name,
            depth,
            "docker proxy recursion depth exceeded"
        );
        return docker_error(
            "MANIFEST_UNKNOWN",
            "manifest not found",
            StatusCode::NOT_FOUND,
        );
    }
    let RepoKind::Proxy { ref members, .. } = config.kind else {
        return docker_error(
            "UNSUPPORTED",
            "expected proxy repo kind",
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    };
    for member_name in members {
        let mn = member_name.clone();
        let member_config = match service::get_repo(state.kv.as_ref(), &mn).await {
            Ok(Some(c)) if c.format() == ArtifactFormat::Docker => c,
            _ => continue,
        };

        match member_config.repo_type() {
            RepoType::Hosted => {
                let blobs = match resolve_blob_store(state, &member_config).await {
                    Ok(b) => b,
                    Err(r) => return r,
                };
                let store = hosted_store(
                    state,
                    member_name,
                    image,
                    blobs.as_ref(),
                    &member_config.store,
                );
                if let Ok(Some((data, ct, digest))) = store.get_manifest(reference).await {
                    let headers = docker_manifest_headers(&ct, data.len(), &digest);
                    return (StatusCode::OK, headers).into_response();
                }
            }
            RepoType::Cache => {
                let resp = cache_head_manifest(state, &member_config, image, reference).await;
                if resp.status() != StatusCode::NOT_FOUND {
                    return resp;
                }
            }
            RepoType::Proxy => {
                let resp = Box::pin(proxy_head_manifest(
                    state,
                    &member_config,
                    image,
                    reference,
                    depth + 1,
                ))
                .await;
                if resp.status() != StatusCode::NOT_FOUND {
                    return resp;
                }
            }
        }
    }
    docker_error(
        "MANIFEST_UNKNOWN",
        "manifest not found",
        StatusCode::NOT_FOUND,
    )
}

// =============================================================================
// Manifest PUT/DELETE — shared implementation
// =============================================================================

#[allow(clippy::too_many_arguments)]
pub async fn do_put_manifest(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    reference: &str,
    headers: &HeaderMap,
    body: &[u8],
    uri_authority: Option<&str>,
) -> Response {
    if let Err(r) = check_docker_permission(
        state,
        username,
        repo_name,
        Capability::Write,
        headers,
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

    // Determine target repo for writes.
    let (target_repo, target_image) = match config.kind {
        RepoKind::Hosted => (config.name.clone(), image),
        RepoKind::Proxy { .. } => {
            let (write_member, _wm_config) = match repo::resolve_format_write_target(
                state.kv.as_ref(),
                &config,
                ArtifactFormat::Docker,
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    return docker_error("UNSUPPORTED", &e.to_string(), StatusCode::BAD_REQUEST)
                }
            };
            (write_member, image)
        }
        RepoKind::Cache { .. } => {
            return docker_error(
                "UNSUPPORTED",
                "cannot push to a cache repository",
                StatusCode::METHOD_NOT_ALLOWED,
            );
        }
    };

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(docker::OCI_MANIFEST_V1);

    if !MANIFEST_TYPES.contains(&content_type) {
        return docker_error(
            "MANIFEST_INVALID",
            &format!("unsupported manifest media type: {}", content_type),
            StatusCode::BAD_REQUEST,
        );
    }

    let manifest: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            return docker_error(
                "MANIFEST_INVALID",
                "manifest is not valid JSON",
                StatusCode::BAD_REQUEST,
            );
        }
    };

    // Validate schemaVersion
    let schema_version = manifest.get("schemaVersion").and_then(|v| v.as_u64());
    if schema_version != Some(2) {
        return docker_error(
            "MANIFEST_INVALID",
            "schemaVersion must be 2",
            StatusCode::BAD_REQUEST,
        );
    }

    let is_list =
        content_type == docker::DOCKER_MANIFEST_LIST || content_type == docker::OCI_INDEX_V1;
    if is_list {
        if manifest
            .get("manifests")
            .and_then(|v| v.as_array())
            .is_none()
        {
            return docker_error(
                "MANIFEST_INVALID",
                "manifest list must contain 'manifests' array",
                StatusCode::BAD_REQUEST,
            );
        }
    } else if manifest.get("layers").and_then(|v| v.as_array()).is_none() {
        return docker_error(
            "MANIFEST_INVALID",
            "manifest must contain 'layers' array",
            StatusCode::BAD_REQUEST,
        );
    }

    // Remember proxy members so blob validation can search across the full
    // group, not just the write target (blobs may live in cache members).
    let proxy_members: Vec<String> = match &config.kind {
        RepoKind::Proxy { ref members, .. } => members.clone(),
        _ => Vec::new(),
    };

    // Resolve the target repo's blob store (may differ from the original repo for proxies).
    let target_config = if target_repo == config.name {
        config
    } else {
        let tr = target_repo.clone();
        match service::get_repo(state.kv.as_ref(), &tr).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                return docker_error(
                    "NAME_UNKNOWN",
                    "target repository not found",
                    StatusCode::NOT_FOUND,
                )
            }
            Err(e) => {
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    };
    let blobs = match resolve_blob_store(state, &target_config).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let store = hosted_store(
        state,
        &target_repo,
        target_image,
        blobs.as_ref(),
        &target_config.store,
    );

    // Validate referenced layer blobs exist (single manifests only).
    // For proxy repos, blobs may reside in cache/upstream members rather than
    // the write target, so search all proxy members when the write target
    // doesn't have the blob.
    if !is_list {
        if let Some(layers) = manifest.get("layers").and_then(|l| l.as_array()) {
            for layer in layers {
                if let Some(d) = layer.get("digest").and_then(|d| d.as_str()) {
                    match store.blob_exists(d).await {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            // Check proxy members before rejecting.
                            let mut found = false;
                            for member_name in &proxy_members {
                                if *member_name == target_repo {
                                    continue; // already checked
                                }
                                let mn = member_name.clone();
                                let member_config =
                                    match service::get_repo(state.kv.as_ref(), &mn).await {
                                        Ok(Some(c)) if c.format() == ArtifactFormat::Docker => c,
                                        _ => continue,
                                    };
                                let member_blobs =
                                    match resolve_blob_store(state, &member_config).await {
                                        Ok(b) => b,
                                        Err(_) => continue,
                                    };
                                let member_store = hosted_store(
                                    state,
                                    member_name,
                                    target_image,
                                    member_blobs.as_ref(),
                                    &member_config.store,
                                );
                                if let Ok(Some(_)) = member_store.blob_exists(d).await {
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                return docker_error(
                                    "BLOB_UNKNOWN",
                                    &format!("referenced blob {} not found in repository", d),
                                    StatusCode::NOT_FOUND,
                                );
                            }
                        }
                        Err(e) => {
                            return docker_error(
                                "UNKNOWN",
                                &e.to_string(),
                                StatusCode::INTERNAL_SERVER_ERROR,
                            );
                        }
                    }
                }
            }
        }
    }

    match store.put_manifest(reference, content_type, body).await {
        Ok(digest) => {
            let mut resp_headers = HeaderMap::new();
            resp_headers.insert(
                "Location",
                format!("{}/manifests/{}", location_prefix(repo_name, image), digest)
                    .parse()
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
            resp_headers.insert(
                "Docker-Content-Digest",
                digest
                    .parse()
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
            (StatusCode::CREATED, resp_headers).into_response()
        }
        Err(e) => docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn do_delete_manifest(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    reference: &str,
    req_headers: &HeaderMap,
    uri_authority: Option<&str>,
) -> Response {
    if let Err(r) = check_docker_permission(
        state,
        username,
        repo_name,
        Capability::Delete,
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

    if config.repo_type() == RepoType::Proxy {
        return docker_error(
            "UNSUPPORTED",
            "cannot delete from proxy repositories",
            StatusCode::METHOD_NOT_ALLOWED,
        );
    }

    if !reference.starts_with("sha256:") {
        return docker_error(
            "UNSUPPORTED",
            "delete by tag not supported, use digest",
            StatusCode::BAD_REQUEST,
        );
    }

    let blobs = match resolve_blob_store(state, &config).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let store = hosted_store(state, repo_name, image, blobs.as_ref(), &config.store);
    match store.delete_manifest(reference).await {
        Ok(true) => StatusCode::ACCEPTED.into_response(),
        Ok(false) => docker_error(
            "MANIFEST_UNKNOWN",
            "manifest not found",
            StatusCode::NOT_FOUND,
        ),
        Err(e) => docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

use depot_core::store::kv::RepoConfig;
