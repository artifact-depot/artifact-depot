// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use metrics::{gauge, histogram};

static UPLOADS_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderName, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;
use utoipa::{IntoParams, ToSchema};

use sha2::Digest as _;

use crate::server::repo::hosted::HostedRepo;
use crate::server::repo::{self as repo_mod, ArtifactContent, ArtifactData};
use crate::server::AppState;
use depot_core::auth::{map_permission_error, AuthenticatedUser};
use depot_core::error::DepotError;
use depot_core::service;
use depot_core::store::kv::ArtifactKind;
use depot_core::store::kv::ArtifactRecord;
use depot_core::store::kv::{
    ArtifactFormat, Capability, ContentDisposition, DirEntry, FormatConfig, Pagination, RepoConfig,
    RepoKind, RepoType, TreeEntry, TreeEntryKind,
};

/// Stream a request body to a blob writer, computing BLAKE3 (and optionally
/// SHA-256) on the fly.  Returns `(blake3_hex, Option<sha256_hex>, byte_count)`.
#[tracing::instrument(level = "debug", name = "blob.stream_write", skip_all, fields(%blob_id))]
async fn stream_body_to_blob(
    body: Body,
    blobs: &dyn depot_core::store::blob::BlobStore,
    blob_id: &str,
    content_length: Option<u64>,
    compute_sha256: bool,
) -> Result<(String, Option<String>, u64), DepotError> {
    let mut blob_writer = blobs.create_writer(blob_id, content_length).await?;
    let mut hasher = blake3::Hasher::new();
    let mut sha256_hasher = if compute_sha256 {
        Some(sha2::Sha256::new())
    } else {
        None
    };
    let mut size = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(result) = stream.next().await {
        let chunk: axum::body::Bytes = match result {
            Ok(c) => c,
            Err(e) => {
                let _ = blob_writer.abort().await;
                return Err(DepotError::DataIntegrity(e.to_string()));
            }
        };
        if chunk.is_empty() {
            continue;
        }
        hasher.update(&chunk);
        if let Some(ref mut h) = sha256_hasher {
            h.update(&chunk);
        }
        if let Err(e) = blob_writer.write_chunk(chunk.as_ref()).await {
            let _ = blob_writer.abort().await;
            return Err(e);
        }
        size += chunk.len() as u64;
    }
    blob_writer.finish().await.map_err(|e| {
        DepotError::Storage(Box::new(e), depot_core::error::Retryability::Transient)
    })?;
    let blake3_hash = hasher.finalize().to_hex().to_string();
    let sha256_hash = sha256_hasher.map(|h| {
        h.finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    });
    Ok((blake3_hash, sha256_hash, size))
}

/// Handle POST/PATCH on `/repository/{repo}/{path}` — dispatches Docker V2
/// paths to Docker handlers and format-specific POSTs (pypi, apt, yum, golang,
/// helm uploads and snapshots) to their write dispatchers; returns 405 for
/// everything else.
pub async fn any_format_request(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    uri: axum::http::Uri,
    method: axum::http::Method,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    let (repo_name, path) = split_rest(&rest);
    let path = match normalize_artifact_path(&path) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };

    let repo_config = match service::get_repo(state.repo.kv.as_ref(), &repo_name).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", repo_name))
                .into_response();
        }
        Err(e) => return e.into_response(),
    };

    // Docker V2 paths: dispatch before the permission check — Docker handles
    // its own auth and returns a proper 401 Bearer challenge when needed.
    if repo_config.format() == ArtifactFormat::Docker && (path == "v2" || path.starts_with("v2/")) {
        return depot_format_docker::api::dispatch::try_handle_docker_v2_path(
            &state.format_state(),
            &user.0,
            &repo_name,
            &path,
            &method,
            uri.query(),
            uri.authority().map(|a| a.as_str()),
            &headers,
            body,
        )
        .await
        .unwrap_or_else(|| {
            DepotError::NotFound("unrecognized Docker V2 path".to_string()).into_response()
        });
    }

    // Format-specific POST writes (pypi upload, apt/yum upload + snapshots,
    // golang upload, helm upload). Each format's dispatcher runs its own
    // permission check via `upload_preamble`.
    if is_format_write_path(repo_config.format(), &method, &path) {
        return dispatch_format_write(&state, &user, &repo_config, &method, &path, headers, body)
            .await;
    }

    axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response()
}

/// Returns `true` if this `(format, method, path)` triple is a format-specific
/// write that should be routed through the format's `try_handle_repository_write`
/// dispatcher. The caller uses this pre-check to decide whether to move the
/// request body into the format dispatcher (otherwise the body would be dropped
/// and the generic handler would have nothing to read).
pub(crate) fn is_format_write_path(
    format: ArtifactFormat,
    method: &axum::http::Method,
    path: &str,
) -> bool {
    use axum::http::Method;
    match format {
        ArtifactFormat::Golang => method == Method::POST && path == "upload",
        ArtifactFormat::Helm => method == Method::POST && path == "upload",
        ArtifactFormat::PyPI => method == Method::POST && (path.is_empty() || path == "/"),
        ArtifactFormat::Apt => method == Method::POST && matches!(path, "upload" | "snapshots"),
        ArtifactFormat::Yum => method == Method::POST && matches!(path, "upload" | "snapshots"),
        ArtifactFormat::Npm => {
            let get_whoami = method == Method::GET && path == "-/whoami";
            let login = method == Method::PUT && path.starts_with("-/user/");
            // Publish goes to an unscoped "package-name" (no slash) or a scoped
            // "@scope/package" (exactly one slash, starts with '@').
            let publish = method == Method::PUT
                && ((!path.is_empty() && !path.contains('/'))
                    || (path.starts_with('@') && path.matches('/').count() == 1));
            get_whoami || login || publish
        }
        ArtifactFormat::Cargo => {
            let publish = method == Method::PUT && path == "api/v1/crates/new";
            let yank_unyank = path.starts_with("api/v1/crates/")
                && ((method == Method::DELETE && path.ends_with("/yank"))
                    || (method == Method::PUT && path.ends_with("/unyank")));
            publish || yank_unyank
        }
        _ => false,
    }
}

/// Dispatch a format-specific write request to the matching format crate. The
/// caller is responsible for gating this with [`is_format_write_path`] — if the
/// predicate says yes, this function takes ownership of the body and always
/// returns a response (404 if the format crate unexpectedly returns `None`).
async fn dispatch_format_write(
    state: &AppState,
    user: &AuthenticatedUser,
    config: &RepoConfig,
    method: &axum::http::Method,
    path: &str,
    headers: HeaderMap,
    body: Body,
) -> axum::response::Response {
    let fs = state.format_state();
    let result = match config.format() {
        ArtifactFormat::Apt => {
            depot_format_apt::api::try_handle_repository_write(
                &fs, user, config, method, path, &headers, body,
            )
            .await
        }
        ArtifactFormat::Cargo => {
            depot_format_cargo::api::try_handle_repository_write(
                &fs, user, config, method, path, &headers, body,
            )
            .await
        }
        ArtifactFormat::Golang => {
            depot_format_golang::api::try_handle_repository_write(
                &fs, user, config, method, path, &headers, body,
            )
            .await
        }
        ArtifactFormat::Helm => {
            depot_format_helm::api::try_handle_repository_write(
                &fs, user, config, method, path, &headers, body,
            )
            .await
        }
        ArtifactFormat::Npm => {
            depot_format_npm::api::try_handle_repository_write(
                &fs, user, config, method, path, &headers, body,
            )
            .await
        }
        ArtifactFormat::PyPI => {
            depot_format_pypi::api::try_handle_repository_write(
                &fs, user, config, method, path, &headers, body,
            )
            .await
        }
        ArtifactFormat::Yum => {
            depot_format_yum::api::try_handle_repository_write(
                &fs, user, config, method, path, &headers, body,
            )
            .await
        }
        _ => None,
    };
    result.unwrap_or_else(|| {
        DepotError::NotFound(format!(
            "no handler for {} {} on {:?} repository",
            method,
            path,
            config.format()
        ))
        .into_response()
    })
}

/// Split the `{*rest}` wildcard capture from `/repository/{*rest}` into
/// `(repo_name, artifact_path)`. Handles all three shapes produced by clients:
///
/// - `/repository/foo`        → rest = `"foo"`        → `("foo", "")`
/// - `/repository/foo/`       → rest = `"foo/"`       → `("foo", "")`
/// - `/repository/foo/bar/..` → rest = `"foo/bar/..."`→ `("foo", "bar/...")`
fn split_rest(rest: &str) -> (String, String) {
    match rest.split_once('/') {
        Some((repo, path)) => (repo.to_string(), path.to_string()),
        None => (rest.to_string(), String::new()),
    }
}

/// Normalize an artifact path: collapse `//`, remove `.` segments, reject `..`.
fn normalize_artifact_path(path: &str) -> Result<String, DepotError> {
    let mut segments = Vec::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(DepotError::BadRequest(
                "path traversal not allowed".to_string(),
            ));
        }
        segments.push(seg);
    }
    Ok(segments.join("/"))
}

/// Add ETag, Last-Modified, and X-Checksum-* headers from an ArtifactRecord.
fn add_cache_headers(headers: &mut HeaderMap, record: &ArtifactRecord) {
    if let Some(etag) = record.effective_etag() {
        if let Ok(v) = format!("\"{}\"", etag).parse() {
            headers.insert(header::ETAG, v);
        }
    }
    if let Some(hash) = record.content_hash.as_deref().filter(|h| !h.is_empty()) {
        if let Ok(v) = hash.parse() {
            headers.insert(HeaderName::from_static("x-checksum-blake3"), v);
        }
    }
    // Docker blobs/manifests carry a SHA-256 digest.
    match &record.kind {
        ArtifactKind::DockerBlob { docker_digest }
        | ArtifactKind::DockerManifest { docker_digest } => {
            let sha = docker_digest
                .strip_prefix("sha256:")
                .unwrap_or(docker_digest);
            if let Ok(v) = sha.parse() {
                headers.insert(HeaderName::from_static("x-checksum-sha256"), v);
            }
        }
        _ => {}
    }
    let last_mod = record
        .updated_at
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    if let Ok(v) = last_mod.parse() {
        headers.insert(header::LAST_MODIFIED, v);
    }
}

/// Check If-None-Match / If-Modified-Since and return 304 if appropriate.
fn check_conditional(
    request_headers: &HeaderMap,
    record: &ArtifactRecord,
) -> Option<axum::response::Response> {
    // If-None-Match
    if let Some(inm) = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(etag_val) = record.effective_etag() {
            let etag = format!("\"{}\"", etag_val);
            if inm == etag || inm == "*" {
                let mut h = HeaderMap::new();
                add_cache_headers(&mut h, record);
                return Some((StatusCode::NOT_MODIFIED, h).into_response());
            }
        }
    }
    // If-Modified-Since
    if let Some(ims) = request_headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok())
    {
        if let Ok(since) = chrono::NaiveDateTime::parse_from_str(ims, "%a, %d %b %Y %H:%M:%S GMT") {
            let since_utc = since.and_utc();
            // Compare at second granularity (HTTP dates don't have sub-second precision)
            if record.updated_at.timestamp() <= since_utc.timestamp() {
                let mut h = HeaderMap::new();
                add_cache_headers(&mut h, record);
                return Some((StatusCode::NOT_MODIFIED, h).into_response());
            }
        }
    }
    None
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactResponse {
    pub id: String,
    pub path: String,
    pub size: u64,
    pub content_type: String,
    pub blob_id: String,
    pub content_hash: Option<String>,
    pub etag: Option<String>,
    #[schema(value_type = String, format = "date-time")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[schema(value_type = String, format = "date-time")]
    pub updated_at: chrono::DateTime<chrono::Utc>,
    #[schema(value_type = String, format = "date-time")]
    pub last_accessed_at: chrono::DateTime<chrono::Utc>,
}

impl ArtifactResponse {
    fn from_record(path: String, r: ArtifactRecord) -> Self {
        let etag = r.effective_etag().map(|s| s.to_string());
        Self {
            id: r.id,
            path,
            size: r.size,
            content_type: r.content_type,
            blob_id: r.blob_id.unwrap_or_default(),
            content_hash: r.content_hash,
            etag,
            created_at: r.created_at,
            updated_at: r.updated_at,
            last_accessed_at: r.last_accessed_at,
        }
    }

    fn from_tree_entry(name: String, _prefix: &str, te: TreeEntry) -> Self {
        let path = name.clone();
        match te.kind {
            TreeEntryKind::File {
                size,
                content_type,
                blob_id,
                content_hash,
                created_at,
                ..
            } => Self {
                id: String::new(),
                path,
                size,
                content_type,
                blob_id: blob_id.unwrap_or_default(),
                etag: content_hash.clone(),
                content_hash,
                created_at,
                updated_at: te.last_modified_at,
                last_accessed_at: te.last_accessed_at,
            },
            TreeEntryKind::Dir { .. } => Self {
                id: String::new(),
                path,
                size: 0,
                content_type: String::new(),
                blob_id: String::new(),
                content_hash: None,
                etag: None,
                created_at: te.last_modified_at,
                updated_at: te.last_modified_at,
                last_accessed_at: te.last_accessed_at,
            },
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DirResponse {
    pub name: String,
    pub artifact_count: u64,
    pub total_bytes: u64,
    #[schema(value_type = String, format = "date-time")]
    pub last_modified_at: chrono::DateTime<chrono::Utc>,
    #[schema(value_type = String, format = "date-time")]
    pub last_accessed_at: chrono::DateTime<chrono::Utc>,
}

impl DirResponse {
    fn from_entry(name: String, d: DirEntry) -> Self {
        Self {
            name,
            artifact_count: d.artifact_count,
            total_bytes: d.total_bytes,
            last_modified_at: d.last_modified_at,
            last_accessed_at: d.last_accessed_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BrowseResponse {
    pub dirs: Vec<DirResponse>,
    pub artifacts: Vec<ArtifactResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct SearchParams {
    /// Search query string (matches against path)
    pub q: Option<String>,
    /// Path prefix to list
    pub prefix: Option<String>,
    /// Maximum number of results to return (default 100, max 10000)
    pub limit: Option<usize>,
    /// Number of results to skip (default 0)
    pub offset: Option<usize>,
    /// Opaque continuation token for cursor-based pagination
    pub cursor: Option<String>,
}

/// Build a streaming HTTP response from ArtifactContent.
fn artifact_response(content: ArtifactContent) -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        content
            .record
            .content_type
            .parse()
            .unwrap_or(header::HeaderValue::from_static("application/octet-stream")),
    );
    let blob_id_str = content.record.blob_id.as_deref().unwrap_or("").to_string();
    if !blob_id_str.is_empty() {
        if let Ok(v) = blob_id_str.parse() {
            headers.insert("X-Blob-Id", v);
        }
    }
    add_cache_headers(&mut headers, &content.record);
    match content.data {
        ArtifactData::Reader(reader) => {
            // Known size — set Content-Length for direct downloads.
            if content.record.size > 0 {
                headers.insert(
                    header::CONTENT_LENGTH,
                    content
                        .record
                        .size
                        .to_string()
                        .parse()
                        .unwrap_or_else(|_| header::HeaderValue::from_static("0")),
                );
            }
            let stream = ReaderStream::with_capacity(reader, 256 * 1024);
            (headers, Body::from_stream(stream)).into_response()
        }
        ArtifactData::Stream(byte_stream) => {
            // Set Content-Length from upstream when available. If the stream
            // errors mid-transfer, the connection resets and the client sees
            // fewer bytes than Content-Length — a detectable failure.
            if content.record.size > 0 {
                headers.insert(
                    header::CONTENT_LENGTH,
                    content
                        .record
                        .size
                        .to_string()
                        .parse()
                        .unwrap_or_else(|_| header::HeaderValue::from_static("0")),
                );
            }
            (headers, Body::from_stream(byte_stream)).into_response()
        }
    }
}

/// Set `Content-Disposition` on raw-format responses based on repo config.
fn set_raw_content_disposition(headers: &mut HeaderMap, config: &FormatConfig, path: &str) {
    if let FormatConfig::Raw {
        content_disposition,
    } = config
    {
        let disposition = content_disposition.unwrap_or(ContentDisposition::Attachment);
        let filename = path.rsplit('/').next().unwrap_or(path);
        let value = match disposition {
            ContentDisposition::Attachment => format!("attachment; filename=\"{filename}\""),
            ContentDisposition::Inline => format!("inline; filename=\"{filename}\""),
        };
        if let Ok(v) = value.parse() {
            headers.insert(header::CONTENT_DISPOSITION, v);
        }
    }
}

/// Fetch an artifact from any repo type (hosted, cache, or proxy).
#[tracing::instrument(level = "debug", name = "fetch_artifact", skip(state, config), fields(repo = %config.name, path))]
async fn fetch_artifact(
    state: &AppState,
    config: &RepoConfig,
    path: &str,
) -> depot_core::error::Result<Option<ArtifactContent>> {
    let blobs = state.repo.blob_store(&config.store).await?;
    let repo = repo_mod::make_raw_repo(state.repo.repo_context(blobs), config);
    repo.get(path).await
}

/// Fetch artifact metadata from any repo type without touching the blob store.
async fn head_fetch_artifact(
    state: &AppState,
    config: &RepoConfig,
    path: &str,
) -> depot_core::error::Result<Option<ArtifactRecord>> {
    let blobs = state.repo.blob_store(&config.store).await?;
    let repo = repo_mod::make_raw_repo(state.repo.repo_context(blobs), config);
    repo.head(path).await
}

/// Check if an artifact exists (metadata only, no body).
#[utoipa::path(
    head,
    path = "/repository/{repo}/{path}",
    params(
        ("repo" = String, Path, description = "Repository name"),
        ("path" = String, Path, description = "Artifact path"),
    ),
    responses(
        (status = 200, description = "Artifact exists"),
        (status = 404, description = "Artifact not found")
    ),
    tag = "artifacts"
)]
pub async fn head_artifact(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    uri: axum::http::Uri,
    Path(rest): Path<String>,
    request_headers: HeaderMap,
) -> impl IntoResponse {
    let (repo_name, path) = split_rest(&rest);
    let path = match normalize_artifact_path(&path) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    if let Err(perm_err) = state
        .auth
        .backend
        .check_permission(&user.0, &repo_name, Capability::Read)
        .await
    {
        // For Docker V2 paths, bypass the generic permission error and let
        // the Docker dispatch return a proper 401 Bearer challenge instead.
        if path == "v2" || path.starts_with("v2/") {
            let repo_name_owned = repo_name.clone();
            if let Ok(Some(rc)) = service::get_repo(state.repo.kv.as_ref(), &repo_name_owned).await
            {
                if rc.format() == ArtifactFormat::Docker {
                    if let Some(resp) =
                        depot_format_docker::api::dispatch::try_handle_docker_v2_path(
                            &state.format_state(),
                            &user.0,
                            &repo_name,
                            &path,
                            &axum::http::Method::HEAD,
                            uri.query(),
                            uri.authority().map(|a| a.as_str()),
                            &request_headers,
                            Body::empty(),
                        )
                        .await
                    {
                        return resp;
                    }
                }
            }
        }
        return map_permission_error(perm_err, &user.0);
    }
    let repo_name_owned = repo_name.clone();
    let repo_config = match service::get_repo(state.repo.kv.as_ref(), &repo_name_owned).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", repo_name))
                .into_response()
        }
        Err(e) => return e.into_response(),
    };

    // Dispatch Docker V2 paths before the permission check — the Docker
    // dispatch handles auth internally with proper 401 Bearer challenges.
    if repo_config.format() == ArtifactFormat::Docker {
        if let Some(resp) = depot_format_docker::api::dispatch::try_handle_docker_v2_path(
            &state.format_state(),
            &user.0,
            &repo_name,
            &path,
            &axum::http::Method::HEAD,
            uri.query(),
            uri.authority().map(|a| a.as_str()),
            &request_headers,
            Body::empty(),
        )
        .await
        {
            return resp;
        }
    }

    if let Err(e) = state
        .auth
        .backend
        .check_permission(&user.0, &repo_name, Capability::Read)
        .await
    {
        return map_permission_error(e, &user.0);
    }

    // Dispatch format-specific paths before falling through to raw handler.
    {
        let fs = state.format_state();
        let format_resp = match repo_config.format() {
            ArtifactFormat::Apt => {
                depot_format_apt::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::Helm => {
                depot_format_helm::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::PyPI => {
                depot_format_pypi::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::Yum => {
                depot_format_yum::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::Cargo => {
                depot_format_cargo::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::Golang => {
                depot_format_golang::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::Npm => {
                let origin = depot_core::api_helpers::external_origin(
                    &request_headers,
                    uri.authority().map(|a| a.as_str()),
                );
                depot_format_npm::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                    &origin,
                )
                .await
            }
            _ => None,
        };
        if let Some(resp) = format_resp {
            return resp;
        }
    }

    match head_fetch_artifact(&state, &repo_config, &path).await {
        Ok(Some(record)) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                record
                    .content_type
                    .parse()
                    .unwrap_or(header::HeaderValue::from_static("application/octet-stream")),
            );
            if record.size > 0 {
                headers.insert(
                    header::CONTENT_LENGTH,
                    record
                        .size
                        .to_string()
                        .parse()
                        .unwrap_or_else(|_| header::HeaderValue::from_static("0")),
                );
            }
            let blob_id_str = record.blob_id.as_deref().unwrap_or("").to_string();
            if !blob_id_str.is_empty() {
                if let Ok(v) = blob_id_str.parse() {
                    headers.insert("X-Blob-Id", v);
                }
            }
            add_cache_headers(&mut headers, &record);
            set_raw_content_disposition(&mut headers, &repo_config.format_config, &path);
            (StatusCode::OK, headers).into_response()
        }
        Ok(None) => {
            DepotError::NotFound(format!("artifact not found at path {}", path)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// Download an artifact from a repository.
#[utoipa::path(
    get,
    path = "/repository/{repo}/{path}",
    params(
        ("repo" = String, Path, description = "Repository name"),
        ("path" = String, Path, description = "Artifact path"),
    ),
    responses(
        (status = 200, description = "Artifact content"),
        (status = 404, description = "Artifact not found")
    ),
    tag = "artifacts"
)]
pub async fn get_artifact(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    uri: axum::http::Uri,
    Path(rest): Path<String>,
    request_headers: HeaderMap,
) -> impl IntoResponse {
    let (repo_name, path) = split_rest(&rest);
    let path = match normalize_artifact_path(&path) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    if let Err(perm_err) = state
        .auth
        .backend
        .check_permission(&user.0, &repo_name, Capability::Read)
        .await
    {
        // For Docker V2 paths, bypass the generic permission error and let
        // the Docker dispatch return a proper 401 Bearer challenge instead.
        if path == "v2" || path.starts_with("v2/") {
            let repo_name_owned = repo_name.clone();
            if let Ok(Some(rc)) = service::get_repo(state.repo.kv.as_ref(), &repo_name_owned).await
            {
                if rc.format() == ArtifactFormat::Docker {
                    if let Some(resp) =
                        depot_format_docker::api::dispatch::try_handle_docker_v2_path(
                            &state.format_state(),
                            &user.0,
                            &repo_name,
                            &path,
                            &axum::http::Method::GET,
                            uri.query(),
                            uri.authority().map(|a| a.as_str()),
                            &request_headers,
                            Body::empty(),
                        )
                        .await
                    {
                        return resp;
                    }
                }
            }
        }
        return map_permission_error(perm_err, &user.0);
    }
    let repo_name_owned = repo_name.clone();
    let repo_config = match service::get_repo(state.repo.kv.as_ref(), &repo_name_owned).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", repo_name))
                .into_response()
        }
        Err(e) => return e.into_response(),
    };

    // Dispatch Docker V2 paths before the permission check — the Docker
    // dispatch handles auth internally with proper 401 Bearer challenges.
    if repo_config.format() == ArtifactFormat::Docker {
        if let Some(resp) = depot_format_docker::api::dispatch::try_handle_docker_v2_path(
            &state.format_state(),
            &user.0,
            &repo_name,
            &path,
            &axum::http::Method::GET,
            uri.query(),
            uri.authority().map(|a| a.as_str()),
            &request_headers,
            Body::empty(),
        )
        .await
        {
            return resp;
        }
    }

    if let Err(e) = state
        .auth
        .backend
        .check_permission(&user.0, &repo_name, Capability::Read)
        .await
    {
        return map_permission_error(e, &user.0);
    }

    // Dispatch format-specific paths before falling through to raw handler.
    {
        let fs = state.format_state();
        let format_resp = match repo_config.format() {
            ArtifactFormat::Apt => {
                depot_format_apt::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::Helm => {
                depot_format_helm::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::PyPI => {
                depot_format_pypi::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::Yum => {
                depot_format_yum::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::Cargo => {
                depot_format_cargo::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::Golang => {
                depot_format_golang::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                )
                .await
            }
            ArtifactFormat::Npm => {
                let origin = depot_core::api_helpers::external_origin(
                    &request_headers,
                    uri.authority().map(|a| a.as_str()),
                );
                depot_format_npm::api::try_handle_repository_path(
                    &fs,
                    &repo_config,
                    &path,
                    uri.query(),
                    &origin,
                )
                .await
            }
            _ => None,
        };
        if let Some(resp) = format_resp {
            return resp;
        }
    }

    // Some format GETs need the authenticated user (e.g. npm's `-/whoami`) and
    // live on the write dispatcher instead of the read dispatcher.
    if is_format_write_path(repo_config.format(), &axum::http::Method::GET, &path) {
        return dispatch_format_write(
            &state,
            &user,
            &repo_config,
            &axum::http::Method::GET,
            &path,
            request_headers,
            Body::empty(),
        )
        .await;
    }

    match fetch_artifact(&state, &repo_config, &path).await {
        Ok(Some(content)) => {
            if let Some(not_modified) = check_conditional(&request_headers, &content.record) {
                return not_modified;
            }
            let mut resp = artifact_response(content).into_response();
            set_raw_content_disposition(resp.headers_mut(), &repo_config.format_config, &path);
            resp
        }
        Ok(None) => {
            DepotError::NotFound(format!("artifact not found at path {}", path)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// Upload an artifact to a hosted repository.
#[utoipa::path(
    put,
    path = "/repository/{repo}/{path}",
    params(
        ("repo" = String, Path, description = "Repository name"),
        ("path" = String, Path, description = "Artifact path"),
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 201, description = "Artifact stored", body = ArtifactResponse),
        (status = 404, description = "Repository not found"),
        (status = 405, description = "Repository does not accept uploads")
    ),
    tag = "artifacts"
)]
pub async fn put_artifact(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    uri: axum::http::Uri,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    let in_flight = UPLOADS_IN_FLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
    gauge!("uploads_in_flight").set(in_flight as f64);

    let (repo_name, path) = split_rest(&rest);
    let path = match normalize_artifact_path(&path) {
        Ok(p) => p,
        Err(e) => {
            UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            return e.into_response();
        }
    };
    // Look up the repo early so format dispatchers (Docker V2 token challenge,
    // npm login, etc.) can run before the outer permission check.
    let repo_name_owned = repo_name.clone();
    let repo_config = match service::get_repo(state.repo.kv.as_ref(), &repo_name_owned).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            return DepotError::NotFound(format!("repository '{}' not found", repo_name))
                .into_response();
        }
        Err(e) => {
            UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            return e.into_response();
        }
    };

    // Dispatch Docker V2 paths before the generic permission check — the Docker
    // dispatch handles auth internally with proper 401 Bearer challenges.
    if repo_config.format() == ArtifactFormat::Docker && (path == "v2" || path.starts_with("v2/")) {
        UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
        return depot_format_docker::api::dispatch::try_handle_docker_v2_path(
            &state.format_state(),
            &user.0,
            &repo_name,
            &path,
            &axum::http::Method::PUT,
            uri.query(),
            uri.authority().map(|a| a.as_str()),
            &headers,
            body,
        )
        .await
        .unwrap_or_else(|| {
            DepotError::NotFound("unrecognized Docker V2 path".to_string()).into_response()
        });
    }

    // Dispatch format-specific writes (npm publish, npm login, cargo publish, cargo
    // unyank, etc.) before the outer permission check. Each format's dispatcher
    // handles its own auth — most call `upload_preamble` which requires Write; npm
    // login authenticates from the request body so must bypass the generic check.
    if is_format_write_path(repo_config.format(), &axum::http::Method::PUT, &path) {
        UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
        return dispatch_format_write(
            &state,
            &user,
            &repo_config,
            &axum::http::Method::PUT,
            &path,
            headers,
            body,
        )
        .await;
    }

    if let Err(perm_err) = state
        .auth
        .backend
        .check_permission(&user.0, &repo_name, Capability::Write)
        .await
    {
        UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
        return map_permission_error(perm_err, &user.0);
    }

    // For proxy (group) repos, route writes to the first hosted member.
    let target_repo =
        match repo_mod::resolve_write_target(state.repo.kv.as_ref(), &repo_config).await {
            Ok(name) => name,
            Err(e) => return e.into_response(),
        };

    // Resolve the target repo config and blob store.
    let target_config = if target_repo == repo_name {
        repo_config.clone()
    } else {
        match service::get_repo(state.repo.kv.as_ref(), &target_repo).await {
            Ok(Some(tc)) => tc,
            Ok(None) => {
                return DepotError::NotFound(format!(
                    "target repository '{}' not found",
                    target_repo
                ))
                .into_response()
            }
            Err(e) => return e.into_response(),
        }
    };
    let store_name = target_config.store.clone();
    let blobs = match state.repo.blob_store(&store_name).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };

    // Format-specific repos reject generic PUT uploads for paths that aren't
    // already handled by the format write dispatcher above. Raw repos accept
    // any PUT; Yum .rpm and Helm .tgz paths fall through to format-specific
    // commit logic below; everything else returns 405.
    {
        use depot_core::store::kv::ArtifactFormat;
        let reject_msg: Option<&'static str> = match repo_config.format() {
            ArtifactFormat::Yum => (!path.ends_with(".rpm")).then_some(
                "Yum repositories only accept PUT for .rpm package paths; \
                 use `dnf`/`createrepo` or POST /repository/{repo}/upload for manual uploads.",
            ),
            ArtifactFormat::Helm => (!path.ends_with(".tgz")).then_some(
                "Helm repositories only accept PUT for .tgz chart paths; \
                 use `helm push` or POST /repository/{repo}/upload for manual uploads.",
            ),
            ArtifactFormat::Apt => Some(
                "APT repositories do not accept PUT uploads; \
                 use POST /repository/{repo}/upload (or the Nexus-compatible /service/rest/v1/components).",
            ),
            ArtifactFormat::PyPI => Some(
                "PyPI repositories do not accept PUT uploads; \
                 use `twine upload` which POSTs to /repository/{repo}/.",
            ),
            ArtifactFormat::Npm => Some(
                "npm repositories only accept PUT via `npm publish` (PUT /repository/{repo}/{package}).",
            ),
            ArtifactFormat::Cargo => Some(
                "Cargo repositories only accept PUT via `cargo publish` (PUT /repository/{repo}/api/v1/crates/new).",
            ),
            ArtifactFormat::Golang => Some(
                "Go module repositories do not accept PUT uploads; \
                 use POST /repository/{repo}/upload.",
            ),
            ArtifactFormat::Docker => Some(
                "Docker repositories only accept writes through the OCI Distribution API (docker push).",
            ),
            // Raw repos accept PUTs — that's their interface.
            _ => None,
        };
        if let Some(msg) = reject_msg {
            UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            return (StatusCode::METHOD_NOT_ALLOWED, msg).into_response();
        }
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|ct| *ct != "application/octet-stream")
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            // Infer MIME type from path extension when the client doesn't
            // provide one (or sends the generic octet-stream default).
            mime_guess::from_path(&path)
                .first_raw()
                .unwrap_or("application/octet-stream")
                .to_string()
        });

    // Stream body through BlobWriter, computing BLAKE3 incrementally.
    // For Helm .tgz and Yum .rpm uploads we also need SHA-256 for metadata.
    let is_helm_chart = repo_config.format() == ArtifactFormat::Helm && path.ends_with(".tgz");
    let is_yum_rpm = repo_config.format() == ArtifactFormat::Yum && path.ends_with(".rpm");
    let blob_id = uuid::Uuid::new_v4().to_string();
    let content_length: Option<u64> = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());

    let blob_start = std::time::Instant::now();

    let (blake3_hash, sha256_hash, size) = match stream_body_to_blob(
        body,
        blobs.as_ref(),
        &blob_id,
        content_length,
        is_helm_chart || is_yum_rpm,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = blobs.delete(&blob_id).await;
            return e.into_response();
        }
    };

    let blob_elapsed = blob_start.elapsed();
    let stream_elapsed = blob_elapsed; // stream_body_to_blob includes finish()

    // Helm .tgz and Yum .rpm go through format-specific commit paths.
    if let Some(sha256_hash) = sha256_hash
        .as_deref()
        .filter(|_| is_helm_chart || is_yum_rpm)
    {
        let sha256_hash = sha256_hash.to_string();

        if is_helm_chart {
            let store = depot_format_helm::store::HelmStore {
                repo: &target_config.name,
                kv: state.repo.kv.as_ref(),
                blobs: blobs.as_ref(),
                store: &store_name,
                updater: &state.repo.updater,
            };
            let kv_start = std::time::Instant::now();
            match store
                .commit_chart(&blob_id, &blake3_hash, &sha256_hash, size)
                .await
            {
                Ok(_record) => {
                    let kv_elapsed = kv_start.elapsed();
                    let remaining = UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed) - 1;
                    gauge!("uploads_in_flight").set(remaining as f64);
                    let fsync_ms = blob_elapsed.saturating_sub(stream_elapsed).as_millis() as u64;
                    tracing::info!(
                        size_mb = size / (1024 * 1024),
                        stream_ms = stream_elapsed.as_millis() as u64,
                        fsync_ms,
                        kv_ms = kv_elapsed.as_millis() as u64,
                        in_flight,
                        repo = %target_repo,
                        path = %path,
                        "helm chart upload complete",
                    );
                    histogram!("upload_blob_write_seconds").record(blob_elapsed.as_secs_f64());
                    histogram!("upload_kv_commit_seconds").record(kv_elapsed.as_secs_f64());

                    let fs = state.format_state();
                    super::propagate_staleness_to_parents(
                        &fs,
                        &repo_name,
                        ArtifactFormat::Helm,
                        "_helm/metadata_stale",
                    )
                    .await;
                    return StatusCode::CREATED.into_response();
                }
                Err(e) => {
                    UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
                    return e.into_response();
                }
            }
        } else if is_yum_rpm {
            // Derive directory and filename from the URL path.
            let (directory, filename) = match path.rsplit_once('/') {
                Some((dir, name)) => (dir.to_string(), name.to_string()),
                None => (String::new(), path.clone()),
            };
            let store = depot_format_yum::store::YumStore {
                repo: &target_config.name,
                kv: state.repo.kv.as_ref(),
                blobs: blobs.as_ref(),
                store: &store_name,
                updater: &state.repo.updater,
                repodata_depth: target_config.format_config.repodata_depth(),
            };
            let kv_start = std::time::Instant::now();
            match store
                .commit_package(
                    &blob_id,
                    &blake3_hash,
                    &sha256_hash,
                    size,
                    &filename,
                    &directory,
                )
                .await
            {
                Ok(_record) => {
                    let kv_elapsed = kv_start.elapsed();
                    let remaining = UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed) - 1;
                    gauge!("uploads_in_flight").set(remaining as f64);
                    let fsync_ms = blob_elapsed.saturating_sub(stream_elapsed).as_millis() as u64;
                    tracing::info!(
                        size_mb = size / (1024 * 1024),
                        stream_ms = stream_elapsed.as_millis() as u64,
                        fsync_ms,
                        kv_ms = kv_elapsed.as_millis() as u64,
                        in_flight,
                        repo = %target_repo,
                        path = %path,
                        "yum rpm upload complete",
                    );
                    histogram!("upload_blob_write_seconds").record(blob_elapsed.as_secs_f64());
                    histogram!("upload_kv_commit_seconds").record(kv_elapsed.as_secs_f64());

                    let fs = state.format_state();
                    super::propagate_staleness_to_parents(
                        &fs,
                        &repo_name,
                        ArtifactFormat::Yum,
                        "_yum/metadata_stale",
                    )
                    .await;
                    return StatusCode::CREATED.into_response();
                }
                Err(e) => {
                    UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
                    return e.into_response();
                }
            }
        }
    }

    let format = repo_config.format().to_string();
    let ctx = repo_mod::IngestionCtx {
        kv: state.repo.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: &store_name,
        repo: &target_repo,
        format: &format,
        repo_type: "hosted",
    };

    let kv_start = std::time::Instant::now();
    match repo_mod::commit_ingestion(&ctx, &path, &blob_id, &blake3_hash, size, &content_type).await
    {
        Ok(result) => {
            let kv_elapsed = kv_start.elapsed();
            let remaining = UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed) - 1;
            gauge!("uploads_in_flight").set(remaining as f64);
            let fsync_ms = blob_elapsed.saturating_sub(stream_elapsed).as_millis() as u64;
            tracing::info!(
                size_mb = size / (1024 * 1024),
                stream_ms = stream_elapsed.as_millis() as u64,
                fsync_ms,
                kv_ms = kv_elapsed.as_millis() as u64,
                in_flight,
                repo = %target_repo,
                path = %path,
                "upload complete",
            );
            histogram!("upload_blob_write_seconds").record(blob_elapsed.as_secs_f64());
            histogram!("upload_kv_commit_seconds").record(kv_elapsed.as_secs_f64());

            let (count_delta, bytes_delta) = match &result.old_record {
                Some(old) => (0i64, result.record.size as i64 - old.size as i64),
                None => (1i64, result.record.size as i64),
            };
            state
                .repo
                .updater
                .dir_changed(&target_repo, &path, count_delta, bytes_delta)
                .await;
            state
                .repo
                .updater
                .store_changed(&store_name, count_delta, bytes_delta)
                .await;
            (
                StatusCode::CREATED,
                Json(ArtifactResponse::from_record(path, result.record)),
            )
                .into_response()
        }
        Err(e) => {
            UPLOADS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            e.into_response()
        }
    }
}

/// Delete an artifact from a hosted repository.
#[utoipa::path(
    delete,
    path = "/repository/{repo}/{path}",
    params(
        ("repo" = String, Path, description = "Repository name"),
        ("path" = String, Path, description = "Artifact path"),
    ),
    responses(
        (status = 204, description = "Artifact deleted"),
        (status = 404, description = "Artifact not found"),
        (status = 405, description = "Repository does not accept deletes")
    ),
    tag = "artifacts"
)]
pub async fn delete_artifact(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    uri: axum::http::Uri,
    Path(rest): Path<String>,
    request_headers: HeaderMap,
) -> impl IntoResponse {
    let (repo_name, path) = split_rest(&rest);
    let path = match normalize_artifact_path(&path) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    if let Err(perm_err) = state
        .auth
        .backend
        .check_permission(&user.0, &repo_name, Capability::Delete)
        .await
    {
        // For Docker V2 paths, bypass the generic permission error and let
        // the Docker dispatch return a proper 401 Bearer challenge instead.
        if path == "v2" || path.starts_with("v2/") {
            let repo_name_owned = repo_name.clone();
            if let Ok(Some(rc)) = service::get_repo(state.repo.kv.as_ref(), &repo_name_owned).await
            {
                if rc.format() == ArtifactFormat::Docker {
                    if let Some(resp) =
                        depot_format_docker::api::dispatch::try_handle_docker_v2_path(
                            &state.format_state(),
                            &user.0,
                            &repo_name,
                            &path,
                            &axum::http::Method::DELETE,
                            uri.query(),
                            uri.authority().map(|a| a.as_str()),
                            &request_headers,
                            Body::empty(),
                        )
                        .await
                    {
                        return resp;
                    }
                }
            }
        }
        return map_permission_error(perm_err, &user.0);
    }
    let repo_name_owned = repo_name.clone();
    let repo_config = match service::get_repo(state.repo.kv.as_ref(), &repo_name_owned).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", repo_name))
                .into_response()
        }
        Err(e) => return e.into_response(),
    };

    // Dispatch format-specific DELETEs (e.g. cargo yank) before generic artifact delete.
    if is_format_write_path(repo_config.format(), &axum::http::Method::DELETE, &path) {
        return dispatch_format_write(
            &state,
            &user,
            &repo_config,
            &axum::http::Method::DELETE,
            &path,
            request_headers,
            Body::empty(),
        )
        .await;
    }

    let member_configs = match repo_config.kind {
        RepoKind::Hosted | RepoKind::Cache { .. } => vec![repo_config],
        RepoKind::Proxy { ref members, .. } => {
            let mut configs = Vec::new();
            for m in members {
                match service::get_repo(state.repo.kv.as_ref(), m).await {
                    Ok(Some(c))
                        if c.repo_type() == RepoType::Hosted
                            || c.repo_type() == RepoType::Cache =>
                    {
                        configs.push(c);
                    }
                    _ => {}
                }
            }
            if configs.is_empty() {
                return DepotError::NotAllowed("no deletable members in proxy repo".into())
                    .into_response();
            }
            configs
        }
    };

    let mut found = false;
    for config in &member_configs {
        let blobs = match state.repo.blob_store(&config.store).await {
            Ok(b) => b,
            Err(e) => return e.into_response(),
        };
        let hosted = HostedRepo {
            name: config.name.clone(),
            kv: Arc::clone(&state.repo.kv),
            blobs,
            updater: state.repo.updater.clone(),
            store: config.store.clone(),
            format: config.format().to_string(),
        };
        match hosted.delete(&path).await {
            Ok(true) => found = true,
            Ok(false) => {}
            Err(e) => return e.into_response(),
        }
    }

    if found {
        StatusCode::NO_CONTENT.into_response()
    } else {
        DepotError::NotFound(format!("artifact not found at path {}", path)).into_response()
    }
}

/// List or search artifacts in a repository.
#[utoipa::path(
    get,
    path = "/api/v1/repositories/{repo}/artifacts",
    params(
        ("repo" = String, Path, description = "Repository name"),
        SearchParams
    ),
    responses(
        (status = 200, description = "Browse response with dirs and artifacts", body = BrowseResponse)
    ),
    tag = "artifacts"
)]
pub async fn list_artifacts(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(repo_name): Path<String>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    if let Err(e) = state
        .auth
        .backend
        .check_permission(&user.0, &repo_name, Capability::Read)
        .await
    {
        return map_permission_error(e, &user.0);
    }
    let repo_name_owned = repo_name.clone();
    let repo_config = match service::get_repo(state.repo.kv.as_ref(), &repo_name_owned).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", repo_name))
                .into_response()
        }
        Err(e) => return e.into_response(),
    };

    // Build pagination from query params.
    let pagination = Pagination {
        cursor: params.cursor,
        offset: params.offset.unwrap_or(0),
        limit: params.limit.unwrap_or(100).min(10000),
    };

    let blobs = match state.repo.blob_store(&repo_config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };

    // Search mode: return flat artifact list with no dirs.
    if let Some(q) = params.q {
        let repo =
            repo_mod::make_raw_repo(state.repo.repo_context(Arc::clone(&blobs)), &repo_config);
        let results = repo.search(&q, &pagination).await;

        return match results {
            Ok(paginated) => {
                let resp = BrowseResponse {
                    dirs: Vec::new(),
                    artifacts: paginated
                        .items
                        .into_iter()
                        .map(|(path, r)| ArtifactResponse::from_record(path, r))
                        .collect(),
                    next_cursor: paginated.next_cursor,
                    limit: Some(pagination.limit),
                    total: None,
                    offset: None,
                };
                Json(resp).into_response()
            }
            Err(e) => e.into_response(),
        };
    }

    // Browse mode: return directory children with paginated artifacts.
    let prefix = params.prefix.as_deref().unwrap_or("");
    let repo = repo_mod::make_raw_repo(state.repo.repo_context(Arc::clone(&blobs)), &repo_config);
    let result = repo.list_children(prefix).await;

    match result {
        Ok((dirs, files)) => {
            let total = files.len();
            let offset = pagination.offset.min(total);
            let end = (offset + pagination.limit).min(total);

            let resp = BrowseResponse {
                dirs: dirs
                    .into_iter()
                    .map(|(name, de)| DirResponse::from_entry(name, de))
                    .collect(),
                artifacts: files
                    .get(offset..end)
                    .unwrap_or_default()
                    .iter()
                    .map(|(name, te)| {
                        ArtifactResponse::from_tree_entry(name.clone(), prefix, te.clone())
                    })
                    .collect(),
                next_cursor: None,
                limit: Some(pagination.limit),
                total: Some(total),
                offset: Some(offset),
            };
            Json(resp).into_response()
        }
        Err(e) => e.into_response(),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BulkDeleteRequest {
    pub prefix: String,
}

/// Start a bulk delete task for all artifacts under a directory prefix.
#[utoipa::path(
    post,
    path = "/api/v1/repositories/{repo}/bulk-delete",
    params(
        ("repo" = String, Path, description = "Repository name"),
    ),
    request_body(content = BulkDeleteRequest, content_type = "application/json"),
    responses(
        (status = 201, description = "Bulk delete task created", body = crate::server::infra::task::TaskInfo),
        (status = 404, description = "Repository not found"),
        (status = 405, description = "Repository does not accept deletes"),
        (status = 409, description = "Bulk delete already running for this prefix"),
    ),
    tag = "artifacts"
)]
pub async fn start_bulk_delete(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(repo_name): Path<String>,
    Json(body): Json<BulkDeleteRequest>,
) -> impl IntoResponse {
    if let Err(e) = state
        .auth
        .backend
        .check_permission(&user.0, &repo_name, Capability::Delete)
        .await
    {
        return map_permission_error(e, &user.0);
    }

    let prefix = match normalize_artifact_path(&body.prefix) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };

    let repo_config = match service::get_repo(state.repo.kv.as_ref(), &repo_name).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", repo_name))
                .into_response()
        }
        Err(e) => return e.into_response(),
    };

    // Collect the list of repos to delete from. For a hosted or cache repo
    // that is just itself; for a proxy repo it is every member.
    let mut targets: Vec<RepoConfig> = Vec::new();
    match &repo_config.kind {
        depot_core::store::kv::RepoKind::Hosted | depot_core::store::kv::RepoKind::Cache { .. } => {
            targets.push(repo_config.clone());
        }
        depot_core::store::kv::RepoKind::Proxy { members, .. } => {
            for m in members {
                match service::get_repo(state.repo.kv.as_ref(), m).await {
                    Ok(Some(mc)) => targets.push(mc),
                    _ => continue,
                }
            }
            if targets.is_empty() {
                return DepotError::NotAllowed("no members found in proxy repo".into())
                    .into_response();
            }
        }
    }

    if state
        .bg
        .tasks
        .has_running_bulk_delete(&repo_name, &prefix)
        .await
    {
        return DepotError::Conflict(format!(
            "bulk delete already running for {}/{}",
            repo_name, prefix
        ))
        .into_response();
    }

    use crate::server::infra::task::{TaskKind, TaskResult};
    use crate::server::worker::bulk_delete;

    let kind = TaskKind::BulkDelete {
        repo: repo_name.clone(),
        prefix: prefix.clone(),
    };
    let (task_id, progress_tx, cancel) = state.bg.tasks.create(kind).await;
    state.bg.tasks.mark_running(task_id).await;
    state.bg.scan_trigger.notify_one();

    let task_manager = state.bg.tasks.clone();
    let kv = Arc::clone(&state.repo.kv);
    let updater = state.repo.updater.clone();
    let repo_for_task = repo_name.clone();
    let prefix_for_task = prefix.clone();

    tokio::spawn(async move {
        task_manager
            .append_log(
                task_id,
                format!(
                    "Starting bulk delete of {}/{}/",
                    repo_for_task, prefix_for_task
                ),
            )
            .await;

        let mut total_deleted: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut failed = false;
        let start = std::time::Instant::now();

        for target in &targets {
            if cancel.is_cancelled() {
                break;
            }
            task_manager
                .append_log(
                    task_id,
                    format!("Deleting from member repo {}", target.name),
                )
                .await;

            let offset = bulk_delete::ProgressOffset {
                artifacts: total_deleted,
                bytes: total_bytes,
            };
            match bulk_delete::bulk_delete(
                kv.clone(),
                &target.name,
                &prefix_for_task,
                &updater,
                &target.store,
                Some(&cancel),
                Some(&progress_tx),
                &offset,
            )
            .await
            {
                Ok(result) => {
                    total_deleted += result.deleted_artifacts;
                    total_bytes += result.deleted_bytes;
                }
                Err(e) => {
                    if cancel.is_cancelled() {
                        break;
                    }
                    task_manager
                        .append_log(task_id, format!("Error deleting from {}: {e}", target.name))
                        .await;
                    task_manager.mark_failed(task_id, format!("{e}")).await;
                    failed = true;
                    break;
                }
            }
        }

        if !failed {
            if cancel.is_cancelled() {
                task_manager.mark_cancelled(task_id).await;
            } else {
                let result = crate::server::infra::task::BulkDeleteResult {
                    repo: repo_for_task.clone(),
                    prefix: prefix_for_task.clone(),
                    deleted_artifacts: total_deleted,
                    deleted_bytes: total_bytes,
                    elapsed_ns: start.elapsed().as_nanos() as u64,
                };
                let summary = format!(
                    "Deleted {} artifacts ({} bytes) from {}/{}",
                    total_deleted, total_bytes, repo_for_task, prefix_for_task
                );
                task_manager.update_summary(task_id, summary.clone()).await;
                task_manager.append_log(task_id, summary).await;
                task_manager
                    .mark_completed(task_id, TaskResult::BulkDelete(result))
                    .await;
            }
        }
    });

    let info = state.bg.tasks.get(task_id).await;
    (StatusCode::CREATED, Json(info)).into_response()
}

// ---------------------------------------------------------------------------
// Directory archive download
// ---------------------------------------------------------------------------

/// Supported archive formats for directory download.
#[derive(Debug, Clone, Copy)]
enum ArchiveFormat {
    Tar,
    TarGz,
    TarXz,
    Zip,
}

impl ArchiveFormat {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "tar" => Some(Self::Tar),
            "tar.gz" => Some(Self::TarGz),
            "tar.xz" => Some(Self::TarXz),
            "zip" => Some(Self::Zip),
            _ => None,
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::Tar => "application/x-tar",
            Self::TarGz => "application/gzip",
            Self::TarXz => "application/x-xz",
            Self::Zip => "application/zip",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Tar => ".tar",
            Self::TarGz => ".tar.gz",
            Self::TarXz => ".tar.xz",
            Self::Zip => ".zip",
        }
    }
}

#[derive(Deserialize)]
pub struct ArchiveDownloadParams {
    prefix: String,
    format: String,
}

/// A synchronous `Write` adapter that sends `Bytes` chunks through a bounded
/// tokio mpsc channel.  Used inside `spawn_blocking` to bridge archive writers
/// (tar, flate2, zip) to an async HTTP response body.
struct ChannelWriter {
    tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    handle: tokio::runtime::Handle,
}

impl std::io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let chunk = bytes::Bytes::copy_from_slice(buf);
        self.handle
            .block_on(self.tx.send(Ok(chunk)))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "receiver dropped"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Recursively collect `(full_path, relative_path)` for all files under `prefix`.
/// Only metadata is collected — no blobs are opened.
async fn collect_archive_files(
    repo: &dyn repo_mod::RawRepo,
    prefix: &str,
    strip_prefix: &str,
) -> depot_core::error::Result<Vec<(String, String)>> {
    let mut result = Vec::new();
    let (dirs, files) = repo.list_children(prefix).await?;

    for (name, te) in files {
        if matches!(&te.kind, TreeEntryKind::File { .. }) {
            let full = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let relative = full.strip_prefix(strip_prefix).unwrap_or(&full).to_string();
            result.push((full, relative));
        }
    }

    for (name, _de) in dirs {
        let sub = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let sub_files = Box::pin(collect_archive_files(repo, &sub, strip_prefix)).await?;
        result.extend(sub_files);
    }

    Ok(result)
}

/// Fetch an artifact for archive inclusion, returning `None` on missing/error.
fn fetch_for_archive(
    handle: &tokio::runtime::Handle,
    repo: &dyn repo_mod::RawRepo,
    full_path: &str,
) -> Option<ArtifactContent> {
    match handle.block_on(repo.get(full_path)) {
        Ok(Some(c)) => Some(c),
        Ok(None) => {
            tracing::warn!(path = %full_path, "skipping missing artifact in archive");
            None
        }
        Err(e) => {
            tracing::warn!(path = %full_path, error = %e, "skipping errored artifact in archive");
            None
        }
    }
}

/// Bridge an `ArtifactData` into a `std::io::Read` via `SyncIoBridge`.
fn sync_read_artifact(data: ArtifactData) -> Box<dyn std::io::Read + Send> {
    match data {
        ArtifactData::Reader(blob_reader) => {
            Box::new(tokio_util::io::SyncIoBridge::new(blob_reader))
        }
        ArtifactData::Stream(stream) => {
            let async_reader = tokio_util::io::StreamReader::new(stream);
            Box::new(tokio_util::io::SyncIoBridge::new(async_reader))
        }
    }
}

/// Write files into a tar archive, streaming each blob via `SyncIoBridge`.
fn write_tar_entries<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    files: &[(String, String)],
    repo: &dyn repo_mod::RawRepo,
    handle: &tokio::runtime::Handle,
) -> std::io::Result<()> {
    for (full_path, relative_path) in files {
        let content = match fetch_for_archive(handle, repo, full_path) {
            Some(c) => c,
            None => continue,
        };

        let mut header = tar::Header::new_gnu();
        header.set_size(content.record.size);
        header.set_mode(0o644);
        header.set_mtime(content.record.updated_at.timestamp().max(0) as u64);
        header.set_cksum();

        let reader = sync_read_artifact(content.data);
        tar.append_data(&mut header, relative_path, reader)?;
    }
    Ok(())
}

/// Download all artifacts under a directory as a streaming archive.
#[utoipa::path(
    get,
    path = "/api/v1/repositories/{repo}/download-archive",
    params(
        ("repo" = String, Path, description = "Repository name"),
        ("prefix" = String, Query, description = "Directory prefix to archive"),
        ("format" = String, Query, description = "Archive format: tar, tar.gz, tar.xz, or zip")
    ),
    responses(
        (status = 200, description = "Streaming archive download", content_type = "application/octet-stream"),
        (status = 400, description = "Invalid format or prefix"),
        (status = 404, description = "Repository not found")
    ),
    tag = "artifacts"
)]
pub async fn download_archive(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(repo_name): Path<String>,
    Query(params): Query<ArchiveDownloadParams>,
) -> impl IntoResponse {
    // Validate format
    let format = match ArchiveFormat::from_str(&params.format) {
        Some(f) => f,
        None => {
            return DepotError::BadRequest("format must be one of: tar, tar.gz, tar.xz, zip".into())
                .into_response()
        }
    };

    // Normalize prefix
    let prefix = match normalize_artifact_path(&params.prefix) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };

    // Auth
    if let Err(e) = state
        .auth
        .backend
        .check_permission(&user.0, &repo_name, Capability::Read)
        .await
    {
        return map_permission_error(e, &user.0);
    }

    // Load repo
    let repo_config = match service::get_repo(state.repo.kv.as_ref(), &repo_name).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", repo_name))
                .into_response()
        }
        Err(e) => return e.into_response(),
    };

    let blobs = match state.repo.blob_store(&repo_config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let repo = repo_mod::make_raw_repo(state.repo.repo_context(blobs), &repo_config);

    // Collect file metadata (no blob reads)
    let strip_prefix = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    let files = match collect_archive_files(repo.as_ref(), &prefix, &strip_prefix).await {
        Ok(f) => f,
        Err(e) => return e.into_response(),
    };

    // Derive archive filename from the directory name
    let dir_name = prefix
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("archive");
    let filename = format!("{}{}", dir_name, format.extension());

    // Build response headers
    let mut headers = HeaderMap::new();
    if let Ok(v) = format.content_type().parse() {
        headers.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = format!("attachment; filename=\"{filename}\"").parse() {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }

    // Stream the archive via a channel
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(32);
    let handle = tokio::runtime::Handle::current();

    tokio::task::spawn_blocking(move || {
        let _guard = handle.enter();

        let writer = ChannelWriter {
            tx,
            handle: handle.clone(),
        };

        let result: std::io::Result<()> = (|| -> std::io::Result<()> {
            match format {
                ArchiveFormat::Tar => {
                    let mut tar = tar::Builder::new(writer);
                    write_tar_entries(&mut tar, &files, repo.as_ref(), &handle)?;
                    tar.finish()?;
                    Ok(())
                }
                ArchiveFormat::TarGz => {
                    let gz = flate2::write::GzEncoder::new(writer, flate2::Compression::default());
                    let mut tar = tar::Builder::new(gz);
                    write_tar_entries(&mut tar, &files, repo.as_ref(), &handle)?;
                    let gz = tar.into_inner()?;
                    gz.finish()?;
                    Ok(())
                }
                ArchiveFormat::TarXz => {
                    let (pipe_reader, pipe_writer) = std::io::pipe()?;

                    std::thread::scope(|s| {
                        let mut writer = writer;
                        let compress_result = s.spawn(move || {
                            let mut buf_reader = std::io::BufReader::new(pipe_reader);
                            lzma_rs::xz_compress(&mut buf_reader, &mut writer)
                        });

                        let mut tar = tar::Builder::new(pipe_writer);
                        let tar_result =
                            write_tar_entries(&mut tar, &files, repo.as_ref(), &handle);
                        // Close the pipe writer so the compressor sees EOF
                        drop(tar.into_inner().ok());

                        tar_result?;
                        compress_result
                            .join()
                            .map_err(|_| std::io::Error::other("xz compressor thread panicked"))?
                            .map_err(std::io::Error::other)?;
                        Ok(())
                    })
                }
                ArchiveFormat::Zip => {
                    let tmp = tempfile::tempfile()?;
                    let mut zip = zip::ZipWriter::new(tmp);
                    let options = zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated);

                    for (full_path, relative_path) in &files {
                        let content = match fetch_for_archive(&handle, repo.as_ref(), full_path) {
                            Some(c) => c,
                            None => continue,
                        };

                        zip.start_file(relative_path, options)
                            .map_err(std::io::Error::other)?;
                        let mut reader = sync_read_artifact(content.data);
                        std::io::copy(&mut reader, &mut zip)?;
                    }

                    let mut tmp = zip.finish().map_err(std::io::Error::other)?;
                    // Seek back and stream the completed zip to the channel
                    use std::io::{Seek, SeekFrom};
                    tmp.seek(SeekFrom::Start(0))?;
                    let mut writer = writer;
                    std::io::copy(&mut tmp, &mut writer)?;
                    Ok(())
                }
            }
        })();

        if let Err(e) = result {
            if e.kind() != std::io::ErrorKind::BrokenPipe {
                tracing::warn!(error = %e, "archive generation failed");
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    (headers, Body::from_stream(stream)).into_response()
}
