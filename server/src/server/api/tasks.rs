// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::server::infra::task::{
    GcResult, RebuildDirEntriesResult, TaskId, TaskInfo, TaskKind, TaskResult,
};
use crate::server::worker::blob_reaper;
use crate::server::worker::check;
use crate::server::worker::update as update_worker;
use crate::server::AppState;
use axum::Extension;
use depot_core::auth::AuthenticatedUser;
use depot_core::error::DepotError;
use depot_core::service;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct StartCheckRequest {
    /// When true (the default), only report findings without fixing them.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// When true (the default), read and hash-verify every blob.
    /// When false, only check that blobs exist and have the correct size.
    #[serde(default = "default_true")]
    pub verify_blob_hashes: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct StartGcRequest {
    /// When true, only report what would be deleted without actually deleting.
    #[serde(default)]
    pub dry_run: bool,
}

/// Start a check task.
#[utoipa::path(
    post,
    path = "/api/v1/tasks/check",
    request_body(content = Option<StartCheckRequest>, content_type = "application/json"),
    responses(
        (status = 201, description = "Check task created", body = TaskInfo),
        (status = 409, description = "Check already running"),
    ),
    tag = "tasks"
)]
pub async fn start_check(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    body: Option<Json<StartCheckRequest>>,
) -> Result<impl IntoResponse, DepotError> {
    state.auth.backend.require_admin(&user.0).await?;
    let dry_run = body.as_ref().map(|b| b.dry_run).unwrap_or(true);
    let verify_blob_hashes = body.as_ref().map(|b| b.verify_blob_hashes).unwrap_or(true);

    if state.bg.tasks.has_running_check().await {
        return Err(DepotError::Conflict(
            "A check task is already running".to_string(),
        ));
    }

    let (task_id, progress_tx, cancel) = state.bg.tasks.create(TaskKind::Check).await;
    state.bg.tasks.mark_running(task_id).await;
    state.bg.scan_trigger.notify_one();

    let task_manager = state.bg.tasks.clone();
    let kv = state.repo.kv.clone();
    let stores = state.repo.stores.clone();
    let updater = state.repo.updater.clone();
    let instance_id = state.bg.instance_id.clone();

    tokio::spawn(async move {
        check::run_check(
            task_id,
            task_manager,
            kv,
            stores,
            instance_id,
            progress_tx,
            cancel,
            dry_run,
            verify_blob_hashes,
            updater,
        )
        .await;
    });

    let info = state.bg.tasks.get(task_id).await;
    Ok((StatusCode::CREATED, Json(info)))
}

/// Start a blob GC task.
#[utoipa::path(
    post,
    path = "/api/v1/tasks/gc",
    responses(
        (status = 201, description = "GC task created", body = TaskInfo),
        (status = 409, description = "GC already running"),
    ),
    tag = "tasks"
)]
pub async fn start_gc(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    body: Option<Json<StartGcRequest>>,
) -> Result<impl IntoResponse, DepotError> {
    state.auth.backend.require_admin(&user.0).await?;
    let dry_run = body.as_ref().map(|b| b.dry_run).unwrap_or(false);

    if state.bg.tasks.has_running_gc().await {
        return Err(DepotError::Conflict(
            "A GC task is already running".to_string(),
        ));
    }

    // Enforce minimum interval between GC runs.
    let gc_min_interval = state.settings.load().gc_min_interval_secs.unwrap_or(3600);
    if let Ok(Some(last)) = service::get_gc_last_started_at(state.repo.kv.as_ref()).await {
        let elapsed = (chrono::Utc::now() - last).num_seconds().max(0) as u64;
        if elapsed < gc_min_interval {
            let remaining = gc_min_interval - elapsed;
            return Err(DepotError::Conflict(format!(
                "GC minimum interval not elapsed ({remaining}s remaining)"
            )));
        }
    }

    let (task_id, progress_tx, cancel) = state.bg.tasks.create(TaskKind::BlobGc).await;
    state.bg.tasks.mark_running(task_id).await;
    state.bg.scan_trigger.notify_one();

    let task_manager = state.bg.tasks.clone();
    let kv = state.repo.kv.clone();
    let stores = state.repo.stores.clone();
    let settings = state.settings.clone();
    let updater = state.repo.updater.clone();
    let gc_state = state.bg.gc_state.clone();

    tokio::spawn(async move {
        // Persist start time so other instances / the scheduler respect the cooldown.
        let _ = service::set_gc_last_started_at(kv.as_ref(), chrono::Utc::now()).await;

        let mode_label = if dry_run { "dry-run" } else { "live" };
        task_manager
            .append_log(task_id, format!("Starting ad-hoc GC pass ({})", mode_label))
            .await;

        let mut gs = gc_state.lock().await;
        let start = std::time::Instant::now();

        let max_orphan_candidates = settings
            .load()
            .gc_max_orphan_candidates
            .unwrap_or(1_000_000);

        match blob_reaper::gc_pass(
            kv.clone(),
            &stores,
            &mut gs,
            Some(&cancel),
            max_orphan_candidates,
            Some(&progress_tx),
            dry_run,
            &updater,
        )
        .await
        {
            Ok(stats) => {
                let elapsed = start.elapsed();
                let gc_result = GcResult {
                    drained_repos: stats.drained_repos,
                    cleaned_upload_sessions: stats.cleaned_upload_sessions,
                    scanned_artifacts: stats.scanned_artifacts,
                    expired_artifacts: stats.expired_artifacts,
                    docker_deleted: stats.docker_deleted,
                    scanned_blobs: stats.scanned_blobs,
                    deleted_dedup_refs: stats.deleted_dedup_refs,
                    orphaned_blobs_deleted: stats.orphaned_blobs_deleted,
                    orphaned_blob_bytes_deleted: stats.orphaned_blob_bytes_deleted,
                    elapsed_ns: elapsed.as_nanos() as u64,
                    dry_run,
                };
                task_manager
                    .update_summary(
                        task_id,
                        format!(
                            "GC: {} artifacts scanned, {} dedup refs deleted, {} orphan blobs removed",
                            stats.scanned_artifacts,
                            stats.deleted_dedup_refs,
                            stats.orphaned_blobs_deleted,
                        ),
                    )
                    .await;
                task_manager
                    .mark_completed(task_id, TaskResult::BlobGc(gc_result))
                    .await;
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    task_manager.mark_cancelled(task_id).await;
                } else {
                    task_manager.mark_failed(task_id, format!("{e}")).await;
                }
            }
        }
    });

    let info = state.bg.tasks.get(task_id).await;
    Ok((StatusCode::CREATED, Json(info)))
}

/// Start a rebuild-dir-entries task.
#[utoipa::path(
    post,
    path = "/api/v1/tasks/rebuild-dir-entries",
    responses(
        (status = 201, description = "Rebuild task created", body = TaskInfo),
        (status = 409, description = "Rebuild already running"),
    ),
    tag = "tasks"
)]
pub async fn start_rebuild_dir_entries(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<impl IntoResponse, DepotError> {
    state.auth.backend.require_admin(&user.0).await?;
    if state.bg.tasks.has_running_rebuild_dir_entries().await {
        return Err(DepotError::Conflict(
            "A rebuild-dir-entries task is already running".to_string(),
        ));
    }

    let (task_id, progress_tx, cancel) = state.bg.tasks.create(TaskKind::RebuildDirEntries).await;
    state.bg.tasks.mark_running(task_id).await;
    state.bg.scan_trigger.notify_one();

    let task_manager = state.bg.tasks.clone();
    let kv = state.repo.kv.clone();

    tokio::spawn(async move {
        task_manager
            .append_log(task_id, "Starting full dir-entry rebuild".to_string())
            .await;

        let start = std::time::Instant::now();
        match update_worker::full_scan(kv.as_ref(), &task_manager, task_id, &progress_tx).await {
            Ok((repos, entries_updated, stores_updated)) => {
                let elapsed = start.elapsed();
                let result = RebuildDirEntriesResult {
                    repos,
                    entries_updated,
                    stores_updated,
                    elapsed_ns: elapsed.as_nanos() as u64,
                };
                task_manager
                    .update_summary(
                        task_id,
                        format!(
                            "Rebuilt dir entries: {} repos, {} entries updated, {} stores updated",
                            repos, entries_updated, stores_updated,
                        ),
                    )
                    .await;
                task_manager
                    .mark_completed(task_id, TaskResult::RebuildDirEntries(result))
                    .await;
            }
            Err(e) => {
                if cancel.is_cancelled() {
                    task_manager.mark_cancelled(task_id).await;
                } else {
                    task_manager.mark_failed(task_id, format!("{e}")).await;
                }
            }
        }
    });

    let info = state.bg.tasks.get(task_id).await;
    Ok((StatusCode::CREATED, Json(info)))
}

/// List all tasks.
#[utoipa::path(
    get,
    path = "/api/v1/tasks",
    responses(
        (status = 200, description = "List of tasks", body = Vec<TaskInfo>),
    ),
    tag = "tasks"
)]
pub async fn list_tasks(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<Vec<TaskInfo>>, DepotError> {
    state.auth.backend.require_admin(&user.0).await?;
    Ok(Json(state.bg.tasks.list().await))
}

/// Get a single task by ID.
#[utoipa::path(
    get,
    path = "/api/v1/tasks/{id}",
    responses(
        (status = 200, description = "Task details", body = TaskInfo),
        (status = 404, description = "Task not found"),
    ),
    tag = "tasks"
)]
pub async fn get_task(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<Json<TaskInfo>, DepotError> {
    state.auth.backend.require_admin(&user.0).await?;
    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|_| DepotError::BadRequest("Invalid task ID".to_string()))?;
    let task_id = TaskId(uuid);
    match state.bg.tasks.get(task_id).await {
        Some(info) => Ok(Json(info)),
        None => Err(DepotError::NotFound("Task not found".to_string())),
    }
}

/// Delete or cancel a task.
#[utoipa::path(
    delete,
    path = "/api/v1/tasks/{id}",
    responses(
        (status = 204, description = "Task deleted"),
        (status = 404, description = "Task not found"),
    ),
    tag = "tasks"
)]
pub async fn delete_task(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, DepotError> {
    state.auth.backend.require_admin(&user.0).await?;
    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|_| DepotError::BadRequest("Invalid task ID".to_string()))?;
    let task_id = TaskId(uuid);
    if state.bg.tasks.delete(task_id).await {
        state.bg.scan_trigger.notify_one();
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(DepotError::NotFound("Task not found".to_string()))
    }
}

/// Scheduler status for the Tasks UI.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SchedulerStatus {
    /// When the last GC pass started (ISO 8601), or null if never.
    pub gc_last_started_at: Option<String>,
    /// Configured GC scheduling interval in seconds.
    pub gc_interval_secs: u64,
    /// Configured minimum seconds between GC runs.
    pub gc_min_interval_secs: u64,
    /// Fixed daily GC start time (UTC `HH:MM`), or null for interval mode.
    pub gc_start_time: Option<String>,
}

/// Get scheduler status (GC timing information).
#[utoipa::path(
    get,
    path = "/api/v1/tasks/scheduler",
    responses(
        (status = 200, description = "Scheduler status", body = SchedulerStatus),
    ),
    tag = "tasks"
)]
pub async fn get_scheduler_status(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Result<Json<SchedulerStatus>, DepotError> {
    state.auth.backend.require_admin(&user.0).await?;
    let s = state.settings.load();
    let gc_interval_secs = s.gc_interval_secs.unwrap_or(86400);
    let gc_min_interval_secs = s.gc_min_interval_secs.unwrap_or(3600);
    let gc_start_time = s.gc_start_time.clone();
    drop(s);

    let gc_last_started_at = service::get_gc_last_started_at(state.repo.kv.as_ref())
        .await
        .ok()
        .flatten()
        .map(|t| t.to_rfc3339());

    Ok(Json(SchedulerStatus {
        gc_last_started_at,
        gc_interval_secs,
        gc_min_interval_secs,
        gc_start_time,
    }))
}
