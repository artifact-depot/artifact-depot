// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Helm chart repository HTTP API.
//!
//! Handles chart upload, index.yaml serving (with on-demand rebuild),
//! chart download, and cache/proxy support.

use axum::{
    body::Body,
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use std::collections::BTreeMap;
use tokio_util::io::ReaderStream;

use depot_core::store_from_config_fn;

use crate::store::HelmStore;
use depot_core::auth::AuthenticatedUser;
use depot_core::error::{DepotError, Retryability};
use depot_core::format_state::FormatState;
use depot_core::repo::proxy::MAX_PROXY_DEPTH;
use depot_core::service;
use depot_core::store::kv::{ArtifactFormat, RepoConfig, RepoKind, RepoType};

store_from_config_fn!(helm_store_from_config, HelmStore);

/// Emit a structured upstream request event for the dashboard/logging pipeline.
/// Inlined here to avoid a cross-crate dependency on depot's log_export module.
fn emit_upstream_event(
    method: &str,
    url: &str,
    status: u16,
    elapsed: std::time::Duration,
    bytes_sent: u64,
    bytes_recv: u64,
    action: &str,
) {
    let peer = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_string(),
            })
        })
        .unwrap_or_default();
    let path = reqwest::Url::parse(url)
        .ok()
        .map(|u| u.path().to_string())
        .unwrap_or_else(|| url.to_string());
    let elapsed_ns = elapsed.as_nanos() as u64;
    tracing::info!(
        target: "depot.request",
        request_id = "",
        username = "",
        ip = peer.as_str(),
        method,
        path = path.as_str(),
        status,
        action,
        elapsed_ns,
        bytes_recv,
        bytes_sent,
        direction = "upstream",
        "request"
    );
}

// =============================================================================
// /repository/{repo}/... dispatch for Helm repos (writes)
// =============================================================================

/// Handle a write request under `/repository/{repo}/<path>` for a Helm repo.
/// Returns `None` if the (method, path) combination is not a Helm write.
pub async fn try_handle_repository_write(
    state: &FormatState,
    user: &AuthenticatedUser,
    config: &RepoConfig,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: Body,
) -> Option<Response> {
    if method == Method::POST && path == "upload" {
        return Some(handle_upload(state, user, config, method, headers, body).await);
    }
    None
}

async fn handle_upload(
    state: &FormatState,
    user: &AuthenticatedUser,
    config: &RepoConfig,
    method: &Method,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let (_target_repo, target_config, blobs) = match depot_core::api_helpers::upload_preamble(
        state,
        &user.0,
        &config.name,
        ArtifactFormat::Helm,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };

    let mut multipart =
        match depot_core::api_helpers::multipart_from_body(method, headers, body).await {
            Ok(m) => m,
            Err(e) => return e,
        };

    let mut blob_info: Option<(String, String, String, u64)> = None;

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "chart" => match depot_core::api_helpers::stream_multipart_field_to_blob(
                &mut field,
                blobs.as_ref(),
            )
            .await
            {
                Ok(r) => blob_info = Some((r.blob_id, r.blake3, r.sha256, r.size)),
                Err(e) => return e.into_response(),
            },
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let (blob_id, blake3, sha256, size) = match blob_info {
        Some(info) => info,
        None => return DepotError::BadRequest("missing 'chart' field".into()).into_response(),
    };

    let store = helm_store_from_config(&target_config, state, blobs.as_ref());

    match store.commit_chart(&blob_id, &blake3, &sha256, size).await {
        Ok(_) => {
            depot_core::api_helpers::propagate_staleness_to_parents(
                state,
                &config.name,
                ArtifactFormat::Helm,
                "_helm/metadata_stale",
            )
            .await;
            StatusCode::OK.into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "helm upload failed");
            e.into_response()
        }
    }
}

/// Handle a Helm request arriving on `/repository/{repo}/{path}`.
/// Dispatches `index.yaml` and bare `.tgz` paths (charts at the repo root)
/// to Helm-specific handlers, or returns `None` to fall through to the
/// generic raw handler.
pub async fn try_handle_repository_path(
    state: &FormatState,
    config: &RepoConfig,
    path: &str,
    query: Option<&str>,
) -> Option<Response> {
    let _ = query;
    if path == "index.yaml" {
        return Some(match config.repo_type() {
            RepoType::Hosted => hosted_get_index(state, config).await,
            RepoType::Cache => cache_get_index(state, config).await,
            RepoType::Proxy => proxy_get_index(state, config).await,
        });
    }
    // Charts live at the repo root: any `*.tgz` is a chart download.
    if path.ends_with(".tgz") {
        return Some(match config.repo_type() {
            RepoType::Hosted => hosted_download_chart(state, config, path).await,
            RepoType::Cache => cache_download_chart(state, config, path).await,
            RepoType::Proxy => proxy_download_chart(state, config, path, 0).await,
        });
    }
    None
}

// =============================================================================
// Hosted index serving (with on-demand rebuild)
// =============================================================================

async fn hosted_get_index(state: &FormatState, config: &RepoConfig) -> Response {
    let blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let store = helm_store_from_config(config, state, blobs.as_ref());

    // Check stale flag and rebuild if needed.
    let is_stale = store.is_metadata_stale().await.unwrap_or(false);
    if is_stale {
        if let Err(e) = store.rebuild_index().await {
            tracing::error!(repo = config.name, error = %e, "helm index rebuild failed");
            return e.into_response();
        }
    }

    match store.get_index().await {
        Ok(Some(data)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/x-yaml")],
            Body::from(data),
        )
            .into_response(),
        Ok(None) => {
            // No index exists. Try a rebuild — charts may have been uploaded
            // via the raw handler (e.g. restore) before the stale-flag fix.
            if let Err(e) = store.rebuild_index().await {
                tracing::warn!(repo = config.name, error = %e, "helm index rebuild failed");
            }
            match store.get_index().await {
                Ok(Some(data)) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/x-yaml")],
                    Body::from(data),
                )
                    .into_response(),
                _ => {
                    let empty_index =
                        "apiVersion: v1\nentries: {}\ngenerated: \"2000-01-01T00:00:00Z\"\n";
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/x-yaml")],
                        Body::from(empty_index),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => e.into_response(),
    }
}

// =============================================================================
// Hosted chart download
// =============================================================================

async fn hosted_download_chart(
    state: &FormatState,
    config: &RepoConfig,
    filename: &str,
) -> Response {
    let blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let store = helm_store_from_config(config, state, blobs.as_ref());

    match store.get_chart_by_filename(filename).await {
        Ok(Some((reader, size, _record))) => chart_stream_response(reader, size, filename),
        Ok(None) => DepotError::NotFound("chart not found".into()).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Build a streaming response for a chart .tgz blob.
fn chart_stream_response(
    reader: depot_core::store::blob::BlobReader,
    size: u64,
    filename: &str,
) -> Response {
    let stream = ReaderStream::with_capacity(reader, 256 * 1024);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/gzip"),
    );
    if let Ok(v) = format!("attachment; filename=\"{filename}\"").parse() {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    headers.insert(header::CONTENT_LENGTH, size.into());
    (StatusCode::OK, headers, Body::from_stream(stream)).into_response()
}

// =============================================================================
// YAML response helper
// =============================================================================

fn yaml_response(data: impl Into<Body>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/x-yaml")],
        data.into(),
    )
        .into_response()
}

// =============================================================================
// Cache index: reusable refresh + HTTP handler
// =============================================================================

/// Refresh a cache repo's index, checking TTL and fetching upstream if needed.
/// Returns the index bytes, or `None` if both upstream and cache are unavailable.
/// If the upstream index content changed, propagates staleness to parent proxies.
async fn refresh_cache_index(
    state: &FormatState,
    config: &RepoConfig,
) -> Result<Option<Vec<u8>>, DepotError> {
    let RepoKind::Cache {
        ref upstream_url,
        cache_ttl_secs,
        ref upstream_auth,
        ..
    } = config.kind
    else {
        return Err(DepotError::Internal("expected cache repo kind".into()));
    };
    let ttl = cache_ttl_secs;

    let blobs = state.blob_store(&config.store).await?;
    let store = helm_store_from_config(config, state, blobs.as_ref());

    // Check cached index freshness.
    let cached = service::get_artifact(state.kv.as_ref(), &config.name, "_helm/meta/index.yaml")
        .await
        .ok()
        .flatten();

    let cache_fresh = if let Some(ref rec) = cached {
        let now = depot_core::repo::now_utc();
        ttl > 0 && (now - rec.updated_at).num_seconds() < ttl as i64
    } else {
        false
    };

    if cache_fresh {
        if let Ok(Some(data)) = store.get_index().await {
            return Ok(Some(data));
        }
    }

    // Fetch from upstream.
    let upstream_index_url = format!("{}/index.yaml", upstream_url.trim_end_matches('/'));
    let req = depot_core::repo::apply_upstream_auth(
        state.http.get(&upstream_index_url),
        upstream_auth.as_ref(),
    );

    let upstream_start = std::time::Instant::now();
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            emit_upstream_event(
                "GET",
                &upstream_index_url,
                resp.status().as_u16(),
                upstream_start.elapsed(),
                0,
                resp.content_length().unwrap_or(0),
                "upstream.helm.index",
            );
            let data = match resp.bytes().await {
                Ok(b) => b.to_vec(),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read upstream helm index");
                    return Ok(store.get_index().await.ok().flatten());
                }
            };

            let text = String::from_utf8_lossy(&data);
            let now = depot_core::repo::now_utc();
            if let Ok(changed) = store.store_index(&text, now).await {
                if changed {
                    depot_core::api_helpers::propagate_staleness_to_parents(
                        state,
                        &config.name,
                        ArtifactFormat::Helm,
                        "_helm/metadata_stale",
                    )
                    .await;
                }
            }

            Ok(Some(data))
        }
        _ => {
            // Upstream unavailable — serve stale cache.
            Ok(store.get_index().await.ok().flatten())
        }
    }
}

async fn cache_get_index(state: &FormatState, config: &RepoConfig) -> Response {
    match refresh_cache_index(state, config).await {
        Ok(Some(data)) => yaml_response(data),
        Ok(None) => DepotError::NotFound("index.yaml not found".into()).into_response(),
        Err(e) => e.into_response(),
    }
}

// =============================================================================
// Cache chart download helper
// =============================================================================

async fn cache_download_chart(
    state: &FormatState,
    config: &RepoConfig,
    filename: &str,
) -> Response {
    let blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let store = helm_store_from_config(config, state, blobs.as_ref());
    let RepoKind::Cache {
        ref upstream_url,
        ref upstream_auth,
        ..
    } = config.kind
    else {
        return DepotError::Internal("expected cache repo kind".into()).into_response();
    };

    let key = format!("helm:{}:{}", config.name, filename);

    loop {
        // Check local cache first.
        if let Ok(Some((reader, size, _record))) = store.get_chart_by_filename(filename).await {
            return chart_stream_response(reader, size, filename);
        }

        match state.inflight.try_claim(&key) {
            Ok(_guard) => {
                // Leader: fetch upstream -> temp -> commit -> serve.
                // Try charts/ prefix first, fall back to bare path (some
                // upstreams like Nexus serve charts at the root).
                let base = upstream_url.trim_end_matches('/');
                let urls = [
                    format!("{base}/charts/{filename}"),
                    format!("{base}/{filename}"),
                ];
                let mut resp = None;
                for url in &urls {
                    let req = depot_core::repo::apply_upstream_auth(
                        state.http.get(url),
                        upstream_auth.as_ref(),
                    );
                    let upstream_start = std::time::Instant::now();
                    if let Ok(r) = req.send().await {
                        emit_upstream_event(
                            "GET",
                            url,
                            r.status().as_u16(),
                            upstream_start.elapsed(),
                            0,
                            r.content_length().unwrap_or(0),
                            "upstream.helm.chart",
                        );
                        if r.status().is_success() {
                            resp = Some(r);
                            break;
                        }
                    }
                }
                let resp = match resp {
                    Some(r) => r,
                    None => {
                        return DepotError::NotFound("chart not found upstream".into())
                            .into_response()
                    }
                };

                // Charts are small — buffer in memory, parse, then store.
                let (data, b3, sha_opt) =
                    match depot_core::repo::fetch_upstream_to_bytes(resp, true).await {
                        Ok(v) => v,
                        Err(e) => {
                            return DepotError::Upstream(format!("upstream error: {e}"))
                                .into_response();
                        }
                    };
                let sha = sha_opt.unwrap_or_default();

                if let Err(e) = store.put_cached_chart(&data, &b3, &sha).await {
                    tracing::error!(error = %e, "helm cache commit failed");
                    return DepotError::Storage(
                        Box::new(std::io::Error::other(e.to_string())),
                        Retryability::Transient,
                    )
                    .into_response();
                }

                // Serve the committed blob.
                match store.get_chart_by_filename(filename).await {
                    Ok(Some((reader, size, _record))) => {
                        return chart_stream_response(reader, size, filename)
                    }
                    _ => {
                        return DepotError::DataIntegrity("blob read failed".into()).into_response()
                    }
                }
                // _guard drops here -> notify_waiters()
            }
            Err(notify) => {
                notify.notified().await;
                continue; // retry from cache
            }
        }
    }
}

// =============================================================================
// Proxy index: cached merged index with staleness propagation
// =============================================================================

async fn proxy_get_index(state: &FormatState, config: &RepoConfig) -> Response {
    let RepoKind::Proxy { ref members, .. } = config.kind else {
        return DepotError::Internal("expected proxy repo kind".into()).into_response();
    };

    let blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let store = helm_store_from_config(config, state, blobs.as_ref());

    // Check if proxy's own cached index is still fresh.
    let is_stale = store.is_metadata_stale().await.unwrap_or(true);
    let has_expired = if !is_stale {
        has_expired_cache_member(state, config, 0).await
    } else {
        false
    };

    if !is_stale && !has_expired {
        if let Ok(Some(data)) = store.get_index().await {
            return yaml_response(data);
        }
        // No cached index — fall through to rebuild.
    }

    match rebuild_proxy_index(state, config, members, 0).await {
        Ok(data) => yaml_response(data),
        Err(e) => {
            // If rebuild fails, try serving stale cache.
            if let Ok(Some(data)) = store.get_index().await {
                return yaml_response(data);
            }
            e.into_response()
        }
    }
}

/// Check whether any cache member (direct or nested) has an expired TTL.
fn has_expired_cache_member<'a>(
    state: &'a FormatState,
    config: &'a RepoConfig,
    depth: u8,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
    Box::pin(async move {
        if depth > MAX_PROXY_DEPTH {
            return false;
        }
        let RepoKind::Proxy { ref members, .. } = config.kind else {
            return false;
        };

        for member_name in members {
            let member_config = match service::get_repo(state.kv.as_ref(), member_name).await {
                Ok(Some(c)) if c.format() == ArtifactFormat::Helm => c,
                _ => continue,
            };
            match member_config.kind {
                RepoKind::Cache { cache_ttl_secs, .. } => {
                    let cached = service::get_artifact(
                        state.kv.as_ref(),
                        &member_config.name,
                        "_helm/meta/index.yaml",
                    )
                    .await
                    .ok()
                    .flatten();
                    match cached {
                        Some(rec) => {
                            let now = depot_core::repo::now_utc();
                            if (now - rec.updated_at).num_seconds() >= cache_ttl_secs as i64 {
                                return true;
                            }
                        }
                        None => return true, // No cached index at all.
                    }
                }
                RepoKind::Proxy { .. }
                    if has_expired_cache_member(state, &member_config, depth + 1).await =>
                {
                    return true;
                }
                _ => {}
            }
        }
        false
    })
}

/// Rebuild the proxy's merged index from all member indexes.
///
/// Uses `Pin<Box<...>>` to support recursion through nested proxy members.
fn rebuild_proxy_index<'a>(
    state: &'a FormatState,
    config: &'a RepoConfig,
    members: &'a [String],
    depth: u8,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, DepotError>> + Send + 'a>> {
    Box::pin(async move {
        if depth > MAX_PROXY_DEPTH {
            return Err(DepotError::Internal(
                "proxy recursion depth exceeded".into(),
            ));
        }

        let blobs = state.blob_store(&config.store).await?;
        let store = helm_store_from_config(config, state, blobs.as_ref());

        // Clear stale flag at start — any concurrent change re-sets it.
        let _ = store.clear_stale_flag().await;

        // Collect and merge entries from all members.
        let mut merged_entries: BTreeMap<String, Vec<serde_yml::Value>> = BTreeMap::new();

        for member_name in members {
            let member_config = match service::get_repo(state.kv.as_ref(), member_name).await {
                Ok(Some(c)) if c.format() == ArtifactFormat::Helm => c,
                _ => continue,
            };

            let member_index = match member_config.repo_type() {
                RepoType::Hosted => {
                    let member_blobs = match state.blob_store(&member_config.store).await {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    let member_store =
                        helm_store_from_config(&member_config, state, member_blobs.as_ref());

                    if member_store.is_metadata_stale().await.unwrap_or(false) {
                        let _ = member_store.rebuild_index().await;
                    }
                    member_store.get_index().await.ok().flatten()
                }
                RepoType::Cache => {
                    // Check TTL and refresh from upstream if expired.
                    refresh_cache_index(state, &member_config)
                        .await
                        .ok()
                        .flatten()
                }
                RepoType::Proxy => {
                    // Recursively ensure nested proxy's index is fresh.
                    let member_blobs = match state.blob_store(&member_config.store).await {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    let member_store =
                        helm_store_from_config(&member_config, state, member_blobs.as_ref());

                    let is_stale = member_store.is_metadata_stale().await.unwrap_or(true);
                    let has_cached = member_store.get_index().await.ok().flatten();
                    let needs_rebuild = is_stale
                        || has_cached.is_none()
                        || has_expired_cache_member(state, &member_config, depth + 1).await;

                    if needs_rebuild {
                        if let RepoKind::Proxy {
                            members: ref child_members,
                            ..
                        } = member_config.kind
                        {
                            let _ = rebuild_proxy_index(
                                state,
                                &member_config,
                                child_members,
                                depth + 1,
                            )
                            .await;
                        }
                        member_store.get_index().await.ok().flatten()
                    } else {
                        has_cached
                    }
                }
            };

            if let Some(index_data) = member_index {
                merge_index_entries(&mut merged_entries, &index_data);
            }
        }

        // Serialize merged index.
        let yaml = serialize_merged_index(&merged_entries)?;

        // Store the merged index on the proxy repo.
        let now = depot_core::repo::now_utc();
        if let Ok(changed) = store.store_index(&yaml, now).await {
            if changed {
                depot_core::api_helpers::propagate_staleness_to_parents(
                    state,
                    &config.name,
                    ArtifactFormat::Helm,
                    "_helm/metadata_stale",
                )
                .await;
            }
        }

        Ok(yaml.into_bytes())
    })
}

/// Merge entries from a member's index.yaml into the accumulated map.
/// First-member-wins: later members don't override the same chart+version.
fn merge_index_entries(merged: &mut BTreeMap<String, Vec<serde_yml::Value>>, index_data: &[u8]) {
    let text = String::from_utf8_lossy(index_data);
    let Ok(yaml) = serde_yml::from_str::<serde_yml::Value>(&text) else {
        return;
    };
    let Some(entries) = yaml.get("entries").and_then(|e| e.as_mapping()) else {
        return;
    };
    for (chart_name, versions) in entries {
        let Some(versions) = versions.as_sequence() else {
            continue;
        };

        let entry = merged.entry(chart_name.clone()).or_default();

        for ver in versions {
            let ver_str = ver.get("version").and_then(|v| v.as_str()).unwrap_or("");
            let already_exists = entry.iter().any(|existing| {
                existing
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    == ver_str
            });
            if !already_exists {
                entry.push(ver.clone());
            }
        }
    }
}

/// Serialize a merged entries map into a complete Helm index.yaml string.
fn serialize_merged_index(
    entries: &BTreeMap<String, Vec<serde_yml::Value>>,
) -> Result<String, DepotError> {
    let mut index = serde_yml::Mapping::new();
    index.insert("apiVersion", serde_yml::Value::String("v1".to_string()));

    let mut entries_map = serde_yml::Mapping::new();
    for (name, versions) in entries {
        entries_map.insert(name.clone(), serde_yml::Value::Sequence(versions.clone()));
    }
    index.insert("entries", serde_yml::Value::Mapping(entries_map));

    let generated = chrono::Utc::now().to_rfc3339();
    index.insert("generated", serde_yml::Value::String(generated));

    serde_yml::to_string(&serde_yml::Value::Mapping(index)).map_err(|e| {
        tracing::error!(error = %e, "failed to serialize merged helm index");
        DepotError::Storage(
            Box::new(std::io::Error::other(e.to_string())),
            Retryability::Transient,
        )
    })
}

// =============================================================================
// Proxy chart download helper
// =============================================================================

fn proxy_download_chart<'a>(
    state: &'a FormatState,
    config: &'a RepoConfig,
    filename: &'a str,
    depth: u8,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + 'a>> {
    Box::pin(async move {
        if depth > MAX_PROXY_DEPTH {
            tracing::warn!(
                proxy = config.name,
                depth,
                "helm proxy recursion depth exceeded"
            );
            return DepotError::NotFound("chart not found in any member".into()).into_response();
        }
        let RepoKind::Proxy { ref members, .. } = config.kind else {
            return DepotError::Internal("expected proxy repo kind".into()).into_response();
        };

        for member_name in members {
            let mn = member_name.clone();
            let member_config = match service::get_repo(state.kv.as_ref(), &mn).await {
                Ok(Some(c)) if c.format() == ArtifactFormat::Helm => c,
                _ => continue,
            };

            let resp = match member_config.repo_type() {
                RepoType::Hosted => hosted_download_chart(state, &member_config, filename).await,
                RepoType::Cache => cache_download_chart(state, &member_config, filename).await,
                RepoType::Proxy => {
                    proxy_download_chart(state, &member_config, filename, depth + 1).await
                }
            };

            if resp.status() == StatusCode::OK {
                return resp;
            }
        }

        DepotError::NotFound("chart not found in any member".into()).into_response()
    })
}
