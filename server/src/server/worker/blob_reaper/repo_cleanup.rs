// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

// ---------------------------------------------------------------------------
// Per-repo artifact cleanup
// ---------------------------------------------------------------------------

use std::borrow::Cow;
use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::server::infra::task::TaskProgress;
use depot_core::store::keys;
use depot_core::store::kv::{KvStore, TreeEntry};
use depot_core::store_registry::StoreRegistry;
use depot_core::update::UpdateSender;

use super::docker_gc::docker_gc;

pub struct RepoCleanStats {
    pub scanned_artifacts: u64,
    pub expired_artifacts: u64,
    pub docker_deleted: u64,
}

/// Read the authoritative last-accessed time for an artifact from its
/// browse-tree file entry (`TABLE_DIR_ENTRIES`). Returns `None` if the entry is
/// missing or unreadable, so the caller can fall back to the record's value.
async fn tree_last_accessed_at(
    kv: &dyn KvStore,
    repo: &str,
    path: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let (table, pk, sk) = keys::tree_entry_key(repo, path);
    let (bytes, _) = kv.get_versioned(table, pk, sk).await.ok()??;
    let entry: TreeEntry = rmp_serde::from_slice(&bytes).ok()?;
    Some(entry.last_accessed_at)
}

/// Decide whether an artifact record is expired under a repo's cleanup policy.
/// Shared by the GC pass and the per-repo cleanup sweep so the two can't drift.
///
/// `max_age` compares the record's creation time. `max_unaccessed` compares the
/// **authoritative** access time, which lives on the browse-tree file entry
/// (refreshed on download), not the record's `last_accessed_at` (frozen at write
/// time) — falling back to the record only if the tree entry is missing.
/// Internal artifacts and Docker bookkeeping paths are never expired here.
pub(super) async fn record_expired(
    kv: &dyn KvStore,
    repo: &str,
    sk: &str,
    record: &depot_core::store::kv::ArtifactRecord,
    is_docker: bool,
    max_age_cutoff: Option<chrono::DateTime<chrono::Utc>>,
    max_unaccessed_cutoff: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    if record.internal || (is_docker && depot_format_docker::store::is_bookkeeping_path(sk)) {
        return false;
    }
    let age_expired = max_age_cutoff
        .map(|c| record.created_at < c)
        .unwrap_or(false);
    let unaccessed_expired = match max_unaccessed_cutoff {
        Some(c) => {
            tree_last_accessed_at(kv, repo, sk)
                .await
                .unwrap_or(record.last_accessed_at)
                < c
        }
        None => false,
    };
    age_expired || unaccessed_expired
}

/// Expire artifacts in a single repository based on its cleanup policy
/// (`cleanup_max_age_days` / `cleanup_max_unaccessed_days`).
///
/// Returns `(scanned_artifacts, scanned_bytes, expired_artifacts)`.
/// Does *not* run Docker GC or touch the blob store.
pub(super) async fn expire_repo_artifacts(
    kv: Arc<dyn KvStore>,
    repo: &depot_core::store::kv::RepoConfig,
    cancel: Option<&CancellationToken>,
    updater: &UpdateSender,
) -> depot_core::error::Result<(u64, u64, u64)> {
    let now = chrono::Utc::now();
    let max_age_cutoff = repo
        .cleanup_max_age_days
        .map(|d| now - chrono::Duration::days(d as i64));
    let max_unaccessed_cutoff = repo
        .cleanup_max_unaccessed_days
        .map(|d| now - chrono::Duration::days(d as i64));

    if max_age_cutoff.is_none() && max_unaccessed_cutoff.is_none() {
        return Ok((0, 0, 0));
    }

    let repo_name = &repo.name;
    let store_name = &repo.store;
    let is_docker = repo.format() == depot_core::store::kv::ArtifactFormat::Docker;
    let mut js = JoinSet::new();
    for shard in 0..keys::NUM_SHARDS {
        let kv = kv.clone();
        let repo_name = repo_name.clone();
        let store_name = store_name.clone();
        let cancel = cancel.cloned();
        let updater = updater.clone();
        js.spawn(async move {
            let pk = keys::artifact_shard_pk(&repo_name, shard);
            let mut local_scanned = 0u64;
            let mut local_scanned_bytes = 0u64;
            let mut local_expired = 0u64;

            let mut cursor: Option<String> = None;
            loop {
                if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                    return Err(depot_core::error::DepotError::Internal(
                        "Repo clean cancelled".to_string(),
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
                    let record =
                        match rmp_serde::from_slice::<depot_core::store::kv::ArtifactRecord>(value)
                        {
                            Ok(r) => r,
                            Err(_) => continue,
                        };

                    local_scanned_bytes += record.size;

                    let expired = record_expired(
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
                        local_scanned += 1;
                    }
                }
                if !expired_entries.is_empty() {
                    let paths: Vec<&str> = expired_entries.iter().map(|(sk, _)| *sk).collect();
                    depot_core::service::delete_artifacts_paired_batch(
                        kv.as_ref(),
                        &repo_name,
                        &paths,
                    )
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
                        .store_changed(&store_name, -(expired_entries.len() as i64), -batch_bytes)
                        .await;
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
                local_expired,
            ))
        });
    }
    let mut shard_results = Vec::new();
    while let Some(res) = js.join_next().await {
        shard_results.push(res??);
    }

    let mut scanned_artifacts = 0u64;
    let mut scanned_bytes = 0u64;
    let mut expired_artifacts = 0u64;
    for (local_scanned, local_bytes, local_expired) in shard_results {
        scanned_artifacts += local_scanned;
        scanned_bytes += local_bytes;
        expired_artifacts += local_expired;
    }

    Ok((scanned_artifacts, scanned_bytes, expired_artifacts))
}

/// Expire artifacts in a single repository based on its cleanup policy
/// (`cleanup_max_age_days` / `cleanup_max_unaccessed_days`) and run
/// Docker-specific GC if applicable. Does *not* touch the blob store or
/// orphan candidates — that remains the responsibility of the full GC pass.
pub async fn clean_repo_artifacts(
    kv: Arc<dyn KvStore>,
    stores: &Arc<StoreRegistry>,
    repo: &depot_core::store::kv::RepoConfig,
    cancel: Option<&CancellationToken>,
    progress_tx: Option<&watch::Sender<TaskProgress>>,
    updater: &UpdateSender,
) -> depot_core::error::Result<RepoCleanStats> {
    let (scanned_artifacts, scanned_bytes, expired_artifacts) =
        expire_repo_artifacts(kv.clone(), repo, cancel, updater).await?;

    if let Some(tx) = progress_tx {
        let arts = scanned_artifacts as usize;
        let bytes = scanned_bytes;
        tx.send_modify(|p| {
            p.checked_blobs = arts;
            p.bytes_read = bytes;
        });
    }

    if cancel.is_some_and(|c| c.is_cancelled()) {
        return Err(depot_core::error::DepotError::Internal(
            "Repo clean cancelled".to_string(),
        ));
    }

    // Docker-specific GC for this repo.
    let mut docker_deleted = 0u64;
    if repo.format() == depot_core::store::kv::ArtifactFormat::Docker {
        docker_deleted = docker_gc(kv.clone(), stores, repo, false, cancel, updater).await?;
    }

    Ok(RepoCleanStats {
        scanned_artifacts,
        expired_artifacts,
        docker_deleted,
    })
}
