// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use depot_core::error::{self, DepotError, Retryability};
use depot_core::service;
use depot_core::store::blob::BlobStore;
use depot_core::store::kv::{
    ArtifactKind, ArtifactRecord, BlobRecord, KvStore, CURRENT_RECORD_VERSION,
};
use depot_core::update::UpdateSender;

use super::hex;
use super::metadata::{
    generate_filelists_xml, generate_other_xml, generate_primary_xml, generate_repomd_xml,
    gzip_compress, RepoDataEntry,
};
use super::parse::parse_rpm;

/// YUM storage operations for a repository.
pub struct YumStore<'a> {
    pub repo: &'a str,
    pub kv: &'a dyn KvStore,
    pub blobs: &'a dyn BlobStore,
    pub store: &'a str,
    pub updater: &'a UpdateSender,
    pub repodata_depth: u32,
}

impl<'a> YumStore<'a> {
    /// Store an RPM package, parse its headers, and set the stale flag.
    pub async fn put_package(
        &self,
        data: &[u8],
        filename: &str,
        directory: &str,
    ) -> error::Result<ArtifactRecord> {
        let sha256_hash = hex::encode(Sha256::digest(data));
        let meta = parse_rpm(std::io::Cursor::new(data), &sha256_hash, data.len() as u64)?;
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let now = depot_core::repo::now_utc();

        let blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&blob_id, data).await?;

        let path = packages_kv_path(directory, filename);

        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.clone(),
            hash: blake3_hash.clone(),
            size: data.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let store = self.store.to_string();
        let repo = self.repo.to_string();
        let blob_id = service::claim_or_reuse_blob(self.kv, self.blobs, &store, &blob_rec).await?;

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: data.len() as u64,
            content_type: "application/x-rpm".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(blob_id),
            content_hash: Some(blake3_hash.clone()),
            etag: Some(blake3_hash.clone()),
            kind: ArtifactKind::YumPackage { rpm: meta },
            internal: false,
        };

        let path_owned = path.clone();
        let record_clone = record.clone();

        let old_record = service::put_artifact(self.kv, &repo, &path_owned, &record_clone).await?;
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, &path_owned, count_delta, bytes_delta)
            .await;
        self.updater
            .store_changed(self.store, count_delta, bytes_delta)
            .await;
        // Set stale flag (if not already set)
        self.set_stale_flag().await?;
        Ok(record)
    }

    /// Commit an RPM package whose blob has already been written via `BlobWriter`.
    /// Reads the blob back to parse RPM headers, then writes KV records.
    pub async fn commit_package(
        &self,
        blob_id: &str,
        blake3_hash: &str,
        sha256_hash: &str,
        size: u64,
        filename: &str,
        directory: &str,
    ) -> error::Result<ArtifactRecord> {
        let (reader, _) = self.blobs.open_read(blob_id).await?.ok_or_else(|| {
            DepotError::DataIntegrity(format!("blob {blob_id} missing after write"))
        })?;
        let sha256_owned = sha256_hash.to_string();
        let archive_size = size;
        let meta = tokio::task::spawn_blocking(move || {
            let sync_reader = tokio_util::io::SyncIoBridge::new(reader);
            parse_rpm(sync_reader, &sha256_owned, archive_size)
        })
        .await
        .map_err(|e| DepotError::Storage(Box::new(e), Retryability::Permanent))??;

        let now = depot_core::repo::now_utc();
        let path = packages_kv_path(directory, filename);

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
        let blob_id = service::claim_or_reuse_blob(self.kv, self.blobs, &store, &blob_rec).await?;

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size,
            content_type: "application/x-rpm".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(blob_id),
            content_hash: Some(blake3_hash.to_string()),
            etag: Some(blake3_hash.to_string()),
            kind: ArtifactKind::YumPackage { rpm: meta },
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
        self.updater
            .store_changed(self.store, count_delta, bytes_delta)
            .await;
        self.set_stale_flag().await?;
        Ok(record)
    }

    /// Set the metadata stale flag on this repo (unconditional overwrite).
    /// Also propagates to any parent proxy repos.
    pub async fn set_metadata_stale(&self) -> error::Result<()> {
        self.set_stale_flag_inner(false).await
    }

    /// Set the stale flag only if not already present.
    /// Also propagates to any parent proxy repos.
    async fn set_stale_flag(&self) -> error::Result<()> {
        self.set_stale_flag_inner(true).await
    }

    async fn set_stale_flag_inner(&self, if_absent: bool) -> error::Result<()> {
        let now = depot_core::repo::now_utc();
        let flag = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: 0,
            content_type: "text/plain".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: None,
            content_hash: None,
            etag: None,
            kind: ArtifactKind::YumMetadata,
            internal: true,
        };
        let repo = self.repo.to_string();
        if if_absent {
            service::put_artifact_if_absent(self.kv, &repo, "_yum/metadata_stale", &flag).await?;
        } else {
            service::put_artifact(self.kv, &repo, "_yum/metadata_stale", &flag).await?;
        }
        // Propagation to parent proxies is handled by the API layer via
        // service::propagate_staleness() spawned as an async task.
        Ok(())
    }

    /// Check if metadata is stale.
    pub async fn is_metadata_stale(&self) -> error::Result<bool> {
        let repo = self.repo.to_string();
        Ok(service::get_artifact(self.kv, &repo, "_yum/metadata_stale")
            .await?
            .is_some())
    }

    /// Clear the stale flag.
    async fn clear_stale_flag(&self) -> error::Result<()> {
        let repo = self.repo.to_string();
        service::delete_artifact(self.kv, &repo, "_yum/metadata_stale").await?;
        Ok(())
    }

    /// Get a metadata file's content, routing by path type.
    ///
    /// Handles root-level paths like `repodata/repomd.xml` and depth-prefixed
    /// paths like `p1/7/x86_64/repodata/repomd.xml`.
    pub async fn get_metadata(&self, meta_path: &str) -> error::Result<Option<Vec<u8>>> {
        // Snapshot paths are handled separately.
        if let Some(rest) = meta_path.strip_prefix("snapshots/") {
            return self.get_snapshot_metadata(rest).await;
        }

        // Handle .asc files: try stored artifact, fall back to on-demand signing.
        if meta_path.ends_with("repomd.xml.asc") {
            match self.read_artifact(meta_path).await? {
                Some(data) => return Ok(Some(data)),
                None => {
                    // Derive the repomd.xml path from the .asc path.
                    let repomd_path = meta_path.strip_suffix(".asc").unwrap_or(meta_path);
                    return self.get_repomd_asc_for(repomd_path).await;
                }
            }
        }

        // All other repodata paths: read directly from KV.
        // Works for both root (repodata/...) and prefixed (p1/7/x86_64/repodata/...).
        self.read_artifact(meta_path).await
    }

    /// Get snapshot metadata, handling both root and prefixed repodata.
    async fn get_snapshot_metadata(&self, rest: &str) -> error::Result<Option<Vec<u8>>> {
        // rest = "{name}/repodata/{file}" or "{name}/{prefix}/repodata/{file}"
        let (name, inner) = match rest.split_once('/') {
            Some(pair) => pair,
            None => return Ok(None),
        };
        if inner.ends_with("repomd.xml.asc") {
            let snap_asc = format!("snapshots/{name}/{inner}");
            match self.read_artifact(&snap_asc).await? {
                Some(data) => Ok(Some(data)),
                None => self.get_snapshot_repomd_asc(name).await,
            }
        } else {
            // Try snapshot namespace first, then fall back to main repodata.
            let snap_path = format!("snapshots/{name}/{inner}");
            match self.read_artifact(&snap_path).await? {
                Some(data) => Ok(Some(data)),
                None => self.read_artifact(inner).await,
            }
        }
    }

    /// Read a single artifact's blob by KV path.
    async fn read_artifact(&self, path: &str) -> error::Result<Option<Vec<u8>>> {
        let repo = self.repo.to_string();
        let record = match service::get_artifact(self.kv, &repo, path).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        let blob_id = record.blob_id.as_deref().unwrap_or("");
        if blob_id.is_empty() {
            return Ok(None);
        }
        self.blobs.get(blob_id).await
    }

    /// Generate repomd.xml.asc on demand from the signing key for any repomd.xml path.
    async fn get_repomd_asc_for(&self, repomd_path: &str) -> error::Result<Option<Vec<u8>>> {
        let repomd = match self.read_artifact(repomd_path).await? {
            Some(d) => d,
            None => return Ok(None),
        };
        let repomd_text = String::from_utf8_lossy(&repomd);
        if let Some(key) = self.get_signing_key().await? {
            match depot_format_apt::store::sign_release_pub(&key, &repomd_text) {
                Ok((_inrelease, detached)) => Ok(Some(detached.into_bytes())),
                Err(_) => Ok(Some(Vec::new())),
            }
        } else {
            Ok(Some(Vec::new()))
        }
    }

    /// Generate repomd.xml.asc for a snapshot on demand.
    async fn get_snapshot_repomd_asc(&self, snapshot_name: &str) -> error::Result<Option<Vec<u8>>> {
        let snap_path = format!("snapshots/{snapshot_name}/repodata/repomd.xml");
        let repomd = match self.read_artifact(&snap_path).await? {
            Some(d) => d,
            None => return Ok(None),
        };
        let repomd_text = String::from_utf8_lossy(&repomd);
        if let Some(key) = self.get_signing_key().await? {
            match depot_format_apt::store::sign_release_pub(&key, &repomd_text) {
                Ok((_inrelease, detached)) => Ok(Some(detached.into_bytes())),
                Err(_) => Ok(Some(Vec::new())),
            }
        } else {
            Ok(Some(Vec::new()))
        }
    }

    /// Rebuild YUM metadata: group packages by repodata prefix, generate
    /// content-addressed metadata for each group, and store repomd.xml as
    /// the mutable commit point.
    pub async fn rebuild_metadata(&self, _signing_key_armor: Option<&str>) -> error::Result<()> {
        // Delete stale flag first — any upload that arrives during our rebuild
        // will re-set it, guaranteeing another rebuild on the next metadata fetch.
        self.clear_stale_flag().await?;

        // Scan both new Packages/ and legacy _packages/ prefixes.
        let repo = self.repo.to_string();
        let scan_prefix = |kv: &'a dyn KvStore, repo: &str, prefix: &str| {
            let repo = repo.to_string();
            let prefix = prefix.to_string();
            async move {
                service::fold_all_artifacts(
                    kv,
                    &repo,
                    &prefix,
                    Vec::new,
                    |vec, sk, record| {
                        if matches!(&record.kind, ArtifactKind::YumPackage { .. }) {
                            vec.push((sk.to_string(), record.clone()));
                        }
                        Ok(())
                    },
                    |mut a, b| {
                        a.extend(b);
                        a
                    },
                )
                .await
            }
        };

        let packages: Vec<(String, ArtifactRecord)> = scan_prefix(self.kv, &repo, "").await?;

        // Group packages by repodata prefix.
        let mut groups: BTreeMap<String, Vec<(String, ArtifactRecord)>> = BTreeMap::new();
        for (path, record) in packages {
            let prefix = repodata_prefix(&path, self.repodata_depth);
            groups.entry(prefix).or_default().push((path, record));
        }
        // For depth=0 with no packages, ensure we still generate empty metadata.
        if groups.is_empty() && self.repodata_depth == 0 {
            groups.insert(String::new(), Vec::new());
        }

        let now = depot_core::repo::now_utc();
        let timestamp = now.timestamp();

        for (prefix, group_packages) in &groups {
            let rd = repodata_dir(prefix);

            let primary_xml = generate_primary_xml(group_packages, prefix);
            let primary_gz = gzip_compress(primary_xml.as_bytes());
            let primary_gz_sha256 = hex::encode(Sha256::digest(&primary_gz));
            let primary_open_sha256 = hex::encode(Sha256::digest(primary_xml.as_bytes()));

            let filelists_xml = generate_filelists_xml(group_packages);
            let filelists_gz = gzip_compress(filelists_xml.as_bytes());
            let filelists_gz_sha256 = hex::encode(Sha256::digest(&filelists_gz));
            let filelists_open_sha256 = hex::encode(Sha256::digest(filelists_xml.as_bytes()));

            let other_xml = generate_other_xml(group_packages);
            let other_gz = gzip_compress(other_xml.as_bytes());
            let other_gz_sha256 = hex::encode(Sha256::digest(&other_gz));
            let other_open_sha256 = hex::encode(Sha256::digest(other_xml.as_bytes()));

            let primary_filename = format!("{primary_gz_sha256}-primary.xml.gz");
            let filelists_filename = format!("{filelists_gz_sha256}-filelists.xml.gz");
            let other_filename = format!("{other_gz_sha256}-other.xml.gz");

            // Store content-addressed files first (immutable).
            self.store_metadata_file(&format!("{rd}/{primary_filename}"), &primary_gz, now)
                .await?;
            self.store_metadata_file(&format!("{rd}/{filelists_filename}"), &filelists_gz, now)
                .await?;
            self.store_metadata_file(&format!("{rd}/{other_filename}"), &other_gz, now)
                .await?;

            let repomd = generate_repomd_xml(
                timestamp,
                &RepoDataEntry {
                    filename: &primary_filename,
                    checksum: &primary_gz_sha256,
                    size: primary_gz.len() as u64,
                    open_checksum: &primary_open_sha256,
                    open_size: primary_xml.len() as u64,
                },
                &RepoDataEntry {
                    filename: &filelists_filename,
                    checksum: &filelists_gz_sha256,
                    size: filelists_gz.len() as u64,
                    open_checksum: &filelists_open_sha256,
                    open_size: filelists_xml.len() as u64,
                },
                &RepoDataEntry {
                    filename: &other_filename,
                    checksum: &other_gz_sha256,
                    size: other_gz.len() as u64,
                    open_checksum: &other_open_sha256,
                    open_size: other_xml.len() as u64,
                },
                timestamp,
            );

            // Store repomd.xml last (commit point).
            self.store_repomd_at(&format!("{rd}/repomd.xml"), repomd.as_bytes(), now)
                .await?;
        }

        Ok(())
    }

    /// Store a content-addressed metadata file (immutable).
    async fn store_metadata_file(
        &self,
        path: &str,
        data: &[u8],
        now: chrono::DateTime<chrono::Utc>,
    ) -> error::Result<()> {
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let sha256 = hex::encode(Sha256::digest(data));

        let blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&blob_id, data).await?;

        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.clone(),
            hash: blake3_hash.clone(),
            size: data.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let blob_id =
            service::claim_or_reuse_blob(self.kv, self.blobs, self.store, &blob_rec).await?;

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: data.len() as u64,
            content_type: "application/octet-stream".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(blob_id),
            etag: Some(blake3_hash.clone()),
            content_hash: Some(sha256),
            kind: ArtifactKind::YumMetadata,
            internal: true,
        };
        // Content-addressed files are immutable — only written once.
        let repo = self.repo.to_string();
        let written = service::put_artifact_if_absent(self.kv, &repo, path, &record).await?;
        if written {
            self.updater
                .dir_changed(self.repo, path, 1, record.size as i64)
                .await;
            self.updater
                .store_changed(self.store, 1, record.size as i64)
                .await;
        }
        Ok(())
    }

    /// Store repomd.xml at its mutable path (the single commit point).
    async fn store_repomd(
        &self,
        data: &[u8],
        now: chrono::DateTime<chrono::Utc>,
    ) -> error::Result<()> {
        self.store_repomd_at("repodata/repomd.xml", data, now).await
    }

    /// Store repomd.xml at an arbitrary mutable path.
    async fn store_repomd_at(
        &self,
        path: &str,
        data: &[u8],
        now: chrono::DateTime<chrono::Utc>,
    ) -> error::Result<()> {
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let sha256 = hex::encode(Sha256::digest(data));

        let blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&blob_id, data).await?;

        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.clone(),
            hash: blake3_hash.clone(),
            size: data.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let blob_id =
            service::claim_or_reuse_blob(self.kv, self.blobs, self.store, &blob_rec).await?;

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: data.len() as u64,
            content_type: "application/xml".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(blob_id),
            etag: Some(blake3_hash.clone()),
            content_hash: Some(sha256),
            kind: ArtifactKind::YumMetadata,
            internal: true,
        };
        let repo = self.repo.to_string();
        let old_record = service::put_artifact(self.kv, &repo, path, &record).await?;
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, path, count_delta, bytes_delta)
            .await;
        self.updater
            .store_changed(self.store, count_delta, bytes_delta)
            .await;
        Ok(())
    }

    /// Delete all repodata artifacts (root and prefixed) from this repo.
    /// Returns the number of deleted entries.
    pub async fn delete_all_repodata(&self) -> error::Result<u64> {
        let repo = self.repo.to_string();
        let mut deleted = 0u64;

        // Collect all paths containing "repodata/" by scanning the full repo.
        let repodata_paths: Vec<String> = service::fold_all_artifacts(
            self.kv,
            &repo,
            "",
            Vec::new,
            |vec, sk, _record| {
                let s = sk.to_string();
                if s.starts_with("repodata/") || s.contains("/repodata/") {
                    vec.push(s);
                }
                Ok(())
            },
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .await?;

        for path in &repodata_paths {
            if let Some(old) = service::delete_artifact(self.kv, &repo, path).await? {
                self.updater
                    .dir_changed(self.repo, path, -1, -(old.size as i64))
                    .await;
                self.updater
                    .store_changed(self.store, -1, -(old.size as i64))
                    .await;
                deleted += 1;
            }
        }

        Ok(deleted)
    }

    /// Public wrapper for storing cached metadata from upstream.
    pub async fn store_cached_metadata(&self, path: &str, data: &[u8]) -> error::Result<()> {
        let now = depot_core::repo::now_utc();
        self.store_metadata_file(path, data, now).await
    }

    /// Public wrapper for storing a cached repomd.xml from upstream.
    pub async fn store_cached_repomd(&self, data: &[u8]) -> error::Result<()> {
        let now = depot_core::repo::now_utc();
        self.store_repomd(data, now).await
    }

    /// Public wrapper for clear_stale_flag (for proxy rebuild).
    pub async fn clear_stale_flag_pub(&self) -> error::Result<()> {
        self.clear_stale_flag().await
    }

    /// Public wrapper for store_metadata_file (for proxy rebuild).
    pub async fn store_metadata_file_pub(&self, path: &str, data: &[u8]) -> error::Result<()> {
        let now = depot_core::repo::now_utc();
        self.store_metadata_file(path, data, now).await
    }

    /// Public wrapper for store_repomd (for proxy rebuild).
    pub async fn store_repomd_pub(&self, data: &[u8]) -> error::Result<()> {
        let now = depot_core::repo::now_utc();
        self.store_repomd(data, now).await
    }

    /// Get a package by its path in the KV store.
    pub async fn get_package(
        &self,
        path: &str,
    ) -> error::Result<Option<(depot_core::store::blob::BlobReader, u64, ArtifactRecord)>> {
        let repo = self.repo.to_string();
        let record = match service::get_artifact(self.kv, &repo, path).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        let blob_id = record.blob_id.as_deref().unwrap_or("");
        let (reader, size) = self.blobs.open_read(blob_id).await?.ok_or_else(|| {
            DepotError::DataIntegrity(format!("blob missing for blob_id {}", blob_id))
        })?;
        Ok(Some((reader, size, record)))
    }

    /// Store the GPG key pair as blob-backed artifacts.
    pub async fn store_signing_key(
        &self,
        private_key_armor: &str,
        public_key_armor: &str,
    ) -> error::Result<()> {
        let now = depot_core::repo::now_utc();
        let store = self.store.to_string();
        let repo = self.repo.to_string();

        // Store private key as blob
        let priv_data = private_key_armor.as_bytes();
        let priv_blake3 = blake3::hash(priv_data).to_hex().to_string();
        let priv_blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&priv_blob_id, priv_data).await?;

        let priv_blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: priv_blob_id.clone(),
            hash: priv_blake3.clone(),
            size: priv_data.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let priv_blob_id =
            service::claim_or_reuse_blob(self.kv, self.blobs, &store, &priv_blob_rec).await?;
        let priv_record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: priv_data.len() as u64,
            content_type: "application/pgp-keys".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(priv_blob_id),
            etag: Some(priv_blake3.clone()),
            content_hash: Some(priv_blake3),
            kind: ArtifactKind::YumMetadata,
            internal: true,
        };

        // Store public key as blob
        let pub_data = public_key_armor.as_bytes();
        let pub_blake3 = blake3::hash(pub_data).to_hex().to_string();
        let pub_blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&pub_blob_id, pub_data).await?;

        let pub_blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: pub_blob_id.clone(),
            hash: pub_blake3.clone(),
            size: pub_data.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let pub_blob_id =
            service::claim_or_reuse_blob(self.kv, self.blobs, &store, &pub_blob_rec).await?;
        let pub_record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: pub_data.len() as u64,
            content_type: "application/pgp-keys".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(pub_blob_id),
            etag: Some(pub_blake3.clone()),
            content_hash: Some(pub_blake3),
            kind: ArtifactKind::YumMetadata,
            internal: true,
        };

        let old_priv =
            service::put_artifact(self.kv, &repo, "_yum/signing_key", &priv_record).await?;
        let (cd, bd) = match &old_priv {
            Some(old) => (0i64, priv_record.size as i64 - old.size as i64),
            None => (1i64, priv_record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, "_yum/signing_key", cd, bd)
            .await;
        self.updater.store_changed(self.store, cd, bd).await;

        let old_pub = service::put_artifact(self.kv, &repo, "_yum/public_key", &pub_record).await?;
        let (cd, bd) = match &old_pub {
            Some(old) => (0i64, pub_record.size as i64 - old.size as i64),
            None => (1i64, pub_record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, "_yum/public_key", cd, bd)
            .await;
        self.updater.store_changed(self.store, cd, bd).await;
        Ok(())
    }

    /// Get the signing private key armor.
    pub async fn get_signing_key(&self) -> error::Result<Option<String>> {
        let repo = self.repo.to_string();
        let record = service::get_artifact(self.kv, &repo, "_yum/signing_key").await?;
        if let Some(record) = record {
            let blob_id = record.blob_id.as_deref().unwrap_or("");
            if !blob_id.is_empty() {
                if let Some(data) = self.blobs.get(blob_id).await? {
                    return Ok(Some(String::from_utf8_lossy(&data).to_string()));
                }
            }
        }
        Ok(None)
    }

    /// Get the public key armor.
    pub async fn get_public_key(&self) -> error::Result<Option<String>> {
        let repo = self.repo.to_string();
        let record = service::get_artifact(self.kv, &repo, "_yum/public_key").await?;
        if let Some(record) = record {
            let blob_id = record.blob_id.as_deref().unwrap_or("");
            if !blob_id.is_empty() {
                if let Some(data) = self.blobs.get(blob_id).await? {
                    return Ok(Some(String::from_utf8_lossy(&data).to_string()));
                }
            }
        }
        Ok(None)
    }

    /// Create a snapshot of repodata under a new name.
    ///
    /// Discovers all repodata directories (root and depth-prefixed),
    /// copies content-addressed file refs to the snapshot namespace, and
    /// stores repomd.xml copies. No blob data is duplicated.
    pub async fn create_snapshot(
        &self,
        snapshot_name: &str,
        _signing_key: Option<&str>,
    ) -> error::Result<()> {
        let repo = self.repo.to_string();
        let now = depot_core::repo::now_utc();

        // Collect all repomd.xml paths in the repo (root and prefixed).
        let repomd_paths: Vec<String> = service::fold_all_artifacts(
            self.kv,
            &repo,
            "",
            Vec::new,
            |vec, sk, _record| {
                let s = sk.to_string();
                if s.ends_with("/repomd.xml")
                    && (s.starts_with("repodata/") || s.contains("/repodata/"))
                    && !s.starts_with("snapshots/")
                {
                    vec.push(s);
                }
                Ok(())
            },
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .await?;

        if repomd_paths.is_empty() {
            return Err(DepotError::NotFound(
                "no repodata found to snapshot".to_string(),
            ));
        }

        for repomd_path in &repomd_paths {
            let repomd_data = match self.read_artifact(repomd_path).await? {
                Some(d) => d,
                None => continue,
            };
            let repomd_text = String::from_utf8_lossy(&repomd_data).to_string();

            // Determine the repodata dir from the repomd path (e.g. "p1/7/x86_64/repodata")
            let rd = repomd_path
                .strip_suffix("/repomd.xml")
                .unwrap_or("repodata");

            let locations = super::metadata::parse_repomd_locations(&repomd_text);

            // Copy content-addressed file refs to snapshot namespace.
            for loc in &locations {
                // loc is like "repodata/{sha256}-primary.xml.gz" — make it absolute
                // by replacing the "repodata/" prefix with the actual rd.
                let actual_path = if let Some(file) = loc.strip_prefix("repodata/") {
                    format!("{rd}/{file}")
                } else {
                    loc.clone()
                };
                if let Some(record) = service::get_artifact(self.kv, &repo, &actual_path).await? {
                    if let Some(blob_id) = record.blob_id.as_deref() {
                        let snap_path = format!("snapshots/{snapshot_name}/{actual_path}");
                        let snap_record = ArtifactRecord {
                            schema_version: CURRENT_RECORD_VERSION,
                            id: String::new(),
                            size: record.size,
                            content_type: "application/octet-stream".to_string(),
                            created_at: now,
                            updated_at: now,
                            last_accessed_at: now,
                            path: String::new(),
                            blob_id: Some(blob_id.to_string()),
                            etag: record.etag.clone(),
                            content_hash: record.content_hash.clone(),
                            kind: ArtifactKind::YumMetadata,
                            internal: true,
                        };
                        let written = service::put_artifact_if_absent(
                            self.kv,
                            &repo,
                            &snap_path,
                            &snap_record,
                        )
                        .await?;
                        if written {
                            self.updater
                                .dir_changed(self.repo, &snap_path, 1, snap_record.size as i64)
                                .await;
                            self.updater
                                .store_changed(self.store, 1, snap_record.size as i64)
                                .await;
                        }
                    }
                }
            }

            // Store repomd.xml in snapshot namespace.
            let snap_repomd = format!("snapshots/{snapshot_name}/{repomd_path}");
            let repomd_bytes = repomd_text.as_bytes();
            let blake3_hash = blake3::hash(repomd_bytes).to_hex().to_string();
            let sha256 = hex::encode(Sha256::digest(repomd_bytes));
            let blob_id = uuid::Uuid::new_v4().to_string();
            self.blobs.put(&blob_id, repomd_bytes).await?;
            let blob_rec = BlobRecord {
                schema_version: CURRENT_RECORD_VERSION,
                blob_id: blob_id.clone(),
                hash: blake3_hash.clone(),
                size: repomd_bytes.len() as u64,
                created_at: now,
                store: self.store.to_string(),
            };
            let blob_id =
                service::claim_or_reuse_blob(self.kv, self.blobs, self.store, &blob_rec).await?;
            let record = ArtifactRecord {
                schema_version: CURRENT_RECORD_VERSION,
                id: String::new(),
                size: repomd_bytes.len() as u64,
                content_type: "application/xml".to_string(),
                created_at: now,
                updated_at: now,
                last_accessed_at: now,
                path: String::new(),
                blob_id: Some(blob_id),
                etag: Some(blake3_hash),
                content_hash: Some(sha256),
                kind: ArtifactKind::YumMetadata,
                internal: true,
            };
            let old_record = service::put_artifact(self.kv, &repo, &snap_repomd, &record).await?;
            let (count_delta, bytes_delta) = match &old_record {
                Some(old) => (0i64, record.size as i64 - old.size as i64),
                None => (1i64, record.size as i64),
            };
            self.updater
                .dir_changed(self.repo, &snap_repomd, count_delta, bytes_delta)
                .await;
            self.updater
                .store_changed(self.store, count_delta, bytes_delta)
                .await;
        }

        Ok(())
    }
}

/// Build the KV path for a package given its directory and filename.
fn packages_kv_path(directory: &str, filename: &str) -> String {
    let dir = directory.trim_matches('/');
    if dir.is_empty() {
        filename.to_string()
    } else {
        format!("{dir}/{filename}")
    }
}

/// Compute the repodata prefix for a package based on its KV path and the configured depth.
///
/// E.g. with depth=3 and path `p1/7/x86_64/foo.rpm`, returns `"p1/7/x86_64"`.
/// With depth=0, always returns `""`.
fn repodata_prefix(kv_path: &str, depth: u32) -> String {
    if depth == 0 {
        return String::new();
    }
    // Take directory portion (strip filename).
    let dir = match kv_path.rsplit_once('/') {
        Some((d, _)) => d,
        None => return String::new(), // package at root, no directory segments
    };
    let segments: Vec<&str> = dir.split('/').collect();
    let take = (depth as usize).min(segments.len());
    segments.get(..take).map_or(String::new(), |s| s.join("/"))
}

/// Build the repodata directory path for a given prefix.
fn repodata_dir(prefix: &str) -> String {
    if prefix.is_empty() {
        "repodata".to_string()
    } else {
        format!("{prefix}/repodata")
    }
}
