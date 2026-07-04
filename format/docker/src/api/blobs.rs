// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{
    body::Body,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use tokio_util::io::ReaderStream;

use depot_core::format_state::FormatState;
use depot_core::repo::proxy::MAX_PROXY_DEPTH;
use depot_core::service;
use depot_core::store::kv::{ArtifactFormat, Capability, RepoConfig, RepoKind, RepoType};

use crate::store as docker;

use super::helpers::{
    check_docker_permission, docker_blob_headers, docker_error, hosted_store, resolve_blob_store,
    validate_docker_repo, wants_atime,
};

pub async fn do_get_blob(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    digest: &str,
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
            // Open the blob file while we have KV info, then release KV
            // and stream the file data to the client in chunks.
            let blobs = match resolve_blob_store(state, &config).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let mut store = hosted_store(state, repo_name, image, blobs.as_ref(), &config.store);
            if !wants_atime(req_headers) {
                store = store.without_atime();
            }
            blob_stream_response(store.open_blob(digest).await, digest)
        }
        RepoType::Cache => {
            cache_get_blob(state, &config, image, digest, wants_atime(req_headers)).await
        }
        RepoType::Proxy => {
            proxy_get_blob(state, &config, image, digest, wants_atime(req_headers), 0).await
        }
    }
}

pub async fn do_head_blob(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    digest: &str,
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
            blob_head_response(store.blob_exists(digest).await, digest)
        }
        RepoType::Cache => cache_head_blob(state, &config, image, digest).await,
        RepoType::Proxy => proxy_head_blob(state, &config, image, digest, 0).await,
    }
}

/// Build a streaming response from an async reader.
/// The reader is consumed in chunks, keeping the async runtime free
/// and avoiding loading the entire blob into memory.
fn blob_stream_response(
    result: depot_core::error::Result<Option<(depot_core::store::blob::BlobReader, u64)>>,
    digest: &str,
) -> Response {
    match result {
        Ok(Some((reader, size))) => {
            let headers = docker_blob_headers(size, digest);
            let stream = ReaderStream::with_capacity(reader, 256 * 1024);
            let body = Body::from_stream(stream);
            (StatusCode::OK, headers, body).into_response()
        }
        Ok(None) => docker_error("BLOB_UNKNOWN", "blob not found", StatusCode::NOT_FOUND),
        Err(e) => docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn blob_head_response(
    result: depot_core::error::Result<Option<(u64, String, String)>>,
    digest: &str,
) -> Response {
    match result {
        Ok(Some((size, _, _))) => {
            let headers = docker_blob_headers(size, digest);
            (StatusCode::OK, headers).into_response()
        }
        Ok(None) => docker_error("BLOB_UNKNOWN", "blob not found", StatusCode::NOT_FOUND),
        Err(e) => docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}

// --- Cache blob operations ---

async fn cache_get_blob(
    state: &FormatState,
    config: &RepoConfig,
    image: Option<&str>,
    digest: &str,
    track_access: bool,
) -> Response {
    let blobs = match resolve_blob_store(state, config).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let mut store = hosted_store(state, &config.name, image, blobs.as_ref(), &config.store);
    if !track_access {
        store = store.without_atime();
    }

    // Check local cache — stream from file, no memory buffering.
    if let Ok(Some(_)) = store.blob_exists(digest).await {
        return blob_stream_response(store.open_blob(digest).await, digest);
    }

    // Fetch from upstream, streaming to temp file.
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

    // Stream upstream blob directly to blob store.
    let blob_id = uuid::Uuid::new_v4().to_string();
    let mut writer = match blobs.create_writer(&blob_id, None).await {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error = %e, "failed to create blob writer");
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    match docker::fetch_upstream_blob_to_writer(
        &state.http,
        upstream_url,
        image_name,
        digest,
        upstream_auth.as_ref(),
        writer.as_mut(),
    )
    .await
    {
        Ok(Some((computed_digest, blake3_hash, size))) => {
            // Verify digest — reject and discard if mismatched.
            if computed_digest != digest {
                tracing::error!(expected = digest, got = %computed_digest, "upstream blob digest mismatch");
                let _ = writer.abort().await;
                return docker_error(
                    "BLOB_UNKNOWN",
                    "upstream blob digest mismatch",
                    StatusCode::BAD_GATEWAY,
                );
            }
            if let Err(e) = writer.finish().await {
                tracing::error!(error = %e, "failed to finish blob write");
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
            // Register the blob in KV.
            if let Err(e) = store
                .put_cached_blob(&blob_id, digest, &blake3_hash, size)
                .await
            {
                tracing::error!(error = %e, "failed to cache blob locally");
            }
            // Serve from the now-cached blob.
            blob_stream_response(store.open_blob(digest).await, digest)
        }
        Ok(None) => {
            let _ = writer.abort().await;
            docker_error("BLOB_UNKNOWN", "blob not found", StatusCode::NOT_FOUND)
        }
        Err(e) => {
            let _ = writer.abort().await;
            tracing::error!(error = %e, "upstream blob fetch failed, not in local cache either");
            docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn cache_head_blob(
    state: &FormatState,
    config: &RepoConfig,
    image: Option<&str>,
    digest: &str,
) -> Response {
    let blobs = match resolve_blob_store(state, config).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let store = hosted_store(state, &config.name, image, blobs.as_ref(), &config.store);

    // Check local.
    if let Ok(Some((size, _, _))) = store.blob_exists(digest).await {
        return blob_head_response(Ok(Some((size, String::new(), String::new()))), digest);
    }

    // Fetch full blob from upstream (streaming to temp) so we can cache it.
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

    // Stream upstream blob directly to blob store for caching.
    let blob_id = uuid::Uuid::new_v4().to_string();
    let mut writer = match blobs.create_writer(&blob_id, None).await {
        Ok(w) => w,
        Err(e) => {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    match docker::fetch_upstream_blob_to_writer(
        &state.http,
        upstream_url,
        image_name,
        digest,
        upstream_auth.as_ref(),
        writer.as_mut(),
    )
    .await
    {
        Ok(Some((_computed_digest, blake3_hash, size))) => {
            if let Err(e) = writer.finish().await {
                tracing::error!(error = %e, "failed to finish blob write on HEAD");
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
            // Register the blob in KV.
            if let Err(e) = store
                .put_cached_blob(&blob_id, digest, &blake3_hash, size)
                .await
            {
                tracing::error!(error = %e, "failed to cache blob on HEAD");
            }
            blob_head_response(Ok(Some((size, String::new(), String::new()))), digest)
        }
        Ok(None) => {
            let _ = writer.abort().await;
            docker_error("BLOB_UNKNOWN", "blob not found", StatusCode::NOT_FOUND)
        }
        Err(e) => {
            let _ = writer.abort().await;
            docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// --- Proxy blob operations ---

async fn proxy_get_blob(
    state: &FormatState,
    config: &RepoConfig,
    image: Option<&str>,
    digest: &str,
    track_access: bool,
    depth: u8,
) -> Response {
    if depth > MAX_PROXY_DEPTH {
        tracing::warn!(
            proxy = config.name,
            depth,
            "docker proxy recursion depth exceeded"
        );
        return docker_error("BLOB_UNKNOWN", "blob not found", StatusCode::NOT_FOUND);
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
                let mut store = hosted_store(
                    state,
                    member_name,
                    image,
                    blobs.as_ref(),
                    &member_config.store,
                );
                if !track_access {
                    store = store.without_atime();
                }
                // Stream from file handle — KV lock released before I/O.
                if let Ok(Some(_)) = store.blob_exists(digest).await {
                    return blob_stream_response(store.open_blob(digest).await, digest);
                }
            }
            RepoType::Cache => {
                let resp = cache_get_blob(state, &member_config, image, digest, track_access).await;
                if resp.status() != StatusCode::NOT_FOUND {
                    return resp;
                }
            }
            RepoType::Proxy => {
                let resp = Box::pin(proxy_get_blob(
                    state,
                    &member_config,
                    image,
                    digest,
                    track_access,
                    depth + 1,
                ))
                .await;
                if resp.status() != StatusCode::NOT_FOUND {
                    return resp;
                }
            }
        }
    }
    docker_error("BLOB_UNKNOWN", "blob not found", StatusCode::NOT_FOUND)
}

async fn proxy_head_blob(
    state: &FormatState,
    config: &RepoConfig,
    image: Option<&str>,
    digest: &str,
    depth: u8,
) -> Response {
    if depth > MAX_PROXY_DEPTH {
        tracing::warn!(
            proxy = config.name,
            depth,
            "docker proxy recursion depth exceeded"
        );
        return docker_error("BLOB_UNKNOWN", "blob not found", StatusCode::NOT_FOUND);
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
                if let Ok(Some((size, _, _))) = store.blob_exists(digest).await {
                    let headers = docker_blob_headers(size, digest);
                    return (StatusCode::OK, headers).into_response();
                }
            }
            RepoType::Cache => {
                let resp = cache_head_blob(state, &member_config, image, digest).await;
                if resp.status() != StatusCode::NOT_FOUND {
                    return resp;
                }
            }
            RepoType::Proxy => {
                let resp = Box::pin(proxy_head_blob(
                    state,
                    &member_config,
                    image,
                    digest,
                    depth + 1,
                ))
                .await;
                if resp.status() != StatusCode::NOT_FOUND {
                    return resp;
                }
            }
        }
    }
    docker_error("BLOB_UNKNOWN", "blob not found", StatusCode::NOT_FOUND)
}
