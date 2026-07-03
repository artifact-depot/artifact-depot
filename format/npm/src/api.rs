// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! npm HTTP API implementation.
//!
//! Implements the npm registry protocol for hosted, cache, and proxy repositories.

use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::collections::HashMap;
use tokio_util::io::ReaderStream;

use depot_core::store_from_config_fn;

use crate::store::{self as npm, NpmStore};
use depot_core::auth::AuthenticatedUser;
use depot_core::error::DepotError;
use depot_core::format_state::FormatState;
use depot_core::service;
use depot_core::store::kv::{
    ArtifactFormat, RepoConfig, RepoKind, RepoType, CURRENT_RECORD_VERSION,
};

store_from_config_fn!(npm_store_from_config, NpmStore);

// =============================================================================
// /repository/{repo}/ dispatch for npm repos
// =============================================================================

pub async fn try_handle_repository_path(
    state: &FormatState,
    config: &RepoConfig,
    path: &str,
    query: Option<&str>,
    origin: &str,
) -> Option<Response> {
    if path == "-/v1/search" {
        // Parse query string for text= and size= params
        let mut text = String::new();
        let mut size: usize = 20; // default
        if let Some(q) = query {
            for part in q.split('&') {
                if let Some(v) = part.strip_prefix("text=") {
                    text = urlencoding::decode(v).unwrap_or_default().into_owned();
                } else if let Some(v) = part.strip_prefix("size=") {
                    if let Ok(n) = v.parse::<usize>() {
                        size = n;
                    }
                }
            }
        }
        return Some(search_packages(state, config, &text, size).await);
    }

    // Scoped package: @scope/package or @scope/package/-/filename
    if let Some(rest) = path.strip_prefix('@') {
        let parts: Vec<&str> = rest.splitn(4, '/').collect();
        if let [scope, pkg] = parts.as_slice() {
            // @scope/package → packument
            let package = format!("@{scope}/{pkg}");
            return Some(get_packument_inner_from_config(state, config, &package, origin).await);
        }
        if let [scope, pkg, "-", filename] = parts.as_slice() {
            // @scope/package/-/filename → tarball download
            let package = format!("@{scope}/{pkg}");
            return Some(
                download_tarball_inner_from_config(state, config, &package, filename).await,
            );
        }
        return None;
    }

    // Unscoped package/-/filename.tgz → tarball download
    if let Some((package, rest)) = path.split_once("/-/") {
        if !package.is_empty() && !package.contains('/') && !rest.is_empty() {
            return Some(download_tarball_inner_from_config(state, config, package, rest).await);
        }
    }

    // Unscoped package → packument (must not contain '/')
    if !path.is_empty() && !path.contains('/') {
        return Some(get_packument_inner_from_config(state, config, path, origin).await);
    }

    None
}

async fn get_packument_inner_from_config(
    state: &FormatState,
    config: &RepoConfig,
    package: &str,
    origin: &str,
) -> Response {
    match config.repo_type() {
        RepoType::Hosted => hosted_packument(state, &config.name, package, origin).await,
        RepoType::Cache => cache_packument(state, config, package, origin).await,
        RepoType::Proxy => proxy_packument(state, config, package, origin).await,
    }
}

// =============================================================================
// /repository/{repo}/ dispatch for npm repos (writes)
// =============================================================================

/// Handle a write request under `/repository/{repo}/<path>` for an npm repo.
/// Returns `None` if the (method, path) combination is not an npm write.
pub async fn try_handle_repository_write(
    state: &FormatState,
    user: &AuthenticatedUser,
    config: &RepoConfig,
    method: &axum::http::Method,
    path: &str,
    _headers: &axum::http::HeaderMap,
    body: axum::body::Body,
) -> Option<axum::response::Response> {
    // GET /-/whoami — doesn't touch the repo config.
    if method == axum::http::Method::GET && path == "-/whoami" {
        let body = serde_json::json!({ "username": user.0 });
        return Some(
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_string(&body).unwrap_or_default(),
            )
                .into_response(),
        );
    }

    // PUT /-/user/{anything} — npm login/adduser.
    if method == axum::http::Method::PUT && path.starts_with("-/user/") {
        let bytes = match depot_core::api_helpers::body_to_bytes(body, usize::MAX).await {
            Ok(b) => b,
            Err(e) => return Some(e),
        };
        let payload: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                return Some(DepotError::BadRequest(format!("invalid JSON: {e}")).into_response());
            }
        };
        let name = match payload.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => {
                return Some(DepotError::BadRequest("missing 'name' field".into()).into_response());
            }
        };
        let password = match payload.get("password").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return Some(
                    DepotError::BadRequest("missing 'password' field".into()).into_response(),
                );
            }
        };
        let authenticated = match state.auth.authenticate(name, password).await {
            Ok(u) => u,
            Err(e) => return Some(e.into_response()),
        };
        return Some(match authenticated {
            Some(u) => {
                let token = match state.auth.create_user_token(&u).await {
                    Ok(t) => t,
                    Err(e) => return Some(e.into_response()),
                };
                let body = serde_json::json!({ "ok": true, "token": token });
                (
                    StatusCode::CREATED,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_string(&body).unwrap_or_default(),
                )
                    .into_response()
            }
            None => StatusCode::UNAUTHORIZED.into_response(),
        });
    }

    // PUT @{scope}/{package} — publish (scoped).
    if method == axum::http::Method::PUT {
        if let Some(rest) = path.strip_prefix('@') {
            let parts: Vec<&str> = rest.splitn(2, '/').collect();
            if let [scope, pkg] = parts.as_slice() {
                if !scope.is_empty() && !pkg.is_empty() && !pkg.contains('/') {
                    let package = format!("@{scope}/{pkg}");
                    let bytes = match depot_core::api_helpers::body_to_bytes(body, usize::MAX).await
                    {
                        Ok(b) => b,
                        Err(e) => return Some(e),
                    };
                    return Some(publish_inner(state, user, &config.name, &package, &bytes).await);
                }
            }
            return None;
        }

        // PUT {package} (unscoped, no '/').
        if !path.is_empty() && !path.contains('/') {
            let bytes = match depot_core::api_helpers::body_to_bytes(body, usize::MAX).await {
                Ok(b) => b,
                Err(e) => return Some(e),
            };
            return Some(publish_inner(state, user, &config.name, path, &bytes).await);
        }
    }

    None
}

async fn download_tarball_inner_from_config(
    state: &FormatState,
    config: &RepoConfig,
    package: &str,
    filename: &str,
) -> Response {
    let version = match extract_version_from_filename(filename, package) {
        Some(v) => v,
        None => {
            return DepotError::BadRequest("invalid tarball filename".into()).into_response();
        }
    };

    match config.repo_type() {
        RepoType::Hosted => hosted_download(state, &config.name, package, &version, filename).await,
        RepoType::Cache => cache_download(state, config, package, &version, filename).await,
        RepoType::Proxy => proxy_download(state, config, package, &version, filename).await,
    }
}

async fn hosted_packument(
    state: &FormatState,
    repo_name: &str,
    package: &str,
    origin: &str,
) -> Response {
    let (blobs, blob_config) =
        match depot_core::api_helpers::resolve_repo_blob_store(state, repo_name).await {
            Ok(b) => b,
            Err(e) => return e.into_response(),
        };
    let store_name = blob_config.store.clone();
    let store = NpmStore {
        repo: repo_name,
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: &store_name,
        updater: &state.updater,
    };

    let base_url = format!("{origin}/repository/{repo_name}");
    match store.get_packument(package, &base_url).await {
        Ok(Some(packument)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&packument).unwrap_or_default(),
        )
            .into_response(),
        Ok(None) => DepotError::NotFound("package not found".into()).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn cache_packument(
    state: &FormatState,
    config: &RepoConfig,
    package: &str,
    origin: &str,
) -> Response {
    let RepoKind::Cache {
        ref upstream_url,
        ref upstream_auth,
        cache_ttl_secs,
        ..
    } = config.kind
    else {
        return DepotError::Internal("expected cache repo kind".into()).into_response();
    };

    let blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };

    // Check local cache freshness
    let store = NpmStore {
        repo: &config.name,
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: &config.store,
        updater: &state.updater,
    };

    // Try to fetch from upstream
    match npm::fetch_upstream_packument(&state.http, upstream_url, package, upstream_auth.as_ref())
        .await
    {
        Ok(Some(upstream_packument)) => {
            // Cache version metadata and dist-tags from upstream
            cache_upstream_packument(state, config, package, &upstream_packument).await;

            // Rewrite tarball URLs and serve
            let base_url = format!("{origin}/repository/{}", config.name);
            let rewritten = rewrite_packument_urls(&upstream_packument, &base_url);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_string(&rewritten).unwrap_or_default(),
            )
                .into_response()
        }
        Ok(None) => DepotError::NotFound("package not found upstream".into()).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "upstream fetch failed, serving stale cache");
            // Serve stale cache
            let base_url = format!("{origin}/repository/{}", config.name);
            match store.get_packument(package, &base_url).await {
                Ok(Some(packument)) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_string(&packument).unwrap_or_default(),
                )
                    .into_response(),
                _ => {
                    let _ = cache_ttl_secs; // suppress unused warning
                    DepotError::Upstream(format!("upstream error: {e}")).into_response()
                }
            }
        }
    }
}

/// Cache version metadata from an upstream packument.
async fn cache_upstream_packument(
    state: &FormatState,
    config: &RepoConfig,
    package: &str,
    packument: &serde_json::Value,
) {
    let blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(_) => return,
    };
    let now = depot_core::repo::now_utc();

    // Cache dist-tags
    if let Some(tags_obj) = packument.get("dist-tags").and_then(|v| v.as_object()) {
        let tags: HashMap<String, String> = tags_obj
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect();
        let tags_json = serde_json::to_string(&tags).unwrap_or_default();
        let tags_bytes = tags_json.as_bytes();
        let tags_blob_id = uuid::Uuid::new_v4().to_string();
        let tags_hash = blake3::hash(tags_bytes).to_hex().to_string();
        if blobs.put(&tags_blob_id, tags_bytes).await.is_ok() {
            let blob_rec = depot_core::store::kv::BlobRecord {
                schema_version: CURRENT_RECORD_VERSION,
                blob_id: tags_blob_id.clone(),
                hash: tags_hash.clone(),
                size: tags_bytes.len() as u64,
                created_at: now,
                store: config.store.clone(),
            };
            let store = config.store.clone();
            // Best-effort cache fill: on claim failure keep the freshly written
            // blob_id (matching the previously swallowed put_dedup_record error).
            let tags_blob_id =
                service::claim_or_reuse_blob(state.kv.as_ref(), blobs.as_ref(), &store, &blob_rec)
                    .await
                    .unwrap_or(tags_blob_id);
            let tags_record = depot_core::store::kv::ArtifactRecord {
                schema_version: CURRENT_RECORD_VERSION,
                id: String::new(),
                size: tags_bytes.len() as u64,
                content_type: "application/json".to_string(),
                kind: depot_core::store::kv::ArtifactKind::NpmMetadata,
                blob_id: Some(tags_blob_id.clone()),
                content_hash: Some(tags_hash.clone()),
                etag: Some(tags_hash.clone()),
                created_at: now,
                updated_at: now,
                last_accessed_at: now,
                path: String::new(),
                internal: true,
            };
            let dt_path = npm::dist_tags_path(package);
            let cn = config.name.clone();
            let _ = service::put_artifact(state.kv.as_ref(), &cn, &dt_path, &tags_record).await;
        }
    }

    // Cache each version's metadata
    if let Some(versions) = packument.get("versions").and_then(|v| v.as_object()) {
        for (version, ver_obj) in versions {
            let ver_json = serde_json::to_string(ver_obj).unwrap_or_default();
            let ver_bytes = ver_json.as_bytes();
            let ver_blob_id = uuid::Uuid::new_v4().to_string();
            let ver_hash = blake3::hash(ver_bytes).to_hex().to_string();
            if blobs.put(&ver_blob_id, ver_bytes).await.is_ok() {
                let blob_rec = depot_core::store::kv::BlobRecord {
                    schema_version: CURRENT_RECORD_VERSION,
                    blob_id: ver_blob_id.clone(),
                    hash: ver_hash.clone(),
                    size: ver_bytes.len() as u64,
                    created_at: now,
                    store: config.store.clone(),
                };
                let store = config.store.clone();
                // Best-effort cache fill: on claim failure keep the freshly written
                // blob_id (matching the previously swallowed put_dedup_record error).
                let ver_blob_id = service::claim_or_reuse_blob(
                    state.kv.as_ref(),
                    blobs.as_ref(),
                    &store,
                    &blob_rec,
                )
                .await
                .unwrap_or(ver_blob_id);
                let ver_record = depot_core::store::kv::ArtifactRecord {
                    schema_version: CURRENT_RECORD_VERSION,
                    id: String::new(),
                    size: ver_bytes.len() as u64,
                    content_type: "application/json".to_string(),
                    kind: depot_core::store::kv::ArtifactKind::NpmMetadata,
                    blob_id: Some(ver_blob_id.clone()),
                    content_hash: Some(ver_hash.clone()),
                    etag: Some(ver_hash.clone()),
                    created_at: now,
                    updated_at: now,
                    last_accessed_at: now,
                    path: String::new(),
                    internal: true,
                };
                let ver_path = npm::version_path(package, version);
                let cn = config.name.clone();
                let _ = service::put_artifact(state.kv.as_ref(), &cn, &ver_path, &ver_record).await;
            }
        }
    }
}

/// Rewrite tarball URLs in a packument to point to our server.
fn rewrite_packument_urls(packument: &serde_json::Value, base_url: &str) -> serde_json::Value {
    let mut result = packument.clone();
    if let Some(versions) = result.get_mut("versions").and_then(|v| v.as_object_mut()) {
        for (version, ver_obj) in versions.iter_mut() {
            // Extract name before mutable borrow of dist
            let name = ver_obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let bare_name = name.rsplit('/').next().unwrap_or(&name);
            let filename = format!("{}-{}.tgz", npm::normalize_name(bare_name), version);
            let tarball_url = format!(
                "{base_url}/{name}/-/{filename}",
                base_url = base_url.trim_end_matches('/'),
            );
            if let Some(dist) = ver_obj.get_mut("dist").and_then(|v| v.as_object_mut()) {
                dist.insert(
                    "tarball".to_string(),
                    serde_json::Value::String(tarball_url),
                );
            }
        }
    }
    result
}

async fn proxy_packument(
    state: &FormatState,
    config: &RepoConfig,
    package: &str,
    origin: &str,
) -> Response {
    let RepoKind::Proxy { ref members, .. } = config.kind else {
        return DepotError::Internal("expected proxy repo kind".into()).into_response();
    };

    // The cached merged packument is stored with a host-relative base so it can
    // be served to any external hostname; tarball URLs are rewritten to the
    // absolute request origin on every serve (both cache hits and fresh merges).
    let absolute_base = format!("{origin}/repository/{}", config.name);
    let serve_rewritten = |json: &str| -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(json).ok()?;
        let rewritten = rewrite_packument_urls(&value, &absolute_base);
        serde_json::to_string(&rewritten).ok()
    };

    let proxy_blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let proxy_store = npm_store_from_config(config, state, proxy_blobs.as_ref());

    // Check if cached packument is fresh (newer than stale watermark).
    let stale_ts = proxy_store
        .get_stale_flag()
        .await
        .ok()
        .flatten()
        .map(|r| r.updated_at);
    if let Ok(Some((json, record))) = proxy_store.get_cached_packument(package).await {
        let is_fresh = match stale_ts {
            Some(ts) => record.updated_at >= ts,
            None => true, // no stale flag means nothing changed
        };
        if is_fresh {
            let body = serve_rewritten(&json).unwrap_or(json);
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response();
        }
    }

    // Re-merge from members.
    let base_url = format!("/repository/{}", config.name);
    let mut merged_versions = serde_json::Map::new();
    let mut merged_dist_tags: HashMap<String, String> = HashMap::new();
    let mut merged_time = serde_json::Map::new();

    for member_name in members {
        let mn = member_name.clone();
        let member_config = match service::get_repo(state.kv.as_ref(), &mn).await {
            Ok(Some(c)) if c.format() == ArtifactFormat::Npm => c,
            _ => continue,
        };

        let member_blobs = match state.blob_store(&member_config.store).await {
            Ok(b) => b,
            Err(_) => continue,
        };

        let store = NpmStore {
            repo: member_name,
            kv: state.kv.as_ref(),
            blobs: member_blobs.as_ref(),
            store: &member_config.store,
            updater: &state.updater,
        };

        if let Ok(Some(packument)) = store.get_packument(package, &base_url).await {
            if let Some(versions) = packument.get("versions").and_then(|v| v.as_object()) {
                for (ver, obj) in versions {
                    if !merged_versions.contains_key(ver) {
                        merged_versions.insert(ver.clone(), obj.clone());
                    }
                }
            }
            if let Some(tags) = packument.get("dist-tags").and_then(|v| v.as_object()) {
                for (k, v) in tags {
                    if !merged_dist_tags.contains_key(k) {
                        if let Some(s) = v.as_str() {
                            merged_dist_tags.insert(k.clone(), s.to_string());
                        }
                    }
                }
            }
            if let Some(time) = packument.get("time").and_then(|v| v.as_object()) {
                for (k, v) in time {
                    if !merged_time.contains_key(k) {
                        merged_time.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }

    if merged_versions.is_empty() {
        return DepotError::NotFound("package not found in any member".into()).into_response();
    }

    let packument = serde_json::json!({
        "name": package,
        "dist-tags": merged_dist_tags,
        "versions": merged_versions,
        "time": merged_time,
    });

    let json = serde_json::to_string(&packument).unwrap_or_default();

    // Cache the merged result on the proxy (host-relative; see absolute_base above).
    let _ = proxy_store.store_cached_packument(package, &json).await;

    let body = serve_rewritten(&json).unwrap_or(json);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// Extract version from an npm tarball filename.
/// Format: `{name}-{version}.tgz`
///
/// Primary: strip the known package name prefix + `-` from the stem.
/// Fallback: scan from the end for the rightmost `-` followed by a digit.
pub fn extract_version_from_filename(filename: &str, package_name: &str) -> Option<String> {
    let stem = filename.strip_suffix(".tgz")?;

    // Primary: strip known package name prefix
    let bare_name = package_name.rsplit('/').next().unwrap_or(package_name);
    let norm = npm::normalize_name(bare_name);
    let prefix = format!("{norm}-");
    if let Some(version) = stem.strip_prefix(&prefix) {
        if !version.is_empty() {
            return Some(version.to_string());
        }
    }

    // Fallback: scan from the end for the rightmost `-` followed by a digit
    for (i, c) in stem.char_indices().rev() {
        if c == '-' && i + 1 < stem.len() {
            let (_, version) = stem.split_at(i + 1);
            if version.starts_with(|ch: char| ch.is_ascii_digit()) && !version.is_empty() {
                return Some(version.to_string());
            }
        }
    }
    None
}

/// Split an absolute `http(s)://authority/rest` URL into `(scheme, authority, path)`.
/// Returns `None` for relative URLs.
fn split_absolute_url(url: &str) -> Option<(&str, &str, &str)> {
    let (scheme, after) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = after.split_once('/').map_or(after, |(a, _)| a);
    let path = &url[scheme.len() + 3 + authority.len()..];
    Some((scheme, authority, path))
}

/// Decide which URL to fetch a tarball's bytes from, given the registry-reported
/// `tarball_url` (which may be relative or absolute) and the cache's configured
/// `upstream_url`.
///
/// - Relative URLs are resolved against the upstream origin.
/// - Absolute URLs that point at the **same host** as `upstream_url` are
///   re-pointed to the upstream's own scheme/host. This keeps caching another
///   Artifact Depot working even when that upstream self-reports a different
///   scheme than the cache reaches it over (e.g. a plain-HTTP depot-on-depot
///   setup where the upstream defaults its packument origin to `https`).
/// - Absolute URLs on a different host (a genuine CDN/registry) are used as-is.
fn resolve_upstream_tarball_url(tarball_url: &str, upstream_url: &str) -> String {
    let upstream_origin = match split_absolute_url(upstream_url) {
        Some((scheme, authority, _)) => format!("{scheme}://{authority}"),
        None => return tarball_url.to_string(),
    };
    match split_absolute_url(tarball_url) {
        Some((_, authority, path)) => {
            let upstream_authority = upstream_origin.split_once("://").map(|(_, a)| a);
            if upstream_authority == Some(authority) {
                // Same host — trust the upstream's scheme/host, keep the path.
                format!("{upstream_origin}{path}")
            } else {
                // Different host (real CDN/registry) — use as reported.
                tarball_url.to_string()
            }
        }
        None => format!("{upstream_origin}{tarball_url}"),
    }
}

fn npm_stream_response(
    reader: depot_core::store::blob::BlobReader,
    size: u64,
    filename: &str,
) -> Response {
    let stream = ReaderStream::with_capacity(reader, 256 * 1024);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/octet-stream"
            .parse()
            .unwrap_or(header::HeaderValue::from_static("application/octet-stream")),
    );
    if let Ok(v) = format!("attachment; filename=\"{filename}\"").parse() {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    headers.insert(header::CONTENT_LENGTH, size.into());
    (StatusCode::OK, headers, Body::from_stream(stream)).into_response()
}

async fn hosted_download(
    state: &FormatState,
    repo_name: &str,
    name: &str,
    version: &str,
    filename: &str,
) -> Response {
    let (blobs, blob_config) =
        match depot_core::api_helpers::resolve_repo_blob_store(state, repo_name).await {
            Ok(b) => b,
            Err(e) => return e.into_response(),
        };
    let store_name = blob_config.store.clone();
    let store = NpmStore {
        repo: repo_name,
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: &store_name,
        updater: &state.updater,
    };

    match store.get_tarball(name, version, filename).await {
        Ok(Some((reader, size, _record))) => npm_stream_response(reader, size, filename),
        Ok(None) => DepotError::NotFound("tarball not found".into()).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn cache_download(
    state: &FormatState,
    config: &RepoConfig,
    name: &str,
    version: &str,
    filename: &str,
) -> Response {
    let RepoKind::Cache {
        ref upstream_url,
        ref upstream_auth,
        ..
    } = config.kind
    else {
        return DepotError::Internal("expected cache repo kind".into()).into_response();
    };
    let blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let store = NpmStore {
        repo: &config.name,
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: &config.store,
        updater: &state.updater,
    };

    // Check local cache first
    if let Ok(Some((reader, size, _record))) = store.get_tarball(name, version, filename).await {
        return npm_stream_response(reader, size, filename);
    }

    // Fetch from upstream — we need to find the tarball URL from cached version metadata
    let ver_meta = store
        .get_version_metadata(name, version)
        .await
        .ok()
        .flatten();
    let upstream_tarball_url = ver_meta.and_then(|meta| {
        let obj: serde_json::Value = serde_json::from_str(&meta).ok()?;
        obj.get("dist")?
            .get("tarball")?
            .as_str()
            .map(|s| s.to_string())
    });

    // If no cached metadata, construct the tarball URL from the upstream base URL
    let tarball_url = upstream_tarball_url
        .unwrap_or_else(|| format!("{}/{name}/-/{filename}", upstream_url.trim_end_matches('/')));

    // Resolve the URL we'll actually fetch the tarball bytes from.
    let url = resolve_upstream_tarball_url(&tarball_url, upstream_url);

    {
        match npm::fetch_upstream_tarball(&state.http, &url, upstream_auth.as_ref()).await {
            Ok(resp) => {
                let data = match resp.bytes().await {
                    Ok(b) => b.to_vec(),
                    Err(e) => {
                        return DepotError::Upstream(format!("failed to read tarball: {e}"))
                            .into_response()
                    }
                };

                // Cache it locally
                let _ = store
                    .put_package_from_upstream(
                        name,
                        version,
                        filename,
                        &data,
                        "", // sha1 unknown
                        "", // sha512 unknown
                        &serde_json::json!({"name": name, "version": version}).to_string(),
                        &url,
                    )
                    .await;

                // Serve it
                if let Ok(Some((reader, size, _))) =
                    store.get_tarball(name, version, filename).await
                {
                    return npm_stream_response(reader, size, filename);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to fetch tarball from upstream");
            }
        }
    }

    DepotError::NotFound("tarball not found".into()).into_response()
}

async fn proxy_download(
    state: &FormatState,
    config: &RepoConfig,
    name: &str,
    version: &str,
    filename: &str,
) -> Response {
    let RepoKind::Proxy { ref members, .. } = config.kind else {
        return DepotError::Internal("expected proxy repo kind".into()).into_response();
    };

    for member_name in members {
        let mn = member_name.clone();
        let member_config = match service::get_repo(state.kv.as_ref(), &mn).await {
            Ok(Some(c)) if c.format() == ArtifactFormat::Npm => c,
            _ => continue,
        };

        match member_config.repo_type() {
            RepoType::Hosted => {
                let resp = hosted_download(state, member_name, name, version, filename).await;
                if resp.status() == StatusCode::OK {
                    return resp;
                }
            }
            RepoType::Cache => {
                let resp = cache_download(state, &member_config, name, version, filename).await;
                if resp.status() == StatusCode::OK {
                    return resp;
                }
            }
            RepoType::Proxy => {
                let resp = hosted_download(state, member_name, name, version, filename).await;
                if resp.status() == StatusCode::OK {
                    return resp;
                }
            }
        }
    }

    DepotError::NotFound("tarball not found in any member".into()).into_response()
}

async fn publish_inner(
    state: &FormatState,
    user: &AuthenticatedUser,
    repo_name: &str,
    package: &str,
    body: &[u8],
) -> Response {
    let (target_repo, target_config, blobs) = match depot_core::api_helpers::upload_preamble(
        state,
        &user.0,
        repo_name,
        ArtifactFormat::Npm,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };

    // Parse the npm publish JSON body
    let payload: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return DepotError::BadRequest(format!("invalid JSON: {e}")).into_response();
        }
    };

    // Extract versions
    let versions = match payload.get("versions").and_then(|v| v.as_object()) {
        Some(v) => v,
        None => {
            return DepotError::BadRequest("missing 'versions' field".into()).into_response();
        }
    };

    // Extract dist-tags
    let mut dist_tags: HashMap<String, String> = payload
        .get("dist-tags")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    // Auto-create "latest" dist-tag if not present
    if !dist_tags.contains_key("latest") {
        // Use the last version from the versions map
        if let Some(last_version) = versions.keys().next_back() {
            dist_tags.insert("latest".to_string(), last_version.clone());
        }
    }

    // Extract _attachments
    let attachments = match payload.get("_attachments").and_then(|v| v.as_object()) {
        Some(a) => a,
        None => {
            return DepotError::BadRequest("missing '_attachments' field".into()).into_response();
        }
    };

    // Process each version
    for (version, ver_obj) in versions {
        let version_metadata_json = serde_json::to_string(ver_obj).unwrap_or_default();

        // Determine attachment filename
        let expected_filename = format!("{}-{}.tgz", package, version);
        // Try the expected filename first, then try normalized
        let norm_filename = format!("{}-{}.tgz", npm::normalize_name(package), version);
        let attachment_key = if attachments.contains_key(&expected_filename) {
            expected_filename.clone()
        } else if attachments.contains_key(&norm_filename) {
            norm_filename.clone()
        } else {
            // Try to find any matching attachment
            match attachments.keys().next() {
                Some(k) => k.clone(),
                None => {
                    return DepotError::BadRequest(format!(
                        "no attachment found for version {version}"
                    ))
                    .into_response();
                }
            }
        };

        // Normalize the stored filename to match what packument URLs produce
        let bare_name = package.rsplit('/').next().unwrap_or(package);
        let stored_filename = format!("{}-{}.tgz", npm::normalize_name(bare_name), version);

        let attachment = &attachments[&attachment_key];
        let data_b64 = match attachment.get("data").and_then(|v| v.as_str()) {
            Some(d) => d,
            None => {
                return DepotError::BadRequest("attachment missing 'data' field".into())
                    .into_response();
            }
        };

        use base64::Engine;
        let tarball_data = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
            Ok(d) => d,
            Err(e) => {
                return DepotError::BadRequest(format!("invalid base64 in attachment: {e}"))
                    .into_response();
            }
        };

        let store = NpmStore {
            repo: &target_repo,
            kv: state.kv.as_ref(),
            blobs: blobs.as_ref(),
            store: &target_config.store,
            updater: &state.updater,
        };

        match store
            .put_package(
                package,
                version,
                &stored_filename,
                &tarball_data,
                &version_metadata_json,
                &dist_tags,
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error = %e, "npm publish failed");
                return e.into_response();
            }
        }
    }

    // Set stale and propagate to parent proxies.
    let store = NpmStore {
        repo: &target_repo,
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: &target_config.store,
        updater: &state.updater,
    };
    let _ = store.set_stale_flag().await;
    depot_core::api_helpers::propagate_staleness_to_parents(
        state,
        repo_name,
        ArtifactFormat::Npm,
        "_npm/metadata_stale",
    )
    .await;

    StatusCode::OK.into_response()
}

#[allow(dead_code)]
#[derive(Deserialize)]
pub struct SearchQuery {
    text: Option<String>,
    #[serde(default = "default_search_size")]
    size: usize,
}

fn default_search_size() -> usize {
    20
}

/// Implementation of npm `-/v1/search` used by the `try_handle_repository_path` dispatcher.
async fn search_packages(
    state: &FormatState,
    config: &RepoConfig,
    text: &str,
    size: usize,
) -> Response {
    let blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let store = NpmStore {
        repo: &config.name,
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: &config.store,
        updater: &state.updater,
    };

    let packages = match store.list_packages().await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };

    let text_lower = text.to_lowercase();
    let filtered: Vec<_> = packages
        .into_iter()
        .filter(|name| text_lower.is_empty() || name.contains(&text_lower))
        .take(size)
        .collect();

    let mut objects: Vec<serde_json::Value> = Vec::new();
    for name in &filtered {
        // Look up dist-tags to find "latest" version
        let version = match store.get_dist_tags(name).await {
            Ok(tags) => tags
                .get("latest")
                .cloned()
                .unwrap_or_else(|| "0.0.0".to_string()),
            Err(_) => "0.0.0".to_string(),
        };
        objects.push(serde_json::json!({
            "package": {
                "name": name,
                "version": version,
                "description": "",
            }
        }));
    }

    let result = serde_json::json!({ "objects": objects });

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&result).unwrap_or_default(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_absolute_url_parts() {
        assert_eq!(
            split_absolute_url("https://host:443/a/b/-/c.tgz"),
            Some(("https", "host:443", "/a/b/-/c.tgz"))
        );
        // No path component.
        assert_eq!(
            split_absolute_url("http://host"),
            Some(("http", "host", ""))
        );
        // Relative paths are not absolute URLs.
        assert_eq!(split_absolute_url("/repository/npm/pkg"), None);
        // Non-http schemes are rejected.
        assert_eq!(split_absolute_url("file:///etc/passwd"), None);
    }

    #[test]
    fn resolve_relative_against_upstream_origin() {
        // A relative tarball path resolves against the upstream origin, dropping
        // any path component of the upstream URL.
        assert_eq!(
            resolve_upstream_tarball_url("/pkg/-/pkg-1.0.0.tgz", "https://registry.npmjs.org"),
            "https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz"
        );
    }

    #[test]
    fn resolve_same_host_repoints_to_upstream_scheme() {
        // Caching another Artifact Depot reached over plain HTTP: the upstream
        // self-reports https (its default), but the cache must fetch over the
        // scheme it actually reaches the upstream on.
        assert_eq!(
            resolve_upstream_tarball_url(
                "https://127.0.0.1:8080/repository/npm-up/pkg/-/pkg-1.0.0.tgz",
                "http://127.0.0.1:8080/repository/npm-up"
            ),
            "http://127.0.0.1:8080/repository/npm-up/pkg/-/pkg-1.0.0.tgz"
        );
    }

    #[test]
    fn resolve_different_host_absolute_used_as_is() {
        // A genuine CDN/registry on a different host is fetched as reported.
        let cdn = "https://cdn.example.com/pkg/-/pkg-1.0.0.tgz";
        assert_eq!(
            resolve_upstream_tarball_url(cdn, "https://registry.npmjs.org"),
            cdn
        );
    }
}
