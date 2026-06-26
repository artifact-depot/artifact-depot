// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Nexus Repository Manager REST API compatibility layer.
//!
//! Implements the subset of the Nexus v1 API that CI/CD scripts commonly use:
//!
//! - `GET /service/rest/v1/assets` — list assets in a repository
//! - `GET /service/rest/v1/components` — list components in a repository
//! - `POST /service/rest/v1/components?repository={repo}` — upload a component
//! - `GET /service/rest/v1/search/assets` — search for assets
//! - `GET /service/rest/v1/search/assets/download` — redirect to first match
//! - `GET /service/rest/v1/search` — search for components
//! - `GET /service/rest/v1/status` — health check
//! - `GET /service/rest/v1/repositories` — list repositories
//! - `POST /service/rest/v1/staging/move/{destination}` — move components by criteria
//! - `POST /service/rest/v1/staging/delete` — delete components by criteria
//! - `POST /service/rest/v1/staging/copy/{destination}` — copy components (incl. cross-store)

use axum::{
    extract::{Multipart, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use depot_core::auth::AuthenticatedUser;
use depot_core::error::{self, DepotError};
use depot_core::format_state::FormatState;
use depot_core::repo::{make_raw_repo, RawRepo};
use depot_core::service;
use depot_core::store::kv::{ArtifactFormat, ArtifactRecord, Capability, Pagination, RepoType};
use depot_format_apt::store::AptStore;
use depot_format_docker::DockerStore;
use depot_format_helm::store::HelmStore;
use depot_format_pypi::store::PypiStore;
use depot_format_yum::store::YumStore;

// ---------------------------------------------------------------------------
// Query parameters (Nexus-compatible)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchAssetsParams {
    /// Repository name
    pub repository: Option<String>,
    /// Asset/component name filter (supports `*` wildcards)
    pub name: Option<String>,
    /// Free-text search
    pub q: Option<String>,
    /// Group/namespace filter
    pub group: Option<String>,
    /// Sort field
    pub sort: Option<String>,
    /// Sort direction (asc/desc)
    pub direction: Option<String>,
    /// Pagination token
    #[serde(rename = "continuationToken")]
    pub continuation_token: Option<String>,
    /// Maximum number of results per page (default: 1000, max: 100000)
    pub limit: Option<usize>,
}

// ---------------------------------------------------------------------------
// Response types (Nexus-compatible)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct NexusAssetItem {
    #[serde(rename = "downloadUrl")]
    pub download_url: String,
    pub path: String,
    pub id: String,
    pub repository: String,
    pub format: String,
    pub checksum: NexusChecksum,
    #[serde(rename = "contentType")]
    pub content_type: String,
    #[serde(rename = "lastModified")]
    pub last_modified: String,
    #[serde(rename = "fileSize")]
    pub file_size: u64,
}

#[derive(Debug, Serialize)]
pub struct NexusChecksum {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha1: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NexusSearchResponse {
    pub items: Vec<NexusAssetItem>,
    #[serde(rename = "continuationToken")]
    pub continuation_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NexusComponentItem {
    pub id: String,
    pub repository: String,
    pub format: String,
    pub name: String,
    pub version: Option<String>,
    pub assets: Vec<NexusAssetItem>,
}

#[derive(Debug, Serialize)]
pub struct NexusComponentSearchResponse {
    pub items: Vec<NexusComponentItem>,
    #[serde(rename = "continuationToken")]
    pub continuation_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NexusRepoItem {
    pub name: String,
    pub format: String,
    #[serde(rename = "type")]
    pub repo_type: String,
    pub url: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the external download URL for an artifact.
fn download_url(origin: &str, repo: &str, path: &str) -> String {
    format!("{origin}/repository/{repo}/{path}")
}

/// Convert a Nexus-style wildcard name filter to a prefix/query.
/// `foo/bar/*` → prefix `foo/bar/`, query `None`
/// `*foo*` → prefix `""`, query `Some("foo")`
/// `foo/bar` → prefix `""`, query `Some("foo/bar")`
fn parse_name_filter(name: &str) -> (String, Option<String>) {
    // Strip leading slash — stored paths never start with '/'.
    let name = name.strip_prefix('/').unwrap_or(name);
    let trimmed = name.trim_matches('*');
    if name.ends_with('*') && !name.starts_with('*') {
        // Trailing wildcard: use as prefix
        (trimmed.to_string(), None)
    } else if trimmed.is_empty() {
        (String::new(), None)
    } else {
        (String::new(), Some(trimmed.to_string()))
    }
}

const DEFAULT_PAGE_SIZE: usize = 1_000;
const MAX_PAGE_SIZE: usize = 100_000;
const SCAN_LIMIT: usize = 1_000;

fn effective_limit(requested: Option<usize>) -> usize {
    match requested {
        Some(n) if n >= 1 => n.min(MAX_PAGE_SIZE),
        _ => DEFAULT_PAGE_SIZE,
    }
}

#[derive(Copy, Clone)]
enum CollectMode<'a> {
    Search { query: &'a str },
    List { prefix: &'a str },
}

/// Accumulate artifacts from the repository (hosted, cache, or proxy) via the
/// [`RawRepo`] abstraction. Each underlying scan is capped at [`SCAN_LIMIT`]
/// so proxy repos with many members don't inflate per-shard batch sizes.
///
/// For proxy repos this delegates to `ProxyRepo::search` / `ProxyRepo::list`,
/// which interleave results from members under a single resumable
/// path-based cursor (see [`depot_core::repo::proxy::ProxyRepo`]).
async fn collect_artifacts(
    repo: &dyn RawRepo,
    mode: CollectMode<'_>,
    initial_cursor: Option<String>,
    requested: usize,
) -> error::Result<(Vec<(String, ArtifactRecord)>, Option<String>)> {
    let mut all_items = Vec::new();
    let mut cursor = initial_cursor;

    while all_items.len() < requested {
        let batch = (requested - all_items.len()).min(SCAN_LIMIT);
        let pagination = Pagination {
            limit: batch,
            offset: 0,
            cursor,
        };

        let page = match mode {
            CollectMode::Search { query } => repo.search(query, &pagination).await?,
            CollectMode::List { prefix } => repo.list(prefix, &pagination).await?,
        };

        all_items.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    Ok((all_items, cursor))
}

fn collect_mode<'a>(prefix: &'a str, query: &'a Option<String>) -> CollectMode<'a> {
    match query.as_deref() {
        Some(q) if !q.is_empty() => CollectMode::Search { query: q },
        _ => CollectMode::List { prefix },
    }
}

/// Build (prefix, query) from the Nexus group/name/q parameters.
fn build_search_params(params: &SearchAssetsParams) -> (String, Option<String>) {
    match (&params.group, &params.name) {
        (Some(g), name_opt) => {
            let group = g.strip_prefix('/').unwrap_or(g).trim_end_matches('*');
            match name_opt {
                Some(n) => {
                    let (_, name_query) = parse_name_filter(n);
                    (group.to_string(), name_query)
                }
                None => (group.to_string(), None),
            }
        }
        (None, Some(n)) => parse_name_filter(n),
        (None, None) => (String::new(), params.q.clone()),
    }
}

/// Extract a raw query parameter without `+` → space decoding.
/// Percent-decodes `%XX` sequences but preserves literal `+`.
fn raw_query_param(query: Option<&str>, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    query?.split('&').find_map(|pair| {
        let val = pair.strip_prefix(&prefix)?;
        Some(percent_decode(val))
    })
}

/// Decode `%XX` sequences but leave `+` as literal `+`.
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().and_then(hex_val);
            let lo = chars.next().and_then(hex_val);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(char::from(h << 4 | l));
            }
        } else {
            out.push(char::from(b));
        }
    }
    out
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /service/rest/v1/search/assets`
pub async fn search_assets(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    Query(mut params): Query<SearchAssetsParams>,
) -> Response {
    // Re-extract `name` from raw query to preserve literal `+` characters
    // (serde_urlencoded decodes `+` as space per x-www-form-urlencoded rules,
    // but clients send raw `+` in timestamps like `+00:00Z`).
    if let Some(raw_name) = raw_query_param(uri.query(), "name") {
        params.name = Some(raw_name);
    }
    if let Some(raw_group) = raw_query_param(uri.query(), "group") {
        params.group = Some(raw_group);
    }

    let origin = state.external_origin(&headers, uri.authority().map(|a| a.as_str()));
    let repo_name = match &params.repository {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(NexusSearchResponse {
                    items: vec![],
                    continuation_token: None,
                }),
            )
                .into_response();
        }
    };

    if let Err(e) = state
        .auth
        .check_permission(&user.0, &repo_name, Capability::Read)
        .await
    {
        return (StatusCode::FORBIDDEN, e.to_string()).into_response();
    }

    let repo_config = match service::get_repo(state.kv.as_ref(), &repo_name).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", repo_name))
                .into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let blobs = match state.blob_store(&repo_config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let repo = make_raw_repo(state.repo_context(blobs), &repo_config);
    let format_str = repo_config.format().to_string();

    let (prefix, query) = build_search_params(&params);
    let mode = collect_mode(&prefix, &query);
    let requested = effective_limit(params.limit);

    match collect_artifacts(
        repo.as_ref(),
        mode,
        params.continuation_token.clone(),
        requested,
    )
    .await
    {
        Ok((collected, continuation_token)) => {
            let items: Vec<NexusAssetItem> = collected
                .iter()
                .map(|(path, record)| NexusAssetItem {
                    download_url: download_url(&origin, &repo_name, path),
                    path: path.clone(),
                    id: record.id.clone(),
                    repository: repo_name.clone(),
                    format: format_str.clone(),
                    checksum: NexusChecksum {
                        sha256: None,
                        sha1: None,
                        md5: None,
                        blake3: record.content_hash.clone(),
                    },
                    content_type: record.content_type.clone(),
                    last_modified: record.updated_at.to_rfc3339(),
                    file_size: record.size,
                })
                .collect();

            Json(NexusSearchResponse {
                items,
                continuation_token,
            })
            .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `GET /service/rest/v1/search/assets/download`
///
/// Redirects to the download URL of the first matching asset.
pub async fn search_assets_download(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    Query(mut params): Query<SearchAssetsParams>,
) -> Response {
    if let Some(raw_name) = raw_query_param(uri.query(), "name") {
        params.name = Some(raw_name);
    }
    if let Some(raw_group) = raw_query_param(uri.query(), "group") {
        params.group = Some(raw_group);
    }
    let origin = state.external_origin(&headers, uri.authority().map(|a| a.as_str()));
    let repo_name = match &params.repository {
        Some(r) => r.clone(),
        None => return (StatusCode::BAD_REQUEST, "repository parameter required").into_response(),
    };

    if let Err(e) = state
        .auth
        .check_permission(&user.0, &repo_name, Capability::Read)
        .await
    {
        return (StatusCode::FORBIDDEN, e.to_string()).into_response();
    }

    let repo_config = match service::get_repo(state.kv.as_ref(), &repo_name).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", repo_name))
                .into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let blobs = match state.blob_store(&repo_config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let repo = make_raw_repo(state.repo_context(blobs), &repo_config);

    let (prefix, query) = build_search_params(&params);
    let mode = collect_mode(&prefix, &query);

    let pagination = Pagination {
        limit: 1,
        offset: 0,
        cursor: None,
    };

    let results = match mode {
        CollectMode::Search { query } => repo.search(query, &pagination).await,
        CollectMode::List { prefix } => repo.list(prefix, &pagination).await,
    };

    match results {
        Ok(page) => {
            if let Some((path, _)) = page.items.first() {
                let url = download_url(&origin, &repo_name, path);
                (StatusCode::FOUND, [(header::LOCATION, url)]).into_response()
            } else {
                (StatusCode::NOT_FOUND, "no matching assets").into_response()
            }
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `GET /service/rest/v1/search`
///
/// Component search. In depot every artifact is its own component,
/// so this wraps the asset search with component-level structure.
pub async fn search_components(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    Query(mut params): Query<SearchAssetsParams>,
) -> Response {
    if let Some(raw_name) = raw_query_param(uri.query(), "name") {
        params.name = Some(raw_name);
    }
    if let Some(raw_group) = raw_query_param(uri.query(), "group") {
        params.group = Some(raw_group);
    }
    let origin = state.external_origin(&headers, uri.authority().map(|a| a.as_str()));
    let repo_name = match &params.repository {
        Some(r) => r.clone(),
        None => {
            return Json(NexusComponentSearchResponse {
                items: vec![],
                continuation_token: None,
            })
            .into_response();
        }
    };

    if let Err(e) = state
        .auth
        .check_permission(&user.0, &repo_name, Capability::Read)
        .await
    {
        return (StatusCode::FORBIDDEN, e.to_string()).into_response();
    }

    let repo_config = match service::get_repo(state.kv.as_ref(), &repo_name).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return DepotError::NotFound(format!("repository '{}' not found", repo_name))
                .into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let blobs = match state.blob_store(&repo_config.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let repo = make_raw_repo(state.repo_context(blobs), &repo_config);
    let format_str = repo_config.format().to_string();

    let (prefix, query) = build_search_params(&params);
    let mode = collect_mode(&prefix, &query);
    let requested = effective_limit(params.limit);

    match collect_artifacts(
        repo.as_ref(),
        mode,
        params.continuation_token.clone(),
        requested,
    )
    .await
    {
        Ok((collected, continuation_token)) => {
            let items: Vec<NexusComponentItem> = collected
                .iter()
                .map(|(path, record)| {
                    let asset = NexusAssetItem {
                        download_url: download_url(&origin, &repo_name, path),
                        path: path.clone(),
                        id: record.id.clone(),
                        repository: repo_name.clone(),
                        format: format_str.clone(),
                        checksum: NexusChecksum {
                            sha256: None,
                            sha1: None,
                            md5: None,
                            blake3: record.content_hash.clone(),
                        },
                        content_type: record.content_type.clone(),
                        last_modified: record.updated_at.to_rfc3339(),
                        file_size: record.size,
                    };
                    // In depot, each artifact is its own component.
                    let name = path
                        .rsplit_once('/')
                        .map(|(_, f)| f)
                        .unwrap_or(path)
                        .to_string();
                    NexusComponentItem {
                        id: record.id.clone(),
                        repository: repo_name.clone(),
                        format: format_str.clone(),
                        name,
                        version: None,
                        assets: vec![asset],
                    }
                })
                .collect();

            Json(NexusComponentSearchResponse {
                items,
                continuation_token,
            })
            .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `GET /service/rest/v1/status`
pub async fn status() -> impl IntoResponse {
    StatusCode::OK
}

/// `GET /service/rest/v1/repositories`
pub async fn list_repositories(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    let origin = state.external_origin(&headers, uri.authority().map(|a| a.as_str()));
    if user.0 == "anonymous" {
        return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    }

    match service::list_repos(state.kv.as_ref()).await {
        Ok(repos) => {
            let items: Vec<NexusRepoItem> = repos
                .iter()
                .map(|r| NexusRepoItem {
                    name: r.name.clone(),
                    format: r.format().to_string(),
                    repo_type: r.repo_type().to_string(),
                    url: format!("{origin}/repository/{}", r.name),
                })
                .collect();
            Json(items).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /service/rest/v1/components?repository={repo}
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UploadComponentParams {
    pub repository: String,
}

/// `POST /service/rest/v1/components?repository={repo}`
///
/// Nexus-compatible component upload. Supports Yum and APT formats.
///
/// Yum multipart fields:
///   - `yum.asset` — the RPM file
///   - `yum.asset.filename` — optional filename override
///   - `yum.directory` — optional directory path
///
/// APT multipart fields:
///   - `apt.asset` — the .deb file
///   - `apt.asset.distribution` — distribution name (required)
///   - `apt.asset.component` — optional component (defaults to "main")
pub async fn upload_component(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<UploadComponentParams>,
    mut multipart: Multipart,
) -> Result<Response, DepotError> {
    // Look up repo to determine format before calling upload_preamble.
    let repo_config = service::get_repo(state.kv.as_ref(), &params.repository)
        .await?
        .ok_or_else(|| {
            DepotError::NotFound(format!("repository '{}' not found", params.repository))
        })?;

    match repo_config.format() {
        ArtifactFormat::Yum => {
            upload_component_yum(&state, &user.0, &params.repository, &mut multipart).await
        }
        ArtifactFormat::Apt => {
            upload_component_apt(&state, &user.0, &params.repository, &mut multipart).await
        }
        ArtifactFormat::Helm => {
            upload_component_helm(&state, &user.0, &params.repository, &mut multipart).await
        }
        ArtifactFormat::PyPI => {
            upload_component_pypi(&state, &user.0, &params.repository, &mut multipart).await
        }
        other => Err(DepotError::BadRequest(format!(
            "Nexus component upload is not supported for {other} repositories"
        ))),
    }
}

async fn upload_component_yum(
    state: &FormatState,
    user: &str,
    repo: &str,
    multipart: &mut Multipart,
) -> Result<Response, DepotError> {
    let (_target_repo, target_config, blobs) =
        super::upload_preamble(state, user, repo, ArtifactFormat::Yum).await?;

    let mut blob_info: Option<(String, String, String, String, u64)> = None;
    let mut directory = String::new();
    let mut explicit_filename: Option<String> = None;

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "yum.asset" => {
                let r = super::stream_multipart_field_to_blob(&mut field, blobs.as_ref()).await?;
                let filename = r.filename.unwrap_or_else(|| "package.rpm".to_string());
                blob_info = Some((filename, r.blob_id, r.blake3, r.sha256, r.size));
            }
            "yum.asset.filename" => {
                explicit_filename = field.text().await.ok();
            }
            "yum.directory" => {
                directory = field.text().await.unwrap_or_default();
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let (filename, blob_id, blake3, sha256, size) = match blob_info {
        Some(info) => info,
        None => return Err(DepotError::BadRequest("missing 'yum.asset' field".into())),
    };

    let filename = explicit_filename.unwrap_or(filename);

    let store = YumStore {
        repo: &target_config.name,
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: &target_config.store,
        updater: &state.updater,
        repodata_depth: target_config.format_config.repodata_depth(),
    };

    store
        .commit_package(&blob_id, &blake3, &sha256, size, &filename, &directory)
        .await?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn upload_component_apt(
    state: &FormatState,
    user: &str,
    repo: &str,
    multipart: &mut Multipart,
) -> Result<Response, DepotError> {
    let (_target_repo, target_config, blobs) =
        super::upload_preamble(state, user, repo, ArtifactFormat::Apt).await?;

    let mut blob_info: Option<(String, String, String, String, u64)> = None;
    let mut distribution: Option<String> = None;
    let mut component = String::from("main");

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "apt.asset" => {
                let r = super::stream_multipart_field_to_blob(&mut field, blobs.as_ref()).await?;
                let filename = r.filename.unwrap_or_else(|| "package.deb".to_string());
                blob_info = Some((filename, r.blob_id, r.blake3, r.sha256, r.size));
            }
            "apt.asset.distribution" => {
                distribution = field.text().await.ok();
            }
            "apt.asset.component" => {
                if let Ok(c) = field.text().await {
                    component = c;
                }
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let (filename, blob_id, blake3, sha256, size) = match blob_info {
        Some(info) => info,
        None => return Err(DepotError::BadRequest("missing 'apt.asset' field".into())),
    };

    let distribution = distribution
        .ok_or_else(|| DepotError::BadRequest("missing 'apt.asset.distribution' field".into()))?;

    let store = AptStore {
        repo: &target_config.name,
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: &target_config.store,
        updater: &state.updater,
    };

    store
        .commit_package(
            &blob_id,
            &blake3,
            &sha256,
            size,
            &distribution,
            &component,
            &filename,
        )
        .await?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn upload_component_helm(
    state: &FormatState,
    user: &str,
    repo: &str,
    multipart: &mut Multipart,
) -> Result<Response, DepotError> {
    let (_target_repo, target_config, blobs) =
        super::upload_preamble(state, user, repo, ArtifactFormat::Helm).await?;

    let mut blob_info: Option<(String, String, String, String, u64)> = None;

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "helm.asset" => {
                let r = super::stream_multipart_field_to_blob(&mut field, blobs.as_ref()).await?;
                let filename = r.filename.unwrap_or_else(|| "chart.tgz".to_string());
                blob_info = Some((filename, r.blob_id, r.blake3, r.sha256, r.size));
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let (_filename, blob_id, blake3, sha256, size) = match blob_info {
        Some(info) => info,
        None => return Err(DepotError::BadRequest("missing 'helm.asset' field".into())),
    };

    let store = HelmStore {
        repo: &target_config.name,
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: &target_config.store,
        updater: &state.updater,
    };

    store.commit_chart(&blob_id, &blake3, &sha256, size).await?;

    super::propagate_staleness_to_parents(
        state,
        &target_config.name,
        ArtifactFormat::Helm,
        "_helm/metadata_stale",
    )
    .await;

    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn upload_component_pypi(
    state: &FormatState,
    user: &str,
    repo: &str,
    multipart: &mut Multipart,
) -> Result<Response, DepotError> {
    let (_target_repo, target_config, blobs) =
        super::upload_preamble(state, user, repo, ArtifactFormat::PyPI).await?;

    let mut blob_info: Option<(String, String, String, String, u64)> = None;
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "pypi.asset" => {
                let r = super::stream_multipart_field_to_blob(&mut field, blobs.as_ref()).await?;
                let filename = r.filename.unwrap_or_else(|| "package.tar.gz".to_string());
                blob_info = Some((filename, r.blob_id, r.blake3, r.sha256, r.size));
            }
            "pypi.asset.name" | "name" => {
                name = field.text().await.ok();
            }
            "pypi.asset.version" | "version" => {
                version = field.text().await.ok();
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let (filename, blob_id, blake3, sha256, size) = match blob_info {
        Some(info) => info,
        None => return Err(DepotError::BadRequest("missing 'pypi.asset' field".into())),
    };

    let name =
        name.ok_or_else(|| DepotError::BadRequest("missing 'pypi.asset.name' field".into()))?;
    let version = version
        .ok_or_else(|| DepotError::BadRequest("missing 'pypi.asset.version' field".into()))?;

    let store = PypiStore {
        repo: &target_config.name,
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: &target_config.store,
        updater: &state.updater,
    };

    store
        .commit_package(
            &depot_format_pypi::store::PypiUploadMeta {
                name: &name,
                version: &version,
                filename: &filename,
                filetype: &depot_format_pypi::store::guess_filetype_pub(&filename),
                expected_sha256: None,
                requires_python: None,
                summary: None,
            },
            &crate::server::repo::BlobInfo {
                blob_id,
                blake3_hash: blake3,
                sha256_hash: sha256,
                size,
            },
        )
        .await?;

    let _ = store.set_stale_flag().await;
    super::propagate_staleness_to_parents(
        state,
        &target_config.name,
        ArtifactFormat::PyPI,
        "_pypi/metadata_stale",
    )
    .await;

    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Staging move / delete (Nexus-compatible)
// ---------------------------------------------------------------------------
//
// `POST /service/rest/v1/staging/move/{destination}` and
// `POST /service/rest/v1/staging/delete` move/delete components matching the
// search criteria. Mirrors Sonatype Nexus' Pro staging API: move is a
// metadata-only operation within a single blob store (cross-store relocation
// is a separate copy endpoint). Currently scoped to Docker repositories.

/// Search criteria shared by the staging move/delete endpoints.
#[derive(Debug, Deserialize)]
pub struct StagingParams {
    /// Source repository (required).
    pub repository: Option<String>,
    /// Docker image name / namespace (e.g. `myriad/api_server`).
    #[serde(rename = "docker.imageName")]
    pub docker_image_name: Option<String>,
    /// Docker image tag (e.g. `1.2.3`).
    #[serde(rename = "docker.imageTag")]
    pub docker_image_tag: Option<String>,
    /// Component tag — accepted as an alias for `docker.imageTag`.
    pub tag: Option<String>,
    /// Component name — accepted as an alias for `docker.imageName`.
    pub name: Option<String>,
    /// Replace an existing destination tag. Defaults to false.
    pub overwrite: Option<bool>,
    /// Delete the source tag after a successful copy (copy endpoint only).
    /// Defaults to false. Ignored by move (which always removes the source).
    #[serde(rename = "deleteSource")]
    pub delete_source: Option<bool>,
}

/// One component touched by a staging operation (Nexus-style payload item).
#[derive(Debug, Serialize)]
pub struct StagingComponent {
    pub repository: String,
    pub format: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Serialize)]
pub struct StagingResponse {
    pub components: Vec<StagingComponent>,
}

/// Look up a repository and require it to be a Docker repository.
async fn load_docker_repo(
    state: &FormatState,
    name: &str,
) -> Result<depot_core::store::kv::RepoConfig, Response> {
    let cfg = match service::get_repo(state.kv.as_ref(), name).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Err(
                DepotError::NotFound(format!("repository '{name}' not found")).into_response(),
            )
        }
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()),
    };
    if cfg.format() != ArtifactFormat::Docker {
        return Err(DepotError::BadRequest(format!(
            "staging operations currently support only Docker repositories; '{name}' is {}",
            cfg.format()
        ))
        .into_response());
    }
    Ok(cfg)
}

/// Resolve the `(image, tag)` criteria, requiring at least one so a
/// criteria-less request can't sweep an entire repository by accident.
fn staging_criteria(
    params: &StagingParams,
) -> Result<(Option<String>, Option<String>), DepotError> {
    let image = params
        .docker_image_name
        .clone()
        .or_else(|| params.name.clone());
    let tag = params
        .docker_image_tag
        .clone()
        .or_else(|| params.tag.clone());
    if image.is_none() && tag.is_none() {
        return Err(DepotError::BadRequest(
            "specify at least docker.imageName or docker.imageTag/tag; \
             refusing to act on all components"
                .into(),
        ));
    }
    Ok((image, tag))
}

/// Expand the criteria into the concrete `(image, tag)` pairs to act on.
/// With an explicit tag this is a single pair; with only an image name it is
/// every tag currently published under that image namespace.
async fn enumerate_targets(
    state: &FormatState,
    repo: &depot_core::store::kv::RepoConfig,
    image: Option<String>,
    tag: Option<String>,
) -> Result<Vec<(Option<String>, String)>, Response> {
    if let Some(tag) = tag {
        return Ok(vec![(image, tag)]);
    }
    // `staging_criteria` guarantees `image` is Some when `tag` is None.
    let blobs = state
        .blob_store(&repo.store)
        .await
        .map_err(|e| e.into_response())?;
    let store = DockerStore {
        repo: &repo.name,
        image: image.as_deref(),
        kv: state.kv.as_ref(),
        blobs: blobs.as_ref(),
        updater: &state.updater,
        store: &repo.store,
    };
    let tags = store.list_tags().await.map_err(|e| e.into_response())?;
    Ok(tags.into_iter().map(|t| (image.clone(), t)).collect())
}

/// `POST /service/rest/v1/staging/move/{destination}`
///
/// Moves Docker components matching the criteria from the source `repository`
/// into `{destination}` (same blob store). Metadata-only; the source tag is
/// removed once the destination copy succeeds.
pub async fn staging_move(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(destination): Path<String>,
    Query(params): Query<StagingParams>,
) -> Response {
    let source = match &params.repository {
        Some(r) => r.clone(),
        None => {
            return DepotError::BadRequest("'repository' query parameter is required".into())
                .into_response()
        }
    };

    // Authz: delete on source, write on destination.
    if let Err(e) = state
        .auth
        .check_permission(&user.0, &source, Capability::Delete)
        .await
    {
        return (StatusCode::FORBIDDEN, e.to_string()).into_response();
    }
    if let Err(e) = state
        .auth
        .check_permission(&user.0, &destination, Capability::Write)
        .await
    {
        return (StatusCode::FORBIDDEN, e.to_string()).into_response();
    }

    let source_cfg = match load_docker_repo(&state, &source).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let dest_cfg = match load_docker_repo(&state, &destination).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if dest_cfg.repo_type() != RepoType::Hosted {
        return DepotError::BadRequest(format!(
            "destination '{destination}' must be a hosted repository"
        ))
        .into_response();
    }
    if source_cfg.store != dest_cfg.store {
        return DepotError::BadRequest(format!(
            "staging move requires the same blob store: '{source}' uses '{}' but \
             '{destination}' uses '{}'. Use the cross-store copy endpoint instead.",
            source_cfg.store, dest_cfg.store
        ))
        .into_response();
    }

    let (image, tag) = match staging_criteria(&params) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let targets = match enumerate_targets(&state, &source_cfg, image, tag).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    let blobs = match state.blob_store(&source_cfg.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let overwrite = params.overwrite.unwrap_or(false);

    // Same-store move: source and destination share the blob store.
    let target = depot_format_docker::CopyTarget {
        kv: state.kv.as_ref(),
        updater: &state.updater,
        source_repo: &source,
        source_store: &source_cfg.store,
        source_blobs: blobs.as_ref(),
        dest_repo: &destination,
        dest_store: &source_cfg.store,
        dest_blobs: blobs.as_ref(),
    };

    let mut components = Vec::new();
    for (image, tag) in targets {
        let name = image.clone().unwrap_or_else(|| source.clone());
        match depot_format_docker::copy_tag(&target, image.as_deref(), &tag, true, overwrite).await
        {
            Ok(_) => components.push(StagingComponent {
                repository: destination.clone(),
                format: "docker".into(),
                name,
                version: tag,
            }),
            Err(e) => return e.into_response(),
        }
    }

    Json(StagingResponse { components }).into_response()
}

/// `POST /service/rest/v1/staging/copy/{destination}`
///
/// Depot extension to the staging family: copies Docker components matching the
/// criteria from the source `repository` into `{destination}`, which may live
/// in a **different** blob store (the bytes are streamed across). With
/// `deleteSource=true` the source tag is removed after a successful copy,
/// giving a cross-store move.
pub async fn staging_copy(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(destination): Path<String>,
    Query(params): Query<StagingParams>,
) -> Response {
    let source = match &params.repository {
        Some(r) => r.clone(),
        None => {
            return DepotError::BadRequest("'repository' query parameter is required".into())
                .into_response()
        }
    };
    let delete_source = params.delete_source.unwrap_or(false);

    // Authz: read on source (delete too if removing it), write on destination.
    let source_cap = if delete_source {
        Capability::Delete
    } else {
        Capability::Read
    };
    if let Err(e) = state
        .auth
        .check_permission(&user.0, &source, source_cap)
        .await
    {
        return (StatusCode::FORBIDDEN, e.to_string()).into_response();
    }
    if let Err(e) = state
        .auth
        .check_permission(&user.0, &destination, Capability::Write)
        .await
    {
        return (StatusCode::FORBIDDEN, e.to_string()).into_response();
    }

    let source_cfg = match load_docker_repo(&state, &source).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let dest_cfg = match load_docker_repo(&state, &destination).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if dest_cfg.repo_type() != RepoType::Hosted {
        return DepotError::BadRequest(format!(
            "destination '{destination}' must be a hosted repository"
        ))
        .into_response();
    }

    let (image, tag) = match staging_criteria(&params) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let targets = match enumerate_targets(&state, &source_cfg, image, tag).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    let source_blobs = match state.blob_store(&source_cfg.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let dest_blobs = match state.blob_store(&dest_cfg.store).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let overwrite = params.overwrite.unwrap_or(false);

    let target = depot_format_docker::CopyTarget {
        kv: state.kv.as_ref(),
        updater: &state.updater,
        source_repo: &source,
        source_store: &source_cfg.store,
        source_blobs: source_blobs.as_ref(),
        dest_repo: &destination,
        dest_store: &dest_cfg.store,
        dest_blobs: dest_blobs.as_ref(),
    };

    let mut components = Vec::new();
    for (image, tag) in targets {
        let name = image.clone().unwrap_or_else(|| source.clone());
        match depot_format_docker::copy_tag(
            &target,
            image.as_deref(),
            &tag,
            delete_source,
            overwrite,
        )
        .await
        {
            Ok(_) => components.push(StagingComponent {
                repository: destination.clone(),
                format: "docker".into(),
                name,
                version: tag,
            }),
            Err(e) => return e.into_response(),
        }
    }

    Json(StagingResponse { components }).into_response()
}

/// `POST /service/rest/v1/staging/delete`
///
/// Deletes Docker components matching the criteria from the source
/// `repository`. The tag is removed; orphaned manifests/blobs are reclaimed by
/// the Docker GC pass.
pub async fn staging_delete(
    State(state): State<FormatState>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<StagingParams>,
) -> Response {
    let source = match &params.repository {
        Some(r) => r.clone(),
        None => {
            return DepotError::BadRequest("'repository' query parameter is required".into())
                .into_response()
        }
    };

    if let Err(e) = state
        .auth
        .check_permission(&user.0, &source, Capability::Delete)
        .await
    {
        return (StatusCode::FORBIDDEN, e.to_string()).into_response();
    }

    let source_cfg = match load_docker_repo(&state, &source).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let (image, tag) = match staging_criteria(&params) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let targets = match enumerate_targets(&state, &source_cfg, image, tag).await {
        Ok(t) => t,
        Err(r) => return r,
    };

    let mut components = Vec::new();
    for (image, tag) in targets {
        let name = image.clone().unwrap_or_else(|| source.clone());
        match depot_format_docker::delete_tag(
            state.kv.as_ref(),
            &state.updater,
            &source,
            image.as_deref(),
            &tag,
        )
        .await
        {
            Ok(true) => components.push(StagingComponent {
                repository: source.clone(),
                format: "docker".into(),
                name,
                version: tag,
            }),
            Ok(false) => {}
            Err(e) => return e.into_response(),
        }
    }

    Json(StagingResponse { components }).into_response()
}
