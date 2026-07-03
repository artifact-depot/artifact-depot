// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! npm package storage logic.
//!
//! Maps npm concepts onto our generic KV + blob store:
//!
//! - Tarballs stored at `_tarballs/{normalized_name}/{version}/{filename}`
//! - Version metadata stored at `_versions/{normalized_name}/{version}` (inline JSON)
//! - Dist-tags stored at `_dist-tags/{normalized_name}` (inline JSON)
//! - BLAKE3 is the blob store key; SHA-1 and SHA-512 stored in metadata for npm verification.

use sha1::Sha1;
use sha2::{Digest, Sha512};
use std::collections::HashMap;

use depot_core::error::{self, DepotError};
use depot_core::repo::apply_upstream_auth;
use depot_core::service;
use depot_core::store::blob::BlobStore;
use depot_core::store::kv::{
    ArtifactKind, ArtifactRecord, BlobRecord, KvStore, NpmPackageMeta, UpstreamAuth,
    CURRENT_RECORD_VERSION,
};
use depot_core::update::UpdateSender;

/// npm name normalization: lowercase only (npm names are case-insensitive).
pub fn normalize_name(name: &str) -> String {
    name.to_lowercase()
}

/// Build the KV path for a tarball artifact.
pub fn tarball_path(name: &str, version: &str, filename: &str) -> String {
    let norm = normalize_name(name);
    format!("_tarballs/{norm}/{version}/{filename}")
}

/// Build the KV path for version metadata.
pub fn version_path(name: &str, version: &str) -> String {
    let norm = normalize_name(name);
    format!("_versions/{norm}/{version}")
}

/// Build the KV path for dist-tags.
pub fn dist_tags_path(name: &str) -> String {
    let norm = normalize_name(name);
    format!("_dist-tags/{norm}")
}

/// npm storage operations for a repository.
pub struct NpmStore<'a> {
    pub repo: &'a str,
    pub kv: &'a dyn KvStore,
    pub blobs: &'a dyn BlobStore,
    pub store: &'a str,
    pub updater: &'a UpdateSender,
}

impl<'a> NpmStore<'a> {
    /// Store a package tarball along with version metadata and dist-tags.
    pub async fn put_package(
        &self,
        name: &str,
        version: &str,
        filename: &str,
        data: &[u8],
        version_metadata_json: &str,
        dist_tags: &HashMap<String, String>,
    ) -> error::Result<ArtifactRecord> {
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let sha1_hash = hex::encode(Sha1::digest(data));
        let sha512_hash = base64_encode_sha512(data);

        let blob_id = uuid::Uuid::new_v4().to_string();
        let now = depot_core::repo::now_utc();

        self.blobs.put(&blob_id, data).await?;

        let path = tarball_path(name, version, filename);
        let store = self.store.to_string();

        // Atomic KV updates
        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.clone(),
            hash: blake3_hash.clone(),
            size: data.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let blob_id = service::claim_or_reuse_blob(self.kv, self.blobs, &store, &blob_rec).await?;

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: data.len() as u64,
            content_type: "application/octet-stream".to_string(),
            kind: ArtifactKind::NpmPackage {
                npm: NpmPackageMeta {
                    name: name.to_string(),
                    version: version.to_string(),
                    filename: filename.to_string(),
                    sha1: sha1_hash,
                    sha512: sha512_hash,
                    upstream_url: None,
                },
            },
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
            blob_id: Some(blob_id.clone()),
            content_hash: Some(blake3_hash.clone()),
            etag: Some(blake3_hash.clone()),
        };

        // Store version metadata as a blob-backed artifact
        let ver_blob_id = uuid::Uuid::new_v4().to_string();
        let ver_bytes = version_metadata_json.as_bytes();
        let ver_hash = blake3::hash(ver_bytes).to_hex().to_string();
        self.blobs.put(&ver_blob_id, ver_bytes).await?;
        let ver_blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: ver_blob_id.clone(),
            hash: ver_hash.clone(),
            size: ver_bytes.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let ver_blob_id =
            service::claim_or_reuse_blob(self.kv, self.blobs, &store, &ver_blob_rec).await?;
        let version_record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: ver_bytes.len() as u64,
            content_type: "application/json".to_string(),
            kind: ArtifactKind::NpmMetadata,
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: true,
            blob_id: Some(ver_blob_id.clone()),
            content_hash: Some(ver_hash.clone()),
            etag: Some(ver_hash.clone()),
        };

        // Merge dist-tags: read existing, overlay new
        let existing_tags = self.get_dist_tags(name).await.unwrap_or_default();
        let mut merged_tags = existing_tags;
        for (k, v) in dist_tags {
            merged_tags.insert(k.clone(), v.clone());
        }
        let tags_json = serde_json::to_string(&merged_tags).unwrap_or_default();
        let tags_bytes = tags_json.as_bytes();
        let tags_blob_id = uuid::Uuid::new_v4().to_string();
        let tags_hash = blake3::hash(tags_bytes).to_hex().to_string();
        self.blobs.put(&tags_blob_id, tags_bytes).await?;
        let tags_blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: tags_blob_id.clone(),
            hash: tags_hash.clone(),
            size: tags_bytes.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let tags_blob_id =
            service::claim_or_reuse_blob(self.kv, self.blobs, &store, &tags_blob_rec).await?;
        let tags_record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: tags_bytes.len() as u64,
            content_type: "application/json".to_string(),
            kind: ArtifactKind::NpmMetadata,
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: true,
            blob_id: Some(tags_blob_id.clone()),
            content_hash: Some(tags_hash.clone()),
            etag: Some(tags_hash.clone()),
        };

        let repo = self.repo.to_string();
        let record_clone = record.clone();
        let ver_path = version_path(name, version);
        let dt_path = dist_tags_path(name);

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
        let old_ver = service::put_artifact(self.kv, &repo, &ver_path, &version_record).await?;
        let (vc, vb) = match &old_ver {
            Some(old) => (0i64, version_record.size as i64 - old.size as i64),
            None => (1i64, version_record.size as i64),
        };
        self.updater.dir_changed(self.repo, &ver_path, vc, vb).await;
        self.updater.store_changed(self.store, vc, vb).await;
        let old_tags = service::put_artifact(self.kv, &repo, &dt_path, &tags_record).await?;
        let (tc, tb) = match &old_tags {
            Some(old) => (0i64, tags_record.size as i64 - old.size as i64),
            None => (1i64, tags_record.size as i64),
        };
        self.updater.dir_changed(self.repo, &dt_path, tc, tb).await;
        self.updater.store_changed(self.store, tc, tb).await;

        Ok(record)
    }

    /// Store a package from already-computed hashes (used by cache repos).
    #[allow(clippy::too_many_arguments)]
    pub async fn put_package_from_upstream(
        &self,
        name: &str,
        version: &str,
        filename: &str,
        data: &[u8],
        sha1_hash: &str,
        sha512_hash: &str,
        version_metadata_json: &str,
        upstream_url: &str,
    ) -> error::Result<ArtifactRecord> {
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let blob_id = uuid::Uuid::new_v4().to_string();
        let now = depot_core::repo::now_utc();

        self.blobs.put(&blob_id, data).await?;

        let path = tarball_path(name, version, filename);
        let store = self.store.to_string();

        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.clone(),
            hash: blake3_hash.clone(),
            size: data.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let blob_id = service::claim_or_reuse_blob(self.kv, self.blobs, &store, &blob_rec).await?;

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: data.len() as u64,
            content_type: "application/octet-stream".to_string(),
            kind: ArtifactKind::NpmPackage {
                npm: NpmPackageMeta {
                    name: name.to_string(),
                    version: version.to_string(),
                    filename: filename.to_string(),
                    sha1: sha1_hash.to_string(),
                    sha512: sha512_hash.to_string(),
                    upstream_url: Some(upstream_url.to_string()),
                },
            },
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
            blob_id: Some(blob_id.clone()),
            content_hash: Some(blake3_hash.clone()),
            etag: Some(blake3_hash.clone()),
        };

        // Store version metadata as a blob-backed artifact
        let ver_blob_id = uuid::Uuid::new_v4().to_string();
        let ver_bytes = version_metadata_json.as_bytes();
        let ver_hash = blake3::hash(ver_bytes).to_hex().to_string();
        self.blobs.put(&ver_blob_id, ver_bytes).await?;
        let ver_blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: ver_blob_id.clone(),
            hash: ver_hash.clone(),
            size: ver_bytes.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let ver_blob_id =
            service::claim_or_reuse_blob(self.kv, self.blobs, &store, &ver_blob_rec).await?;
        let version_record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: ver_bytes.len() as u64,
            content_type: "application/json".to_string(),
            kind: ArtifactKind::NpmMetadata,
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: true,
            blob_id: Some(ver_blob_id.clone()),
            content_hash: Some(ver_hash.clone()),
            etag: Some(ver_hash.clone()),
        };

        let repo = self.repo.to_string();
        let record_clone = record.clone();
        let ver_path = version_path(name, version);

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
        let old_ver = service::put_artifact(self.kv, &repo, &ver_path, &version_record).await?;
        let (vc, vb) = match &old_ver {
            Some(old) => (0i64, version_record.size as i64 - old.size as i64),
            None => (1i64, version_record.size as i64),
        };
        self.updater.dir_changed(self.repo, &ver_path, vc, vb).await;
        self.updater.store_changed(self.store, vc, vb).await;

        Ok(record)
    }

    /// Look up a tarball and return a blob reader.
    pub async fn get_tarball(
        &self,
        name: &str,
        version: &str,
        filename: &str,
    ) -> error::Result<Option<(depot_core::store::blob::BlobReader, u64, ArtifactRecord)>> {
        let path = tarball_path(name, version, filename);
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
            DepotError::DataIntegrity(format!("blob missing for blob_id {}", blob_id))
        })?;
        Ok(Some((reader, size, record)))
    }

    /// Get dist-tags for a package.
    pub async fn get_dist_tags(&self, name: &str) -> error::Result<HashMap<String, String>> {
        let path = dist_tags_path(name);
        let repo = self.repo.to_string();
        let record = service::get_artifact(self.kv, &repo, &path).await?;
        match record {
            Some(r) => {
                if let Some(blob_id) = r.blob_id.as_deref() {
                    if let Some(data) = self.blobs.get(blob_id).await? {
                        Ok(serde_json::from_slice(&data).unwrap_or_default())
                    } else {
                        Ok(HashMap::new())
                    }
                } else {
                    Ok(HashMap::new())
                }
            }
            None => Ok(HashMap::new()),
        }
    }

    /// Put dist-tags for a package (overwrites existing).
    pub async fn put_dist_tags(
        &self,
        name: &str,
        tags: &HashMap<String, String>,
    ) -> error::Result<()> {
        let path = dist_tags_path(name);
        let now = depot_core::repo::now_utc();
        let tags_json = serde_json::to_string(tags).unwrap_or_default();
        let tags_bytes = tags_json.as_bytes();
        let tags_blob_id = uuid::Uuid::new_v4().to_string();
        let tags_hash = blake3::hash(tags_bytes).to_hex().to_string();
        self.blobs.put(&tags_blob_id, tags_bytes).await?;
        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: tags_blob_id.clone(),
            hash: tags_hash.clone(),
            size: tags_bytes.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let store = self.store.to_string();
        let tags_blob_id =
            service::claim_or_reuse_blob(self.kv, self.blobs, &store, &blob_rec).await?;
        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: tags_bytes.len() as u64,
            content_type: "application/json".to_string(),
            kind: ArtifactKind::NpmMetadata,
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: true,
            blob_id: Some(tags_blob_id.clone()),
            content_hash: Some(tags_hash.clone()),
            etag: Some(tags_hash.clone()),
        };
        let repo = self.repo.to_string();
        let old_record = service::put_artifact(self.kv, &repo, &path, &record).await?;
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
        Ok(())
    }

    /// Get version metadata JSON for a specific version.
    pub async fn get_version_metadata(
        &self,
        name: &str,
        version: &str,
    ) -> error::Result<Option<String>> {
        let path = version_path(name, version);
        let repo = self.repo.to_string();
        let record = service::get_artifact(self.kv, &repo, &path).await?;
        match record {
            Some(r) => {
                if let Some(blob_id) = r.blob_id.as_deref() {
                    if let Some(data) = self.blobs.get(blob_id).await? {
                        Ok(Some(String::from_utf8_lossy(&data).into_owned()))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// List all unique package names in this repo.
    pub async fn list_packages(&self) -> error::Result<Vec<String>> {
        let repo = self.repo.to_string();
        let mut seen = service::fold_all_artifacts(
            self.kv,
            &repo,
            "_tarballs/",
            std::collections::HashSet::new,
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
        let mut packages: Vec<String> = seen.drain().collect();
        packages.sort();
        Ok(packages)
    }

    /// List all tarball artifacts for a given package.
    pub async fn list_package_versions(
        &self,
        name: &str,
    ) -> error::Result<Vec<(String, ArtifactRecord)>> {
        let norm = normalize_name(name);
        let prefix = format!("_tarballs/{norm}/");
        let repo = self.repo.to_string();
        service::fold_all_artifacts(
            self.kv,
            &repo,
            &prefix,
            Vec::new,
            |acc, sk, record| {
                acc.push((sk.to_owned(), record.clone()));
                Ok(())
            },
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .await
    }

    /// Dynamically assemble a packument (package document) by scanning version metadata
    /// and dist-tags, rewriting tarball URLs to point to our server.
    pub async fn get_packument(
        &self,
        name: &str,
        base_url: &str,
    ) -> error::Result<Option<serde_json::Value>> {
        let norm = normalize_name(name);

        // Collect version metadata
        let ver_prefix = format!("_versions/{norm}/");
        let repo = self.repo.to_string();
        let ver_artifacts: Vec<(String, ArtifactRecord)> = service::fold_all_artifacts(
            self.kv,
            &repo,
            &ver_prefix,
            Vec::new,
            |acc, sk, record| {
                acc.push((sk.to_owned(), record.clone()));
                Ok(())
            },
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .await?;

        if ver_artifacts.is_empty() {
            return Ok(None);
        }

        // Collect tarball artifacts to get sha1/sha512 hashes
        let tarball_prefix = format!("_tarballs/{norm}/");
        let repo2 = self.repo.to_string();
        let tarball_meta: HashMap<String, NpmPackageMeta> = service::fold_all_artifacts(
            self.kv,
            &repo2,
            &tarball_prefix,
            HashMap::new,
            |map, _sk, record| {
                if let ArtifactKind::NpmPackage { ref npm } = record.kind {
                    map.insert(npm.version.clone(), npm.clone());
                }
                Ok(())
            },
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .await?;

        let dist_tags = self.get_dist_tags(name).await?;

        let mut versions = serde_json::Map::new();
        let mut time_map = serde_json::Map::new();

        for (_path, record) in &ver_artifacts {
            let content = if let Some(blob_id) = record.blob_id.as_deref() {
                match self.blobs.get(blob_id).await {
                    Ok(Some(data)) => Some(String::from_utf8_lossy(&data).into_owned()),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(ref content) = content {
                if let Ok(mut ver_obj) = serde_json::from_str::<serde_json::Value>(content) {
                    if let Some(ver_str) = ver_obj.get("version").and_then(|v| v.as_str()) {
                        let version = ver_str.to_string();
                        // Rewrite tarball URL
                        let pkg_name = ver_obj.get("name").and_then(|v| v.as_str()).unwrap_or(name);
                        // For scoped packages (@scope/pkg), filename uses just the pkg part
                        let bare_name = pkg_name.rsplit('/').next().unwrap_or(pkg_name);
                        let filename = format!("{}-{}.tgz", normalize_name(bare_name), version);
                        let tarball_url = format!(
                            "{base_url}/{pkg_name}/-/{filename}",
                            base_url = base_url.trim_end_matches('/'),
                        );

                        // Update dist.tarball in the version object and inject shasum/integrity
                        let dist_obj = if let Some(dist) = ver_obj.get_mut("dist") {
                            dist.as_object_mut()
                        } else {
                            ver_obj.as_object_mut().and_then(|obj| {
                                obj.insert("dist".to_string(), serde_json::json!({}));
                                obj.get_mut("dist").and_then(|d| d.as_object_mut())
                            })
                        };
                        if let Some(obj) = dist_obj {
                            obj.insert(
                                "tarball".to_string(),
                                serde_json::Value::String(tarball_url),
                            );
                            // Inject shasum and integrity from tarball metadata
                            if let Some(meta) = tarball_meta.get(&version) {
                                if !meta.sha1.is_empty() {
                                    obj.insert(
                                        "shasum".to_string(),
                                        serde_json::Value::String(meta.sha1.clone()),
                                    );
                                }
                                if !meta.sha512.is_empty() {
                                    obj.insert(
                                        "integrity".to_string(),
                                        serde_json::Value::String(format!(
                                            "sha512-{}",
                                            meta.sha512
                                        )),
                                    );
                                }
                            }
                        }

                        time_map.insert(
                            version.clone(),
                            serde_json::Value::String(record.created_at.to_rfc3339()),
                        );
                        versions.insert(version, ver_obj);
                    }
                }
            }
        }

        if versions.is_empty() {
            return Ok(None);
        }

        let packument = serde_json::json!({
            "name": name,
            "dist-tags": dist_tags,
            "versions": versions,
            "time": time_map,
        });

        Ok(Some(packument))
    }

    /// Delete a package tarball.
    pub async fn delete_package(
        &self,
        name: &str,
        version: &str,
        filename: &str,
    ) -> error::Result<bool> {
        let path = tarball_path(name, version, filename);
        let _store = self.store.to_string();
        let repo = self.repo.to_string();
        Ok(service::delete_artifact(self.kv, &repo, &path)
            .await?
            .is_some())
    }

    // ------------------------------------------------------------------
    // Stale flag + cached proxy packuments
    // ------------------------------------------------------------------

    /// Set the metadata stale flag (unconditional overwrite).
    /// The `updated_at` timestamp serves as the invalidation watermark.
    pub async fn set_metadata_stale(&self) -> error::Result<()> {
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
            kind: ArtifactKind::NpmMetadata,
            internal: true,
        };
        let repo = self.repo.to_string();
        service::put_artifact(self.kv, &repo, "_npm/metadata_stale", &flag).await?;
        Ok(())
    }

    /// Set the stale flag only if not already present.
    pub async fn set_stale_flag(&self) -> error::Result<()> {
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
            kind: ArtifactKind::NpmMetadata,
            internal: true,
        };
        let repo = self.repo.to_string();
        service::put_artifact_if_absent(self.kv, &repo, "_npm/metadata_stale", &flag).await?;
        Ok(())
    }

    /// Get the stale flag record (used to check its updated_at timestamp).
    pub async fn get_stale_flag(&self) -> error::Result<Option<ArtifactRecord>> {
        let repo = self.repo.to_string();
        service::get_artifact(self.kv, &repo, "_npm/metadata_stale").await
    }

    /// Store a cached merged packument on a proxy repo.
    pub async fn store_cached_packument(&self, package: &str, json: &str) -> error::Result<()> {
        let data = json.as_bytes();
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&blob_id, data).await?;

        let now = depot_core::repo::now_utc();
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

        let norm = normalize_name(package);
        let path = format!("_npm/packument/{norm}");
        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: data.len() as u64,
            content_type: "application/json".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(blob_id),
            etag: Some(blake3_hash.clone()),
            content_hash: Some(blake3_hash),
            kind: ArtifactKind::NpmMetadata,
            internal: true,
        };
        let repo = self.repo.to_string();
        service::put_artifact(self.kv, &repo, &path, &record).await?;
        Ok(())
    }

    /// Get a cached merged packument from a proxy repo.
    /// Returns (json_string, record) so the caller can check updated_at.
    pub async fn get_cached_packument(
        &self,
        package: &str,
    ) -> error::Result<Option<(String, ArtifactRecord)>> {
        let norm = normalize_name(package);
        let path = format!("_npm/packument/{norm}");
        let repo = self.repo.to_string();
        let record = match service::get_artifact(self.kv, &repo, &path).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        let blob_id = record.blob_id.as_deref().unwrap_or("");
        if blob_id.is_empty() {
            return Ok(None);
        }
        if let Some(data) = self.blobs.get(blob_id).await? {
            let json = String::from_utf8_lossy(&data).to_string();
            return Ok(Some((json, record)));
        }
        Ok(None)
    }
}

/// Compute SHA-512 and return as base64 (npm "integrity" format without the `sha512-` prefix).
fn base64_encode_sha512(data: &[u8]) -> String {
    use base64::Engine;
    let hash = Sha512::digest(data);
    base64::engine::general_purpose::STANDARD.encode(hash)
}

/// Hex-encode helper.
mod hex {
    pub fn encode(data: impl AsRef<[u8]>) -> String {
        data.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Fetch an upstream npm packument.
pub async fn fetch_upstream_packument(
    http: &reqwest::Client,
    upstream_url: &str,
    package: &str,
    upstream_auth: Option<&UpstreamAuth>,
) -> error::Result<Option<serde_json::Value>> {
    let url = format!("{}/{}", upstream_url.trim_end_matches('/'), package);
    tracing::info!(%url, "fetching upstream npm packument");

    let upstream_start = std::time::Instant::now();
    let resp = apply_upstream_auth(
        http.get(&url).header("Accept", "application/json"),
        upstream_auth,
    )
    .send()
    .await?;

    depot_core::repo::emit_upstream_event(
        "GET",
        &url,
        resp.status().as_u16(),
        upstream_start.elapsed(),
        0,
        resp.content_length().unwrap_or(0),
        "upstream.npm.packument",
    );

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(DepotError::Upstream(format!(
            "upstream returned {}: {}",
            resp.status(),
            url
        )));
    }

    let body: serde_json::Value = resp.json().await?;
    Ok(Some(body))
}

/// Fetch a tarball from an upstream URL.
pub async fn fetch_upstream_tarball(
    http: &reqwest::Client,
    url: &str,
    upstream_auth: Option<&UpstreamAuth>,
) -> error::Result<reqwest::Response> {
    tracing::info!(%url, "fetching upstream npm tarball");
    let upstream_start = std::time::Instant::now();
    let resp = apply_upstream_auth(http.get(url), upstream_auth)
        .send()
        .await?;
    depot_core::repo::emit_upstream_event(
        "GET",
        url,
        resp.status().as_u16(),
        upstream_start.elapsed(),
        0,
        resp.content_length().unwrap_or(0),
        "upstream.npm.tarball",
    );
    if !resp.status().is_success() {
        return Err(DepotError::Upstream(format!(
            "upstream returned {}: {}",
            resp.status(),
            url
        )));
    }
    Ok(resp)
}
