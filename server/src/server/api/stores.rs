// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Stores CRUD API — manage named blob stores.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use crate::server::api::conversions::{build_store_kind, normalize_s3_prefix};
use crate::server::infra::store_registry::instantiate_store;
use crate::server::AppState;
use depot_core::auth::AuthenticatedUser;
use depot_core::error::DepotError;
use depot_core::service;
use depot_core::store::kv::{StoreKind, StoreRecord, StoreStatsRecord, CURRENT_RECORD_VERSION};

#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct CreateStoreRequest {
    pub name: String,
    pub store_type: String,
    // File fields
    pub root: Option<String>,
    #[serde(default = "default_true")]
    pub sync: bool,
    /// Read/write buffer size in KiB (default 1024 = 1 MiB).
    #[serde(default = "default_io_size")]
    pub io_size: usize,
    /// Use O_DIRECT for reads and writes (bypasses page cache).
    #[serde(default)]
    pub direct_io: bool,
    // S3 fields
    pub bucket: Option<String>,
    pub endpoint: Option<String>,
    #[serde(default = "default_region")]
    pub region: String,
    pub prefix: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    /// Maximum number of retry attempts for failed S3 requests (default 3).
    #[serde(default = "default_s3_max_retries")]
    pub max_retries: u32,
    /// TCP connection timeout in seconds (default 3).
    #[serde(default = "default_s3_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// Per-request read timeout in seconds (default 30).
    #[serde(default = "default_s3_read_timeout_secs")]
    pub read_timeout_secs: u64,
    /// Retry strategy: "standard" or "adaptive" (default "standard").
    #[serde(default = "default_s3_retry_mode")]
    pub retry_mode: String,
}

fn default_true() -> bool {
    true
}

fn default_io_size() -> usize {
    1024
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_s3_max_retries() -> u32 {
    3
}

fn default_s3_connect_timeout_secs() -> u64 {
    3
}

fn default_s3_read_timeout_secs() -> u64 {
    0
}

fn default_s3_retry_mode() -> String {
    "standard".to_string()
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct StoreResponse {
    pub name: String,
    pub store_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<bool>,
    /// Read/write buffer size in KiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_io: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_mode: Option<String>,
    #[schema(value_type = String, format = "date-time")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Total number of blobs on this store (populated by GC).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob_count: Option<u64>,
    /// Total bytes across all blobs on this store (populated by GC).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

impl From<StoreRecord> for StoreResponse {
    fn from(r: StoreRecord) -> Self {
        match r.kind {
            StoreKind::File {
                root,
                sync,
                io_size,
                direct_io,
            } => StoreResponse {
                name: r.name,
                store_type: "file".to_string(),
                root: Some(root),
                sync: Some(sync),
                io_size: Some(io_size),
                direct_io: Some(direct_io),
                bucket: None,
                endpoint: None,
                region: None,
                prefix: None,
                access_key: None,
                max_retries: None,
                connect_timeout_secs: None,
                read_timeout_secs: None,
                retry_mode: None,
                created_at: r.created_at,
                blob_count: None,
                total_bytes: None,
            },
            StoreKind::S3 {
                bucket,
                endpoint,
                region,
                prefix,
                access_key,
                max_retries,
                connect_timeout_secs,
                read_timeout_secs,
                retry_mode,
                ..
            } => StoreResponse {
                name: r.name,
                store_type: "s3".to_string(),
                root: None,
                sync: None,
                io_size: None,
                direct_io: None,
                bucket: Some(bucket),
                endpoint,
                region: Some(region),
                prefix,
                access_key,
                max_retries: Some(max_retries),
                connect_timeout_secs: Some(connect_timeout_secs),
                read_timeout_secs: Some(read_timeout_secs),
                retry_mode: Some(retry_mode),
                created_at: r.created_at,
                blob_count: None,
                total_bytes: None,
            },
        }
    }
}

/// List all stores.
#[utoipa::path(
    get,
    path = "/api/v1/stores",
    responses(
        (status = 200, description = "List of stores", body = Vec<StoreResponse>)
    ),
    tag = "stores"
)]
pub async fn list_stores(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }
    match service::list_stores(state.repo.kv.as_ref()).await {
        Ok(stores) => {
            let mut resp: Vec<StoreResponse> =
                stores.into_iter().map(StoreResponse::from).collect();
            // Attach stats to each store (best-effort).
            for r in &mut resp {
                if let Ok(Some(stats)) =
                    service::get_store_stats(state.repo.kv.as_ref(), &r.name).await
                {
                    r.blob_count = Some(stats.blob_count);
                    r.total_bytes = Some(stats.total_bytes);
                }
            }
            Json(resp).into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// Get a store by name.
#[utoipa::path(
    get,
    path = "/api/v1/stores/{name}",
    params(("name" = String, Path, description = "Store name")),
    responses(
        (status = 200, description = "Store details", body = StoreResponse),
        (status = 404, description = "Store not found")
    ),
    tag = "stores"
)]
pub async fn get_store(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }
    let name_owned = name.clone();
    match service::get_store(state.repo.kv.as_ref(), &name_owned).await {
        Ok(Some(store)) => {
            let mut resp = StoreResponse::from(store);
            if let Ok(Some(stats)) = service::get_store_stats(state.repo.kv.as_ref(), &name).await {
                resp.blob_count = Some(stats.blob_count);
                resp.total_bytes = Some(stats.total_bytes);
            }
            Json(resp).into_response()
        }
        Ok(None) => DepotError::NotFound(format!("store '{}' not found", name)).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Create a new store.
#[utoipa::path(
    post,
    path = "/api/v1/stores",
    request_body = CreateStoreRequest,
    responses(
        (status = 201, description = "Store created", body = StoreResponse),
        (status = 400, description = "Invalid store configuration"),
        (status = 409, description = "Store already exists")
    ),
    tag = "stores"
)]
pub async fn create_store(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(req): Json<CreateStoreRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }

    if req.name.is_empty() {
        return DepotError::BadRequest("name is required".into()).into_response();
    }

    let kind = match build_store_kind(&req) {
        Ok(k) => k,
        Err(e) => return e.into_response(),
    };

    // Reject duplicate names before doing any filesystem / network work.
    // Reordering this above `instantiate_store` lets idempotent POSTs (e.g.
    // a seeding script that always tries to create "default") return a
    // clean 409 even when the request body's `root` happens to be
    // unwritable from the server process -- the previous order would
    // surface a misleading 400 EACCES instead of the real conflict.
    if service::get_store(state.repo.kv.as_ref(), &req.name)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return DepotError::Conflict("store already exists".into()).into_response();
    }

    // Verify connectivity by instantiating the store.
    let blob_store = match instantiate_store(&kind).await {
        Ok(s) => s,
        Err(e) => {
            return DepotError::BadRequest(format!("failed to instantiate store: {}", e))
                .into_response();
        }
    };

    // Verify the store actually works end-to-end (put, get, list, delete).
    if let Err(e) = blob_store.health_check().await {
        return DepotError::BadRequest(format!("store health check failed: {e}")).into_response();
    }

    let now = crate::server::repo::now_utc();
    let record = StoreRecord {
        schema_version: CURRENT_RECORD_VERSION,
        name: req.name.clone(),
        kind,
        created_at: now,
    };

    if service::get_store(state.repo.kv.as_ref(), &req.name)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return DepotError::Conflict("store already exists".into()).into_response();
    }
    match service::put_store(state.repo.kv.as_ref(), &record).await {
        Ok(()) => {
            // Initialise stats to zero.
            let stats = StoreStatsRecord {
                blob_count: 0,
                total_bytes: 0,
                updated_at: now,
            };
            let _ = service::put_store_stats(state.repo.kv.as_ref(), &req.name, &stats).await;

            state.repo.stores.add(&req.name, blob_store).await;
            let mut resp = StoreResponse::from(record);
            resp.blob_count = Some(0);
            resp.total_bytes = Some(0);
            state.bg.event_bus.publish(
                crate::server::infra::event_bus::Topic::Stores,
                crate::server::infra::event_bus::ModelEvent::StoreCreated {
                    store: resp.clone(),
                },
            );
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => e.into_response(),
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateStoreRequest {
    // File fields
    pub root: Option<String>,
    pub sync: Option<bool>,
    /// Read/write buffer size in KiB.
    pub io_size: Option<usize>,
    /// Use O_DIRECT for reads and writes.
    pub direct_io: Option<bool>,
    // S3 fields
    pub bucket: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub prefix: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub max_retries: Option<u32>,
    pub connect_timeout_secs: Option<u64>,
    pub read_timeout_secs: Option<u64>,
    pub retry_mode: Option<String>,
}

/// Update a store's mutable settings.
#[utoipa::path(
    put,
    path = "/api/v1/stores/{name}",
    params(("name" = String, Path, description = "Store name")),
    request_body = UpdateStoreRequest,
    responses(
        (status = 200, description = "Store updated", body = StoreResponse),
        (status = 400, description = "Invalid store configuration"),
        (status = 404, description = "Store not found")
    ),
    tag = "stores"
)]
pub async fn update_store(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
    Json(req): Json<UpdateStoreRequest>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }

    let mut record = match service::get_store(state.repo.kv.as_ref(), &name).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return DepotError::NotFound(format!("store '{}' not found", name)).into_response();
        }
        Err(e) => return e.into_response(),
    };

    if let Some(new_io_size) = req.io_size {
        if !(4..=65536).contains(&new_io_size) {
            return DepotError::BadRequest("io_size must be between 4 and 65536 KiB".into())
                .into_response();
        }
    }

    record.kind = match record.kind {
        StoreKind::File {
            root,
            sync,
            io_size,
            direct_io,
        } => StoreKind::File {
            root: req.root.unwrap_or(root),
            sync: req.sync.unwrap_or(sync),
            io_size: req.io_size.unwrap_or(io_size),
            direct_io: req.direct_io.unwrap_or(direct_io),
        },
        StoreKind::S3 {
            bucket,
            endpoint,
            region,
            prefix,
            access_key,
            secret_key,
            max_retries,
            connect_timeout_secs,
            read_timeout_secs,
            retry_mode,
        } => {
            // Preserve existing secret_key if the sentinel "********" is sent back.
            let new_secret = match req.secret_key {
                Some(ref s) if s == "********" => secret_key,
                Some(s) => Some(s),
                None => secret_key,
            };
            let new_max_retries = req.max_retries.unwrap_or(max_retries);
            let new_connect_timeout = req.connect_timeout_secs.unwrap_or(connect_timeout_secs);
            let new_read_timeout = req.read_timeout_secs.unwrap_or(read_timeout_secs);
            let new_retry_mode = req.retry_mode.unwrap_or(retry_mode);
            if new_max_retries > 20 {
                return DepotError::BadRequest(format!(
                    "max_retries: {} must be in [0, 20]",
                    new_max_retries
                ))
                .into_response();
            }
            if new_connect_timeout == 0 || new_connect_timeout > 300 {
                return DepotError::BadRequest(format!(
                    "connect_timeout_secs: {} must be in [1, 7200]",
                    new_connect_timeout
                ))
                .into_response();
            }
            if new_read_timeout > 300 {
                return DepotError::BadRequest(format!(
                    "read_timeout_secs: {} must be in [0, 300] (0 = disabled)",
                    new_read_timeout
                ))
                .into_response();
            }
            if new_retry_mode != "standard" && new_retry_mode != "adaptive" {
                return DepotError::BadRequest(format!(
                    "retry_mode: {:?} must be \"standard\" or \"adaptive\"",
                    new_retry_mode
                ))
                .into_response();
            }
            StoreKind::S3 {
                bucket: req.bucket.unwrap_or(bucket),
                endpoint: if req.endpoint.is_some() {
                    req.endpoint
                } else {
                    endpoint
                },
                region: req.region.unwrap_or(region),
                prefix: if req.prefix.is_some() {
                    normalize_s3_prefix(req.prefix)
                } else {
                    prefix
                },
                access_key: if req.access_key.is_some() {
                    req.access_key
                } else {
                    access_key
                },
                secret_key: new_secret,
                max_retries: new_max_retries,
                connect_timeout_secs: new_connect_timeout,
                read_timeout_secs: new_read_timeout,
                retry_mode: new_retry_mode,
            }
        }
    };

    // Verify connectivity with updated config.
    let blob_store = match instantiate_store(&record.kind).await {
        Ok(s) => s,
        Err(e) => {
            return DepotError::BadRequest(format!("failed to instantiate store: {}", e))
                .into_response();
        }
    };

    match service::put_store(state.repo.kv.as_ref(), &record).await {
        Ok(()) => {
            state.repo.stores.add(&name, blob_store).await;
            let mut resp = StoreResponse::from(record);
            if let Ok(Some(stats)) = service::get_store_stats(state.repo.kv.as_ref(), &name).await {
                resp.blob_count = Some(stats.blob_count);
                resp.total_bytes = Some(stats.total_bytes);
            }
            state.bg.event_bus.publish(
                crate::server::infra::event_bus::Topic::Stores,
                crate::server::infra::event_bus::ModelEvent::StoreUpdated {
                    store: resp.clone(),
                },
            );
            Json(resp).into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// `BrowseEntry` lives in `depot_core::store::blob` — each BlobStore backend
/// implements `browse_prefix` to produce them.
pub use depot_core::store::blob::BrowseEntry;

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = StoreBrowseResponse)]
pub struct BrowseResponse {
    pub path: String,
    pub entries: Vec<BrowseEntry>,
}

#[derive(Debug, Deserialize)]
pub struct BrowseQuery {
    /// Directory path to list (e.g. "ab/cd/"). Empty or absent means root.
    #[serde(default)]
    pub path: String,
}

/// Browse blob store directory structure.
#[utoipa::path(
    get,
    path = "/api/v1/stores/{name}/browse",
    params(
        ("name" = String, Path, description = "Store name"),
        ("path" = Option<String>, Query, description = "Directory path to list")
    ),
    responses(
        (status = 200, description = "Directory listing", body = BrowseResponse),
        (status = 404, description = "Store not found")
    ),
    tag = "stores"
)]
pub async fn browse_store_blobs(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
    axum::extract::Query(query): axum::extract::Query<BrowseQuery>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }

    if let Err(e) = service::get_store(state.repo.kv.as_ref(), &name).await {
        return e.into_response();
    };

    let blob_store = match state.repo.stores.get(&name).await {
        Some(s) => s,
        None => {
            return DepotError::NotFound(format!("store '{}' not loaded", name)).into_response()
        }
    };

    // Normalise: ensure path ends with / if non-empty, and never starts with /
    let path = query.path.trim_start_matches('/').to_string();
    let path = if path.is_empty() || path.ends_with('/') {
        path
    } else {
        format!("{path}/")
    };

    let entries = match blob_store.browse_prefix(&path).await {
        Ok(e) => e,
        Err(e) => return e.into_response(),
    };

    Json(BrowseResponse {
        path: path.clone(),
        entries,
    })
    .into_response()
}

/// Delete a store (must be empty — no repos and no blobs).
#[utoipa::path(
    delete,
    path = "/api/v1/stores/{name}",
    params(("name" = String, Path, description = "Store name")),
    responses(
        (status = 204, description = "Store deleted"),
        (status = 404, description = "Store not found"),
        (status = 409, description = "Store still in use or has blobs")
    ),
    tag = "stores"
)]
pub async fn delete_store(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }

    // Check store exists.
    if service::get_store(state.repo.kv.as_ref(), &name)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return DepotError::NotFound(format!("store '{}' not found", name)).into_response();
    }
    // Check no repos use this store.
    match service::list_repos(state.repo.kv.as_ref()).await {
        Ok(repos) => {
            for repo in &repos {
                if repo.store == name {
                    return DepotError::Conflict(format!(
                        "store '{}' is in use by repository '{}'",
                        name, repo.name
                    ))
                    .into_response();
                }
            }
        }
        Err(e) => return e.into_response(),
    }
    // Check no blobs remain.
    match service::count_blobs_for_store(state.repo.kv.as_ref(), &name).await {
        Ok(blob_count) if blob_count > 0 => {
            return DepotError::Conflict(format!(
                "store '{}' still has {} blob(s)",
                name, blob_count
            ))
            .into_response();
        }
        Err(e) => return e.into_response(),
        _ => {}
    }
    match service::delete_store(state.repo.kv.as_ref(), &name).await {
        Ok(_) => {
            let _ = service::delete_store_stats(state.repo.kv.as_ref(), &name).await;
            state.repo.stores.remove(&name).await;
            state.bg.event_bus.publish(
                crate::server::infra::event_bus::Topic::Stores,
                crate::server::infra::event_bus::ModelEvent::StoreDeleted { name: name.clone() },
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => e.into_response(),
    }
}

// Health checking is a backend-implemented trait method now:
// each BlobStore impl does its own put/get/list/delete round trip via
// `BlobStore::health_check` in core/src/store/blob.rs.

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct StoreCheckResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Run a health check on an existing blob store.
#[utoipa::path(
    post,
    path = "/api/v1/stores/{name}/check",
    params(("name" = String, Path, description = "Store name")),
    responses(
        (status = 200, description = "Health check result", body = StoreCheckResponse),
        (status = 404, description = "Store not found")
    ),
    tag = "stores"
)]
pub async fn check_store(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }

    match service::get_store(state.repo.kv.as_ref(), &name).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return DepotError::NotFound(format!("store '{}' not found", name)).into_response()
        }
        Err(e) => return e.into_response(),
    };

    let blob_store = match state.repo.stores.get(&name).await {
        Some(s) => s,
        None => {
            return DepotError::NotFound(format!("store '{}' not loaded", name)).into_response()
        }
    };

    match blob_store.health_check().await {
        Ok(()) => Json(StoreCheckResponse {
            ok: true,
            error: None,
        })
        .into_response(),
        Err(e) => Json(StoreCheckResponse {
            ok: false,
            error: Some(e.to_string()),
        })
        .into_response(),
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ReconcileEntry {
    pub store: String,
    pub blob_count: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ReconcileResponse {
    pub stores: Vec<ReconcileEntry>,
}

/// Recompute every store's `blob_count`/`total_bytes` from the authoritative
/// blob records and persist the corrected values. Use after bulk operations
/// (e.g. a reorg) or whenever the reported sizes look off; the GC pass also runs
/// this automatically. Admin only.
#[utoipa::path(
    post,
    path = "/api/v1/stores/reconcile",
    responses(
        (status = 200, description = "Reconciled per-store stats", body = ReconcileResponse)
    ),
    tag = "stores"
)]
pub async fn reconcile_stores(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> impl IntoResponse {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }
    match service::reconcile_store_stats(state.repo.kv.as_ref()).await {
        Ok(stats) => Json(ReconcileResponse {
            stores: stats
                .into_iter()
                .map(|(store, s)| ReconcileEntry {
                    store,
                    blob_count: s.blob_count,
                    total_bytes: s.total_bytes,
                })
                .collect(),
        })
        .into_response(),
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_s3_prefix() {
        assert_eq!(normalize_s3_prefix(None), None);
        assert_eq!(normalize_s3_prefix(Some("".into())), None);
        assert_eq!(normalize_s3_prefix(Some("/".into())), None);
        assert_eq!(normalize_s3_prefix(Some("//".into())), None);
        assert_eq!(
            normalize_s3_prefix(Some("depot".into())),
            Some("depot/".into())
        );
        assert_eq!(
            normalize_s3_prefix(Some("depot/".into())),
            Some("depot/".into())
        );
        assert_eq!(
            normalize_s3_prefix(Some("/depot/".into())),
            Some("depot/".into())
        );
        assert_eq!(
            normalize_s3_prefix(Some("/depot".into())),
            Some("depot/".into())
        );
        assert_eq!(
            normalize_s3_prefix(Some("a/b/c".into())),
            Some("a/b/c/".into())
        );
        assert_eq!(
            normalize_s3_prefix(Some("a/b/c/".into())),
            Some("a/b/c/".into())
        );
    }
}
