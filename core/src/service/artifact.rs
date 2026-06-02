// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Artifact CRUD, listing, search, collection, and access tracking.

use chrono::{DateTime, Utc};

use crate::error::{self, DepotError};
use crate::store::keys;
use crate::store::kv::*;

use super::pagination::{decode_cursor, k_way_merge, paginate_results, ShardStream};
use super::scan::{for_each_in_partition, per_shard_batch, SCAN_BATCH_SIZE, SHARD_CONCURRENCY};
use super::typed::{typed_get, typed_get_versioned};

// --- Tree file entry helpers ---

/// Upsert a `TreeEntry::File` in the browse tree for the given artifact.
async fn upsert_tree_file(kv: &dyn KvStore, repo: &str, path: &str, record: &ArtifactRecord) {
    let entry = TreeEntry {
        kind: TreeEntryKind::File {
            size: record.size,
            content_type: record.content_type.clone(),
            artifact_kind: Box::new(record.kind.clone()),
            blob_id: record.blob_id.clone(),
            content_hash: record.content_hash.clone(),
            created_at: record.created_at,
        },
        last_modified_at: record.updated_at,
        last_accessed_at: record.last_accessed_at,
    };
    if let Ok(bytes) = rmp_serde::to_vec(&entry) {
        let (table, pk, sk) = keys::tree_entry_key(repo, path);
        let _ = kv.put(table, pk, sk, &bytes).await;
    }
}

/// Delete the browse tree file entry for the given artifact path.
async fn delete_tree_file(kv: &dyn KvStore, repo: &str, path: &str) {
    let (table, pk, sk) = keys::tree_entry_key(repo, path);
    let _ = kv.delete(table, pk, sk).await;
}

// --- Artifact CRUD ---

/// Maximum retries for conditional write conflicts in `put_artifact`.
const PUT_ARTIFACT_MAX_RETRIES: usize = 3;

#[tracing::instrument(level = "debug", skip(kv, record), fields(repo, path))]
pub async fn put_artifact(
    kv: &dyn KvStore,
    repo: &str,
    path: &str,
    record: &ArtifactRecord,
) -> error::Result<Option<ArtifactRecord>> {
    if path.is_empty() {
        return Err(DepotError::BadRequest("empty artifact path".to_string()));
    }

    for attempt in 0..PUT_ARTIFACT_MAX_RETRIES {
        let (table, pk, sk) = keys::artifact_key(repo, path);

        // Read existing record with version stamp.
        let (old_record, expected_version): (Option<ArtifactRecord>, Option<u64>) =
            match typed_get_versioned(kv, table, pk.clone(), sk.clone()).await? {
                Some((rec, version)) => (Some(rec), Some(version)),
                None => (None, None),
            };

        // Prepare record.
        let mut record = record.clone();
        if record.id.is_empty() {
            record.id = uuid::Uuid::new_v4().to_string();
        }
        if record.last_accessed_at == DateTime::UNIX_EPOCH {
            record.last_accessed_at = record.created_at;
        }
        record.path = path.to_string();

        // Conditional write — fails if another writer modified the record since our read.
        let bytes = rmp_serde::to_vec(&record)?;
        if kv
            .put_if_version(table, pk, sk, &bytes, expected_version)
            .await?
        {
            upsert_tree_file(kv, repo, path, &record).await;
            return Ok(old_record);
        }

        // Version conflict — retry.
        tracing::debug!(
            repo,
            path,
            attempt,
            "put_artifact version conflict, retrying"
        );
    }

    Err(DepotError::Conflict(format!(
        "put_artifact failed after {} retries due to concurrent writes: {}/{}",
        PUT_ARTIFACT_MAX_RETRIES, repo, path
    )))
}

/// Like `put_artifact`, but only writes if the key does not already exist.
/// Returns `true` if the record was written, `false` if it already existed.
#[tracing::instrument(level = "debug", skip(kv, record), fields(repo, path))]
pub async fn put_artifact_if_absent(
    kv: &dyn KvStore,
    repo: &str,
    path: &str,
    record: &ArtifactRecord,
) -> error::Result<bool> {
    let (table, pk, sk) = keys::artifact_key(repo, path);
    let mut record = record.clone();
    if record.id.is_empty() {
        record.id = uuid::Uuid::new_v4().to_string();
    }
    if record.last_accessed_at == DateTime::UNIX_EPOCH {
        record.last_accessed_at = record.created_at;
    }
    record.path = path.to_string();
    let bytes = rmp_serde::to_vec(&record)?;
    let written = kv.put_if_absent(table, pk, sk, &bytes).await?;
    if written {
        upsert_tree_file(kv, repo, path, &record).await;
    }
    Ok(written)
}

#[tracing::instrument(level = "debug", skip(kv), fields(repo, path))]
pub async fn get_artifact(
    kv: &dyn KvStore,
    repo: &str,
    path: &str,
) -> error::Result<Option<ArtifactRecord>> {
    let (table, pk, sk) = keys::artifact_key(repo, path);
    typed_get(kv, table, pk, sk).await
}

#[tracing::instrument(level = "debug", skip(kv), fields(repo, path))]
pub async fn delete_artifact(
    kv: &dyn KvStore,
    repo: &str,
    path: &str,
) -> error::Result<Option<ArtifactRecord>> {
    let (table, pk, sk) = keys::artifact_key(repo, path);
    match kv.delete_returning(table, pk, sk).await? {
        Some(bytes) => {
            delete_tree_file(kv, repo, path).await;
            Ok(Some(rmp_serde::from_slice(&bytes)?))
        }
        None => Ok(None),
    }
}

/// Bulk-delete artifact records together with their paired browse-tree
/// file entries — the bulk equivalent of [`delete_artifact`].
///
/// The UI browse listing reads from `TABLE_DIR_ENTRIES`, not
/// `TABLE_ARTIFACTS`. Dropping only the `ArtifactRecord` leaves a stale
/// `TreeEntry::File` behind, which surfaces as the UI listing rows whose
/// backing record is gone. `full_scan` (the `rebuild-dir-entries` task)
/// reconciles the two tables eventually, but it's not part of normal GC.
///
/// Safety:
///
/// * The artifact-records batch is issued first. The tree-entry batch
///   runs only if it succeeds — we never delete a tree entry whose
///   backing record we failed to remove.
/// * If the artifact-records batch fails, the error is propagated and
///   tree entries are left untouched.
/// * Tree-entry deletion errors are logged but do not propagate. The
///   artifact records are already gone, and `full_scan` will reconcile
///   any leftovers on the next run.
/// * `delete` on a non-existent key is a no-op, so duplicate paths or
///   already-cleaned tree entries are harmless.
#[tracing::instrument(level = "debug", skip(kv, paths), fields(repo, count = paths.len()))]
pub async fn delete_artifacts_paired_batch(
    kv: &dyn KvStore,
    repo: &str,
    paths: &[&str],
) -> error::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }

    let artifact_keys: Vec<(String, String)> = paths
        .iter()
        .map(|path| {
            let (_, pk, sk) = keys::artifact_key(repo, path);
            (pk.into_owned(), sk.into_owned())
        })
        .collect();
    let artifact_keys_ref: Vec<(&str, &str)> = artifact_keys
        .iter()
        .map(|(pk, sk)| (pk.as_str(), sk.as_str()))
        .collect();
    kv.delete_batch(keys::TABLE_ARTIFACTS, &artifact_keys_ref)
        .await?;

    let tree_keys: Vec<(String, String)> = paths
        .iter()
        .map(|path| {
            let (_, pk, sk) = keys::tree_entry_key(repo, path);
            (pk.into_owned(), sk.into_owned())
        })
        .collect();
    let tree_keys_ref: Vec<(&str, &str)> = tree_keys
        .iter()
        .map(|(pk, sk)| (pk.as_str(), sk.as_str()))
        .collect();
    if let Err(e) = kv
        .delete_batch(keys::TABLE_DIR_ENTRIES, &tree_keys_ref)
        .await
    {
        tracing::warn!(
            repo,
            error = %e,
            count = paths.len(),
            "paired tree-entry delete failed; full_scan will reconcile"
        );
    }
    Ok(())
}

// --- Listing and search ---

#[tracing::instrument(level = "debug", skip(kv, pagination), fields(repo))]
pub async fn list_artifacts(
    kv: &dyn KvStore,
    repo: &str,
    prefix: &str,
    pagination: &Pagination,
) -> error::Result<PaginatedResult<(String, ArtifactRecord)>> {
    let cursor_path = decode_cursor(&pagination.cursor);
    let per_shard = per_shard_batch(pagination.limit);

    let mut streams: Vec<ShardStream> = (0..keys::NUM_SHARDS)
        .map(|shard| {
            let pk = keys::artifact_shard_pk(repo, shard);
            let start = match &cursor_path {
                Some(cp) if cp.as_bytes() > prefix.as_bytes() => cp.clone(),
                _ => prefix.to_string(),
            };
            ShardStream::new(pk, prefix.to_string(), start, None, per_shard)
        })
        .collect();

    let skip = if cursor_path.is_some() {
        0
    } else {
        pagination.offset
    };

    let items = k_way_merge(
        kv,
        keys::TABLE_ARTIFACTS,
        &mut streams,
        cursor_path.as_deref(),
        skip,
        pagination.limit,
    )
    .await?;

    Ok(paginate_results(items, pagination.limit))
}

#[tracing::instrument(level = "debug", skip(kv, pagination), fields(repo))]
pub async fn search_artifacts(
    kv: &dyn KvStore,
    repo: &str,
    query: &str,
    pagination: &Pagination,
) -> error::Result<PaginatedResult<(String, ArtifactRecord)>> {
    let query_lower = query.to_lowercase();
    let cursor_path = decode_cursor(&pagination.cursor);
    let per_shard = per_shard_batch(pagination.limit);

    let mut streams: Vec<ShardStream> = (0..keys::NUM_SHARDS)
        .map(|shard| {
            let pk = keys::artifact_shard_pk(repo, shard);
            let start = match &cursor_path {
                Some(cp) => cp.clone(),
                None => String::new(),
            };
            ShardStream::new(
                pk,
                String::new(),
                start,
                Some(query_lower.clone()),
                per_shard,
            )
        })
        .collect();

    let skip = if cursor_path.is_some() {
        0
    } else {
        pagination.offset
    };

    let items = k_way_merge(
        kv,
        keys::TABLE_ARTIFACTS,
        &mut streams,
        cursor_path.as_deref(),
        skip,
        pagination.limit,
    )
    .await?;

    Ok(paginate_results(items, pagination.limit))
}

// --- Access tracking ---

/// Update `last_accessed_at` on the tree file entry (not the artifact record).
/// Uses a 60-second debounce to avoid excessive writes.
pub async fn touch_artifact(
    kv: &dyn KvStore,
    repo: &str,
    path: &str,
    now: DateTime<Utc>,
) -> error::Result<()> {
    let (_, pk, sk) = keys::tree_entry_key(repo, path);
    let (bytes, version) = match kv
        .get_versioned(keys::TABLE_DIR_ENTRIES, pk.clone(), sk.clone())
        .await?
    {
        Some(rv) => rv,
        None => return Ok(()),
    };
    let mut te: TreeEntry = rmp_serde::from_slice(&bytes)?;

    // 60s debounce.
    if (now - te.last_accessed_at).num_seconds() < 60 {
        return Ok(());
    }

    te.last_accessed_at = now;
    let new_bytes = rmp_serde::to_vec(&te)?;
    // Best-effort: if the entry was modified concurrently, skip.
    kv.put_if_version(keys::TABLE_DIR_ENTRIES, pk, sk, &new_bytes, Some(version))
        .await?;

    Ok(())
}

// --- Collection helpers ---

/// Fold over all artifacts matching a prefix without collecting into a Vec.
///
/// Each shard builds local state via `fold`, then per-shard results are
/// combined with `merge`. This mirrors the GC bloom-filter pattern and
/// avoids unbounded memory growth on large repos.
pub async fn fold_all_artifacts<S, F, M>(
    kv: &dyn KvStore,
    repo: &str,
    prefix: &str,
    init: impl Fn() -> S + Send + Sync,
    fold: F,
    merge: M,
) -> error::Result<S>
where
    S: Send + 'static,
    F: Fn(&mut S, &str, &ArtifactRecord) -> error::Result<()> + Send + Sync,
    M: Fn(S, S) -> S,
{
    use futures::stream::{self, StreamExt, TryStreamExt};
    let shard_states: Vec<S> = stream::iter(0..keys::NUM_SHARDS)
        .map(|shard| {
            let init = &init;
            let fold = &fold;
            async move {
                let pk = keys::artifact_shard_pk(repo, shard);
                let mut state = init();
                for_each_in_partition(
                    kv,
                    keys::TABLE_ARTIFACTS,
                    &pk,
                    prefix,
                    SCAN_BATCH_SIZE,
                    |sk, v| {
                        let record: ArtifactRecord = rmp_serde::from_slice(v)?;
                        fold(&mut state, sk, &record)?;
                        Ok(true)
                    },
                )
                .await?;
                Ok::<_, error::DepotError>(state)
            }
        })
        .buffer_unordered(SHARD_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(shard_states
        .into_iter()
        .reduce(&merge)
        .unwrap_or_else(&init))
}
