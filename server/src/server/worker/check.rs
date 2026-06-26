// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Integrity check task: verifies all blobs match their stored BLAKE3 checksums
//! and validates repository metadata consistency.
//!
//! The check holds the GC lease for its duration so that garbage collection
//! cannot run concurrently. Since the system is eventually consistent, the
//! check tolerates expected transient dangling references (e.g. Docker tags
//! pointing to not-yet-written manifests) and only reports findings.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bloomfilter::Bloom;
use futures::stream::StreamExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::server::infra::task::{
    CheckFinding, CheckResult, TaskId, TaskManager, TaskProgress, TaskResult,
};
use crate::server::worker::blob_reaper::{bloom_empty_like, bloom_union};
use crate::server::worker::cluster::{self, LeaseGuard};
use depot_core::service;
use depot_core::store::keys;
use depot_core::store::kv::KvStore;
use depot_core::store::kv::{ArtifactFormat, ArtifactKind, BlobRecord, RepoKind};
use depot_core::store_registry::StoreRegistry;

/// Maximum in-flight blob verification tasks.
const BLOB_CHECK_IN_FLIGHT: usize = 1024;

/// Lease TTL for the check task (1 hour). The check explicitly releases
/// the lease on completion; the TTL is a safety net for crashes.
const CHECK_LEASE_TTL_SECS: u64 = 3600;

/// Maximum number of lease acquisition attempts before failing.
const LEASE_ACQUIRE_ATTEMPTS: u32 = 5;

/// Delay between lease acquisition retries.
const LEASE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// Run an integrity check across all blobs and repository metadata.
/// When `dry_run` is false, dangling references are removed and malformed
/// paths are repaired.
#[allow(clippy::too_many_arguments)]
pub async fn run_check(
    task_id: TaskId,
    task_manager: Arc<TaskManager>,
    kv: Arc<dyn KvStore>,
    stores: Arc<StoreRegistry>,
    instance_id: String,
    progress_tx: watch::Sender<TaskProgress>,
    cancel: CancellationToken,
    dry_run: bool,
    verify_blob_hashes: bool,
    updater: depot_core::update::UpdateSender,
) {
    let mode_label = if dry_run { "dry-run" } else { "fix" };
    task_manager
        .append_log(
            task_id,
            format!("Starting integrity check in {} mode", mode_label),
        )
        .await;

    // 1. Acquire the GC lease to prevent garbage collection during the check.
    //    Qualify the holder so the lease distinguishes check from the GC loop
    //    even on the same instance.
    let lease_holder = format!("{instance_id}:check");
    task_manager
        .append_log(task_id, "Acquiring GC lease...".to_string())
        .await;
    let mut lease_guard = LeaseGuard::new(kv.clone(), cluster::LEASE_GC, &lease_holder);

    let mut acquired = false;
    for attempt in 1..=LEASE_ACQUIRE_ATTEMPTS {
        if cancel.is_cancelled() {
            task_manager.mark_cancelled(task_id).await;
            return;
        }
        match cluster::try_acquire_lease(
            kv.as_ref(),
            cluster::LEASE_GC,
            &lease_holder,
            CHECK_LEASE_TTL_SECS,
        )
        .await
        {
            Ok(true) => {
                acquired = true;
                break;
            }
            Ok(false) => {
                task_manager
                    .append_log(
                        task_id,
                        format!(
                            "GC lease held by another instance, retrying ({}/{})",
                            attempt, LEASE_ACQUIRE_ATTEMPTS
                        ),
                    )
                    .await;
                if attempt < LEASE_ACQUIRE_ATTEMPTS {
                    tokio::time::sleep(LEASE_RETRY_DELAY).await;
                }
            }
            Err(e) => {
                let is_transient = e
                    .downcast_ref::<depot_core::error::DepotError>()
                    .is_some_and(|de| de.is_transient());
                if is_transient {
                    task_manager
                        .append_log(
                            task_id,
                            format!(
                                "Transient error acquiring GC lease, retrying ({}/{}): {}",
                                attempt, LEASE_ACQUIRE_ATTEMPTS, e
                            ),
                        )
                        .await;
                    if attempt < LEASE_ACQUIRE_ATTEMPTS {
                        tokio::time::sleep(LEASE_RETRY_DELAY).await;
                    }
                } else {
                    task_manager
                        .mark_failed(task_id, format!("Failed to acquire GC lease: {}", e))
                        .await;
                    return;
                }
            }
        }
    }

    if !acquired {
        task_manager
            .mark_failed(
                task_id,
                "Could not acquire GC lease — GC may be running on another instance".to_string(),
            )
            .await;
        return;
    }

    // 2. Estimate totals from store stats and repo root dir entries
    //    so the check can start immediately without scanning every artifact.
    let repos = match service::list_repos(kv.as_ref()).await {
        Ok(r) => r,
        Err(e) => {
            task_manager
                .mark_failed(task_id, format!("Failed to list repos: {}", e))
                .await;
            return;
        }
    };

    let total_repos = repos.len();

    let store_records = service::list_stores(kv.as_ref()).await.unwrap_or_default();
    let mut total_blobs: usize = 0;
    let mut total_bytes: u64 = 0;
    for sr in &store_records {
        if let Ok(Some(stats)) = service::get_store_stats(kv.as_ref(), &sr.name).await {
            total_blobs += stats.blob_count as usize;
            total_bytes += stats.total_bytes;
        }
    }

    let mut total_artifacts: usize = 0;
    for repo_config in &repos {
        if matches!(repo_config.kind, RepoKind::Proxy { .. }) {
            continue;
        }
        total_artifacts += service::get_repo_stats(kv.as_ref(), repo_config)
            .await
            .map(|s| s.artifact_count as usize)
            .unwrap_or(0);
    }

    task_manager
        .append_log(
            task_id,
            format!(
                "GC lease acquired: {} blobs ({} bytes), {} repos, {} artifacts",
                total_blobs, total_bytes, total_repos, total_artifacts
            ),
        )
        .await;

    let _ = progress_tx.send(TaskProgress {
        total_repos,
        completed_repos: 0,
        total_blobs,
        checked_blobs: 0,
        failed_blobs: 0,
        bytes_read: 0,
        total_bytes,
        total_artifacts,
        checked_artifacts: 0,
        phase: "Verifying blobs".to_string(),
    });

    task_manager
        .append_log(
            task_id,
            format!(
                "Snapshot taken: {} blobs ({} bytes), {} repos, {} artifacts",
                total_blobs, total_bytes, total_repos, total_artifacts
            ),
        )
        .await;
    task_manager
        .update_summary(
            task_id,
            format!(
                "Checking {} blobs, {} repos ({} artifacts)...",
                total_blobs, total_repos, total_artifacts
            ),
        )
        .await;

    // Note: total_blobs/total_artifacts are estimates from store stats and
    // dir entries — they may be zero even when blobs exist (e.g. stats not
    // yet populated). Always proceed with the full check.

    // 3. Build bloom filter from actual blob store listings.
    //    This is the source of truth for which blobs physically exist,
    //    rather than relying on KV dedup records.
    let findings = Arc::new(tokio::sync::Mutex::new(Vec::<CheckFinding>::new()));
    let checked_counter = Arc::new(AtomicU64::new(0));
    let missing_blobs: Arc<tokio::sync::Mutex<Vec<(String, String)>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let fixed_count = Arc::new(AtomicU64::new(0));

    let store_list = service::list_stores(kv.as_ref()).await.unwrap_or_default();

    // 3a. Walk/list each blob store and build a bloom filter of all blob_ids
    //     that actually exist on disk.
    progress_tx.send_modify(|p| {
        p.phase = "Listing blob stores".to_string();
    });

    let bf_template = Bloom::new_for_fp_rate(total_blobs.max(1000), 0.01);
    let mut combined_bf = bloom_empty_like(&bf_template);
    let mut actual_blob_count: u64 = 0;

    for store_rec in &store_list {
        if cancel.is_cancelled() {
            break;
        }

        task_manager
            .append_log(
                task_id,
                format!("Listing blob store '{}'...", store_rec.name),
            )
            .await;

        // Drive listing through the BlobStore trait — the backend decides
        // how to iterate (filesystem walk vs. S3 ListObjectsV2 fan-out).
        let (store_bf, count) = {
            let mut bf = bloom_empty_like(&bf_template);
            let mut count: u64 = 0;
            let Some(blob_store) = stores.get(&store_rec.name).await else {
                task_manager
                    .append_log(
                        task_id,
                        format!(
                            "Blob store '{}' not found in registry, skipping",
                            store_rec.name
                        ),
                    )
                    .await;
                continue;
            };
            match blob_store.scan_all_blobs().await {
                Ok(mut stream) => {
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok((blob_id, _size)) => {
                                bf.set(blob_id.as_bytes());
                                count += 1;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    store = %store_rec.name,
                                    error = %e,
                                    "blob store scan yielded error during bloom build"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        store = %store_rec.name,
                        error = %e,
                        "blob store scan failed during bloom build"
                    );
                }
            }
            (bf, count)
        };

        bloom_union(&mut combined_bf, &store_bf);
        actual_blob_count += count;

        task_manager
            .append_log(
                task_id,
                format!("Listed {} blobs in store '{}'", count, store_rec.name),
            )
            .await;
    }

    // Update total_blobs from actual listing (the initial estimate may be stale).
    total_blobs = actual_blob_count as usize;
    progress_tx.send_modify(|p| {
        p.total_blobs = total_blobs;
    });

    task_manager
        .append_log(
            task_id,
            format!(
                "Bloom filter built from {} blobs across {} stores",
                actual_blob_count,
                store_list.len()
            ),
        )
        .await;

    if cancel.is_cancelled() {
        task_manager
            .append_log(task_id, "Check cancelled".to_string())
            .await;
        task_manager
            .update_summary(task_id, "Cancelled".to_string())
            .await;
        task_manager.mark_cancelled(task_id).await;
        if let Err(e) = lease_guard.release().await {
            tracing::warn!("failed to release GC lease: {}", e);
        }
        return;
    }

    // 3b. Verify KV BlobRecords against the bloom filter.
    //     Records whose blob_id is NOT in the filter are missing from the
    //     store. Records that ARE in the filter may optionally have their
    //     BLAKE3 hash verified by reading the blob.
    progress_tx.send_modify(|p| {
        p.phase = "Verifying blob records".to_string();
    });
    task_manager
        .update_summary(
            task_id,
            format!(
                "Verifying blob records against {} listed blobs...",
                actual_blob_count
            ),
        )
        .await;

    // Wrap bloom filter in Arc for concurrent access (check() takes &self).
    let bf = Arc::new(combined_bf);

    let pks: Vec<String> = store_list
        .iter()
        .flat_map(|s| (0..keys::NUM_SHARDS).map(move |shard| keys::blob_shard_pk(&s.name, shard)))
        .collect();

    let semaphore = Arc::new(tokio::sync::Semaphore::new(BLOB_CHECK_IN_FLIGHT));
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    for pk in &pks {
        if cancel.is_cancelled() {
            break;
        }

        let mut work = Vec::new();
        let scan_result = service::scan::for_each_in_partition(
            kv.as_ref(),
            keys::TABLE_BLOBS,
            pk,
            "",
            service::scan::SCAN_BATCH_SIZE,
            |_, v| {
                let rec: BlobRecord = rmp_serde::from_slice(v)?;
                work.push((rec.blob_id, rec.hash, rec.size, rec.store));
                Ok(true)
            },
        )
        .await;
        if let Err(e) = scan_result {
            task_manager
                .append_log(
                    task_id,
                    format!("Warning: failed to scan blobs partition {}: {}", pk, e),
                )
                .await;
            continue;
        }

        for (blob_id, hash, expected_size, store_name) in work {
            if cancel.is_cancelled() {
                break;
            }

            let Ok(permit) = semaphore.clone().acquire_owned().await else {
                break;
            };
            let findings = findings.clone();
            let checked = checked_counter.clone();
            let stores = stores.clone();
            let missing_blobs = missing_blobs.clone();
            let bf = bf.clone();
            let progress_tx = progress_tx.clone();
            let cancel = cancel.clone();
            let task_manager = task_manager.clone();

            handles.push(tokio::spawn(async move {
                if cancel.is_cancelled() {
                    drop(permit);
                    return;
                }

                // Check if the store is registered before anything else.
                let blob_store = match stores.get(&store_name).await {
                    Some(s) => s,
                    None => {
                        let msg = format!(
                            "Blob {} (store '{}'): blob store not found in registry",
                            hash, store_name
                        );
                        task_manager.append_log(task_id, msg).await;
                        findings.lock().await.push(CheckFinding {
                            repo: None,
                            path: hash.clone(),
                            expected_hash: hash.clone(),
                            actual_hash: None,
                            error: Some(format!("blob store '{}' not found", store_name)),
                            fixed: false,
                        });
                        progress_tx.send_modify(|p| {
                            p.checked_blobs += 1;
                            p.failed_blobs += 1;
                        });
                        checked.fetch_add(1, Ordering::Relaxed);
                        drop(permit);
                        return;
                    }
                };

                // Check bloom filter: is this blob_id physically on disk?
                if !bf.check(blob_id.as_bytes()) {
                    // Bloom filters have zero false negatives — this blob
                    // is definitively missing from the store.
                    let msg = format!(
                        "Blob {} (store '{}', id {}): blob file missing from store",
                        hash, store_name, blob_id
                    );
                    task_manager.append_log(task_id, msg).await;
                    findings.lock().await.push(CheckFinding {
                        repo: None,
                        path: hash.clone(),
                        expected_hash: hash.clone(),
                        actual_hash: None,
                        error: Some("blob missing from store".to_string()),
                        fixed: false,
                    });
                    missing_blobs
                        .lock()
                        .await
                        .push((store_name.clone(), hash.clone()));
                    progress_tx.send_modify(|p| {
                        p.checked_blobs += 1;
                        p.failed_blobs += 1;
                    });
                    checked.fetch_add(1, Ordering::Relaxed);
                    drop(permit);
                    return;
                }

                // Blob exists per bloom filter.
                if verify_blob_hashes {
                    // Full verification: stream and hash every byte.
                    match blob_store.open_read(&blob_id).await {
                        Ok(Some((mut reader, actual_size))) => {
                            let mut hasher = blake3::Hasher::new();
                            let mut buf = vec![0u8; 256 * 1024];
                            let mut bytes_read = 0u64;
                            let mut read_err = None;
                            loop {
                                match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        if let Some(chunk) = buf.get(..n) {
                                            hasher.update(chunk);
                                        }
                                        bytes_read += n as u64;
                                    }
                                    Err(e) => {
                                        read_err = Some(e);
                                        break;
                                    }
                                }
                            }
                            if let Some(e) = read_err {
                                let msg = format!(
                                    "Blob {} (store '{}'): I/O error reading blob data — {}",
                                    hash, store_name, e
                                );
                                task_manager.append_log(task_id, msg).await;
                                findings.lock().await.push(CheckFinding {
                                    repo: None,
                                    path: hash.clone(),
                                    expected_hash: hash.clone(),
                                    actual_hash: None,
                                    error: Some(format!("read error: {}", e)),
                                    fixed: false,
                                });
                                progress_tx.send_modify(|p| {
                                    p.checked_blobs += 1;
                                    p.failed_blobs += 1;
                                    p.bytes_read += bytes_read;
                                });
                            } else {
                                let actual_hash = hasher.finalize().to_hex().to_string();
                                if actual_hash != hash {
                                    let msg = format!(
                                        "Blob {} (store '{}'): BLAKE3 hash mismatch — expected {}, actual {}",
                                        hash, store_name, hash, actual_hash
                                    );
                                    task_manager.append_log(task_id, msg).await;
                                    findings.lock().await.push(CheckFinding {
                                        repo: None,
                                        path: hash.clone(),
                                        expected_hash: hash.clone(),
                                        actual_hash: Some(actual_hash),
                                        error: None,
                                        fixed: false,
                                    });
                                    progress_tx.send_modify(|p| {
                                        p.checked_blobs += 1;
                                        p.failed_blobs += 1;
                                        p.bytes_read += bytes_read;
                                    });
                                } else if actual_size != expected_size {
                                    let msg = format!(
                                        "Blob {} (store '{}'): size mismatch — expected {} bytes, actual {} bytes",
                                        hash, store_name, expected_size, actual_size
                                    );
                                    task_manager.append_log(task_id, msg).await;
                                    findings.lock().await.push(CheckFinding {
                                        repo: None,
                                        path: hash.clone(),
                                        expected_hash: format!("size={}", expected_size),
                                        actual_hash: Some(format!("size={}", actual_size)),
                                        error: Some("size mismatch".to_string()),
                                        fixed: false,
                                    });
                                    progress_tx.send_modify(|p| {
                                        p.checked_blobs += 1;
                                        p.failed_blobs += 1;
                                        p.bytes_read += bytes_read;
                                    });
                                } else {
                                    progress_tx.send_modify(|p| {
                                        p.checked_blobs += 1;
                                        p.bytes_read += bytes_read;
                                    });
                                }
                            }
                        }
                        Ok(None) => {
                            // Bloom filter false positive or race condition.
                            let msg = format!(
                                "Blob {} (store '{}', id {}): blob file missing from store",
                                hash, store_name, blob_id
                            );
                            task_manager.append_log(task_id, msg).await;
                            findings.lock().await.push(CheckFinding {
                                repo: None,
                                path: hash.clone(),
                                expected_hash: hash.clone(),
                                actual_hash: None,
                                error: Some("blob missing from store".to_string()),
                                fixed: false,
                            });
                            missing_blobs
                                .lock()
                                .await
                                .push((store_name.clone(), hash.clone()));
                            progress_tx.send_modify(|p| {
                                p.checked_blobs += 1;
                                p.failed_blobs += 1;
                            });
                        }
                        Err(e) => {
                            let msg = format!(
                                "Blob {} (store '{}', id {}): error opening blob — {}",
                                hash, store_name, blob_id, e
                            );
                            task_manager.append_log(task_id, msg).await;
                            findings.lock().await.push(CheckFinding {
                                repo: None,
                                path: hash.clone(),
                                expected_hash: hash.clone(),
                                actual_hash: None,
                                error: Some(format!("read error: {}", e)),
                                fixed: false,
                            });
                            progress_tx.send_modify(|p| {
                                p.checked_blobs += 1;
                                p.failed_blobs += 1;
                            });
                        }
                    }
                } else {
                    // Quick mode: bloom filter confirms existence, no I/O needed.
                    progress_tx.send_modify(|p| {
                        p.checked_blobs += 1;
                    });
                }

                let n = checked.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_multiple_of(100) {
                    let p = progress_tx.borrow().clone();
                    task_manager
                        .update_summary(
                            task_id,
                            format!(
                                "Blobs: {}/{} checked, {} failed",
                                p.checked_blobs, p.total_blobs, p.failed_blobs
                            ),
                        )
                        .await;
                }

                drop(permit);
            }));
        }
    }

    // Wait for all in-flight verification tasks to complete.
    for h in handles {
        let _ = h.await;
    }

    // Update total_blobs to reflect KV records actually checked.
    let kv_blobs_checked = checked_counter.load(Ordering::Relaxed) as usize;

    if cancel.is_cancelled() {
        task_manager
            .append_log(task_id, "Check cancelled".to_string())
            .await;
        task_manager
            .update_summary(task_id, "Cancelled".to_string())
            .await;
        task_manager.mark_cancelled(task_id).await;
        if let Err(e) = lease_guard.release().await {
            tracing::warn!("failed to release GC lease: {}", e);
        }
        return;
    }

    task_manager
        .append_log(
            task_id,
            format!(
                "Blob verification complete: {} KV records checked against {} listed blobs",
                kv_blobs_checked, actual_blob_count
            ),
        )
        .await;

    // Fix: delete dangling dedup KV entries for blobs missing from store.
    if !dry_run {
        let to_delete = missing_blobs.lock().await.clone();
        for (store, hash) in &to_delete {
            // Counted delete: the dangling dedup record was being counted in the
            // store stat though its blob is already gone — debit it on removal.
            if let Ok(true) = service::delete_blob_counted(kv.as_ref(), store, hash, &updater).await
            {
                task_manager
                    .append_log(
                        task_id,
                        format!(
                            "Fixed: deleted dangling blob KV entry {} (store: {})",
                            hash, store
                        ),
                    )
                    .await;
                let mut f = findings.lock().await;
                if let Some(finding) = f.iter_mut().rev().find(|f| {
                    f.repo.is_none()
                        && f.path == *hash
                        && f.error.as_deref() == Some("blob missing from store")
                }) {
                    finding.fixed = true;
                }
            }
        }
        let blob_fixes = to_delete.len() as u64;
        if blob_fixes > 0 {
            fixed_count.fetch_add(blob_fixes, Ordering::Relaxed);
        }
    }

    // Use the larger of the two counts as total_blobs for the result.
    total_blobs = kv_blobs_checked;

    progress_tx.send_modify(|p| {
        p.phase = "Checking repos".to_string();
    });

    // 4. Per-repo coherency checks.
    //    Use the bloom filter of blob_ids from actual blob store listings
    //    to verify artifact→blob references. Bloom filters have zero false
    //    negatives, so any blob_id not in the filter is definitively missing.
    for repo_config in &repos {
        if cancel.is_cancelled() {
            task_manager
                .append_log(task_id, "Check cancelled during repo coherency".to_string())
                .await;
            task_manager.mark_cancelled(task_id).await;
            if let Err(e) = lease_guard.release().await {
                tracing::warn!("failed to release GC lease: {}", e);
            }
            return;
        }

        let repo_name = &repo_config.name;

        // First pass: collect artifact paths (for Docker tag→manifest lookup)
        // and detect issues. Check blob references against the bloom filter
        // built from actual blob store listings.
        #[derive(Default)]
        struct RepoScanState {
            artifact_paths: std::collections::HashSet<String>,
            findings: Vec<CheckFinding>,
            log_messages: Vec<String>,
            /// Artifacts to delete in fix mode (path).
            to_delete: Vec<String>,
            /// Artifacts to rename in fix mode: (old_path, new_path, record).
            to_rename: Vec<(String, String, depot_core::store::kv::ArtifactRecord)>,
            /// Local artifact counter, flushed to progress every 1000 artifacts.
            checked: usize,
        }

        let ptx = progress_tx.clone();
        let bf_ref = &bf;
        let mut scan: RepoScanState = service::fold_all_artifacts(
            kv.as_ref(),
            repo_name,
            "",
            RepoScanState::default,
            |state, _path, record| {
                state.checked += 1;
                if state.checked % 1000 == 0 {
                    let n = state.checked;
                    ptx.send_modify(|p| {
                        p.checked_artifacts += n;
                    });
                    state.checked = 0;
                }

                state.artifact_paths.insert(record.path.clone());

                // Check artifact blob_id against bloom filter. Bloom filters
                // have zero false negatives, so if the filter says absent,
                // the blob is definitively missing from the store.
                if let Some(blob_id) = record.blob_id.as_deref() {
                    if !bf_ref.check(blob_id.as_bytes()) {
                        state.log_messages.push(format!(
                            "Repo '{}': artifact '{}' references blob_id {} which does not exist on disk",
                            repo_name, record.path, blob_id
                        ));
                        state.findings.push(CheckFinding {
                            repo: Some(repo_name.clone()),
                            path: record.path.clone(),
                            expected_hash: blob_id.to_string(),
                            actual_hash: None,
                            error: Some("artifact references missing blob".to_string()),
                            fixed: false,
                        });
                        state.to_delete.push(record.path.clone());
                    }
                }

                // Check: double-slash in path.
                if record.path.contains("//") {
                    let normalized = keys::normalize_path(&record.path).into_owned();
                    state.log_messages.push(format!(
                        "Repo '{}': artifact '{}' has double slashes in path, should be '{}'",
                        repo_name, record.path, normalized
                    ));
                    state.findings.push(CheckFinding {
                        repo: Some(repo_name.clone()),
                        path: record.path.clone(),
                        expected_hash: normalized.clone(),
                        actual_hash: None,
                        error: Some("path contains double slashes".to_string()),
                        fixed: false,
                    });
                    state
                        .to_rename
                        .push((record.path.clone(), normalized, record.clone()));
                }

                Ok(())
            },
            |mut a, b| {
                a.artifact_paths.extend(b.artifact_paths);
                a.findings.extend(b.findings);
                a.log_messages.extend(b.log_messages);
                a.to_delete.extend(b.to_delete);
                a.to_rename.extend(b.to_rename);
                a.checked += b.checked;
                a
            },
        )
        .await
        .unwrap_or_default();

        // Flush remaining artifact count to progress.
        if scan.checked > 0 {
            let remaining = scan.checked;
            progress_tx.send_modify(|p| {
                p.checked_artifacts += remaining;
            });
            scan.checked = 0;
        }

        // Flush log messages from the scan immediately.
        for msg in &scan.log_messages {
            task_manager.append_log(task_id, msg.clone()).await;
        }

        // Second pass over findings: check Docker tags → manifest references.
        // We need the full artifact_paths set to look up manifests.
        let mut docker_tag_deletes: Vec<String> = Vec::new();
        {
            // Re-scan for Docker tags — we check against the collected paths.
            let ptx2 = progress_tx.clone();
            let tag_findings: Vec<(CheckFinding, String, String)> = service::fold_all_artifacts(
                kv.as_ref(),
                repo_name,
                "",
                Vec::new,
                |acc, _path, record| {
                    // Don't double-count progress — this is a lightweight second pass.
                    let _ = &ptx2;
                    if let ArtifactKind::DockerTag { ref digest, .. } = record.kind {
                        // Derive manifest path from tag path.
                        // Tag path: "_tags/{tag}" or "{image}/_tags/{tag}"
                        // Manifest path: "_manifests/{digest}" or "{image}/_manifests/{digest}"
                        let manifest_path = if let Some(prefix) = record.path.rsplit_once("/_tags/")
                        {
                            format!("{}/_manifests/{}", prefix.0, digest)
                        } else if record.path.starts_with("_tags/") {
                            format!("_manifests/{}", digest)
                        } else {
                            // Unexpected path format — skip.
                            return Ok(());
                        };

                        if !scan.artifact_paths.contains(&manifest_path) {
                            let msg = format!(
                                "Repo '{}': docker tag '{}' references manifest {} which does not exist",
                                repo_name, record.path, digest
                            );
                            acc.push((
                                CheckFinding {
                                    repo: Some(repo_name.clone()),
                                    path: record.path.clone(),
                                    expected_hash: digest.clone(),
                                    actual_hash: None,
                                    error: Some(
                                        "docker tag references missing manifest".to_string(),
                                    ),
                                    fixed: false,
                                },
                                record.path.clone(),
                                msg,
                            ));
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
            .unwrap_or_default();

            for (finding, path, msg) in tag_findings {
                task_manager.append_log(task_id, msg).await;
                findings.lock().await.push(finding);
                docker_tag_deletes.push(path);
            }
        }

        // Append repo-level findings.
        if !scan.findings.is_empty() {
            findings.lock().await.extend(scan.findings);
        }

        // Apply fixes if not in dry-run mode.
        if !dry_run {
            // Fix: delete artifacts referencing unknown blobs.
            for path in &scan.to_delete {
                if let Ok(Some(_)) = service::delete_artifact(kv.as_ref(), repo_name, path).await {
                    task_manager
                        .append_log(
                            task_id,
                            format!(
                                "Fixed: deleted artifact {}/{} (missing blob)",
                                repo_name, path
                            ),
                        )
                        .await;
                    // Mark the corresponding finding as fixed.
                    let mut f = findings.lock().await;
                    if let Some(finding) = f.iter_mut().rev().find(|f| {
                        f.repo.as_deref() == Some(repo_name)
                            && f.path == *path
                            && f.error.as_deref() == Some("artifact references unknown blob")
                    }) {
                        finding.fixed = true;
                    }
                    fixed_count.fetch_add(1, Ordering::Relaxed);
                }
            }

            // Fix: delete dangling Docker tags.
            for path in &docker_tag_deletes {
                if let Ok(Some(_)) = service::delete_artifact(kv.as_ref(), repo_name, path).await {
                    task_manager
                        .append_log(
                            task_id,
                            format!(
                                "Fixed: deleted docker tag {}/{} (missing manifest)",
                                repo_name, path
                            ),
                        )
                        .await;
                    let mut f = findings.lock().await;
                    if let Some(finding) = f.iter_mut().rev().find(|f| {
                        f.repo.as_deref() == Some(repo_name)
                            && f.path == *path
                            && f.error.as_deref() == Some("docker tag references missing manifest")
                    }) {
                        finding.fixed = true;
                    }
                    fixed_count.fetch_add(1, Ordering::Relaxed);
                }
            }

            // Fix: rename double-slash paths.
            for (old_path, new_path, mut record) in scan.to_rename {
                // Skip if this artifact was already deleted (e.g. missing blob).
                if scan.to_delete.contains(&old_path) {
                    continue;
                }
                // Delete the old entry.
                if service::delete_artifact(kv.as_ref(), repo_name, &old_path)
                    .await
                    .is_ok()
                {
                    // Check if the normalized path already exists.
                    let existing = service::get_artifact(kv.as_ref(), repo_name, &new_path).await;
                    if matches!(existing, Ok(Some(_))) {
                        // Normalized path already occupied — just delete the malformed one.
                        task_manager
                            .append_log(
                                task_id,
                                format!(
                                    "Fixed: deleted malformed path {}/{} (normalized path already exists)",
                                    repo_name, old_path
                                ),
                            )
                            .await;
                    } else {
                        // Re-insert at normalized path.
                        record.path = new_path.clone();
                        if let Err(e) =
                            service::put_artifact(kv.as_ref(), repo_name, &new_path, &record).await
                        {
                            task_manager
                                .append_log(
                                    task_id,
                                    format!(
                                        "Warning: failed to re-insert {}/{}: {}",
                                        repo_name, new_path, e
                                    ),
                                )
                                .await;
                        } else {
                            task_manager
                                .append_log(
                                    task_id,
                                    format!(
                                        "Fixed: renamed {}/{} -> {}",
                                        repo_name, old_path, new_path
                                    ),
                                )
                                .await;
                        }
                    }
                    let mut f = findings.lock().await;
                    if let Some(finding) = f.iter_mut().rev().find(|f| {
                        f.repo.as_deref() == Some(repo_name)
                            && f.path == old_path
                            && f.error.as_deref() == Some("path contains double slashes")
                    }) {
                        finding.fixed = true;
                    }
                    fixed_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        if repo_config.format() == ArtifactFormat::Helm {
            check_helm_canonicalization(
                kv.as_ref(),
                repo_config,
                dry_run,
                task_id,
                task_manager.as_ref(),
                &findings,
                &fixed_count,
            )
            .await;
        }

        if let depot_core::store::kv::RepoKind::Cache {
            ref upstream_url, ..
        } = repo_config.kind
        {
            if upstream_url.is_empty() {
                task_manager
                    .append_log(
                        task_id,
                        format!("Repo '{}': cache repo has empty upstream_url", repo_name),
                    )
                    .await;
                findings.lock().await.push(CheckFinding {
                    repo: Some(repo_name.clone()),
                    path: String::new(),
                    expected_hash: String::new(),
                    actual_hash: None,
                    error: Some("cache repo has empty upstream_url".to_string()),
                    fixed: false,
                });
            }
        }

        if let depot_core::store::kv::RepoKind::Proxy { ref members, .. } = repo_config.kind {
            let repo_names: std::collections::HashSet<&str> =
                repos.iter().map(|r| r.name.as_str()).collect();
            for member in members {
                if !repo_names.contains(member.as_str()) {
                    task_manager
                        .append_log(
                            task_id,
                            format!(
                                "Repo '{}': proxy member '{}' does not exist",
                                repo_name, member
                            ),
                        )
                        .await;
                    findings.lock().await.push(CheckFinding {
                        repo: Some(repo_name.clone()),
                        path: String::new(),
                        expected_hash: String::new(),
                        actual_hash: None,
                        error: Some(format!("proxy member '{}' does not exist", member)),
                        fixed: false,
                    });
                }
            }
        }

        progress_tx.send_modify(|p| {
            p.completed_repos += 1;
        });
    }

    // 5. Aggregate results.
    let all_findings = findings.lock().await.clone();
    let progress = progress_tx.borrow().clone();

    let missing = all_findings
        .iter()
        .filter(|f| {
            f.actual_hash.is_none() && f.error.as_deref() == Some("blob missing from store")
        })
        .count();
    let mismatched = all_findings
        .iter()
        .filter(|f| f.actual_hash.is_some() && f.error.is_none())
        .count();
    let errors = all_findings.len() - missing - mismatched;

    let total_fixed = fixed_count.load(Ordering::Relaxed) as usize;

    let result = CheckResult {
        total_blobs,
        passed: total_blobs.saturating_sub(
            missing
                + mismatched
                + all_findings
                    .iter()
                    .filter(|f| {
                        f.repo.is_none()
                            && f.error.is_some()
                            && f.error.as_deref() != Some("blob missing from store")
                    })
                    .count(),
        ),
        mismatched,
        missing,
        errors,
        findings: all_findings,
        bytes_read: progress.bytes_read,
        dry_run,
        fixed: total_fixed,
    };

    info!(
        total = result.total_blobs,
        passed = result.passed,
        mismatched = result.mismatched,
        missing = result.missing,
        errors = result.errors,
        fixed = result.fixed,
        dry_run = result.dry_run,
        bytes_read = result.bytes_read,
        "integrity check completed"
    );

    task_manager
        .append_log(
            task_id,
            format!(
                "Check complete ({}): {} blobs, {} passed, {} mismatched, {} missing, {} errors, {} fixed, {} bytes",
                mode_label,
                result.total_blobs,
                result.passed,
                result.mismatched,
                result.missing,
                result.errors,
                result.fixed,
                result.bytes_read
            ),
        )
        .await;
    let summary_suffix = if total_fixed > 0 {
        format!(", {} fixed", total_fixed)
    } else {
        String::new()
    };
    task_manager
        .update_summary(
            task_id,
            format!(
                "Complete ({}): {} passed, {} mismatched, {} missing, {} errors{}",
                mode_label,
                result.passed,
                result.mismatched,
                result.missing,
                result.errors,
                summary_suffix
            ),
        )
        .await;

    // 6. Release GC lease and mark completed.
    let _ = lease_guard.release().await;
    task_manager
        .mark_completed(task_id, TaskResult::Check(result))
        .await;
}

/// Find chart records still at a legacy storage prefix (`_charts/...` or
/// `charts/...`) and (in fix mode) relocate them to the canonical repo root
/// `{name}-{version}.tgz`. On any successful relocation, set the helm
/// metadata-stale flag so the next index fetch rebuilds.
async fn check_helm_canonicalization(
    kv: &dyn KvStore,
    repo_config: &depot_core::store::kv::RepoConfig,
    dry_run: bool,
    task_id: TaskId,
    task_manager: &TaskManager,
    findings: &Arc<tokio::sync::Mutex<Vec<CheckFinding>>>,
    fixed_count: &AtomicU64,
) {
    let repo_name = &repo_config.name;
    let plan = match depot_format_helm::enumerate_legacy_chart_relocations(kv, repo_name).await {
        Ok(p) => p,
        Err(e) => {
            task_manager
                .append_log(
                    task_id,
                    format!(
                        "Warning: helm legacy-chart enumeration failed for '{}': {}",
                        repo_name, e
                    ),
                )
                .await;
            return;
        }
    };

    if plan.is_empty() {
        return;
    }

    {
        let mut f = findings.lock().await;
        for (old, new, _) in &plan {
            f.push(CheckFinding {
                repo: Some(repo_name.clone()),
                path: old.clone(),
                expected_hash: new.clone(),
                actual_hash: None,
                error: Some("helm chart at legacy chart path".to_string()),
                fixed: false,
            });
        }
    }

    for (old, new, _) in &plan {
        task_manager
            .append_log(
                task_id,
                format!(
                    "Repo '{}': helm chart at '{}' should be at '{}'",
                    repo_name, old, new
                ),
            )
            .await;
    }

    if dry_run {
        return;
    }

    let mut applied: u64 = 0;
    for (old_path, new_path, record) in &plan {
        if let Err(e) = service::put_artifact(kv, repo_name, new_path, record).await {
            task_manager
                .append_log(
                    task_id,
                    format!("Warning: write {}/{}: {}", repo_name, new_path, e),
                )
                .await;
            continue;
        }
        if let Err(e) = service::delete_artifact(kv, repo_name, old_path).await {
            // The new copy is in place; the next pass will retry the delete.
            task_manager
                .append_log(
                    task_id,
                    format!(
                        "Warning: delete legacy {}/{}: {} (new copy at {} is in place)",
                        repo_name, old_path, e, new_path
                    ),
                )
                .await;
        }
        task_manager
            .append_log(
                task_id,
                format!(
                    "Fixed: relocated {}/{} -> {}",
                    repo_name, old_path, new_path
                ),
            )
            .await;

        let mut f = findings.lock().await;
        if let Some(finding) = f.iter_mut().rev().find(|f| {
            f.repo.as_deref() == Some(repo_name)
                && f.path == *old_path
                && f.error.as_deref() == Some("helm chart at legacy chart path")
        }) {
            finding.fixed = true;
        }
        applied += 1;
    }

    if applied > 0 {
        if let Err(e) = depot_format_helm::set_stale_flag(kv, repo_name).await {
            task_manager
                .append_log(
                    task_id,
                    format!("Warning: set helm stale flag on '{}': {}", repo_name, e),
                )
                .await;
        }
        fixed_count.fetch_add(applied, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::infra::task::{TaskKind, TaskManager, TaskStatus};
    use crate::server::store::BlobStore;
    use depot_blob_file::FileBlobStore;
    use depot_core::error::Retryability;
    use depot_core::store::kv::*;
    use depot_core::store::test_mock::{FailingKvStore, KvOp};
    use depot_core::store_registry::StoreRegistry;
    use depot_kv_redb::ShardedRedbKvStore;

    /// Create a test environment with a FailingKvStore wrapping real storage.
    async fn setup_failing_check_env() -> (
        Arc<FailingKvStore>,
        Arc<StoreRegistry>,
        Arc<TaskManager>,
        tempfile::TempDir,
    ) {
        let dir = crate::server::infra::test_support::test_tempdir();
        let real_kv = Arc::new(
            ShardedRedbKvStore::open(&dir.path().join("kv"), 2, 0)
                .await
                .unwrap(),
        ) as Arc<dyn KvStore>;
        let blobs = Arc::new(
            FileBlobStore::new(&dir.path().join("blobs"), false, 1_048_576, false).unwrap(),
        ) as Arc<dyn BlobStore>;
        let registry = Arc::new(StoreRegistry::new());
        registry.add("default", blobs).await;

        let now = chrono::Utc::now();
        depot_core::service::put_store(
            real_kv.as_ref(),
            &StoreRecord {
                schema_version: CURRENT_RECORD_VERSION,
                name: "default".to_string(),
                kind: StoreKind::File {
                    root: dir.path().join("blobs").to_string_lossy().to_string(),
                    sync: false,
                    io_size: 1024,
                    direct_io: false,
                },
                created_at: now,
            },
        )
        .await
        .unwrap();

        let failing_kv = FailingKvStore::wrap(real_kv);
        let task_manager = Arc::new(TaskManager::new(
            failing_kv.clone() as Arc<dyn KvStore>,
            "check-test-inst".to_string(),
        ));
        (failing_kv, registry, task_manager, dir)
    }

    /// Run check with the given FailingKvStore and return the task status + error.
    async fn run_check_and_get_result(
        kv: Arc<FailingKvStore>,
        stores: Arc<StoreRegistry>,
        task_manager: Arc<TaskManager>,
    ) -> (TaskStatus, Option<String>) {
        let (task_id, progress_tx, cancel) = task_manager.create(TaskKind::Check).await;
        run_check(
            task_id,
            task_manager.clone(),
            kv as Arc<dyn KvStore>,
            stores,
            "test-instance".to_string(),
            progress_tx,
            cancel,
            true, // dry_run
            true, // verify_blob_hashes
            depot_core::update::UpdateSender::noop(),
        )
        .await;
        let tasks = task_manager.list().await;
        let task = tasks.iter().find(|t| t.id == task_id).unwrap();
        (task.status, task.error.clone())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn check_lease_transient_error_retries_and_succeeds() {
        let (kv, stores, tm, _dir) = setup_failing_check_env().await;

        // Fail only the 1st get_versioned on leases table (transient).
        // The retry logic should succeed on attempt 2.
        kv.fail_on_table(
            KvOp::GetVersioned,
            "leases",
            Some(1),
            Retryability::Transient,
        );

        let (status, _) = run_check_and_get_result(kv, stores, tm).await;
        assert_eq!(
            status,
            TaskStatus::Completed,
            "check should succeed after transient lease error retries"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn check_lease_permanent_error_fails_immediately() {
        let (kv, stores, tm, _dir) = setup_failing_check_env().await;

        // Fail ALL get_versioned on leases table with permanent error.
        kv.fail_on_table(KvOp::GetVersioned, "leases", None, Retryability::Permanent);

        let (status, error) = run_check_and_get_result(kv, stores, tm).await;
        assert_eq!(status, TaskStatus::Failed);
        assert!(
            error.unwrap().contains("GC lease"),
            "error should mention GC lease"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn check_fold_blobs_error() {
        let (kv, stores, tm, _dir) = setup_failing_check_env().await;

        // Let lease succeed, then fail scan_range on blobs table.
        kv.fail_on_table(KvOp::ScanRange, "blobs", None, Retryability::Permanent);

        let (status, _error) = run_check_and_get_result(kv, stores, tm).await;
        // Blob scan errors during verification are non-fatal — the check
        // completes with zero blobs verified (scan partitions that fail
        // are skipped with a warning).
        assert_eq!(status, TaskStatus::Completed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn check_list_repos_error() {
        let (kv, stores, tm, _dir) = setup_failing_check_env().await;

        // Let lease + blob fold succeed, then fail scan_prefix on repos table.
        kv.fail_on_table(KvOp::ScanPrefix, "repos", None, Retryability::Permanent);

        let (status, error) = run_check_and_get_result(kv, stores, tm).await;
        assert_eq!(status, TaskStatus::Failed);
        assert!(
            error.unwrap().contains("Failed to list repos"),
            "should fail on list_repos"
        );
    }
}
