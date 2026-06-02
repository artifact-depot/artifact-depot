// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Background model materializer.
//!
//! Maintains an in-memory `MaterializedModel` by subscribing to the event bus
//! and applying domain events incrementally. Readers (SSE endpoints) load the
//! model via `ArcSwap` in O(1) with no KV queries.

use std::sync::Arc;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use metrics::gauge;

use crate::server::api::repositories::RepoResponse;
use crate::server::api::roles::RoleResponse;
use crate::server::api::stores::StoreResponse;
use crate::server::api::system::HealthResponse;
use crate::server::api::users::UserResponse;
use crate::server::config::settings::SettingsHandle;
use crate::server::infra::event_bus::{
    AdminModel, Envelope, EventBus, MaterializedModel, ModelEvent, ModelHandle, PublicModel,
};
use crate::server::infra::task::TaskManager;
use depot_core::service;
use depot_core::store::kv::KvStore;

/// Maximum events to coalesce before swapping a new model version.
const COALESCE_BATCH_LIMIT: usize = 64;

/// Maximum time to wait for additional events after the first arrives.
const COALESCE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(50);

pub async fn run_model_materializer(
    model: Arc<ModelHandle>,
    event_bus: Arc<EventBus>,
    kv: Arc<dyn KvStore>,
    tasks: Arc<TaskManager>,
    settings: Arc<SettingsHandle>,
    cancel: CancellationToken,
) {
    // Subscribe to broadcast BEFORE loading from KV so we don't miss events
    // published during the initial scan.
    let mut rx = event_bus.subscribe();

    // Phase 1: Initial population from KV.
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        match async { build_full_model(&kv, &tasks, &settings).await }
            .instrument(tracing::debug_span!("materializer_init"))
            .await
        {
            Ok(initial) => {
                model.store(Arc::new(initial));
                tracing::info!("model materializer: initial snapshot loaded");
                break;
            }
            Err(e) => {
                tracing::error!(error = %e, "model materializer: failed to build initial snapshot");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.cancelled() => return,
                }
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
            }
        }
    }

    // Drain any events that arrived during the initial KV scan. These may
    // duplicate state already in the model, but apply_batch handles that
    // via upsert semantics (find-or-push).
    drain_buffered(&mut rx, &model);

    // Phase 2: Steady-state event loop.
    loop {
        let mut batch: Vec<Envelope> = Vec::with_capacity(COALESCE_BATCH_LIMIT);
        let mut needs_resync = false;

        // Wait for the first event.
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(env) => batch.push(env),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "model materializer lagged, scheduling resync");
                        needs_resync = true;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = cancel.cancelled() => break,
        }

        if needs_resync {
            resync(&model, &kv, &tasks, &settings).await;
            continue;
        }

        // Drain more events without blocking, up to batch limit or deadline.
        let deadline = tokio::time::Instant::now() + COALESCE_DEADLINE;
        while batch.len() < COALESCE_BATCH_LIMIT {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(env) => batch.push(env),
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(missed = n, "model materializer lagged during batch");
                            needs_resync = true;
                            break;
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
                _ = cancel.cancelled() => return,
            }
        }

        if needs_resync {
            resync(&model, &kv, &tasks, &settings).await;
            continue;
        }

        apply_batch(&model, &batch);
    }

    tracing::info!("model materializer shutting down");
}

/// Drain all immediately available events from the broadcast receiver and
/// apply them, without blocking.
fn drain_buffered(rx: &mut broadcast::Receiver<Envelope>, model: &ModelHandle) {
    let mut batch = Vec::new();
    while let Ok(env) = rx.try_recv() {
        batch.push(env);
    }
    if !batch.is_empty() {
        apply_batch(model, &batch);
    }
}

/// Clone the current model, apply a batch of events, and swap in the result.
fn apply_batch(model: &ModelHandle, events: &[Envelope]) {
    let current = model.load();
    let mut public = (*current.public).clone();
    let mut admin = (*current.admin).clone();
    let mut modified_public = false;
    let mut modified_admin = false;

    for env in events {
        match &env.event {
            // --- Repos ---
            ModelEvent::RepoCreated { repo } => {
                if !public.repos.iter().any(|r| r.name == repo.name) {
                    public.repos.push(repo.clone());
                }
                modified_public = true;
            }
            ModelEvent::RepoUpdated { repo } => {
                if let Some(existing) = public.repos.iter_mut().find(|r| r.name == repo.name) {
                    // Preserve stats — RepoUpdated carries config changes, not stats.
                    let count = existing.artifact_count;
                    let bytes = existing.total_bytes;
                    *existing = repo.clone();
                    existing.artifact_count = count;
                    existing.total_bytes = bytes;
                }
                modified_public = true;
            }
            ModelEvent::RepoDeleted { name } => {
                public.repos.retain(|r| r.name != *name);
                modified_public = true;
            }
            ModelEvent::RepoStatsUpdated {
                name,
                artifact_count,
                total_bytes,
            } => {
                if let Some(repo) = public.repos.iter_mut().find(|r| r.name == *name) {
                    repo.artifact_count = *artifact_count;
                    repo.total_bytes = *total_bytes;
                    let labels = [
                        ("repository", repo.name.clone()),
                        ("repo_type", repo.repo_type.to_string()),
                    ];
                    gauge!("depot_repository_artifacts", &labels).set(*artifact_count as f64);
                    gauge!("depot_repository_bytes", &labels).set(*total_bytes as f64);
                }
                modified_public = true;
            }

            // --- Stores ---
            ModelEvent::StoreCreated { store } => {
                if !public.stores.iter().any(|s| s.name == store.name) {
                    public.stores.push(store.clone());
                }
                modified_public = true;
            }
            ModelEvent::StoreUpdated { store } => {
                if let Some(existing) = public.stores.iter_mut().find(|s| s.name == store.name) {
                    *existing = store.clone();
                }
                modified_public = true;
            }
            ModelEvent::StoreDeleted { name } => {
                public.stores.retain(|s| s.name != *name);
                modified_public = true;
            }
            ModelEvent::StoreStatsUpdated {
                name,
                blob_count,
                total_bytes,
            } => {
                if let Some(store) = public.stores.iter_mut().find(|s| s.name == *name) {
                    store.blob_count = Some(*blob_count);
                    store.total_bytes = Some(*total_bytes);
                }
                modified_public = true;
            }

            // --- Tasks ---
            ModelEvent::TaskCreated { task } => {
                if !public.tasks.iter().any(|t| t.id == task.id) {
                    public.tasks.push(task.clone());
                }
                modified_public = true;
            }
            ModelEvent::TaskUpdated { task } => {
                if let Some(existing) = public.tasks.iter_mut().find(|t| t.id == task.id) {
                    *existing = task.clone();
                } else {
                    public.tasks.push(task.clone());
                }
                modified_public = true;
            }
            ModelEvent::TaskDeleted { id } => {
                public.tasks.retain(|t| t.id.to_string() != *id);
                modified_public = true;
            }

            // --- Users (admin) ---
            ModelEvent::UserCreated { user } => {
                if !admin.users.iter().any(|u| u.username == user.username) {
                    admin.users.push(user.clone());
                }
                modified_admin = true;
            }
            ModelEvent::UserUpdated { user } => {
                if let Some(existing) = admin.users.iter_mut().find(|u| u.username == user.username)
                {
                    *existing = user.clone();
                }
                modified_admin = true;
            }
            ModelEvent::UserDeleted { username } => {
                admin.users.retain(|u| u.username != *username);
                modified_admin = true;
            }

            // --- Roles (admin) ---
            ModelEvent::RoleCreated { role } => {
                if !admin.roles.iter().any(|r| r.name == role.name) {
                    admin.roles.push(role.clone());
                }
                modified_admin = true;
            }
            ModelEvent::RoleUpdated { role } => {
                if let Some(existing) = admin.roles.iter_mut().find(|r| r.name == role.name) {
                    *existing = role.clone();
                }
                modified_admin = true;
            }
            ModelEvent::RoleDeleted { name } => {
                admin.roles.retain(|r| r.name != *name);
                modified_admin = true;
            }

            // --- Settings ---
            ModelEvent::SettingsUpdated { settings } => {
                public.settings = settings.clone();
                modified_public = true;
            }

            ModelEvent::Resync => {}
        }

        public.seq = env.seq;
    }

    // Only allocate new Arcs for halves that actually changed.
    let new_model = MaterializedModel {
        public: if modified_public {
            Arc::new(public)
        } else {
            Arc::clone(&current.public)
        },
        admin: if modified_admin {
            Arc::new(admin)
        } else {
            Arc::clone(&current.admin)
        },
    };
    model.store(Arc::new(new_model));
}

/// Build the full model from KV (used at startup and on resync).
async fn build_full_model(
    kv: &Arc<dyn KvStore>,
    tasks: &Arc<TaskManager>,
    settings: &Arc<SettingsHandle>,
) -> anyhow::Result<MaterializedModel> {
    let configs = service::list_repos(kv.as_ref()).await?;
    let stats_futs = configs
        .iter()
        .map(|r| service::get_repo_stats(kv.as_ref(), r));
    let all_stats = futures::future::join_all(stats_futs).await;
    let repos: Vec<RepoResponse> = configs
        .into_iter()
        .zip(all_stats)
        .map(|(c, stats)| RepoResponse::from(c).with_stats(stats.unwrap_or_default()))
        .collect();

    // Seed Prometheus gauges for repository stats.
    for r in &repos {
        let labels = [
            ("repository", r.name.clone()),
            ("repo_type", r.repo_type.to_string()),
        ];
        gauge!("depot_repository_artifacts", &labels).set(r.artifact_count as f64);
        gauge!("depot_repository_bytes", &labels).set(r.total_bytes as f64);
    }

    let store_records = service::list_stores(kv.as_ref()).await?;
    let mut stores: Vec<StoreResponse> =
        store_records.into_iter().map(StoreResponse::from).collect();
    for s in &mut stores {
        if let Ok(Some(stats)) = service::get_store_stats(kv.as_ref(), &s.name).await {
            s.blob_count = Some(stats.blob_count);
            s.total_bytes = Some(stats.total_bytes);
        }
    }

    let mut task_list = tasks.list().await;
    for t in &mut task_list {
        t.log.clear();
    }

    let user_records = service::list_users(kv.as_ref()).await?;
    let users: Vec<UserResponse> = user_records.into_iter().map(UserResponse::from).collect();

    let role_records = service::list_roles(kv.as_ref()).await?;
    let roles: Vec<RoleResponse> = role_records.into_iter().map(RoleResponse::from).collect();

    let current_settings = (**settings.load()).clone();

    let health = HealthResponse {
        status: "ok".to_string(),
        version: crate::VERSION.to_string(),
    };

    Ok(MaterializedModel {
        public: Arc::new(PublicModel {
            seq: 0,
            repos,
            stores,
            tasks: task_list,
            settings: current_settings,
            health,
        }),
        admin: Arc::new(AdminModel { users, roles }),
    })
}

async fn resync(
    model: &ModelHandle,
    kv: &Arc<dyn KvStore>,
    tasks: &Arc<TaskManager>,
    settings: &Arc<SettingsHandle>,
) {
    match build_full_model(kv, tasks, settings).await {
        Ok(fresh) => {
            model.store(Arc::new(fresh));
            tracing::info!("model materializer: resync complete");
        }
        Err(e) => {
            tracing::error!(error = %e, "model materializer: resync failed, keeping stale model");
        }
    }
}
