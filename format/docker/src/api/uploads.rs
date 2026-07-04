// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use metrics::{counter, gauge};
use sha2::{Digest, Sha256};

use depot_core::format_state::FormatState;
use depot_core::repo;
use depot_core::service;
use depot_core::store::kv::{ArtifactFormat, Capability, RepoKind, UploadSessionRecord};

use crate::store::DockerStore;

use super::helpers::{
    check_docker_permission, docker_error, hosted_store, location_prefix, resolve_blob_store,
    validate_docker_repo,
};
use super::types::UploadInitParams;

#[allow(clippy::too_many_arguments)]
pub async fn do_start_upload(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    params: &UploadInitParams,
    body: Body,
    req_headers: &HeaderMap,
    uri_authority: Option<&str>,
) -> Response {
    if let Err(r) = check_docker_permission(
        state,
        username,
        repo_name,
        Capability::Write,
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

    // Determine target repo for writes.
    let target_repo = match config.kind {
        RepoKind::Hosted => config.name.clone(),
        RepoKind::Proxy { .. } => {
            match repo::resolve_format_write_target(
                state.kv.as_ref(),
                &config,
                ArtifactFormat::Docker,
            )
            .await
            {
                Ok((name, _)) => name,
                Err(e) => {
                    return docker_error("UNSUPPORTED", &e.to_string(), StatusCode::BAD_REQUEST)
                }
            }
        }
        RepoKind::Cache { .. } => {
            return docker_error(
                "UNSUPPORTED",
                "cannot push to a cache repository",
                StatusCode::METHOD_NOT_ALLOWED,
            );
        }
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
        image,
        blobs.as_ref(),
        &target_config.store,
    );

    // Handle cross-repo blob mount.
    if let (Some(mount_digest), Some(from_repo)) = (&params.mount, &params.from) {
        if let Ok(Some(_)) = store.blob_exists(mount_digest).await {
            let mut headers = HeaderMap::new();
            headers.insert(
                "Location",
                format!(
                    "{}/blobs/{}",
                    location_prefix(repo_name, image),
                    mount_digest
                )
                .parse()
                .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
            headers.insert(
                "Docker-Content-Digest",
                mount_digest
                    .parse()
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
            return (StatusCode::CREATED, headers).into_response();
        }
        // Verify the caller has read access to the source repository.
        if let Err(r) = check_docker_permission(
            state,
            username,
            from_repo,
            Capability::Read,
            req_headers,
            uri_authority,
        )
        .await
        {
            return r;
        }
        // Resolve the from_repo's blob store for cross-repo mount.
        let from_repo_name = from_repo.clone();
        let from_config_opt = service::get_repo(state.kv.as_ref(), &from_repo_name)
            .await
            .ok()
            .flatten();
        let from_blobs = if let Some(ref fc) = from_config_opt {
            resolve_blob_store(state, fc).await.ok()
        } else {
            None
        };
        // Only attempt mount if we could resolve the from_repo's blob store.
        if let Some(ref from_blobs) = from_blobs {
            let from_store_name = from_config_opt
                .as_ref()
                .map(|c| c.store.as_str())
                .unwrap_or("default");
            let from_store = DockerStore {
                repo: from_repo,
                image: None,
                kv: state.kv.as_ref(),
                blobs: from_blobs.as_ref(),
                updater: &state.updater,
                store: from_store_name,
                track_access: true,
            };
            if let Ok(Some((size, blob_id, content_hash))) =
                from_store.blob_exists(mount_digest).await
            {
                match store
                    .mount_blob(mount_digest, &blob_id, &content_hash, size)
                    .await
                {
                    Ok(digest) => {
                        let mut headers = HeaderMap::new();
                        headers.insert(
                            "Location",
                            format!("{}/blobs/{}", location_prefix(repo_name, image), digest)
                                .parse()
                                .unwrap_or_else(|_| HeaderValue::from_static("")),
                        );
                        headers.insert(
                            "Docker-Content-Digest",
                            digest
                                .parse()
                                .unwrap_or_else(|_| HeaderValue::from_static("")),
                        );
                        return (StatusCode::CREATED, headers).into_response();
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

    // Handle monolithic upload — stream body through BlobWriter with dual hashing.
    if let Some(digest) = &params.digest {
        let mono_blob_id = uuid::Uuid::new_v4().to_string();

        let mut mono_writer = match blobs.create_writer(&mono_blob_id, None).await {
            Ok(w) => w,
            Err(e) => {
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
            }
        };
        let mut sha256 = Sha256::new();
        let mut blake3 = blake3::Hasher::new();
        let mut size = 0u64;

        let mut stream = body.into_data_stream();
        while let Some(result) = stream.next().await {
            let chunk: axum::body::Bytes = match result {
                Ok(c) => c,
                Err(e) => {
                    let _ = mono_writer.abort().await;
                    return docker_error(
                        "UNKNOWN",
                        &e.to_string(),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
            };
            if chunk.is_empty() {
                continue;
            }
            sha256.update(&chunk);
            blake3.update(&chunk);
            if let Err(e) = mono_writer.write_chunk(&chunk).await {
                let _ = mono_writer.abort().await;
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
            size += chunk.len() as u64;
        }

        let computed = format!("sha256:{:x}", sha256.finalize());
        if computed != *digest {
            let _ = mono_writer.abort().await;
            return docker_error(
                "DIGEST_INVALID",
                &format!("expected {}, got {}", digest, computed),
                StatusCode::BAD_REQUEST,
            );
        }

        if let Err(e) = mono_writer.finish().await {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }

        let blake3_hash = blake3.finalize().to_hex().to_string();
        let now = depot_core::repo::now_utc();
        let store_name = target_config.store.clone();
        let blob_rec = depot_core::store::kv::BlobRecord {
            schema_version: depot_core::store::kv::CURRENT_RECORD_VERSION,
            blob_id: mono_blob_id.clone(),
            hash: blake3_hash.clone(),
            size,
            created_at: now,
            store: store_name.clone(),
        };

        let mut record = depot_core::store::kv::ArtifactRecord {
            schema_version: depot_core::store::kv::CURRENT_RECORD_VERSION,
            id: String::new(),
            size,
            content_type: "application/octet-stream".to_string(),
            kind: depot_core::store::kv::ArtifactKind::DockerBlob {
                docker_digest: computed.clone(),
            },
            blob_id: Some(mono_blob_id.clone()),
            etag: Some(blake3_hash.clone()),
            content_hash: Some(blake3_hash),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
        };
        let blob_ref_path = store.blob_ref_path(&computed);
        let txn_result: Result<Option<String>, depot_core::error::DepotError> = async {
            let existing = service::put_dedup_record_counted(
                state.kv.as_ref(),
                &store_name,
                &blob_rec,
                &state.updater,
            )
            .await?;
            if let Some(ref existing_id) = existing {
                record.blob_id = Some(existing_id.to_string());
            }
            service::put_artifact(state.kv.as_ref(), &target_repo, &blob_ref_path, &record).await?;
            Ok(existing)
        }
        .await;

        match txn_result {
            Ok(dedup) => {
                if dedup.is_some() {
                    let _ = blobs.delete(&mono_blob_id).await;
                }
                // Per-repo dir stats only; store credit handled by
                // put_dedup_record_counted above.
                state
                    .updater
                    .dir_changed(&target_repo, &blob_ref_path, 1, size as i64)
                    .await;
                let mut headers = HeaderMap::new();
                headers.insert(
                    "Location",
                    format!("{}/blobs/{}", location_prefix(repo_name, image), computed)
                        .parse()
                        .unwrap_or_else(|_| HeaderValue::from_static("")),
                );
                headers.insert(
                    "Docker-Content-Digest",
                    computed
                        .parse()
                        .unwrap_or_else(|_| HeaderValue::from_static("")),
                );
                return (StatusCode::CREATED, headers).into_response();
            }
            Err(e) => {
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    // Normal chunked upload — create session, persist to KV.
    // Each PATCH chunk is stored as an ordinary blob; at PUT (finalize)
    // they are assembled into the final blob and hashed.
    let uuid = uuid::Uuid::new_v4().to_string();
    let blob_id = uuid::Uuid::new_v4().to_string();

    // Stream any initial body data as a chunk blob.
    let mut chunk_blob_ids = Vec::new();
    let mut initial_size = 0u64;
    let chunk_id = uuid::Uuid::new_v4().to_string();
    let mut writer = match blobs.create_writer(&chunk_id, None).await {
        Ok(w) => w,
        Err(e) => {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)
        }
    };
    let mut has_data = false;
    let mut stream = body.into_data_stream();
    while let Some(result) = stream.next().await {
        let chunk: axum::body::Bytes = match result {
            Ok(c) => c,
            Err(e) => {
                let _ = writer.abort().await;
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        };
        if chunk.is_empty() {
            continue;
        }
        has_data = true;
        if let Err(e) = writer.write_chunk(&chunk).await {
            let _ = writer.abort().await;
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        initial_size += chunk.len() as u64;
    }
    if has_data {
        if let Err(e) = writer.finish().await {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        chunk_blob_ids.push(chunk_id);
    } else {
        let _ = writer.abort().await;
    }

    let now = depot_core::repo::now_utc();
    let session = UploadSessionRecord {
        uuid: uuid.clone(),
        repo: target_repo,
        image: image.map(|s| s.to_string()),
        blob_id,
        store: target_config.store.clone(),
        size: initial_size,
        chunk_blob_ids,
        created_at: now,
        updated_at: now,
    };
    if let Err(e) = service::put_upload_session(state.kv.as_ref(), &session).await {
        return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    gauge!("docker_uploads_active").increment(1.0);

    let mut headers = HeaderMap::new();
    let loc = format!(
        "{}/blobs/uploads/{}",
        location_prefix(repo_name, image),
        uuid
    );
    headers.insert(
        "Location",
        loc.parse().unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        "Docker-Upload-UUID",
        uuid.parse()
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        "Range",
        "0-0"
            .parse()
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    (StatusCode::ACCEPTED, headers).into_response()
}

#[allow(clippy::too_many_arguments)]
pub async fn do_patch_upload(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    uuid: &str,
    body: Body,
    req_headers: &HeaderMap,
    uri_authority: Option<&str>,
) -> Response {
    if let Err(r) = check_docker_permission(
        state,
        username,
        repo_name,
        Capability::Write,
        req_headers,
        uri_authority,
    )
    .await
    {
        return r;
    }

    // Resolve the effective repo name, following proxy write-target resolution
    // so that the session match check works when default_docker_repo points to
    // a proxy whose write target differs from the proxy name itself.
    let effective_repo = match validate_docker_repo(state, repo_name).await {
        Ok(config) => match config.kind {
            RepoKind::Hosted => config.name,
            RepoKind::Proxy { .. } => {
                match repo::resolve_format_write_target(
                    state.kv.as_ref(),
                    &config,
                    ArtifactFormat::Docker,
                )
                .await
                {
                    Ok((name, _)) => name,
                    Err(_) => config.name,
                }
            }
            RepoKind::Cache { .. } => config.name,
        },
        Err(_) => repo_name.to_string(),
    };

    // Look up the upload session from KV (with version for optimistic locking).
    let uuid_owned = uuid.to_string();
    let (session, _version) =
        match service::get_upload_session_versioned(state.kv.as_ref(), &uuid_owned).await {
            Ok(Some(pair)) => {
                let (s, v) = pair;
                // Validate the session belongs to this repo/image context.
                // Two-segment routes pass repo_name as the actual repo.
                // Single-segment follow-ups (via Location header) pass repo_name
                // as either the repo (flat) or the image (namespaced).
                let matches = if image.is_some() {
                    s.repo == repo_name || s.repo == effective_repo
                } else {
                    s.repo == repo_name
                        || s.repo == effective_repo
                        || s.image.as_deref() == Some(repo_name)
                };
                if !matches {
                    return docker_error(
                        "BLOB_UPLOAD_UNKNOWN",
                        "upload not found",
                        StatusCode::NOT_FOUND,
                    );
                }
                (s, v)
            }
            Ok(None) => {
                return docker_error(
                    "BLOB_UPLOAD_UNKNOWN",
                    "upload not found",
                    StatusCode::NOT_FOUND,
                );
            }
            Err(e) => {
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        };

    // Resolve blob store and stream body as a new chunk blob.
    let blobs = match state.blob_store(&session.store).await {
        Ok(b) => b,
        Err(e) => {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let chunk_id = uuid::Uuid::new_v4().to_string();
    let mut writer = match blobs.create_writer(&chunk_id, None).await {
        Ok(w) => w,
        Err(e) => {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let mut chunk_bytes = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(result) = stream.next().await {
        let chunk: axum::body::Bytes = match result {
            Ok(c) => c,
            Err(e) => {
                let _ = writer.abort().await;
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        };
        if chunk.is_empty() {
            continue;
        }
        if let Err(e) = writer.write_chunk(&chunk).await {
            let _ = writer.abort().await;
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        counter!("docker_upload_bytes_total").increment(chunk.len() as u64);
        chunk_bytes += chunk.len() as u64;
    }

    if let Err(e) = writer.finish().await {
        return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Update the session with the new chunk using optimistic locking.
    // Retry on version conflict (another concurrent PATCH updated the session).
    const MAX_RETRIES: u32 = 3;
    let mut new_size = 0u64;
    for attempt in 0..MAX_RETRIES {
        let (session, version) = if attempt == 0 {
            // Use the session we already fetched on the first attempt.
            (session.clone(), _version)
        } else {
            // Re-read on retry to get the latest version.
            match service::get_upload_session_versioned(state.kv.as_ref(), &uuid_owned).await {
                Ok(Some(pair)) => pair,
                Ok(None) => {
                    return docker_error(
                        "BLOB_UPLOAD_UNKNOWN",
                        "upload not found",
                        StatusCode::NOT_FOUND,
                    );
                }
                Err(e) => {
                    return docker_error(
                        "UNKNOWN",
                        &e.to_string(),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
            }
        };

        let max_chunks = state.settings.max_docker_chunk_count() as usize;
        if session.chunk_blob_ids.len() >= max_chunks {
            return docker_error(
                "BLOB_UPLOAD_INVALID",
                "too many chunks",
                StatusCode::BAD_REQUEST,
            );
        }
        new_size = session.size + chunk_bytes;
        let mut updated_session = session;
        updated_session.size = new_size;
        updated_session.chunk_blob_ids.push(chunk_id.clone());
        updated_session.updated_at = depot_core::repo::now_utc();
        match service::put_upload_session_if_version(state.kv.as_ref(), &updated_session, version)
            .await
        {
            Ok(true) => break,
            Ok(false) => {
                if attempt == MAX_RETRIES - 1 {
                    return docker_error(
                        "UNKNOWN",
                        "concurrent upload session update conflict",
                        StatusCode::CONFLICT,
                    );
                }
                // Retry with fresh read.
                continue;
            }
            Err(e) => {
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

    let mut headers = HeaderMap::new();
    let loc = format!(
        "{}/blobs/uploads/{}",
        location_prefix(repo_name, image),
        uuid
    );
    headers.insert(
        "Location",
        loc.parse().unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        "Docker-Upload-UUID",
        uuid.parse()
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        "Range",
        format!("0-{}", new_size.saturating_sub(1))
            .parse()
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        "0".parse().unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    (StatusCode::ACCEPTED, headers).into_response()
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(level = "debug", name = "docker.complete_upload", skip(state, body, req_headers), fields(repo = repo_name, uuid, digest = expected_digest))]
pub async fn do_complete_upload(
    state: &FormatState,
    username: &str,
    repo_name: &str,
    image: Option<&str>,
    uuid: &str,
    expected_digest: &str,
    body: Body,
    req_headers: &HeaderMap,
    uri_authority: Option<&str>,
) -> Response {
    if let Err(r) = check_docker_permission(
        state,
        username,
        repo_name,
        Capability::Write,
        req_headers,
        uri_authority,
    )
    .await
    {
        return r;
    }

    // Resolve the effective repo name, following proxy write-target resolution
    // so that the session match check works when default_docker_repo points to
    // a proxy whose write target differs from the proxy name itself.
    let effective_repo = match validate_docker_repo(state, repo_name).await {
        Ok(config) => match config.kind {
            RepoKind::Hosted => config.name,
            RepoKind::Proxy { .. } => {
                match repo::resolve_format_write_target(
                    state.kv.as_ref(),
                    &config,
                    ArtifactFormat::Docker,
                )
                .await
                {
                    Ok((name, _)) => name,
                    Err(_) => config.name,
                }
            }
            RepoKind::Cache { .. } => config.name,
        },
        Err(_) => repo_name.to_string(),
    };

    // Look up the upload session from KV.
    let uuid_owned = uuid.to_string();
    let session = match service::get_upload_session(state.kv.as_ref(), &uuid_owned).await {
        Ok(Some(s)) => {
            let matches = if image.is_some() {
                s.repo == repo_name || s.repo == effective_repo
            } else {
                s.repo == repo_name
                    || s.repo == effective_repo
                    || s.image.as_deref() == Some(repo_name)
            };
            if !matches {
                return docker_error(
                    "BLOB_UPLOAD_UNKNOWN",
                    "upload not found",
                    StatusCode::NOT_FOUND,
                );
            }
            gauge!("docker_uploads_active").decrement(1.0);
            s
        }
        Ok(None) => {
            return docker_error(
                "BLOB_UPLOAD_UNKNOWN",
                "upload not found",
                StatusCode::NOT_FOUND,
            );
        }
        Err(e) => {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let blob_id = session.blob_id.clone();
    let target_repo = session.repo.clone();
    let stored_image = session.image.clone();
    let store_name = session.store.clone();
    let mut chunk_blob_ids = session.chunk_blob_ids.clone();

    // Resolve blob store.
    let blobs = match state.blob_store(&store_name).await {
        Ok(b) => b,
        Err(e) => {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    // If there's a final body chunk, store it as one more chunk blob.
    let final_chunk_id = uuid::Uuid::new_v4().to_string();
    let mut final_writer = match blobs.create_writer(&final_chunk_id, None).await {
        Ok(w) => w,
        Err(e) => {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let mut final_bytes = 0u64;
    let mut has_final_data = false;
    let mut stream = body.into_data_stream();
    while let Some(result) = stream.next().await {
        let chunk: axum::body::Bytes = match result {
            Ok(c) => c,
            Err(e) => {
                let _ = final_writer.abort().await;
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        };
        if chunk.is_empty() {
            continue;
        }
        has_final_data = true;
        if let Err(e) = final_writer.write_chunk(&chunk).await {
            let _ = final_writer.abort().await;
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        counter!("docker_upload_bytes_total").increment(chunk.len() as u64);
        final_bytes += chunk.len() as u64;
    }
    if has_final_data {
        if let Err(e) = final_writer.finish().await {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        chunk_blob_ids.push(final_chunk_id.clone());
    } else {
        let _ = final_writer.abort().await;
    }

    let size = session.size + final_bytes;

    // Assemble + hash: read all chunks, stream through a fresh writer + hashers.
    let mut writer = match blobs.create_writer(&blob_id, Some(size)).await {
        Ok(w) => w,
        Err(e) => {
            return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let mut sha256 = Sha256::new();
    let mut blake3 = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    for chunk_id in &chunk_blob_ids {
        let (mut reader, _chunk_size) = match blobs.open_read(chunk_id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                let _ = writer.abort().await;
                return docker_error(
                    "UNKNOWN",
                    "chunk blob missing during assembly",
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
            Err(e) => {
                let _ = writer.abort().await;
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        };
        loop {
            let n = match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    let _ = writer.abort().await;
                    return docker_error(
                        "UNKNOWN",
                        &e.to_string(),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
            };
            // SAFETY: n is returned by read() into buf, so n <= buf.len()
            #[allow(clippy::indexing_slicing)]
            let chunk = &buf[..n];
            sha256.update(chunk);
            blake3.update(chunk);
            if let Err(e) = writer.write_chunk(chunk).await {
                let _ = writer.abort().await;
                return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }
    if let Err(e) = writer.finish().await {
        return docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    let docker_digest = format!("sha256:{:x}", sha256.finalize());
    let blake3_hash = blake3.finalize().to_hex().to_string();

    // Verify digest.
    if docker_digest != expected_digest {
        let _ = blobs.delete(&blob_id).await;
        return docker_error(
            "DIGEST_INVALID",
            &format!("expected {}, got {}", expected_digest, docker_digest),
            StatusCode::BAD_REQUEST,
        );
    }

    // Use the stored image from the upload session, falling back to the URL image.
    let effective_image = stored_image.as_deref().or(image).map(|s| s.to_string());
    let loc_prefix = location_prefix(repo_name, image);

    tracing::info!(
        size_mb = size / (1024 * 1024),
        digest = %docker_digest,
        "blob upload received, committing",
    );

    // Create KV records for the blob.
    let now = depot_core::repo::now_utc();
    let blob_rec = depot_core::store::kv::BlobRecord {
        schema_version: depot_core::store::kv::CURRENT_RECORD_VERSION,
        blob_id: blob_id.clone(),
        hash: blake3_hash.clone(),
        size,
        created_at: now,
        store: store_name.clone(),
    };
    let store = DockerStore {
        repo: &target_repo,
        image: effective_image.as_deref(),
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        updater: &state.updater,
        store: &store_name,
        track_access: true,
    };

    let mut record = depot_core::store::kv::ArtifactRecord {
        schema_version: depot_core::store::kv::CURRENT_RECORD_VERSION,
        id: String::new(),
        size,
        content_type: "application/octet-stream".to_string(),
        kind: depot_core::store::kv::ArtifactKind::DockerBlob {
            docker_digest: docker_digest.clone(),
        },
        blob_id: Some(blob_id.clone()),
        etag: Some(blake3_hash.clone()),
        content_hash: Some(blake3_hash),
        created_at: now,
        updated_at: now,
        last_accessed_at: now,
        path: String::new(),
        internal: false,
    };
    let blob_ref_path = store.blob_ref_path(&docker_digest);
    let commit_result: Result<Option<String>, depot_core::error::DepotError> = async {
        let existing = service::put_dedup_record_counted(
            state.kv.as_ref(),
            &store_name,
            &blob_rec,
            &state.updater,
        )
        .await?;
        if let Some(ref existing_id) = existing {
            record.blob_id = Some(existing_id.to_string());
        }
        service::put_artifact(state.kv.as_ref(), &target_repo, &blob_ref_path, &record).await?;
        service::delete_upload_session(state.kv.as_ref(), uuid).await?;
        Ok(existing)
    }
    .await;
    match commit_result {
        Ok(dedup) => {
            if dedup.is_some() {
                let _ = blobs.delete(&blob_id).await;
            }
            // Clean up chunk blobs (best-effort, after successful commit).
            for chunk_id in &chunk_blob_ids {
                let _ = blobs.delete(chunk_id).await;
            }
            // Per-repo dir stats only; store credit handled by
            // put_dedup_record_counted above.
            state
                .updater
                .dir_changed(&target_repo, &blob_ref_path, 1, size as i64)
                .await;
            let mut headers = HeaderMap::new();
            headers.insert(
                "Location",
                format!("{}/blobs/{}", loc_prefix, docker_digest)
                    .parse()
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
            headers.insert(
                "Docker-Content-Digest",
                docker_digest
                    .parse()
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
            (StatusCode::CREATED, headers).into_response()
        }
        Err(e) => docker_error("UNKNOWN", &e.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
    }
}
