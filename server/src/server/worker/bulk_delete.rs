// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

// ---------------------------------------------------------------------------
// Bulk directory delete worker
// ---------------------------------------------------------------------------

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::server::infra::task::{BulkDeleteResult, TaskProgress};
use depot_core::error::{self, DepotError};
use depot_core::store::keys;
use depot_core::store::kv::{ArtifactRecord, KvStore, TreeEntryKind};
use depot_core::update::UpdateSender;

/// Offset for progress reporting when processing multiple repos sequentially,
/// allowing the progress bar to accumulate across members.
pub struct ProgressOffset {
    pub artifacts: u64,
    pub bytes: u64,
}

/// Delete all artifacts and tree entries under `prefix` in the given repo.
///
/// Ordering:
///   1. Delete tree entries (file + dir) in descending depth order so the
///      directory vanishes from the browse UI immediately.
///   2. Delete the actual artifact records (no dir-entry update events).
///   3. Send a single aggregate dir + store delta for the whole operation.
#[allow(clippy::too_many_arguments)]
pub async fn bulk_delete(
    kv: Arc<dyn KvStore>,
    repo: &str,
    prefix: &str,
    updater: &UpdateSender,
    cancel: Option<&CancellationToken>,
    progress_tx: Option<&watch::Sender<TaskProgress>>,
    progress_offset: &ProgressOffset,
) -> error::Result<BulkDeleteResult> {
    let start = Instant::now();
    let prefix_with_slash = if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{}/", prefix)
    };

    // ------------------------------------------------------------------
    // Phase 1: Delete tree entries (files and dirs) deepest-first.
    //
    // This makes the directory disappear from the browse UI immediately
    // and avoids the update worker processing stale dir entries.
    // ------------------------------------------------------------------
    if let Some(tx) = progress_tx {
        tx.send_modify(|p| {
            p.phase = "Removing directory entries".to_string();
        });
    }

    delete_tree_recursive(&kv, repo, prefix).await?;

    if cancel.is_some_and(|c| c.is_cancelled()) {
        return Err(DepotError::Internal("Bulk delete cancelled".to_string()));
    }

    // ------------------------------------------------------------------
    // Phase 2: Delete artifact records across all 256 shards.
    //
    // No dir_changed / store_changed events are sent per artifact — we
    // send a single aggregate delta at the end.
    // ------------------------------------------------------------------
    if let Some(tx) = progress_tx {
        tx.send_modify(|p| {
            p.phase = "Counting artifacts".to_string();
        });
    }

    // First pass: count so we can report progress.
    let estimated = count_prefix_artifacts(&kv, repo, &prefix_with_slash, cancel).await?;

    if let Some(tx) = progress_tx {
        let offset = progress_offset;
        tx.send_modify(|p| {
            p.total_artifacts = (offset.artifacts + estimated.count) as usize;
            p.total_bytes = offset.bytes + estimated.bytes;
            p.phase = "Deleting artifacts".to_string();
        });
    }

    if cancel.is_some_and(|c| c.is_cancelled()) {
        return Err(DepotError::Internal("Bulk delete cancelled".to_string()));
    }

    // Second pass: delete.
    let deleted = delete_prefix_artifacts(
        &kv,
        repo,
        &prefix_with_slash,
        cancel,
        progress_tx,
        progress_offset.artifacts,
    )
    .await?;

    // ------------------------------------------------------------------
    // Phase 3: Send a single aggregate delta to the update worker.
    // ------------------------------------------------------------------
    if deleted.count > 0 {
        // dir_changed takes a *file* path and derives the parent dir.
        // We pass a synthetic child path under the prefix so the delta
        // targets the prefix's parent directory.
        let synthetic_child = format!("{}__bulk_delete_sentinel", prefix_with_slash);
        updater
            .dir_changed(
                repo,
                &synthetic_child,
                -(deleted.count as i64),
                -(deleted.bytes as i64),
            )
            .await;
        // No store_changed: bulk delete removes artifact records, not blobs —
        // the orphaned blobs are reclaimed (and store stats recomputed) by GC.
    }

    Ok(BulkDeleteResult {
        repo: repo.to_string(),
        prefix: prefix.to_string(),
        deleted_artifacts: deleted.count,
        deleted_bytes: deleted.bytes,
        elapsed_ns: start.elapsed().as_nanos() as u64,
    })
}

// ---------------------------------------------------------------------------
// Tree entry deletion (phase 1)
// ---------------------------------------------------------------------------

/// Recursively delete all tree entries (files and dirs) under `dir_path`,
/// processing children before the parent (deepest-first / post-order).
async fn delete_tree_recursive(
    kv: &Arc<dyn KvStore>,
    repo: &str,
    dir_path: &str,
) -> error::Result<()> {
    let clean = dir_path.trim_end_matches('/');
    let pk = keys::tree_children_pk(repo, clean);
    let sk_prefix = keys::tree_children_sk_prefix(clean);
    let sk_end = keys::prefix_end(sk_prefix.as_bytes()).and_then(|b| String::from_utf8(b).ok());

    let mut child_dirs: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;

    // Scan immediate children, deleting file entries and collecting dirs.
    loop {
        let start = match &cursor {
            Some(c) => c.as_str(),
            None => sk_prefix.as_str(),
        };
        let result = kv
            .scan_range(
                keys::TABLE_DIR_ENTRIES,
                Cow::Borrowed(pk.as_str()),
                Cow::Borrowed(start),
                sk_end.as_deref(),
                1000,
            )
            .await?;

        let mut file_del_keys: Vec<(String, String)> = Vec::new();

        for (sk, value) in &result.items {
            if !sk.starts_with(&sk_prefix) {
                break;
            }
            let name = keys::tree_sk_name(sk);
            if let Ok(te) = rmp_serde::from_slice::<depot_core::store::kv::TreeEntry>(value) {
                match te.kind {
                    TreeEntryKind::Dir { .. } => {
                        let child_path = if clean.is_empty() {
                            name.to_string()
                        } else {
                            format!("{}/{}", clean, name)
                        };
                        child_dirs.push(child_path);
                    }
                    TreeEntryKind::File { .. } => {
                        file_del_keys.push((pk.clone(), sk.clone()));
                    }
                }
            }
        }

        // Batch-delete file entries in this partition.
        if !file_del_keys.is_empty() {
            let refs: Vec<(&str, &str)> = file_del_keys
                .iter()
                .map(|(p, s)| (p.as_str(), s.as_str()))
                .collect();
            let _ = kv.delete_batch(keys::TABLE_DIR_ENTRIES, &refs).await;
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

    // Recurse into child directories (deepest-first).
    for child in child_dirs {
        Box::pin(delete_tree_recursive(kv, repo, &child)).await?;
    }

    // Delete this directory's own entry.
    let (table, dpk, dsk) = keys::tree_entry_key(repo, clean);
    let _ = kv.delete(table, dpk, dsk).await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Artifact counting + deletion (phase 2)
// ---------------------------------------------------------------------------

struct ArtifactTotals {
    count: u64,
    bytes: u64,
}

/// Count artifacts matching `prefix` across all shards.
async fn count_prefix_artifacts(
    kv: &Arc<dyn KvStore>,
    repo: &str,
    prefix: &str,
    cancel: Option<&CancellationToken>,
) -> error::Result<ArtifactTotals> {
    let count = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));

    let mut js = JoinSet::new();
    for shard in 0..keys::NUM_SHARDS {
        let kv = kv.clone();
        let repo = repo.to_string();
        let pfx = prefix.to_string();
        let count = count.clone();
        let bytes = bytes.clone();
        let cancel = cancel.cloned();

        js.spawn(async move {
            let pk = keys::artifact_shard_pk(&repo, shard);
            let mut cursor: Option<String> = None;
            loop {
                if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                    return Err(DepotError::Internal("Bulk delete cancelled".to_string()));
                }
                let start = match &cursor {
                    Some(c) => c.as_str(),
                    None => pfx.as_str(),
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

                for (sk, value) in &result.items {
                    if !sk.starts_with(&pfx) {
                        return Ok(());
                    }
                    if let Ok(record) = rmp_serde::from_slice::<ArtifactRecord>(value) {
                        count.fetch_add(1, Ordering::Relaxed);
                        bytes.fetch_add(record.size, Ordering::Relaxed);
                    }
                }

                if result.done {
                    return Ok(());
                }
                if let Some((last_sk, _)) = result.items.last() {
                    cursor = Some(format!("{}\0", last_sk));
                } else {
                    return Ok(());
                }
                tokio::task::yield_now().await;
            }
        });
    }
    while let Some(res) = js.join_next().await {
        res.map_err(|e| DepotError::Internal(format!("join error: {e}")))??;
    }

    Ok(ArtifactTotals {
        count: count.load(Ordering::Relaxed),
        bytes: bytes.load(Ordering::Relaxed),
    })
}

/// Delete all artifact records matching `prefix` across all shards.
/// Does NOT emit per-artifact dir/store events.
/// `checked_offset` is added to the running deleted count for progress reporting.
async fn delete_prefix_artifacts(
    kv: &Arc<dyn KvStore>,
    repo: &str,
    prefix: &str,
    cancel: Option<&CancellationToken>,
    progress_tx: Option<&watch::Sender<TaskProgress>>,
    checked_offset: u64,
) -> error::Result<ArtifactTotals> {
    let deleted_count = Arc::new(AtomicU64::new(0));
    let deleted_bytes = Arc::new(AtomicU64::new(0));

    let mut js = JoinSet::new();
    for shard in 0..keys::NUM_SHARDS {
        let kv = kv.clone();
        let repo = repo.to_string();
        let pfx = prefix.to_string();
        let deleted_count = deleted_count.clone();
        let deleted_bytes = deleted_bytes.clone();
        let cancel = cancel.cloned();
        let progress_tx = progress_tx.cloned();

        js.spawn(async move {
            let pk = keys::artifact_shard_pk(&repo, shard);
            let mut cursor: Option<String> = None;

            loop {
                if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                    return Err(DepotError::Internal("Bulk delete cancelled".to_string()));
                }

                let start = match &cursor {
                    Some(c) => c.as_str(),
                    None => pfx.as_str(),
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

                let mut del_keys: Vec<(String, String)> = Vec::new();
                let mut batch_bytes = 0u64;

                for (sk, value) in &result.items {
                    if !sk.starts_with(&pfx) {
                        break;
                    }
                    if let Ok(record) = rmp_serde::from_slice::<ArtifactRecord>(value) {
                        del_keys.push((pk.clone(), sk.clone()));
                        batch_bytes += record.size;
                    }
                }

                let past_prefix = result
                    .items
                    .last()
                    .map(|(sk, _)| !sk.starts_with(&pfx))
                    .unwrap_or(false);

                if !del_keys.is_empty() {
                    let batch_count = del_keys.len() as u64;
                    let refs: Vec<(&str, &str)> = del_keys
                        .iter()
                        .map(|(p, s)| (p.as_str(), s.as_str()))
                        .collect();
                    let _ = kv.delete_batch(keys::TABLE_ARTIFACTS, &refs).await;

                    deleted_count.fetch_add(batch_count, Ordering::Relaxed);
                    deleted_bytes.fetch_add(batch_bytes, Ordering::Relaxed);

                    if let Some(ref tx) = progress_tx {
                        let total_deleted = checked_offset + deleted_count.load(Ordering::Relaxed);
                        tx.send_modify(|p| {
                            p.checked_artifacts = total_deleted as usize;
                        });
                    }
                }

                if result.done || past_prefix {
                    break;
                }
                if let Some((last_sk, _)) = result.items.last() {
                    cursor = Some(format!("{}\0", last_sk));
                } else {
                    break;
                }
                tokio::task::yield_now().await;
            }
            Ok::<_, DepotError>(())
        });
    }
    while let Some(res) = js.join_next().await {
        res.map_err(|e| DepotError::Internal(format!("join error: {e}")))??;
    }

    Ok(ArtifactTotals {
        count: deleted_count.load(Ordering::Relaxed),
        bytes: deleted_bytes.load(Ordering::Relaxed),
    })
}
