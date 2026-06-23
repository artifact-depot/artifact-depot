// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Server-side copy / move of a Docker image tag between repositories.
//!
//! A tagged Docker image is a small set of `TABLE_ARTIFACTS` records: the
//! `_tags/<tag>` record, the `_manifests/<digest>` record(s) (one per manifest,
//! including the children of a multi-arch manifest list), and the
//! `_blobs/<digest>` reference records for the config + layers. All of them
//! point at content-addressable blobs that are shared store-wide.
//!
//! When the source and destination repositories live in the **same** blob
//! store, re-homing a tag is a metadata-only operation: the records are copied
//! into the destination repo verbatim (so timestamps are preserved) and no blob
//! bytes move. This mirrors how `clone_repo` copies records, but selectively for
//! a single tag.
//!
//! When the destination is on a **different** blob store, the bytes can't be
//! shared, so each blob-backed record's data is streamed into the destination
//! store (and de-duplicated there) before the record is written. Both paths are
//! driven by [`CopyTarget`], which selects the strategy from whether the two
//! stores match.

use std::collections::BTreeSet;

use depot_core::error::{self, DepotError};
use depot_core::repo::now_utc;
use depot_core::service;
use depot_core::store::blob::BlobStore;
use depot_core::store::kv::{
    ArtifactKind, ArtifactRecord, BlobRecord, KvStore, CURRENT_RECORD_VERSION,
};
use depot_core::update::UpdateSender;

use crate::store::{blob_ref_path_for, manifest_path_for, tag_path_for};

/// Outcome of a [`copy_tag`] operation.
#[derive(Debug, Clone)]
pub struct PromoteOutcome {
    /// The manifest digest the moved tag resolved to.
    pub digest: String,
    /// Number of artifact records written into the destination repo
    /// (tag + manifests + blob references).
    pub copied_records: u64,
}

/// Source and destination context for a tag copy/move.
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
    fn same_store(&self) -> bool {
        self.source_store == self.dest_store
    }

    /// Copy one artifact record from the source repo to the destination repo,
    /// preserving its timestamps, and keep the destination's directory + store
    /// counters in step. Returns 1 if a record was copied, 0 if the source
    /// record was absent.
    async fn copy_record(&self, path: &str) -> error::Result<u64> {
        let rec = match service::get_artifact(self.kv, self.source_repo, path).await? {
            Some(r) => r,
            None => return Ok(0),
        };
        let dest_rec = if self.same_store() {
            // Shared store: the blob pointer is valid as-is, copy verbatim.
            rec
        } else {
            // Different store: stream the blob bytes across, re-point the record.
            self.rehome_blob(rec).await?
        };
        let old = service::put_artifact(self.kv, self.dest_repo, path, &dest_rec).await?;
        let (cd, bd) = delta(dest_rec.size, old.as_ref().map(|r| r.size));
        self.updater.dir_changed(self.dest_repo, path, cd, bd).await;
        self.updater.store_changed(self.dest_store, cd, bd).await;
        Ok(1)
    }

    /// Stream a blob-backed record's bytes from the source store into the
    /// destination store (de-duplicating there) and return the record cloned
    /// with its `blob_id` re-pointed at the destination blob. Records without a
    /// blob (e.g. tags) are returned unchanged. Timestamps are preserved.
    async fn rehome_blob(&self, rec: ArtifactRecord) -> error::Result<ArtifactRecord> {
        let blob_id = match rec.blob_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(rec),
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
        let effective = existing.clone().unwrap_or_else(|| new_blob_id.clone());
        // If the destination store already had this content, drop our upload.
        if existing.is_some() {
            let _ = self.dest_blobs.delete(&new_blob_id).await;
        }

        let mut dest_rec = rec;
        dest_rec.blob_id = Some(effective);
        Ok(dest_rec)
    }
}

/// Copy — and optionally move — a single Docker tag `image:tag` between the
/// repositories described by `t`.
///
/// Walks the tag's manifest closure (the manifest, plus the children of a
/// multi-arch manifest list, plus the config/layer blob references) and copies
/// every record into the destination repo, preserving `created_at` /
/// `last_accessed_at` so the destination's retention policy is applied against
/// the artifact's true age. Same-store copies move no bytes; cross-store copies
/// stream the blobs into the destination store (see [`CopyTarget`]).
///
/// When `delete_source` is `true` this is a *move*: after the copy succeeds,
/// only the **source tag** record is removed. The now-orphaned manifest and
/// blob records in the source repo are left to `docker_gc`, which reclaims them
/// once no surviving tag references them — so layers shared with other tags in
/// the source repo are never clobbered.
///
/// `overwrite` governs only the destination *tag* collision; shared
/// manifest/blob records are always upserted idempotently.
///
/// # Errors
/// - [`DepotError::NotFound`] if the source tag does not exist.
/// - [`DepotError::Conflict`] if the destination tag exists and `!overwrite`.
pub async fn copy_tag(
    t: &CopyTarget<'_>,
    image: Option<&str>,
    tag: &str,
    delete_source: bool,
    overwrite: bool,
) -> error::Result<PromoteOutcome> {
    let tag_path = tag_path_for(image, tag);

    // Resolve the tag → root manifest digest.
    let tag_rec = service::get_artifact(t.kv, t.source_repo, &tag_path)
        .await?
        .ok_or_else(|| {
            DepotError::NotFound(format!(
                "tag '{tag}' not found in repository '{}'",
                t.source_repo
            ))
        })?;
    let root_digest = match &tag_rec.kind {
        ArtifactKind::DockerTag { digest, .. } => digest.clone(),
        _ => {
            return Err(DepotError::DataIntegrity(format!(
                "record at '{tag_path}' in '{}' is not a Docker tag",
                t.source_repo
            )))
        }
    };

    // Destination tag collision check.
    if !overwrite
        && service::get_artifact(t.kv, t.dest_repo, &tag_path)
            .await?
            .is_some()
    {
        return Err(DepotError::Conflict(format!(
            "tag '{tag}' already exists in repository '{}' (set overwrite to replace)",
            t.dest_repo
        )));
    }

    // Walk the manifest closure: every manifest reachable from the root
    // (the root plus, for a manifest list/index, its children) and every
    // config/layer blob they reference.
    let mut manifest_digests: Vec<String> = Vec::new();
    let mut seen_manifests: BTreeSet<String> = BTreeSet::new();
    let mut blob_digests: BTreeSet<String> = BTreeSet::new();

    let mut stack = vec![root_digest.clone()];
    while let Some(digest) = stack.pop() {
        if !seen_manifests.insert(digest.clone()) {
            continue;
        }
        let manifest_path = manifest_path_for(image, &digest);
        let json = read_manifest_json(t.kv, t.source_blobs, t.source_repo, &manifest_path)
            .await?
            .ok_or_else(|| {
                DepotError::DataIntegrity(format!(
                    "manifest '{digest}' referenced by tag '{tag}' is missing in '{}'",
                    t.source_repo
                ))
            })?;
        manifest_digests.push(digest);

        let (children, blobs_referenced) = classify_manifest(&json);
        for child in children {
            stack.push(child);
        }
        for blob in blobs_referenced {
            blob_digests.insert(blob);
        }
    }

    // Copy order: blobs and manifests first (so the destination is a complete,
    // pullable image), then the tag last.
    let mut copied = 0u64;
    for digest in &blob_digests {
        copied += t.copy_record(&blob_ref_path_for(digest)).await?;
    }
    for digest in &manifest_digests {
        copied += t.copy_record(&manifest_path_for(image, digest)).await?;
    }
    copied += t.copy_record(&tag_path).await?;

    // Move: drop only the source tag; docker_gc reclaims orphaned bookkeeping.
    if delete_source {
        delete_tag(t.kv, t.updater, t.source_repo, t.source_store, image, tag).await?;
    }

    Ok(PromoteOutcome {
        digest: root_digest,
        copied_records: copied,
    })
}

/// Delete a Docker tag record from `repo` and keep its directory + store
/// counters in step. The now-orphaned manifest/blob bookkeeping is left to
/// `docker_gc`, which reclaims it once no surviving tag references it.
///
/// Returns `true` if a tag was removed, `false` if it did not exist.
pub async fn delete_tag(
    kv: &dyn KvStore,
    updater: &UpdateSender,
    repo: &str,
    store: &str,
    image: Option<&str>,
    tag: &str,
) -> error::Result<bool> {
    let tag_path = tag_path_for(image, tag);
    match service::delete_artifact(kv, repo, &tag_path).await? {
        Some(old) => {
            updater
                .dir_changed(repo, &tag_path, -1, -(old.size as i64))
                .await;
            updater.store_changed(store, -1, -(old.size as i64)).await;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// `(count_delta, bytes_delta)` for upserting a record of `new_size` over an
/// optional existing record of `old_size`.
fn delta(new_size: u64, old_size: Option<u64>) -> (i64, i64) {
    match old_size {
        Some(old) => (0, new_size as i64 - old as i64),
        None => (1, new_size as i64),
    }
}

/// Read a manifest's JSON bytes from the blob store without touching its
/// access time (a move should not refresh the source's atime).
async fn read_manifest_json(
    kv: &dyn KvStore,
    blobs: &dyn BlobStore,
    repo: &str,
    manifest_path: &str,
) -> error::Result<Option<Vec<u8>>> {
    let rec = match service::get_artifact(kv, repo, manifest_path).await? {
        Some(r) => r,
        None => return Ok(None),
    };
    let blob_id = match rec.blob_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(None),
    };
    blobs.get(blob_id).await
}

/// Classify a manifest JSON into `(child_manifest_digests, blob_digests)`.
///
/// A manifest list / OCI index carries a `manifests` array of child manifest
/// digests (each its own `_manifests/<digest>` record to recurse into). A plain
/// image manifest carries a `config` digest and `layers` digests — these are
/// `_blobs/<digest>` references. The two shapes are mutually exclusive.
fn classify_manifest(json: &[u8]) -> (Vec<String>, Vec<String>) {
    let parsed: serde_json::Value = match serde_json::from_slice(json) {
        Ok(v) => v,
        Err(_) => return (Vec::new(), Vec::new()),
    };

    if let Some(manifests) = parsed.get("manifests").and_then(|m| m.as_array()) {
        let children = manifests
            .iter()
            .filter_map(|m| m.get("digest").and_then(|d| d.as_str()).map(String::from))
            .collect();
        return (children, Vec::new());
    }

    let mut blobs = Vec::new();
    if let Some(digest) = parsed
        .get("config")
        .and_then(|c| c.get("digest"))
        .and_then(|d| d.as_str())
    {
        blobs.push(digest.to_string());
    }
    if let Some(layers) = parsed.get("layers").and_then(|l| l.as_array()) {
        for layer in layers {
            if let Some(digest) = layer.get("digest").and_then(|d| d.as_str()) {
                blobs.push(digest.to_string());
            }
        }
    }
    (Vec::new(), blobs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_image_manifest_yields_config_and_layers() {
        let json = br#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "config": {"digest": "sha256:cfg"},
            "layers": [{"digest": "sha256:l1"}, {"digest": "sha256:l2"}]
        }"#;
        let (children, blobs) = classify_manifest(json);
        assert!(children.is_empty());
        assert_eq!(blobs, vec!["sha256:cfg", "sha256:l1", "sha256:l2"]);
    }

    #[test]
    fn classify_manifest_list_yields_children() {
        let json = br#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
            "manifests": [{"digest": "sha256:amd64"}, {"digest": "sha256:arm64"}]
        }"#;
        let (children, blobs) = classify_manifest(json);
        assert_eq!(children, vec!["sha256:amd64", "sha256:arm64"]);
        assert!(blobs.is_empty());
    }

    #[test]
    fn delta_new_vs_existing() {
        assert_eq!(delta(100, None), (1, 100));
        assert_eq!(delta(100, Some(40)), (0, 60));
        assert_eq!(delta(0, Some(0)), (0, 0));
    }
}
