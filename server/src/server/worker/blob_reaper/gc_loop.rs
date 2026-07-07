// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

// ---------------------------------------------------------------------------
// GC worker loop
// ---------------------------------------------------------------------------

use std::sync::Arc;

use chrono::{DateTime, Utc};
use metrics::{counter, gauge, histogram};
use tokio_util::sync::CancellationToken;

use crate::server::config::settings::SettingsHandle;
use crate::server::infra::task::{GcResult, TaskKind, TaskManager, TaskResult};
use depot_core::service;
use depot_core::store::kv::KvStore;
use depot_core::store_registry::StoreRegistry;
use depot_core::update::UpdateSender;

use super::gc_pass::{gc_pass, GcState};
use super::{DEFAULT_GC_INTERVAL_SECS, DEFAULT_GC_MIN_INTERVAL_SECS};

/// Decide whether an automatic GC pass is due.
///
/// Interval mode (`start_time` unset): due once `gc_interval` has elapsed
/// since the last pass started. Fixed-time mode: due once a scheduled
/// occurrence of `start_time` (a UTC time of day) has passed since the last
/// pass started, so passes anchor to the configured hour instead of
/// drifting with the previous pass's start (or with manual runs).
/// `gc_min_interval` applies in both modes.
pub(super) fn gc_due(
    last: DateTime<Utc>,
    now: DateTime<Utc>,
    gc_interval: u64,
    gc_min_interval: u64,
    start_time: Option<chrono::NaiveTime>,
) -> bool {
    let elapsed = (now - last).num_seconds().max(0) as u64;
    if elapsed < gc_min_interval {
        return false;
    }
    match start_time {
        Some(t) => {
            // Most recent scheduled occurrence at or before `now`.
            let today = now.date_naive().and_time(t).and_utc();
            let sched = if today <= now {
                today
            } else {
                today - chrono::Duration::days(1)
            };
            last < sched
        }
        None => elapsed >= gc_interval,
    }
}

/// Run the GC loop. Reads `gc_interval_secs` from settings each tick.
///
/// Only the instance holding the `gc` lease runs GC passes. The lease TTL
/// is set to the GC interval so that if the holder dies, no other instance
/// takes over until the original holder's orphan-candidate grace period
/// has expired — avoiding premature blob deletion.
#[allow(clippy::too_many_arguments)]
pub async fn run_blob_reaper(
    kv: Arc<dyn KvStore>,
    stores: Arc<StoreRegistry>,
    instance_id: String,
    cancel: CancellationToken,
    settings: Arc<SettingsHandle>,
    task_manager: Arc<TaskManager>,
    updater: UpdateSender,
    gc_state: Arc<tokio::sync::Mutex<GcState>>,
) {
    // Qualify the holder so the lease distinguishes the GC loop from other
    // jobs (e.g. integrity check) running on the same instance.
    let lease_holder = format!("{instance_id}:gc");

    // Use a short tick (60s) and check elapsed time against settings interval.
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    interval.tick().await; // skip first immediate tick

    // Load the persisted last-started timestamp. On fresh installs (no
    // prior GC), default to the current time so the first automatic GC
    // respects gc_interval rather than firing on the first tick.
    let mut last_gc_utc: Option<DateTime<Utc>> = service::get_gc_last_started_at(kv.as_ref())
        .await
        .ok()
        .flatten()
        .or(Some(Utc::now()));

    let mut transient_retries: u32 = 0;
    const MAX_TRANSIENT_RETRIES: u32 = 3;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let s = settings.load();
                let gc_interval = s.gc_interval_secs.unwrap_or(DEFAULT_GC_INTERVAL_SECS);
                let gc_min_interval = s.gc_min_interval_secs.unwrap_or(DEFAULT_GC_MIN_INTERVAL_SECS);
                let gc_start_time = s
                    .gc_start_time
                    .as_deref()
                    .and_then(|t| chrono::NaiveTime::parse_from_str(t, "%H:%M").ok());
                drop(s);

                // Check if the schedule is due before acquiring the lease.
                // This avoids holding the lease while idling between
                // passes, which would block the check job.
                if let Some(last) = last_gc_utc {
                    if !gc_due(last, Utc::now(), gc_interval, gc_min_interval, gc_start_time) {
                        continue;
                    }
                }

                // Try to acquire or renew the GC lease.
                // TTL = the effective scheduling interval so a dead holder's
                // grace period expires before anyone else can take over. In
                // fixed-time mode the effective cadence is daily, so the TTL
                // is at least a day regardless of gc_interval_secs.
                let lease_ttl = if gc_start_time.is_some() {
                    gc_interval.max(DEFAULT_GC_INTERVAL_SECS)
                } else {
                    gc_interval
                };
                match crate::server::worker::cluster::try_acquire_lease(
                    kv.as_ref(),
                    crate::server::worker::cluster::LEASE_GC,
                    &lease_holder,
                    lease_ttl,
                )
                .await
                {
                    Ok(true) => {
                        tracing::info!("acquired GC lease");
                    }
                    Ok(false) => {
                        tracing::debug!("GC lease held by another holder, skipping pass");
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to acquire GC lease");
                        continue;
                    }
                }

                // Persist start time before running the pass.
                let start_utc = Utc::now();
                let _ = service::set_gc_last_started_at(kv.as_ref(), start_utc).await;
                last_gc_utc = Some(start_utc);

                // Lock the shared GC state for the duration of this pass.
                let mut gs = gc_state.lock().await;

                // Create a task entry for this GC pass.
                let (task_id, progress_tx, task_cancel) =
                    task_manager.create(TaskKind::BlobGc).await;
                task_manager.mark_running(task_id).await;
                task_manager
                    .append_log(
                        task_id,
                        format!("Starting GC pass {}", gs.pass + 1),
                    )
                    .await;

                // Merge the task cancel token with the global shutdown token
                // so either can stop a pass.
                let merged_cancel = task_cancel.clone();
                let global_cancel = cancel.clone();
                let pass_cancel = CancellationToken::new();
                let pc = pass_cancel.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = merged_cancel.cancelled() => {}
                        _ = global_cancel.cancelled() => {}
                    }
                    pc.cancel();
                });

                let max_orphan_candidates = settings
                    .load()
                    .gc_max_orphan_candidates
                    .unwrap_or(super::DEFAULT_GC_MAX_ORPHAN_CANDIDATES);

                let start = std::time::Instant::now();
                let gc_span = tracing::info_span!("gc_pass", pass = gs.pass + 1);
                let _gc_guard = gc_span.enter();
                match gc_pass(kv.clone(), &stores, &mut gs, Some(&pass_cancel), max_orphan_candidates, Some(&progress_tx), false, &updater).await {
                    Ok(stats) => {
                        let elapsed = start.elapsed();
                        tracing::info!(
                            pass = gs.pass,
                            drained_repos = stats.drained_repos,
                            cleaned_upload_sessions = stats.cleaned_upload_sessions,
                            scanned_artifacts = stats.scanned_artifacts,
                            expired_artifacts = stats.expired_artifacts,
                            docker_deleted = stats.docker_deleted,
                            scanned_blobs = stats.scanned_blobs,
                            deleted_dedup_refs = stats.deleted_dedup_refs,
                            orphan_candidates = gs.orphan_candidates.len(),
                            orphaned_blobs_deleted = stats.orphaned_blobs_deleted,
                            orphaned_blob_bytes_deleted = stats.orphaned_blob_bytes_deleted,
                            elapsed_ns = elapsed.as_nanos() as u64,
                            "GC pass complete"
                        );
                        gauge!("gc_orphan_candidates")
                            .set(gs.orphan_candidates.len() as f64);
                        counter!("gc_deleted_dedup_refs_total")
                            .increment(stats.deleted_dedup_refs);
                        counter!("gc_orphaned_blobs_deleted_total")
                            .increment(stats.orphaned_blobs_deleted);
                        counter!("gc_orphaned_blob_bytes_deleted_total")
                            .increment(stats.orphaned_blob_bytes_deleted);
                        histogram!("gc_pass_duration_seconds")
                            .record(elapsed.as_secs_f64());
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
                            dry_run: false,
                        };
                        task_manager
                            .update_summary(
                                task_id,
                                format!(
                                    "GC pass {}: {} artifacts scanned, {} dedup refs deleted, {} orphan blobs removed",
                                    gs.pass,
                                    stats.scanned_artifacts,
                                    stats.deleted_dedup_refs,
                                    stats.orphaned_blobs_deleted,
                                ),
                            )
                            .await;
                        task_manager
                            .mark_completed(task_id, TaskResult::BlobGc(gc_result))
                            .await;
                        transient_retries = 0;
                        // (gc_pass already writes authoritative per-store stats
                        // from the live blob set during its sweep, so there is no
                        // separate reconcile to run here.)
                    }
                    Err(e) => {
                        if e.is_transient() && transient_retries < MAX_TRANSIENT_RETRIES {
                            transient_retries += 1;
                            tracing::warn!(
                                error = %e,
                                attempt = transient_retries,
                                "GC pass hit transient error, will retry"
                            );
                            if task_cancel.is_cancelled() {
                                task_manager.mark_cancelled(task_id).await;
                            } else {
                                task_manager
                                    .mark_failed(task_id, format!("{e}"))
                                    .await;
                            }
                            // Allow retry sooner: reset the persisted timestamp
                            // back so the interval check passes on the next tick.
                            let retry_at = start_utc - chrono::Duration::seconds(gc_interval as i64 - 5);
                            let _ = service::set_gc_last_started_at(kv.as_ref(), retry_at).await;
                            last_gc_utc = Some(retry_at);
                        } else {
                            transient_retries = 0;
                            tracing::warn!(error = %e, "GC pass failed");
                            // Clear orphan candidates on permanent failure (or
                            // exhausted transient retries) — a partial pass may
                            // have left the map in an inconsistent state and we
                            // must not delete blobs based on stale data.
                            *gs = GcState::new();
                            if task_cancel.is_cancelled() {
                                task_manager.mark_cancelled(task_id).await;
                            } else {
                                task_manager
                                    .mark_failed(task_id, format!("{e}"))
                                    .await;
                            }
                        }
                    }
                }
                drop(gs);

                // Release the lease after the pass completes so other
                // instances are not blocked while we idle until the
                // next interval.
                let _ = crate::server::worker::cluster::release_lease(
                    kv.as_ref(),
                    crate::server::worker::cluster::LEASE_GC,
                    &lease_holder,
                )
                .await;

                // Compact the KV store unconditionally after every pass.
                // redb frees pages on every overwrite (PUT of an existing
                // key), so ingestion traffic accumulates free pages even
                // on passes where GC itself deletes nothing. Gating
                // compaction on GC stats would let the shard files grow
                // unboundedly on a store that mostly sees updates rather
                // than deletions. Backends where compact is a no-op
                // (DynamoDB) absorb the call cheaply.
                let compact_start = std::time::Instant::now();
                let compact_result = kv.compact().await;
                let elapsed = compact_start.elapsed();
                histogram!("kv_compact_duration_seconds").record(elapsed.as_secs_f64());
                match compact_result {
                    Ok(compacted) => {
                        tracing::info!(
                            compacted,
                            elapsed_ms = elapsed.as_millis() as u64,
                            "post-GC KV compaction complete"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "post-GC KV compaction failed");
                        counter!("kv_compact_failures_total").increment(1);
                    }
                }
            }
            _ = cancel.cancelled() => {
                tracing::info!("GC worker shutting down");
                return;
            }
        }
    }
}
