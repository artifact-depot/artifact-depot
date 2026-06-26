// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Store configuration CRUD.

use std::borrow::Cow;

use crate::error;
use crate::store::keys;
use crate::store::kv::*;

use super::scan::ADMIN_ENTITY_CAP;
use super::typed::{typed_get, typed_list, typed_put};

pub async fn put_store(kv: &dyn KvStore, record: &StoreRecord) -> error::Result<()> {
    let (table, pk, sk) = keys::store_key(&record.name);
    typed_put(kv, table, pk, sk, record).await
}

pub async fn get_store(kv: &dyn KvStore, name: &str) -> error::Result<Option<StoreRecord>> {
    let (table, pk, sk) = keys::store_key(name);
    typed_get(kv, table, pk, sk).await
}

pub async fn list_stores(kv: &dyn KvStore) -> error::Result<Vec<StoreRecord>> {
    typed_list(
        kv,
        keys::TABLE_STORES,
        Cow::Borrowed(keys::SINGLE_PK),
        Cow::Borrowed(""),
        ADMIN_ENTITY_CAP,
    )
    .await
}

pub async fn delete_store(kv: &dyn KvStore, name: &str) -> error::Result<bool> {
    let (table, pk, sk) = keys::store_key(name);
    kv.delete(table, pk, sk).await
}

// --- Store stats ---

pub async fn put_store_stats(
    kv: &dyn KvStore,
    name: &str,
    stats: &StoreStatsRecord,
) -> error::Result<()> {
    let (table, pk, sk) = keys::store_stats_key(name);
    typed_put(kv, table, pk, sk, stats).await
}

pub async fn get_store_stats(
    kv: &dyn KvStore,
    name: &str,
) -> error::Result<Option<StoreStatsRecord>> {
    let (table, pk, sk) = keys::store_stats_key(name);
    typed_get(kv, table, pk, sk).await
}

pub async fn delete_store_stats(kv: &dyn KvStore, name: &str) -> error::Result<bool> {
    let (table, pk, sk) = keys::store_stats_key(name);
    kv.delete(table, pk, sk).await
}

/// Recompute every store's `StoreStatsRecord` from the authoritative blob
/// records and persist it, returning the freshly-written `(store, stats)` pairs.
///
/// `blob_count`/`total_bytes` are otherwise maintained by incremental
/// `store_changed` deltas, which can drift (e.g. GC frees bytes without
/// crediting them back). This single pass over `TABLE_BLOBS` — deduplicated by
/// `(store, content-hash)`, so shared content is counted once per store —
/// re-derives the truth. Backend-agnostic (a `KvStore` scan) and per-store, so
/// it reconciles every configured blob store, including ones with zero blobs.
pub async fn reconcile_store_stats(
    kv: &dyn KvStore,
) -> error::Result<Vec<(String, StoreStatsRecord)>> {
    use std::collections::HashMap;

    // One pass over all blob records: per-store (count, total_bytes).
    let tallies: HashMap<String, (u64, u64)> = super::blob::fold_all_blobs(
        kv,
        HashMap::new,
        |acc: &mut HashMap<String, (u64, u64)>, rec| {
            let e = acc.entry(rec.store.clone()).or_insert((0, 0));
            e.0 += 1;
            e.1 += rec.size;
            Ok(())
        },
        |mut a, b| {
            for (store, (c, bytes)) in b {
                let e = a.entry(store).or_insert((0, 0));
                e.0 += c;
                e.1 += bytes;
            }
            a
        },
    )
    .await?;

    // Write a record for every configured store (zeroed if it has no blobs), so
    // a store that was emptied reconciles down to zero rather than going stale.
    let now = chrono::Utc::now();
    let mut out = Vec::new();
    for store in list_stores(kv).await? {
        let (blob_count, total_bytes) = tallies.get(&store.name).copied().unwrap_or((0, 0));
        let stats = StoreStatsRecord {
            blob_count,
            total_bytes,
            updated_at: now,
        };
        put_store_stats(kv, &store.name, &stats).await?;
        out.push((store.name, stats));
    }
    Ok(out)
}
