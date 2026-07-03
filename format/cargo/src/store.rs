#![deny(clippy::string_slice)]
// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Cargo crate registry storage logic.
//!
//! Maps Cargo registry concepts onto our generic KV + blob store:
//!
//! - Crates stored as artifacts at path `_crates/{normalized_name}/{version}/{name}-{version}.crate`
//! - BLAKE3 is the blob store key, SHA-256 stored in metadata for Cargo checksum verification.

use sha2::{Digest, Sha256};
use std::collections::HashSet;

use depot_core::error::{self, DepotError};
use depot_core::service;
use depot_core::store::blob::BlobStore;
use depot_core::store::kv::{
    ArtifactKind, ArtifactRecord, BlobRecord, CrateVersionMeta, KvStore, CURRENT_RECORD_VERSION,
};
use depot_core::update::UpdateSender;

/// Cargo name normalization: lowercase, replace `-` with `_`.
pub fn normalize_crate_name(name: &str) -> String {
    name.to_lowercase().replace('-', "_")
}

/// Compute the sparse index prefix path for a crate name.
pub fn index_prefix(name: &str) -> String {
    let lower = name.to_lowercase();
    match lower.len() {
        0 => "1".to_string(),
        1 => "1".to_string(),
        2 => "2".to_string(),
        3 => format!("3/{}", lower.get(..1).unwrap_or_default()),
        _ => format!(
            "{}/{}",
            lower.get(..2).unwrap_or_default(),
            lower.get(2..4).unwrap_or_default()
        ),
    }
}

/// Build the KV path for a crate file.
pub fn crate_path(name: &str, version: &str) -> String {
    let norm = normalize_crate_name(name);
    format!("_crates/{norm}/{version}/{name}-{version}.crate")
}

/// Cargo storage operations for a repository.
pub struct CargoStore<'a> {
    pub repo: &'a str,
    pub kv: &'a dyn KvStore,
    pub blobs: &'a dyn BlobStore,
    pub store: &'a str,
    pub updater: &'a UpdateSender,
}

impl<'a> CargoStore<'a> {
    /// Publish a crate, computing BLAKE3 + SHA-256, writing blob and artifact records.
    pub async fn publish_crate(
        &self,
        meta: &CrateVersionMeta,
        crate_bytes: &[u8],
    ) -> error::Result<ArtifactRecord> {
        let blake3_hash = blake3::hash(crate_bytes).to_hex().to_string();
        let sha256_hash = hex_encode(Sha256::digest(crate_bytes));

        if meta.cksum != sha256_hash {
            return Err(DepotError::BadRequest(format!(
                "SHA-256 mismatch: metadata says {}, computed {sha256_hash}",
                meta.cksum,
            )));
        }

        let blob_id = uuid::Uuid::new_v4().to_string();
        let now = depot_core::repo::now_utc();

        self.blobs.put(&blob_id, crate_bytes).await?;

        let path = crate_path(&meta.name, &meta.vers);

        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.clone(),
            hash: blake3_hash.clone(),
            size: crate_bytes.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let store = self.store.to_string();
        let repo = self.repo.to_string();
        let blob_id = service::claim_or_reuse_blob_counted(
            self.kv,
            self.blobs,
            &store,
            &blob_rec,
            self.updater,
        )
        .await?;

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: crate_bytes.len() as u64,
            content_type: "application/x-tar".to_string(),
            kind: ArtifactKind::CargoCrate {
                cargo: meta.clone(),
            },
            blob_id: Some(blob_id),
            content_hash: Some(blake3_hash.clone()),
            etag: Some(blake3_hash.clone()),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
        };
        let record_clone = record.clone();
        let old_record = service::put_artifact(self.kv, &repo, &path, &record_clone).await?;
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, &path, count_delta, bytes_delta)
            .await;
        Ok(record)
    }

    /// Commit a crate whose blob has already been written via `BlobWriter`.
    pub async fn commit_crate(
        &self,
        meta: &CrateVersionMeta,
        blob_id: &str,
        blake3_hash: &str,
        size: u64,
    ) -> error::Result<ArtifactRecord> {
        let now = depot_core::repo::now_utc();
        let path = crate_path(&meta.name, &meta.vers);

        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.to_string(),
            hash: blake3_hash.to_string(),
            size,
            created_at: now,
            store: self.store.to_string(),
        };
        let store = self.store.to_string();
        let repo = self.repo.to_string();
        let blob_id = service::claim_or_reuse_blob_counted(
            self.kv,
            self.blobs,
            &store,
            &blob_rec,
            self.updater,
        )
        .await?;

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size,
            content_type: "application/x-tar".to_string(),
            kind: ArtifactKind::CargoCrate {
                cargo: meta.clone(),
            },
            blob_id: Some(blob_id),
            content_hash: Some(blake3_hash.to_string()),
            etag: Some(blake3_hash.to_string()),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
        };
        let record_clone = record.clone();
        let old_record = service::put_artifact(self.kv, &repo, &path, &record_clone).await?;
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, &path, count_delta, bytes_delta)
            .await;
        Ok(record)
    }

    /// Look up a crate artifact record and return a blob reader.
    pub async fn get_crate(
        &self,
        name: &str,
        version: &str,
    ) -> error::Result<Option<(depot_core::store::blob::BlobReader, u64, ArtifactRecord)>> {
        let path = crate_path(name, version);
        let repo = self.repo.to_string();
        let record = service::get_artifact(self.kv, &repo, &path).await?;
        let record = match record {
            Some(r) => r,
            None => return Ok(None),
        };
        let blob_id = record.blob_id.as_deref().unwrap_or("");
        if blob_id.is_empty() {
            return Ok(None);
        }
        let (reader, size) = self.blobs.open_read(blob_id).await?.ok_or_else(|| {
            DepotError::DataIntegrity(format!("blob missing for blob_id {blob_id}"))
        })?;
        Ok(Some((reader, size, record)))
    }

    /// Toggle the yanked flag on a crate version.
    pub async fn set_yanked(&self, name: &str, version: &str, yanked: bool) -> error::Result<bool> {
        let path = crate_path(name, version);
        let repo = self.repo.to_string();
        let path_clone = path.clone();

        let record = service::get_artifact(self.kv, &repo, &path_clone).await?;

        let mut record = match record {
            Some(r) => r,
            None => return Ok(false),
        };

        match &mut record.kind {
            ArtifactKind::CargoCrate { ref mut cargo, .. } => {
                cargo.yanked = yanked;
            }
            _ => return Ok(false),
        }

        record.updated_at = depot_core::repo::now_utc();

        let repo = self.repo.to_string();
        let old_record = service::put_artifact(self.kv, &repo, &path, &record).await?;
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, &path, count_delta, bytes_delta)
            .await;
        Ok(true)
    }

    /// Build the sparse index JSON lines for a crate name.
    pub async fn build_index_entry(&self, name: &str) -> error::Result<String> {
        let norm = normalize_crate_name(name);
        let prefix = format!("_crates/{norm}/");
        let repo = self.repo.to_string();
        let lines = service::fold_all_artifacts(
            self.kv,
            &repo,
            &prefix,
            String::new,
            |acc, _sk, record| {
                if let ArtifactKind::CargoCrate { ref cargo, .. } = record.kind {
                    let features: serde_json::Value =
                        serde_json::from_str(&cargo.features).unwrap_or(serde_json::json!({}));
                    let features2: Option<serde_json::Value> = cargo
                        .features2
                        .as_ref()
                        .and_then(|s| serde_json::from_str(s).ok());
                    let entry = serde_json::json!({
                        "name": cargo.name,
                        "vers": cargo.vers,
                        "deps": cargo.deps,
                        "cksum": cargo.cksum,
                        "features": features,
                        "features2": features2,
                        "yanked": cargo.yanked,
                        "links": cargo.links,
                        "rust_version": cargo.rust_version,
                        "v": 2,
                    });
                    acc.push_str(&serde_json::to_string(&entry).unwrap_or_default());
                    acc.push('\n');
                }
                Ok(())
            },
            |mut a, b| {
                a.push_str(&b);
                a
            },
        )
        .await?;
        Ok(lines)
    }

    /// List all unique crate names in this repo.
    pub async fn list_crates(&self) -> error::Result<Vec<String>> {
        let repo = self.repo.to_string();
        let mut seen = service::fold_all_artifacts(
            self.kv,
            &repo,
            "_crates/",
            HashSet::new,
            |set, sk, _record| {
                let parts: Vec<&str> = sk.splitn(4, '/').collect();
                if let Some(&name) = parts.get(1) {
                    set.insert(name.to_string());
                }
                Ok(())
            },
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .await?;
        let mut crates: Vec<String> = seen.drain().collect();
        crates.sort();
        Ok(crates)
    }
}

fn hex_encode(data: impl AsRef<[u8]>) -> String {
    data.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_crate_name() {
        assert_eq!(normalize_crate_name("My-Crate"), "my_crate");
        assert_eq!(normalize_crate_name("hello_world"), "hello_world");
        assert_eq!(normalize_crate_name("UPPER-CASE"), "upper_case");
    }

    #[test]
    fn test_index_prefix() {
        assert_eq!(index_prefix("a"), "1");
        assert_eq!(index_prefix("ab"), "2");
        assert_eq!(index_prefix("abc"), "3/a");
        assert_eq!(index_prefix("abcd"), "ab/cd");
        assert_eq!(index_prefix("serde"), "se/rd");
        assert_eq!(index_prefix("tokio"), "to/ki");
    }

    #[test]
    fn test_crate_path() {
        assert_eq!(
            crate_path("my-crate", "1.0.0"),
            "_crates/my_crate/1.0.0/my-crate-1.0.0.crate"
        );
    }
}
