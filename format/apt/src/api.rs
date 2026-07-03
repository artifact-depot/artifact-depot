// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! APT HTTP API implementation.
//!
//! Handles .deb upload, metadata serving (with on-demand rebuild),
//! and public key serving for hosted, cache, and proxy repos.

use axum::{
    body::Body,
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use std::collections::HashSet;

use depot_core::store_from_config_fn;

use depot_core::api_helpers;
use depot_core::auth::AuthenticatedUser;
use depot_core::error::DepotError;
use depot_core::format_state::FormatState;
use depot_core::service;
use depot_core::store::kv::{ArtifactFormat, RepoConfig, RepoKind, RepoType};

use crate::store::{self as apt, AptStore};

store_from_config_fn!(apt_store_from_config, AptStore);

/// Ensure a hosted apt repo has a signing key, generating one on first use.
/// Returns `Ok(None)` for non-hosted repos: cache repos inherit the upstream
/// signature, and group/proxy repos opt in explicitly via the API. Callers on
/// the repo-creation path treat a failure as non-fatal (the repo is still
/// usable unsigned, and a key can be provisioned later via the API).
pub async fn ensure_repo_signing_key(
    state: &FormatState,
    config: &RepoConfig,
) -> depot_core::error::Result<Option<String>> {
    if !matches!(config.kind, RepoKind::Hosted) {
        return Ok(None);
    }
    let blobs = state.blob_store(&config.store).await?;
    let store = apt_store_from_config(config, state, blobs.as_ref());
    store.ensure_signing_key(&config.name).await
}

/// Generate a brand-new signing key for an apt repo (rotation), re-signing any
/// already-published metadata. Returns the armored public key.
pub async fn rotate_repo_signing_key(
    state: &FormatState,
    config: &RepoConfig,
) -> depot_core::error::Result<String> {
    let blobs = state.blob_store(&config.store).await?;
    let store = apt_store_from_config(config, state, blobs.as_ref());
    let public = store.generate_and_store_signing_key(&config.name).await?;
    store.resign_all_distributions().await?;
    Ok(public)
}

/// Import an externally-supplied armored secret key for an apt repo, re-signing
/// any already-published metadata. Returns the armored public key.
pub async fn import_repo_signing_key(
    state: &FormatState,
    config: &RepoConfig,
    secret_key_armor: &str,
) -> depot_core::error::Result<String> {
    let blobs = state.blob_store(&config.store).await?;
    let store = apt_store_from_config(config, state, blobs.as_ref());
    let public = store.import_signing_key(secret_key_armor).await?;
    store.resign_all_distributions().await?;
    Ok(public)
}

/// Handle an APT request arriving on `/repository/{repo}/{path}`.
/// Parses the path to detect `dists/` metadata and dispatches to the
/// appropriate APT-specific handler, or returns `None` to fall through
/// to the generic raw handler (e.g. for `pool/` package downloads).
pub async fn try_handle_repository_path(
    state: &FormatState,
    config: &RepoConfig,
    path: &str,
    query: Option<&str>,
) -> Option<Response> {
    let _ = query;

    // public.key → signed-release public key.
    if path == "public.key" {
        let blobs = match state.blob_store(&config.store).await {
            Ok(b) => b,
            Err(e) => return Some(e.into_response()),
        };
        let store = apt_store_from_config(config, state, blobs.as_ref());
        return Some(match store.get_public_key().await {
            Ok(Some(key)) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/pgp-keys")],
                key,
            )
                .into_response(),
            Ok(None) => DepotError::NotFound("no public key configured".into()).into_response(),
            Err(e) => e.into_response(),
        });
    }

    // distributions → JSON array of distribution names discovered via `dists/*/InRelease`.
    if path == "distributions" {
        let kv = state.kv.as_ref();
        let mut names: Vec<String> = Vec::new();
        if let Ok(v) = service::fold_all_artifacts(
            kv,
            &config.name,
            "dists/",
            Vec::new,
            |acc, sk, _record| {
                if let Some(rest) = sk.strip_prefix("dists/") {
                    if let Some(dist) = rest.strip_suffix("/InRelease") {
                        if !dist.is_empty() && !dist.contains('/') {
                            acc.push(dist.to_string());
                        }
                    }
                }
                Ok(())
            },
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .await
        {
            names.extend(v);
        }
        names.sort();
        names.dedup();
        return Some(axum::Json(names).into_response());
    }

    // dists/{distribution}/{meta_file}
    let rest = path.strip_prefix("dists/")?;
    let (distribution, meta_path) = rest.split_once('/')?;

    Some(match config.repo_type() {
        RepoType::Hosted => serve_metadata(state, config, distribution, meta_path).await,
        RepoType::Cache => cache_serve_metadata(state, config, distribution, meta_path).await,
        RepoType::Proxy => proxy_serve_metadata(state, config, distribution, meta_path).await,
    })
}

// =============================================================================
// /repository/{repo}/... dispatch for APT repos (writes)
// =============================================================================

/// Handle a write request under `/repository/{repo}/<path>` for an APT repo.
/// Returns `None` if the (method, path) combination is not an APT write.
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
    if method == Method::POST && path == "snapshots" {
        return Some(handle_create_snapshot(state, config, body).await);
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
    let (_target_repo, target_config, blobs) =
        match api_helpers::upload_preamble(state, &user.0, &config.name, ArtifactFormat::Apt).await
        {
            Ok(v) => v,
            Err(e) => return e.into_response(),
        };

    let mut multipart = match api_helpers::multipart_from_body(method, headers, body).await {
        Ok(m) => m,
        Err(e) => return e,
    };

    let mut distribution = None;
    let mut component = None;
    let mut blob_info: Option<(String, String, String, String, u64)> = None; // (filename, blob_id, blake3, sha256, size)

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "distribution" => {
                distribution = field.text().await.ok();
            }
            "component" => {
                component = field.text().await.ok();
            }
            "file" => {
                match api_helpers::stream_multipart_field_to_blob(&mut field, blobs.as_ref()).await
                {
                    Ok(r) => {
                        let filename = r.filename.unwrap_or_else(|| "package.deb".to_string());
                        blob_info = Some((filename, r.blob_id, r.blake3, r.sha256, r.size));
                    }
                    Err(e) => return e.into_response(),
                }
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let (filename, blob_id, blake3, sha256, size) = match blob_info {
        Some(info) => info,
        None => return DepotError::BadRequest("missing 'file' field".into()).into_response(),
    };
    let distribution = distribution.unwrap_or_else(|| "stable".to_string());
    let component = component.unwrap_or_else(|| "main".to_string());

    let store = apt_store_from_config(&target_config, state, blobs.as_ref());

    match store
        .commit_package(
            &blob_id,
            &blake3,
            &sha256,
            size,
            &distribution,
            &component,
            &filename,
        )
        .await
    {
        Ok(_) => {
            let stale_path = format!("_apt/metadata_stale/{distribution}");
            api_helpers::propagate_staleness_to_parents(
                state,
                &config.name,
                ArtifactFormat::Apt,
                &stale_path,
            )
            .await;
            StatusCode::OK.into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "apt upload failed");
            e.into_response()
        }
    }
}

async fn handle_create_snapshot(state: &FormatState, config: &RepoConfig, body: Body) -> Response {
    let config =
        match api_helpers::validate_format_repo(state, &config.name, ArtifactFormat::Apt).await {
            Ok(c) => c,
            Err(e) => return e.into_response(),
        };

    let bytes = match api_helpers::body_to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let req: CreateSnapshotRequest = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return DepotError::BadRequest(format!("invalid JSON body: {e}")).into_response(),
    };

    let blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let store = apt_store_from_config(&config, state, blobs.as_ref());

    let signing_key = store.get_signing_key().await.unwrap_or(None);
    match store
        .create_snapshot(&req.source, &req.name, signing_key.as_deref())
        .await
    {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct CreateSnapshotRequest {
    pub source: String,
    pub name: String,
}

// =============================================================================
// Hosted metadata serving (with on-demand rebuild)
// =============================================================================

async fn serve_metadata(
    state: &FormatState,
    config: &RepoConfig,
    distribution: &str,
    meta_path: &str,
) -> Response {
    let blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let store = apt_store_from_config(config, state, blobs.as_ref());

    // By-hash content is immutable — skip rebuild check.
    let is_by_hash = meta_path.contains("/by-hash/SHA256/");
    if !is_by_hash {
        let is_stale = store.is_metadata_stale(distribution).await.unwrap_or(false);
        if is_stale {
            let signing_key = store.get_signing_key().await.unwrap_or(None);
            if let Err(e) = store
                .rebuild_metadata(distribution, signing_key.as_deref())
                .await
            {
                tracing::error!(repo = config.name, error = %e, "metadata rebuild failed");
                return e.into_response();
            }
        }
    }

    match store.get_metadata(distribution, meta_path).await {
        Ok(Some(data)) => {
            let content_type = if meta_path.ends_with(".gz") {
                "application/gzip"
            } else if meta_path.contains("Release.gpg") {
                "application/pgp-signature"
            } else {
                "text/plain"
            };
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, content_type)],
                Body::from(data),
            )
                .into_response()
        }
        Ok(None) => DepotError::NotFound("metadata not found".into()).into_response(),
        Err(e) => e.into_response(),
    }
}

// =============================================================================
// Cache metadata serving
// =============================================================================

async fn cache_serve_metadata(
    state: &FormatState,
    config: &RepoConfig,
    distribution: &str,
    meta_path: &str,
) -> Response {
    let RepoKind::Cache {
        ref upstream_url,
        cache_ttl_secs,
        ref upstream_auth,
        ..
    } = config.kind
    else {
        return DepotError::Internal("expected cache repo kind".into()).into_response();
    };
    let ttl = cache_ttl_secs;

    let blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let store = apt_store_from_config(config, state, blobs.as_ref());

    // Check if the cached metadata is fresh.
    let bundle_ts_path = format!("_apt/cache_bundle_updated/{distribution}");
    let config_name = config.name.clone();
    let cached_ts = service::get_artifact(state.kv.as_ref(), &config_name, &bundle_ts_path)
        .await
        .ok()
        .flatten();

    let cache_fresh = if let Some(ref rec) = cached_ts {
        let now = depot_core::repo::now_utc();
        (now - rec.updated_at).num_seconds() < ttl as i64
    } else {
        false
    };

    if !cache_fresh {
        // Fetch upstream InRelease (the single source of truth).
        let base = upstream_url.trim_end_matches('/');
        let inrelease_url = format!("{base}/dists/{distribution}/InRelease");
        let mut http_req = state.http.get(&inrelease_url);
        if let Some(auth) = upstream_auth {
            http_req = http_req.basic_auth(&auth.username, auth.password.as_deref());
        }
        let upstream_start = std::time::Instant::now();
        match depot_core::repo::with_trace_context(http_req).send().await {
            Ok(resp) if resp.status().is_success() => {
                depot_core::repo::emit_upstream_event(
                    "GET",
                    &inrelease_url,
                    resp.status().as_u16(),
                    upstream_start.elapsed(),
                    0,
                    resp.content_length().unwrap_or(0),
                    "upstream.apt.release",
                );
                if let Ok(inrelease_bytes) = resp.bytes().await {
                    let inrelease_text = String::from_utf8_lossy(&inrelease_bytes);
                    let release_text = apt::extract_release_from_inrelease(&inrelease_text);

                    // Inject Acquire-By-Hash if upstream doesn't have it.
                    let inrelease_bytes = if !release_text.contains("Acquire-By-Hash:") {
                        let patched =
                            release_text.replace("SHA256:\n", "Acquire-By-Hash: yes\nSHA256:\n");
                        patched.into_bytes()
                    } else {
                        inrelease_bytes.to_vec()
                    };

                    // Store InRelease only — individual metadata files are
                    // fetched lazily on cache miss (see cache_fetch_by_hash).
                    let _ = store
                        .store_cached_inrelease(distribution, &inrelease_bytes)
                        .await;

                    // Propagate stale to parent proxy repos.
                    let _ = store.set_metadata_stale(distribution).await;
                    let stale_path = format!("_apt/metadata_stale/{distribution}");
                    api_helpers::propagate_staleness_to_parents(
                        state,
                        &config_name,
                        ArtifactFormat::Apt,
                        &stale_path,
                    )
                    .await;

                    // Update the freshness timestamp.
                    let now = depot_core::repo::now_utc();
                    let ts_record = depot_core::store::kv::ArtifactRecord {
                        schema_version: depot_core::store::kv::CURRENT_RECORD_VERSION,
                        id: String::new(),
                        size: 0,
                        content_type: "text/plain".to_string(),
                        created_at: now,
                        updated_at: now,
                        last_accessed_at: now,
                        path: String::new(),
                        blob_id: None,
                        content_hash: None,
                        etag: None,
                        kind: depot_core::store::kv::ArtifactKind::AptMetadata,
                        internal: true,
                    };
                    let _ = service::put_artifact(
                        state.kv.as_ref(),
                        &config_name,
                        &bundle_ts_path,
                        &ts_record,
                    )
                    .await;
                }
            }
            _ => {
                // Upstream unavailable — will serve stale cache below.
            }
        }
    }

    // Serve from per-file artifacts, fetching from upstream on cache miss.
    let data = match store.get_metadata(distribution, meta_path).await {
        Ok(Some(data)) => Some(data),
        Ok(None) => {
            // Lazy fetch: if the file is missing locally, try fetching from upstream.
            // This handles by-hash requests and mutable paths like Packages.gz
            // that weren't eagerly fetched during the InRelease refresh.
            let base = upstream_url.trim_end_matches('/');
            let upstream_path = if meta_path.contains("/by-hash/SHA256/") {
                // by-hash: resolve the hash back to a file path via the Release
                // file, then fetch the original file and store at the by-hash path.
                // For simplicity, just fetch the by-hash URL directly from upstream.
                format!("{base}/dists/{distribution}/{meta_path}")
            } else {
                format!("{base}/dists/{distribution}/{meta_path}")
            };
            let mut req = state.http.get(&upstream_path);
            if let Some(auth) = upstream_auth {
                req = req.basic_auth(&auth.username, auth.password.as_deref());
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(bytes) = resp.bytes().await {
                        // Cache the fetched file. For by-hash paths, store at the
                        // by-hash artifact path. For mutable paths (Packages.gz),
                        // resolve the hash and store at the by-hash path.
                        let _ = store
                            .store_cached_metadata(distribution, meta_path, &bytes)
                            .await;
                        Some(bytes.to_vec())
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        Err(e) => return e.into_response(),
    };

    match data {
        Some(data) => {
            let content_type = if meta_path.ends_with(".gz") {
                "application/gzip"
            } else if meta_path.contains("Release.gpg") {
                "application/pgp-signature"
            } else {
                "text/plain"
            };
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, content_type)],
                Body::from(data),
            )
                .into_response()
        }
        None => DepotError::NotFound("metadata not found".into()).into_response(),
    }
}

// =============================================================================
// Proxy metadata serving (merged from members)
// =============================================================================

/// Proxy metadata serving: cached merge from members.
///
/// Uses the same stale-check + rebuild pattern as hosted repos. Merged
/// metadata is stored as real by-hash artifacts on the proxy repo.
async fn proxy_serve_metadata(
    state: &FormatState,
    config: &RepoConfig,
    distribution: &str,
    meta_path: &str,
) -> Response {
    let RepoKind::Proxy { ref members, .. } = config.kind else {
        return DepotError::Internal("expected proxy repo kind".into()).into_response();
    };

    let proxy_blobs = match state.blob_store(&config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let proxy_store = apt_store_from_config(config, state, proxy_blobs.as_ref());

    // By-hash requests are immutable — skip rebuild.
    let is_by_hash = meta_path.contains("/by-hash/SHA256/");
    if !is_by_hash {
        let is_stale = proxy_store
            .is_metadata_stale(distribution)
            .await
            .unwrap_or(true);

        // Also rebuild if no InRelease exists yet (first access).
        let needs_rebuild = is_stale
            || proxy_store
                .get_metadata(distribution, "InRelease")
                .await
                .ok()
                .flatten()
                .is_none();

        if needs_rebuild {
            if let Err(e) =
                rebuild_proxy_apt_metadata(state, config, &proxy_store, members, distribution).await
            {
                tracing::error!(repo = config.name, error = %e, "proxy metadata rebuild failed");
                return e.into_response();
            }
        }
    }

    // Serve from proxy's own stored artifacts (same as hosted).
    match proxy_store.get_metadata(distribution, meta_path).await {
        Ok(Some(data)) => {
            let content_type = if meta_path.ends_with(".gz") {
                "application/gzip"
            } else if meta_path.contains("Release.gpg") {
                "application/pgp-signature"
            } else {
                "text/plain"
            };
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, content_type)],
                Body::from(data),
            )
                .into_response()
        }
        Ok(None) => DepotError::NotFound("metadata not found".into()).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Rebuild merged APT proxy metadata from all members and store as by-hash
/// artifacts on the proxy repo.
async fn rebuild_proxy_apt_metadata(
    state: &FormatState,
    _config: &RepoConfig,
    proxy_store: &apt::AptStore<'_>,
    members: &[String],
    distribution: &str,
) -> Result<(), DepotError> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    proxy_store.clear_stale_flag_pub(distribution).await?;

    // Collect components and architectures from all members.
    let mut all_archs: HashSet<String> = HashSet::new();
    let mut all_components: HashSet<String> = HashSet::new();
    all_archs.insert("amd64".to_string());

    // Ensure hosted members are fresh, collect components/archs.
    for member_name in members {
        let mn = member_name.clone();
        let member_config = match service::get_repo(state.kv.as_ref(), &mn).await {
            Ok(Some(c)) if c.format() == ArtifactFormat::Apt => c,
            _ => continue,
        };
        let mb = match state.blob_store(&member_config.store).await {
            Ok(b) => b,
            Err(_) => continue,
        };
        let member_store = apt_store_from_config(&member_config, state, mb.as_ref());

        if member_config.repo_type() == RepoType::Hosted {
            let is_stale = member_store
                .is_metadata_stale(distribution)
                .await
                .unwrap_or(false);
            if is_stale {
                let signing_key = member_store.get_signing_key().await.unwrap_or(None);
                let _ = member_store
                    .rebuild_metadata(distribution, signing_key.as_deref())
                    .await;
            }
        }

        if let Ok(Some(release_data)) = member_store.get_metadata(distribution, "Release").await {
            let text = String::from_utf8_lossy(&release_data);
            for line in text.lines() {
                if let Some(comps) = line.strip_prefix("Components:") {
                    for c in comps.split_whitespace() {
                        all_components.insert(c.to_string());
                    }
                } else if let Some(archs) = line.strip_prefix("Architectures:") {
                    for a in archs.split_whitespace() {
                        all_archs.insert(a.to_string());
                    }
                }
            }
        }
    }

    let components: Vec<String> = if all_components.is_empty() {
        vec!["main".to_string()]
    } else {
        let mut v: Vec<String> = all_components.into_iter().collect();
        v.sort();
        v
    };
    let mut arch_list: Vec<String> = all_archs.into_iter().collect();
    arch_list.sort();

    let now = depot_core::repo::now_utc();
    let mut release_checksums = Vec::new();

    // Merge Packages from all members for each component/arch, store as by-hash.
    for comp in &components {
        for arch in &arch_list {
            let packages_path = format!("{comp}/binary-{arch}/Packages");
            let mut merged = String::new();
            for member_name in members {
                let mn = member_name.clone();
                let member_config = match service::get_repo(state.kv.as_ref(), &mn).await {
                    Ok(Some(c)) if c.format() == ArtifactFormat::Apt => c,
                    _ => continue,
                };
                let mb = match state.blob_store(&member_config.store).await {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let member_store = apt_store_from_config(&member_config, state, mb.as_ref());
                if let Ok(Some(data)) = member_store
                    .get_metadata(distribution, &packages_path)
                    .await
                {
                    merged.push_str(&String::from_utf8_lossy(&data));
                }
            }

            let packages_bytes = merged.into_bytes();
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            std::io::Write::write_all(&mut encoder, &packages_bytes)?;
            let packages_gz = encoder.finish()?;

            let rel_plain = format!("{comp}/binary-{arch}/Packages");
            let rel_gz = format!("{comp}/binary-{arch}/Packages.gz");

            let sha_plain = proxy_store
                .store_metadata_file_pub(distribution, &rel_plain, &packages_bytes, now)
                .await?;
            let sha_gz = proxy_store
                .store_metadata_file_pub(distribution, &rel_gz, &packages_gz, now)
                .await?;

            release_checksums.push(apt::metadata_entry(
                rel_plain,
                sha_plain,
                packages_bytes.len() as u64,
            ));
            release_checksums.push(apt::metadata_entry(
                rel_gz,
                sha_gz,
                packages_gz.len() as u64,
            ));
        }
    }

    // Generate Release with Acquire-By-Hash: yes.
    let release_text =
        apt::generate_release_pub(distribution, &components, &arch_list, &release_checksums);

    // Sign and store InRelease (commit point).
    let signing_key = proxy_store.get_signing_key().await.unwrap_or(None);
    let inrelease_text = if let Some(ref key) = signing_key {
        match apt::sign_release_pub(key, &release_text) {
            Ok((inrelease, _)) => inrelease,
            Err(_) => release_text.clone(),
        }
    } else {
        release_text.clone()
    };

    proxy_store
        .store_inrelease_pub(distribution, inrelease_text.as_bytes(), now)
        .await?;

    Ok(())
}
