// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

// ---------------------------------------------------------------------------
// GC pass
// ---------------------------------------------------------------------------

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bloomfilter::Bloom;
use futures::stream::StreamExt;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::server::infra::task::TaskProgress;
use depot_core::service;
use depot_core::store::keys;
use depot_core::store::kv::KvStore;
use depot_core::store_registry::StoreRegistry;
use depot_core::update::UpdateSender;

use super::bloom::{bloom_empty_like, BloomAccumulator};
use super::docker_gc::docker_gc;
use super::repo_cleanup::expire_repo_artifacts;

// ---------------------------------------------------------------------------
// GC state (persists across passes in-memory)
// ---------------------------------------------------------------------------

pub(super) struct OrphanCandidate {
    pub(super) first_seen_pass: u64,
}

#[derive(Default)]
pub struct GcState {
    /// Orphan blob files on disk with no artifact reference.
    /// Key = (store_name, blob_id).
    pub(super) orphan_candidates: HashMap<(String, String), OrphanCandidate>,
    /// Monotonically increasing pass counter.
    pub(super) pass: u64,
}

impl GcState {
    pub fn new() -> Self {
        Self::default()
    }
}

pub struct GcStats {
    pub drained_repos: u64,
    pub cleaned_upload_sessions: u64,
    pub scanned_artifacts: u64,
    pub expired_artifacts: u64,
    pub docker_deleted: u64,
    pub scanned_blobs: u64,
    pub deleted_dedup_refs: u64,
    pub orphaned_blobs_deleted: u64,
    pub orphaned_blob_bytes_deleted: u64,
}

#[allow(clippy::too_many_arguments)]
pub async fn gc_pass(
    kv: Arc<dyn KvStore>,
    stores: &Arc<StoreRegistry>,
    state: &mut GcState,
    cancel: Option<&CancellationToken>,
    max_orphan_candidates: usize,
    progress_tx: Option<&watch::Sender<TaskProgress>>,
    dry_run: bool,
    updater: &UpdateSender,
) -> depot_core::error::Result<GcStats> {
    state.pass += 1;
    let current_pass = state.pass;

    // --- Phase: Drain repos marked for deletion ---
    // Loop until no deleting repos remain, so repos marked during a drain
    // pass are caught immediately.
    let mut drained_repos = 0u64;
    if let Some(tx) = progress_tx {
        tx.send_modify(|p| {
            p.phase = "Draining deleted repos".into();
        });
    }
    loop {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            return Err(depot_core::error::DepotError::Internal(
                "GC pass cancelled".to_string(),
            ));
        }

        let deleting: Vec<_> = service::list_repos_raw(kv.as_ref())
            .await?
            .into_iter()
            .filter(|r| r.deleting)
            .collect();

        if deleting.is_empty() {
            break;
        }

        for repo in &deleting {
            tracing::info!(repo = %repo.name, "draining deleting repo");
            if let Err(e) = service::drain_deleting_repo(kv.as_ref(), &repo.name).await {
                tracing::error!(repo = %repo.name, "failed to drain deleting repo: {}", e);
            } else {
                drained_repos += 1;
            }
        }
    }

    // --- Phase: Clean up stale upload sessions ---
    const UPLOAD_SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
    let cleaned_upload_sessions =
        match service::cleanup_upload_sessions(kv.as_ref(), stores, UPLOAD_SESSION_TTL).await {
            Ok(n) => n as u64,
            Err(e) => {
                tracing::error!("upload session cleanup failed: {}", e);
                0
            }
        };

    let repos = service::list_repos(kv.as_ref()).await?;
    let store_records = service::list_stores(kv.as_ref()).await?;

    // --- Pre-phase: Expire artifacts in Docker repos with cleanup policies ---
    // Run artifact expiration for Docker repos BEFORE Docker GC, so that
    // layers orphaned by manifest expiration are caught in the same pass.
    let docker_repos_with_policy: Vec<_> = repos
        .iter()
        .filter(|r| r.format() == depot_core::store::kv::ArtifactFormat::Docker)
        .filter(|r| r.cleanup_max_age_days.is_some() || r.cleanup_max_unaccessed_days.is_some())
        .collect();

    let mut pre_expired = 0u64;
    if !docker_repos_with_policy.is_empty() {
        if let Some(tx) = progress_tx {
            tx.send_modify(|p| {
                p.phase = "Docker pre-expiration".into();
                p.total_repos = docker_repos_with_policy.len();
                p.completed_repos = 0;
            });
        }
        let repo_concurrency = repo_loop_concurrency();
        let sem = Arc::new(tokio::sync::Semaphore::new(repo_concurrency));
        let mut js = JoinSet::new();
        for repo in &docker_repos_with_policy {
            let kv = kv.clone();
            let repo = (*repo).clone();
            let cancel = cancel.cloned();
            let updater = updater.clone();
            let sem = Arc::clone(&sem);
            js.spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|e| depot_core::error::DepotError::Internal(e.to_string()))?;
                let (_, _, expired) =
                    expire_repo_artifacts(kv, &repo, cancel.as_ref(), &updater).await?;
                Ok::<u64, depot_core::error::DepotError>(expired)
            });
        }
        while let Some(res) = js.join_next().await {
            pre_expired += res??;
            if let Some(tx) = progress_tx {
                tx.send_modify(|p| p.completed_repos += 1);
            }
        }
    }

    if cancel.is_some_and(|c| c.is_cancelled()) {
        return Err(depot_core::error::DepotError::Internal(
            "GC pass cancelled".to_string(),
        ));
    }

    // --- Phase 0: Docker-specific GC (runs after pre-expiration) ---
    // Delete Docker layer blobs not referenced by any manifest, and
    // optionally delete manifests not referenced by any tag.
    let docker_repos: Vec<_> = repos
        .iter()
        .filter(|r| r.format() == depot_core::store::kv::ArtifactFormat::Docker)
        .collect();
    if let Some(tx) = progress_tx {
        tx.send_modify(|p| {
            p.phase = "Docker GC".into();
            p.total_repos = docker_repos.len();
            p.completed_repos = 0;
        });
    }
    let mut docker_deleted = 0u64;
    if !docker_repos.is_empty() {
        let repo_concurrency = repo_loop_concurrency();
        let sem = Arc::new(tokio::sync::Semaphore::new(repo_concurrency));
        let mut js = JoinSet::new();
        for repo in &docker_repos {
            let kv = kv.clone();
            let stores = stores.clone();
            let repo = (*repo).clone();
            let cancel = cancel.cloned();
            let updater = updater.clone();
            let sem = Arc::clone(&sem);
            js.spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|e| depot_core::error::DepotError::Internal(e.to_string()))?;
                docker_gc(kv, &stores, &repo, dry_run, cancel.as_ref(), &updater).await
            });
        }
        while let Some(res) = js.join_next().await {
            docker_deleted += res??;
            if let Some(tx) = progress_tx {
                tx.send_modify(|p| p.completed_repos += 1);
            }
        }
    }

    if cancel.is_some_and(|c| c.is_cancelled()) {
        return Err(depot_core::error::DepotError::Internal(
            "GC pass cancelled".to_string(),
        ));
    }

    // --- Artifact scan: build blob_id bloom filter + expire old artifacts ---

    // Use per-repo directory stats (same source as the dashboard) for BF sizing.
    // Skip proxy repos to avoid double-counting their members.
    let mut estimated_total = 0usize;
    for repo in &repos {
        if matches!(repo.kind, depot_core::store::kv::RepoKind::Proxy { .. }) {
            continue;
        }
        if let Ok(stats) = service::get_repo_stats(kv.as_ref(), repo).await {
            estimated_total += stats.artifact_count as usize;
        }
    }
    let estimated_total = estimated_total.max(1000); // minimum for BF sizing

    if let Some(tx) = progress_tx {
        tx.send_modify(|p| {
            p.phase = "Scanning artifacts".into();
            p.total_repos = repos.len();
            p.completed_repos = 0;
            p.total_blobs = estimated_total;
            p.checked_blobs = 0;
        });
    }

    // Create a template BF with the right dimensions. The combined
    // accumulator is wrapped in `Arc` so each shard task can fold its
    // local filter into shared atomic bytes *inside* the task — this
    // keeps peak memory bounded by the number of concurrent scans
    // (rather than the total shard count) and parallelises the fold
    // across cores.
    let template = Arc::new(Bloom::new_for_fp_rate(estimated_total, 0.01));
    let combined_acc = Arc::new(BloomAccumulator::empty_like(&template));
    let mut scanned_artifacts = 0u64;
    let mut expired_artifacts = 0u64;
    let now = chrono::Utc::now();

    // Shared atomic counter so shard tasks can report progress without
    // holding the watch sender.  A 1-second ticker pushes the value into
    // the progress channel so the UI updates smoothly.
    let live_scanned = Arc::new(AtomicU64::new(0));

    // Spawn a background ticker that pushes progress every second.
    let ticker_done = CancellationToken::new();
    if let Some(tx) = progress_tx {
        let live = live_scanned.clone();
        let done = ticker_done.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            interval.tick().await; // skip first immediate tick
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let val = live.load(Ordering::Relaxed) as usize;
                        tx.send_modify(|p| p.checked_blobs = val);
                    }
                    _ = done.cancelled() => break,
                }
            }
        });
    }

    // Scan all artifact partitions: build bloom filter + expire old artifacts.
    //
    // Fan out `repos.len() * NUM_SHARDS` shard tasks into a single `JoinSet`
    // guarded by an outer semaphore so we can exploit multi-core machines
    // without overwhelming the KV backend with too many concurrent
    // `scan_range` calls.  Each task folds its shard-local bloom filter
    // into the shared `Arc<BloomAccumulator>` via atomic OR *before*
    // returning, so the 40+ MB local filter drops on task end. This caps
    // in-flight BF memory at `concurrency * local_bf_size` rather than the
    // `N_shards * local_bf_size` that join-time folding would imply — on
    // stores with millions of artifacts the difference is the whole of
    // system RAM.
    let scan_concurrency = artifact_scan_concurrency();
    let scan_sem = Arc::new(tokio::sync::Semaphore::new(scan_concurrency));
    let mut js = JoinSet::new();
    for (repo_idx, repo) in repos.iter().enumerate() {
        let max_age_cutoff = repo
            .cleanup_max_age_days
            .map(|d| now - chrono::Duration::days(d as i64));
        let max_unaccessed_cutoff = repo
            .cleanup_max_unaccessed_days
            .map(|d| now - chrono::Duration::days(d as i64));

        let repo_name = repo.name.clone();
        let store_name = repo.store.clone();
        let is_docker = repo.format() == depot_core::store::kv::ArtifactFormat::Docker;
        for shard in 0..keys::NUM_SHARDS {
            let kv = kv.clone();
            let tmpl = template.clone();
            let repo_name = repo_name.clone();
            let store_name = store_name.clone();
            let cancel = cancel.cloned();
            let updater = updater.clone();
            let live_scanned = live_scanned.clone();
            let sem = Arc::clone(&scan_sem);
            let acc = Arc::clone(&combined_acc);
            js.spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|e| depot_core::error::DepotError::Internal(e.to_string()))?;
                let pk = keys::artifact_shard_pk(&repo_name, shard);
                let mut local_bf = bloom_empty_like(&tmpl);
                let mut local_scanned = 0u64;
                let mut local_expired = 0u64;

                let mut cursor: Option<String> = None;
                loop {
                    if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                        return Err(depot_core::error::DepotError::Internal(
                            "GC pass cancelled".to_string(),
                        ));
                    }

                    let start = match &cursor {
                        Some(c) => c.as_str(),
                        None => "",
                    };
                    let result = kv
                        .scan_range(
                            keys::TABLE_ARTIFACTS,
                            Cow::Borrowed(pk.as_str()),
                            Cow::Borrowed(start),
                            None,
                            1000,
                        )
                        .await?;

                    let mut expired_entries: Vec<(&str, u64)> = Vec::new();
                    for (sk, value) in &result.items {
                        let record = match rmp_serde::from_slice::<
                            depot_core::store::kv::ArtifactRecord,
                        >(value)
                        {
                            Ok(r) => r,
                            Err(_) => continue,
                        };

                        let expired = super::repo_cleanup::record_expired(
                            kv.as_ref(),
                            &repo_name,
                            sk,
                            &record,
                            is_docker,
                            max_age_cutoff,
                            max_unaccessed_cutoff,
                        )
                        .await;

                        if expired {
                            expired_entries.push((sk, record.size));
                        } else {
                            if let Some(ref blob_id) = record.blob_id {
                                local_bf.set(blob_id.as_bytes());
                            }
                            local_scanned += 1;
                        }
                    }
                    if !expired_entries.is_empty() && !dry_run {
                        let paths: Vec<&str> = expired_entries.iter().map(|(sk, _)| *sk).collect();
                        service::delete_artifacts_paired_batch(kv.as_ref(), &repo_name, &paths)
                            .await?;
                        local_expired += expired_entries.len() as u64;
                        let mut batch_bytes = 0i64;
                        for &(sk, size) in &expired_entries {
                            updater
                                .dir_changed(&repo_name, sk, -1, -(size as i64))
                                .await;
                            batch_bytes += size as i64;
                        }
                        updater
                            .store_changed(
                                &store_name,
                                -(expired_entries.len() as i64),
                                -batch_bytes,
                            )
                            .await;
                    }

                    // Update the shared counter so the progress ticker picks it up.
                    live_scanned.fetch_add(result.items.len() as u64, Ordering::Relaxed);

                    if result.done {
                        break;
                    }
                    if let Some((last_sk, _)) = result.items.last() {
                        cursor = Some(format!("{}\0", last_sk));
                    } else {
                        break;
                    }
                    tokio::task::yield_now().await;
                }

                // Fold into the shared accumulator before dropping
                // `local_bf`. This is the one write point that needs to
                // happen before task end so the combined filter sees this
                // shard's contributions.
                acc.or_from(&local_bf);
                drop(local_bf);

                Ok::<_, depot_core::error::DepotError>((repo_idx, local_scanned, local_expired))
            });
        }
    }

    let mut shards_done_per_repo = vec![0u16; repos.len()];
    while let Some(res) = js.join_next().await {
        let (repo_idx, local_scanned, local_expired) = res??;
        scanned_artifacts += local_scanned;
        expired_artifacts += local_expired;
        if let Some(slot) = shards_done_per_repo.get_mut(repo_idx) {
            *slot += 1;
            if *slot == keys::NUM_SHARDS {
                if let Some(tx) = progress_tx {
                    tx.send_modify(|p| p.completed_repos += 1);
                }
            }
        }
    }

    // All tasks have returned, so every `Arc<BloomAccumulator>` clone they
    // held has been dropped and we own the sole remaining reference.
    // `try_unwrap` lets us move out of the accumulator to finalize without
    // an extra 40+ MB clone.
    let combined_acc = Arc::try_unwrap(combined_acc).map_err(|_| {
        depot_core::error::DepotError::Internal(
            "BloomAccumulator still has outstanding references after join".to_string(),
        )
    })?;
    let combined = Arc::new(combined_acc.finalize());

    // Stop the progress ticker and do a final update.
    ticker_done.cancel();
    if let Some(tx) = progress_tx {
        tx.send_modify(|p| p.checked_blobs = live_scanned.load(Ordering::Relaxed) as usize);
    }

    // Include artifacts expired in the Docker pre-phase.
    expired_artifacts += pre_expired;

    if cancel.is_some_and(|c| c.is_cancelled()) {
        return Err(depot_core::error::DepotError::Internal(
            "GC pass cancelled".to_string(),
        ));
    }

    // --- Sweep: clean KV records + walk filesystem per store ---
    //
    // For each store we do two things in a single logical pass:
    //
    // 1. Scan KV BlobRecords.  Records whose blob_id is absent from the BF
    //    are deleted immediately.  This is safe: deleting a KV record only
    //    prevents future dedup — it does not touch the blob file on disk.
    //
    // 2. Walk the blob store filesystem.  Files whose blob_id is absent from
    //    the BF and were already candidates from a previous pass are deleted.
    //    Newly-unreferenced files become candidates.  Candidates whose blob_id
    //    now appears in the BF (artifact created since last pass) are cleared.

    // Sum existing store stats to get an estimated total_bytes for progress.
    let mut estimated_total_bytes = 0u64;
    for sr in &store_records {
        if let Ok(Some(stats)) = service::get_store_stats(kv.as_ref(), &sr.name).await {
            estimated_total_bytes += stats.total_bytes;
        }
    }

    if let Some(tx) = progress_tx {
        tx.send_modify(|p| {
            p.phase = "Sweeping blob records".into();
            p.total_repos = store_records.len();
            p.completed_repos = 0;
            p.total_blobs = 0;
            p.checked_blobs = 0;
            p.bytes_read = 0;
            p.total_bytes = estimated_total_bytes;
        });
    }

    let mut scanned_blobs = 0u64;
    let mut deleted_dedup_refs = 0u64;
    let mut orphaned_blobs_deleted = 0u64;
    let mut orphaned_blob_bytes_deleted = 0u64;

    // Prune orphan candidates whose blob_id is now referenced (artifact
    // created between passes).
    state
        .orphan_candidates
        .retain(|(_s, blob_id), _| !combined.check(blob_id.as_bytes()));

    for store_rec in &store_records {
        // --- KV BlobRecord cleanup (parallel across shards) ---
        let store_name = &store_rec.name;
        let mut js = JoinSet::new();
        for shard in 0..keys::NUM_SHARDS {
            let kv = kv.clone();
            let store_name = store_name.clone();
            let bf = Arc::clone(&combined);
            let cancel = cancel.cloned();
            js.spawn(async move {
                let pk = keys::blob_shard_pk(&store_name, shard);
                let mut local_scanned = 0u64;
                let mut local_scanned_bytes = 0u64;
                let mut local_deleted = 0u64;
                let mut local_live_count = 0u64;
                let mut local_live_bytes = 0u64;

                let mut cursor: Option<String> = None;
                loop {
                    if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                        return Err(depot_core::error::DepotError::Internal(
                            "GC pass cancelled".to_string(),
                        ));
                    }

                    let start = match &cursor {
                        Some(c) => c.as_str(),
                        None => "",
                    };
                    let result = kv
                        .scan_range(
                            keys::TABLE_BLOBS,
                            Cow::Borrowed(pk.as_str()),
                            Cow::Borrowed(start),
                            None,
                            1000,
                        )
                        .await?;

                    let mut del_sks: Vec<&str> = Vec::new();
                    for (sk, value) in &result.items {
                        local_scanned += 1;
                        let record: depot_core::store::kv::BlobRecord =
                            match rmp_serde::from_slice(value) {
                                Ok(r) => r,
                                Err(_) => continue,
                            };

                        local_scanned_bytes += record.size;
                        if !bf.check(record.blob_id.as_bytes()) {
                            del_sks.push(sk);
                        } else {
                            local_live_count += 1;
                            local_live_bytes += record.size;
                        }
                    }

                    if !del_sks.is_empty() && !dry_run {
                        let del_keys: Vec<(&str, &str)> =
                            del_sks.iter().map(|sk| (pk.as_str(), *sk)).collect();
                        kv.delete_batch(keys::TABLE_BLOBS, &del_keys).await?;
                        local_deleted += del_sks.len() as u64;
                    }

                    if result.done {
                        break;
                    }
                    if let Some((last_sk, _)) = result.items.last() {
                        cursor = Some(format!("{}\0", last_sk));
                    } else {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                Ok::<_, depot_core::error::DepotError>((
                    local_scanned,
                    local_scanned_bytes,
                    local_deleted,
                    local_live_count,
                    local_live_bytes,
                ))
            });
        }
        let mut blob_shard_results = Vec::new();
        while let Some(res) = js.join_next().await {
            blob_shard_results.push(res??);
        }

        let mut store_live_count = 0u64;
        let mut store_live_bytes = 0u64;
        let mut store_scanned_bytes = 0u64;
        for (local_scanned, local_scanned_bytes, local_deleted, live_count, live_bytes) in
            blob_shard_results
        {
            scanned_blobs += local_scanned;
            store_scanned_bytes += local_scanned_bytes;
            deleted_dedup_refs += local_deleted;
            store_live_count += live_count;
            store_live_bytes += live_bytes;
        }

        // Persist per-store stats.
        let stats = depot_core::store::kv::StoreStatsRecord {
            blob_count: store_live_count,
            total_bytes: store_live_bytes,
            updated_at: chrono::Utc::now(),
        };
        let _ = service::put_store_stats(kv.as_ref(), store_name, &stats).await;

        if let Some(tx) = progress_tx {
            let bytes = store_scanned_bytes;
            tx.send_modify(|p| {
                p.completed_repos += 1;
                p.checked_blobs = scanned_blobs as usize;
                p.bytes_read += bytes;
            });
        }

        // --- Orphan blob cleanup (filesystem or S3) ---
        if let Some(tx) = progress_tx {
            tx.send_modify(|p| {
                p.phase = format!("Walking blob store '{}'", store_rec.name);
            });
        }
        let blob_store = match stores.get(&store_rec.name).await {
            Some(s) => s,
            None => continue,
        };

        let store_name = store_rec.name.clone();

        // Fan out the blob-store walk across all 256 hex shards (256 on
        // both FileBlobStore and S3BlobStore; falls back to one sequential
        // stream for any backend that hasn't overridden `scan_shard_count`).
        // Each worker checks candidate blob_ids against the shared
        // `Arc<Bloom>` inline and forwards orphans to a single consumer that
        // owns the grace-period state machine — no shared-state locks.
        let shard_count = blob_store.scan_shard_count();
        let walk_concurrency = blob_walk_concurrency();
        let walk_sem = Arc::new(tokio::sync::Semaphore::new(walk_concurrency));
        let (orph_tx, mut orph_rx) = tokio::sync::mpsc::channel::<(String, u64)>(2048);

        let mut walk_js = JoinSet::new();
        for shard in 0..shard_count {
            let blob_store_task = blob_store.clone();
            let bf = Arc::clone(&combined);
            let sem = Arc::clone(&walk_sem);
            let orph_tx = orph_tx.clone();
            let cancel = cancel.cloned();
            let store_name_log = store_name.clone();
            walk_js.spawn(async move {
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                    return;
                }
                match blob_store_task.scan_blobs_sharded(shard).await {
                    Ok(mut stream) => {
                        while let Some(item) = stream.next().await {
                            match item {
                                Ok((blob_id, size)) => {
                                    if !bf.check(blob_id.as_bytes())
                                        && orph_tx.send((blob_id, size)).await.is_err()
                                    {
                                        // Consumer closed — main task exited
                                        // the sweep (e.g. cancellation).
                                        return;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        store = %store_name_log,
                                        shard = shard,
                                        error = %e,
                                        "blob store scan yielded error during orphan sweep"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            store = %store_name_log,
                            shard = shard,
                            error = %e,
                            "blob store scan failed during orphan sweep"
                        );
                    }
                }
            });
        }
        // Drop the outer sender so `orph_rx.recv()` returns `None` once all
        // worker clones are dropped.
        drop(orph_tx);

        // Consume orphan candidates as they arrive and apply the two-pass
        // grace period. Single-owner mutation of `state.orphan_candidates`
        // keeps the cap bookkeeping correct without a lock. `blob_store.delete`
        // calls fan out via `delete_js` + `delete_sem` so we don't bottleneck
        // on the per-blob round-trip latency of the backing store.
        let delete_sem = Arc::new(tokio::sync::Semaphore::new(ORPHAN_DELETE_CONCURRENCY));
        let mut delete_js: JoinSet<()> = JoinSet::new();
        let mut cap_warned = false;
        while let Some((blob_id, size)) = orph_rx.recv().await {
            let key = (store_name.clone(), blob_id.clone());

            match state.orphan_candidates.get(&key) {
                Some(c) if current_pass - c.first_seen_pass >= 1 => {
                    // Candidate from a previous pass — delete the orphan.
                    if !dry_run {
                        let blob_store_del = blob_store.clone();
                        let sem = Arc::clone(&delete_sem);
                        let blob_id_del = blob_id.clone();
                        delete_js.spawn(async move {
                            let _permit = match sem.acquire_owned().await {
                                Ok(p) => p,
                                Err(_) => return,
                            };
                            let _ = blob_store_del.delete(&blob_id_del).await;
                        });
                    }
                    // Match pre-parallel semantics: counter reflects
                    // deletion *decisions* (delete errors were already
                    // silently discarded with `let _ = …`).
                    orphaned_blobs_deleted += 1;
                    orphaned_blob_bytes_deleted += size;
                    state.orphan_candidates.remove(&key);
                }
                Some(_) => {
                    // Still in grace period — keep as candidate.
                }
                None => {
                    if state.orphan_candidates.len() >= max_orphan_candidates {
                        if !cap_warned {
                            tracing::warn!(
                                max_orphan_candidates,
                                "orphan candidate cap reached; skipping new candidates this pass"
                            );
                            cap_warned = true;
                        }
                    } else {
                        // New candidate — will be eligible for deletion next pass.
                        state.orphan_candidates.insert(
                            key,
                            OrphanCandidate {
                                first_seen_pass: current_pass,
                            },
                        );
                    }
                }
            }
        }

        // Wait for in-flight orphan deletes and walk tasks to finish before
        // moving on to the next store. Errors were already logged in-task.
        while delete_js.join_next().await.is_some() {}
        while walk_js.join_next().await.is_some() {}
    }

    Ok(GcStats {
        drained_repos,
        cleaned_upload_sessions,
        scanned_artifacts,
        expired_artifacts,
        docker_deleted,
        scanned_blobs,
        deleted_dedup_refs,
        orphaned_blobs_deleted,
        orphaned_blob_bytes_deleted,
    })
}

// ---------------------------------------------------------------------------
// Concurrency sizing
// ---------------------------------------------------------------------------

fn cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Outer concurrency for the (repo × shard) artifact-scan fan-out.
/// Scaled to CPU count but clamped so we don't drown the KV backend.
fn artifact_scan_concurrency() -> usize {
    (cpus() * 4).clamp(32, 128)
}

/// Outer concurrency for per-repo phases (docker_gc, pre-expiration) that
/// themselves fan out 256 shards internally. Keep the outer bound modest.
fn repo_loop_concurrency() -> usize {
    cpus().clamp(1, 16)
}

/// Outer concurrency for sharded blob-store walks. Filesystem walks are
/// mostly I/O-bound (dirent + stat), S3 walks are network-bound; either way
/// a moderate bound protects the backing store while still giving a healthy
/// multi-core speedup over the old single-stream walk.
fn blob_walk_concurrency() -> usize {
    (cpus() * 2).clamp(4, 64)
}

/// Concurrency for per-store orphan-delete fan-out. Each permit holds one
/// `blob_store.delete(..)` round-trip; 32 is tuned for good S3 parallelism
/// without starving other stores of client connections.
const ORPHAN_DELETE_CONCURRENCY: usize = 32;
