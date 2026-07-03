// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Blob record CRUD (scoped by store name).

use crate::error;
use crate::store::keys;
use crate::store::kv::*;

use super::scan::{for_each_in_partition, SCAN_BATCH_SIZE, SHARD_CONCURRENCY};
use super::store::list_stores;
use super::typed::{typed_get, typed_put};

pub async fn put_blob(kv: &dyn KvStore, store: &str, record: &BlobRecord) -> error::Result<()> {
    let mut rec = record.clone();
    rec.store = store.to_string();
    let (table, pk, sk) = keys::blob_key(store, &rec.hash);
    typed_put(kv, table, pk, sk, &rec).await
}

pub async fn get_blob(
    kv: &dyn KvStore,
    store: &str,
    hash: &str,
) -> error::Result<Option<BlobRecord>> {
    let (table, pk, sk) = keys::blob_key(store, hash);
    typed_get(kv, table, pk, sk).await
}

/// Atomically claim a blob hash→UUID mapping.
/// Returns `None` if a new record was created (we won the race),
/// or `Some(existing_blob_id)` if a record already existed (dedup — caller
/// should delete the duplicate blob file and use the existing blob_id).
///
/// Most callers that have just written a fresh blob should use
/// [`claim_or_reuse_blob`] instead, which handles the dedup hit (duplicate
/// deletion + blob_id substitution) so it cannot be forgotten. Call this
/// directly only when registering a pre-existing blob (e.g. cross-repo
/// mount), where there is no duplicate to delete.
#[tracing::instrument(level = "debug", name = "put_dedup_record", skip(kv, record), fields(store, hash = %record.hash))]
pub async fn put_dedup_record(
    kv: &dyn KvStore,
    store: &str,
    record: &BlobRecord,
) -> error::Result<Option<String>> {
    let mut rec = record.clone();
    rec.store = store.to_string();
    let (table, pk, sk) = keys::blob_key(store, &rec.hash);
    let bytes = rmp_serde::to_vec(&rec)?;

    if kv
        .put_if_absent(table, pk.clone(), sk.clone(), &bytes)
        .await?
    {
        // We won — new record created.
        Ok(None)
    } else {
        // Lost the race — read existing to get its blob_id.
        let existing: Option<BlobRecord> = typed_get(kv, table, pk.clone(), sk.clone()).await?;
        match existing {
            Some(existing_rec) => Ok(Some(existing_rec.blob_id)),
            None => {
                // Extremely unlikely: the record existed a moment ago but was
                // deleted between put_if_absent and get. Treat as new.
                kv.put(table, pk, sk, &bytes).await?;
                Ok(None)
            }
        }
    }
}

/// Claim the `hash→blob_id` dedup mapping for a freshly written blob, or
/// reuse an existing blob with the same content hash.
///
/// On a dedup hit the just-written duplicate blob file (`record.blob_id`) is
/// deleted from the blob store and the existing blob_id is returned; the
/// caller MUST reference the returned blob_id in its artifact records, never
/// `record.blob_id` directly. Delete failures are logged and ignored — the
/// orphaned duplicate is unreferenced, so blob GC will collect it.
#[must_use = "artifact records must reference the returned blob_id, not the one passed in"]
pub async fn claim_or_reuse_blob(
    kv: &dyn KvStore,
    blobs: &dyn crate::store::blob::BlobStore,
    store: &str,
    record: &BlobRecord,
) -> error::Result<String> {
    match put_dedup_record(kv, store, record).await? {
        Some(existing_blob_id) if existing_blob_id != record.blob_id => {
            // Content already stored under another blob_id — drop our duplicate.
            if let Err(e) = blobs.delete(&record.blob_id).await {
                tracing::warn!(
                    store,
                    blob_id = %record.blob_id,
                    error = %e,
                    "failed to delete duplicate blob after dedup hit; GC will collect it"
                );
            }
            Ok(existing_blob_id)
        }
        _ => Ok(record.blob_id.clone()),
    }
}

pub async fn delete_blob(kv: &dyn KvStore, store: &str, hash: &str) -> error::Result<bool> {
    let (table, pk, sk) = keys::blob_key(store, hash);
    kv.delete(table, pk, sk).await
}

/// Fold over all blob records across all stores without collecting into a Vec.
///
/// Each store×shard partition builds local state via `fold`, then per-partition
/// results are combined with `merge`.
pub async fn fold_all_blobs<S, F, M>(
    kv: &dyn KvStore,
    init: impl Fn() -> S + Send + Sync,
    fold: F,
    merge: M,
) -> error::Result<S>
where
    S: Send + 'static,
    F: Fn(&mut S, &BlobRecord) -> error::Result<()> + Send + Sync,
    M: Fn(S, S) -> S,
{
    use futures::stream::{self, StreamExt, TryStreamExt};
    let stores = list_stores(kv).await?;
    let pks: Vec<String> = stores
        .iter()
        .flat_map(|s| (0..keys::NUM_SHARDS).map(move |shard| keys::blob_shard_pk(&s.name, shard)))
        .collect();
    let shard_states: Vec<S> = stream::iter(pks)
        .map(|pk| {
            let init = &init;
            let fold = &fold;
            async move {
                let mut state = init();
                for_each_in_partition(kv, keys::TABLE_BLOBS, &pk, "", SCAN_BATCH_SIZE, |_, v| {
                    let record: BlobRecord = rmp_serde::from_slice(v)?;
                    fold(&mut state, &record)?;
                    Ok(true)
                })
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

/// List blobs in a specific store with offset-based pagination.
pub async fn list_blobs_for_store(
    kv: &dyn KvStore,
    store: &str,
    offset: usize,
    limit: usize,
) -> error::Result<(Vec<BlobRecord>, usize)> {
    use futures::stream::{self, StreamExt, TryStreamExt};
    // Collect all blobs across shards (blobs are not hierarchical, so simple collect + sort).
    let shard_vecs: Vec<Vec<BlobRecord>> = stream::iter(0..keys::NUM_SHARDS)
        .map(|shard| async move {
            let pk = keys::blob_shard_pk(store, shard);
            let mut blobs = Vec::new();
            for_each_in_partition(kv, keys::TABLE_BLOBS, &pk, "", SCAN_BATCH_SIZE, |_, v| {
                let record: BlobRecord = rmp_serde::from_slice(v)?;
                blobs.push(record);
                Ok(true)
            })
            .await?;
            Ok::<_, error::DepotError>(blobs)
        })
        .buffer_unordered(SHARD_CONCURRENCY)
        .try_collect()
        .await?;
    let mut all: Vec<BlobRecord> = shard_vecs.into_iter().flatten().collect();
    all.sort_by(|a, b| a.hash.cmp(&b.hash));
    let total = all.len();
    let page = all.into_iter().skip(offset).take(limit).collect();
    Ok((page, total))
}

/// Count blobs in a specific store.
pub async fn count_blobs_for_store(kv: &dyn KvStore, store: &str) -> error::Result<usize> {
    use futures::stream::{self, StreamExt, TryStreamExt};
    let counts: Vec<usize> = stream::iter(0..keys::NUM_SHARDS)
        .map(|shard| async move {
            let pk = keys::blob_shard_pk(store, shard);
            let mut count = 0usize;
            for_each_in_partition(kv, keys::TABLE_BLOBS, &pk, "", SCAN_BATCH_SIZE, |_, _| {
                count += 1;
                Ok(true)
            })
            .await?;
            Ok::<_, error::DepotError>(count)
        })
        .buffer_unordered(SHARD_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(counts.into_iter().sum())
}
