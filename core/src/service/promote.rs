// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Format-agnostic building blocks for server-side copy / move of artifact
//! records between repositories.
//!
//! A "promotion" re-homes selected artifact records from one repository to
//! another. When the source and destination repositories live in the **same**
//! blob store, this is a metadata-only operation: records are copied verbatim
//! (timestamps preserved) and no blob bytes move, exactly like `clone_repo`
//! but selective. When the destination is on a **different** blob store, each
//! blob-backed record's bytes are streamed into the destination store (and
//! de-duplicated there) before the record is written.
//!
//! Format crates layer their own record-graph knowledge on top: Docker walks
//! a tag's manifest closure and copies every reachable record; Helm copies a
//! single chart record. Both drive the same [`CopyTarget`].

use crate::error::{self, DepotError};
use crate::repo::now_utc;
use crate::service;
use crate::store::blob::BlobStore;
use crate::store::kv::{ArtifactRecord, BlobRecord, KvStore, CURRENT_RECORD_VERSION};
use crate::update::UpdateSender;

/// Source and destination context for a record copy/move.
///
/// When `source_store == dest_store` the two repos share a content-addressable
/// blob store, so records are copied verbatim and no bytes move. When the
/// stores differ, each blob-backed record's bytes are streamed into the
/// destination store (and de-duplicated there) before the record is written —
/// the cross-store copy path. Either way, record timestamps are preserved.
pub struct CopyTarget<'a> {
    pub kv: &'a dyn KvStore,
    pub updater: &'a UpdateSender,
    pub source_repo: &'a str,
    pub source_store: &'a str,
    pub source_blobs: &'a dyn BlobStore,
    pub dest_repo: &'a str,
    pub dest_store: &'a str,
    pub dest_blobs: &'a dyn BlobStore,
}

impl CopyTarget<'_> {
    pub fn same_store(&self) -> bool {
        self.source_store == self.dest_store
    }

    /// Copy one artifact record from the source repo to the destination repo,
    /// preserving its timestamps, and keep the destination's directory + store
    /// counters in step. Returns 1 if a record was copied, 0 if the source
    /// record was absent.
    pub async fn copy_record(&self, path: &str) -> error::Result<u64> {
        let rec = match service::get_artifact(self.kv, self.source_repo, path).await? {
            Some(r) => r,
            None => return Ok(0),
        };
        let (dest_rec, store_bytes_added) = if self.same_store() {
            // Shared store: the blob pointer is valid as-is, copy verbatim. No
            // physical bytes are added — the content-addressed blob is shared —
            // so the store's physical counter must NOT move.
            (rec, 0u64)
        } else {
            // Different store: stream the blob bytes across, re-point the record.
            // Reports the bytes actually newly persisted (0 on a dedup hit).
            self.rehome_blob(rec).await?
        };
        let old = service::put_artifact(self.kv, self.dest_repo, path, &dest_rec).await?;
        // Per-repo (directory) stats are logical — every repo counts the records
        // it holds — so the destination legitimately grows here.
        let (cd, bd) = delta(dest_rec.size, old.as_ref().map(|r| r.size));
        self.updater.dir_changed(self.dest_repo, path, cd, bd).await;
        // Store stats track *physical* (deduplicated) bytes — only credit blobs
        // that were genuinely newly stored, never a same-store verbatim copy.
        if store_bytes_added > 0 {
            self.updater
                .store_changed(self.dest_store, 1, store_bytes_added as i64)
                .await;
        }
        Ok(1)
    }

    /// Stream a blob-backed record's bytes from the source store into the
    /// destination store (de-duplicating there) and return the record cloned
    /// with its `blob_id` re-pointed at the destination blob, plus the number of
    /// bytes **newly persisted** to the destination store (0 if the record has
    /// no blob, or if the destination already had this content). Timestamps are
    /// preserved.
    async fn rehome_blob(&self, rec: ArtifactRecord) -> error::Result<(ArtifactRecord, u64)> {
        let blob_id = match rec.blob_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok((rec, 0)),
        };
        let data = self.source_blobs.get(&blob_id).await?.ok_or_else(|| {
            DepotError::DataIntegrity(format!(
                "blob {blob_id} missing in source store '{}'",
                self.source_store
            ))
        })?;
        let blake3 = rec
            .content_hash
            .clone()
            .unwrap_or_else(|| blake3::hash(&data).to_hex().to_string());

        let new_blob_id = uuid::Uuid::new_v4().to_string();
        self.dest_blobs.put(&new_blob_id, &data).await?;
        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: new_blob_id.clone(),
            hash: blake3,
            size: data.len() as u64,
            created_at: now_utc(),
            store: self.dest_store.to_string(),
        };
        let existing = service::put_dedup_record(self.kv, self.dest_store, &blob_rec).await?;
        // If the destination store already had this content, drop our upload and
        // credit nothing; otherwise the bytes we just stored are genuinely new.
        let (effective, added) = match existing {
            Some(existing_id) => {
                let _ = self.dest_blobs.delete(&new_blob_id).await;
                (existing_id, 0u64)
            }
            None => (new_blob_id, data.len() as u64),
        };

        let mut dest_rec = rec;
        dest_rec.blob_id = Some(effective);
        Ok((dest_rec, added))
    }
}

/// `(count_delta, bytes_delta)` for upserting a record of `new_size` over an
/// optional existing record of `old_size`.
pub fn delta(new_size: u64, old_size: Option<u64>) -> (i64, i64) {
    match old_size {
        Some(old) => (0, new_size as i64 - old as i64),
        None => (1, new_size as i64),
    }
}
