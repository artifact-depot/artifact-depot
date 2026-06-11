// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! PyPI package storage logic.
//!
//! Maps PyPI concepts onto our generic KV + blob store:
//!
//! - Packages stored as artifacts at path `_packages/{normalized_name}/{version}/{filename}`
//! - Cached upstream index pages stored at `_index/{normalized_name}`
//! - BLAKE3 is the blob store key, SHA-256 stored in metadata for PEP 503 `#sha256=` fragments.

use sha2::{Digest, Sha256};
use std::collections::HashSet;

use depot_core::error::{self, DepotError};
use depot_core::repo::apply_upstream_auth;
use depot_core::service;
use depot_core::store::blob::BlobStore;
use depot_core::store::kv::{
    ArtifactKind, ArtifactRecord, PypiPackageMeta, PypiProjectIndex, UpstreamAuth,
    CURRENT_RECORD_VERSION,
};
use depot_core::store::kv::{BlobRecord, KvStore};
use depot_core::update::UpdateSender;

/// Emit a structured upstream request event for the dashboard/logging pipeline.
/// Inlined here to avoid a cross-crate dependency on depot's log_export module.
fn emit_upstream_event(
    method: &str,
    url: &str,
    status: u16,
    elapsed: std::time::Duration,
    bytes_sent: u64,
    bytes_recv: u64,
    action: &str,
) {
    let peer = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_string(),
            })
        })
        .unwrap_or_default();
    let path = reqwest::Url::parse(url)
        .ok()
        .map(|u| u.path().to_string())
        .unwrap_or_else(|| url.to_string());
    let elapsed_ns = elapsed.as_nanos() as u64;
    tracing::info!(
        target: "depot.request",
        request_id = "",
        username = "",
        ip = peer.as_str(),
        method,
        path = path.as_str(),
        status,
        action,
        elapsed_ns,
        bytes_recv,
        bytes_sent,
        direction = "upstream",
        "request"
    );
}

/// Metadata for a PyPI package upload (fields from multipart form).
pub struct PypiUploadMeta<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub filename: &'a str,
    pub filetype: &'a str,
    pub expected_sha256: Option<&'a str>,
    pub requires_python: Option<&'a str>,
    pub summary: Option<&'a str>,
}

/// PEP 503 name normalization: lowercase, collapse runs of `[-_.]+` to single `-`.
pub fn normalize_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut result = String::with_capacity(lower.len());
    let mut prev_sep = false;
    for c in lower.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !prev_sep {
                result.push('-');
                prev_sep = true;
            }
        } else {
            result.push(c);
            prev_sep = false;
        }
    }
    result
}

/// Build the KV path for a package file.
pub fn package_path(name: &str, version: &str, filename: &str) -> String {
    let norm = normalize_name(name);
    format!("_packages/{norm}/{version}/{filename}")
}

/// PyPI storage operations for a repository.
pub struct PypiStore<'a> {
    pub repo: &'a str,
    pub kv: &'a dyn KvStore,
    pub blobs: &'a dyn BlobStore,
    pub store: &'a str,
    pub updater: &'a UpdateSender,
}

impl<'a> PypiStore<'a> {
    /// Store a package, computing BLAKE3 + SHA-256, writing blob and artifact records.
    /// Returns the ArtifactRecord.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_package(
        &self,
        name: &str,
        version: &str,
        filename: &str,
        filetype: &str,
        data: &[u8],
        expected_sha256: Option<&str>,
        requires_python: Option<&str>,
        summary: Option<&str>,
    ) -> error::Result<ArtifactRecord> {
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let sha256_hash = hex::encode(Sha256::digest(data));

        // Validate SHA-256 if provided by twine.
        if let Some(expected) = expected_sha256 {
            if expected != sha256_hash {
                return Err(DepotError::BadRequest(format!(
                    "SHA-256 mismatch: expected {expected}, computed {sha256_hash}"
                )));
            }
        }

        let blob_id = uuid::Uuid::new_v4().to_string();
        let now = depot_core::repo::now_utc();

        // Store blob under UUID key.
        self.blobs.put(&blob_id, data).await?;

        let path = package_path(name, version, filename);

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
            content_type: "application/octet-stream".to_string(),
            kind: ArtifactKind::PypiPackage {
                pypi: PypiPackageMeta {
                    name: name.to_string(),
                    version: version.to_string(),
                    filename: filename.to_string(),
                    filetype: filetype.to_string(),
                    sha256: sha256_hash,
                    requires_python: requires_python.map(|s| s.to_string()),
                    summary: summary.map(|s| s.to_string()),
                    upstream_url: None,
                },
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
        self.updater
            .store_changed(self.store, count_delta, bytes_delta)
            .await;
        Ok(record)
    }

    /// Commit a package whose blob has already been written via `BlobWriter`.
    /// Takes pre-computed blob info and writes only KV records.
    pub async fn commit_package(
        &self,
        meta: &PypiUploadMeta<'_>,
        blob: &depot_core::repo::BlobInfo,
    ) -> error::Result<ArtifactRecord> {
        // Validate SHA-256 if provided by twine.
        if let Some(expected) = meta.expected_sha256 {
            if expected != blob.sha256_hash {
                return Err(DepotError::BadRequest(format!(
                    "SHA-256 mismatch: expected {expected}, computed {}",
                    blob.sha256_hash
                )));
            }
        }

        let now = depot_core::repo::now_utc();
        let path = package_path(meta.name, meta.version, meta.filename);

        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob.blob_id.clone(),
            hash: blob.blake3_hash.clone(),
            size: blob.size,
            created_at: now,
            store: self.store.to_string(),
        };
        let store = self.store.to_string();
        let repo = self.repo.to_string();
        let blob_id = service::claim_or_reuse_blob(self.kv, self.blobs, &store, &blob_rec).await?;

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: blob.size,
            content_type: "application/octet-stream".to_string(),
            kind: ArtifactKind::PypiPackage {
                pypi: PypiPackageMeta {
                    name: meta.name.to_string(),
                    version: meta.version.to_string(),
                    filename: meta.filename.to_string(),
                    filetype: meta.filetype.to_string(),
                    sha256: blob.sha256_hash.clone(),
                    requires_python: meta.requires_python.map(|s| s.to_string()),
                    summary: meta.summary.map(|s| s.to_string()),
                    upstream_url: None,
                },
            },
            blob_id: Some(blob_id),
            content_hash: Some(blob.blake3_hash.clone()),
            etag: Some(blob.blake3_hash.clone()),
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
        self.updater
            .store_changed(self.store, count_delta, bytes_delta)
            .await;
        Ok(record)
    }

    /// Store a package from already-computed hashes (used by cache repos).
    pub async fn put_package_from_upstream(
        &self,
        name: &str,
        version: &str,
        filename: &str,
        data: &[u8],
        sha256_hash: &str,
        upstream_url: &str,
    ) -> error::Result<ArtifactRecord> {
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let blob_id = uuid::Uuid::new_v4().to_string();

        let now = depot_core::repo::now_utc();

        // Store blob under UUID key.
        self.blobs.put(&blob_id, data).await?;

        let path = package_path(name, version, filename);

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
            content_type: "application/octet-stream".to_string(),
            kind: ArtifactKind::PypiPackage {
                pypi: PypiPackageMeta {
                    name: name.to_string(),
                    version: version.to_string(),
                    filename: filename.to_string(),
                    filetype: guess_filetype(filename).to_string(),
                    sha256: sha256_hash.to_string(),
                    requires_python: None,
                    summary: None,
                    upstream_url: Some(upstream_url.to_string()),
                },
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
        self.updater
            .store_changed(self.store, count_delta, bytes_delta)
            .await;
        Ok(record)
    }

    /// Store a package whose blob has already been written to the blob store.
    /// Used by cache repos after streaming the upstream response to a blob writer.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_cached_package(
        &self,
        blob_id: &str,
        blake3_hash: &str,
        sha256_hash: &str,
        size: u64,
        name: &str,
        version: &str,
        filename: &str,
        upstream_url: &str,
        requires_python: Option<&str>,
    ) -> error::Result<ArtifactRecord> {
        let now = depot_core::repo::now_utc();

        let path = package_path(name, version, filename);

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
            content_type: "application/octet-stream".to_string(),
            kind: ArtifactKind::PypiPackage {
                pypi: PypiPackageMeta {
                    name: name.to_string(),
                    version: version.to_string(),
                    filename: filename.to_string(),
                    filetype: guess_filetype(filename).to_string(),
                    sha256: sha256_hash.to_string(),
                    requires_python: requires_python.map(|s| s.to_string()),
                    summary: None,
                    upstream_url: Some(upstream_url.to_string()),
                },
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
        self.updater
            .store_changed(self.store, count_delta, bytes_delta)
            .await;

        Ok(record)
    }

    /// Look up a package artifact record.
    pub async fn get_package(
        &self,
        name: &str,
        version: &str,
        filename: &str,
    ) -> error::Result<Option<(depot_core::store::blob::BlobReader, u64, ArtifactRecord)>> {
        let path = package_path(name, version, filename);
        let repo = self.repo.to_string();
        let record = service::get_artifact(self.kv, &repo, &path).await?;
        let record = match record {
            Some(r) => r,
            None => return Ok(None),
        };
        let blob_id = record.blob_id.as_deref().unwrap_or("");
        let (reader, size) = self.blobs.open_read(blob_id).await?.ok_or_else(|| {
            DepotError::DataIntegrity(format!("blob missing for blob_id {}", blob_id))
        })?;
        Ok(Some((reader, size, record)))
    }

    /// List all unique project names in this repo.
    pub async fn list_projects(&self) -> error::Result<Vec<String>> {
        let repo = self.repo.to_string();
        let seen = service::fold_all_artifacts(
            self.kv,
            &repo,
            "_packages/",
            HashSet::new,
            |acc, sk, _record| {
                // sk = _packages/{name}/{version}/{filename}
                let parts: Vec<&str> = sk.splitn(4, '/').collect();
                if let Some(&name) = parts.get(1) {
                    acc.insert(name.to_string());
                }
                Ok(())
            },
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .await?;
        let mut projects: Vec<String> = seen.into_iter().collect();
        projects.sort();
        Ok(projects)
    }

    /// List all files for a given project.
    pub async fn list_project_files(
        &self,
        name: &str,
    ) -> error::Result<Vec<(String, ArtifactRecord)>> {
        let norm = normalize_name(name);
        let prefix = format!("_packages/{norm}/");
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

    /// Store a cached project index as a JSON blob at `_index/{project}`.
    ///
    /// This replaces the old approach of writing hundreds of placeholder
    /// artifact records — one per package file — with a single record whose
    /// blob contains the full set of parsed upstream links.
    pub async fn store_project_index(
        &self,
        project: &str,
        index: &PypiProjectIndex,
    ) -> error::Result<()> {
        let json = serde_json::to_vec(index)
            .map_err(|e| DepotError::Internal(format!("failed to serialize project index: {e}")))?;
        let blob_id = uuid::Uuid::new_v4().to_string();
        let blake3_hash = blake3::hash(&json).to_hex().to_string();
        let now = depot_core::repo::now_utc();

        self.blobs.put(&blob_id, &json).await?;

        let index_path = format!("_index/{}", normalize_name(project));

        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.clone(),
            hash: blake3_hash.clone(),
            size: json.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let blob_id =
            service::claim_or_reuse_blob(self.kv, self.blobs, self.store, &blob_rec).await?;

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: json.len() as u64,
            content_type: "application/json".to_string(),
            kind: ArtifactKind::Raw,
            blob_id: Some(blob_id),
            content_hash: Some(blake3_hash.clone()),
            etag: Some(blake3_hash),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
        };
        let _ = service::put_artifact(self.kv, self.repo, &index_path, &record).await?;
        Ok(())
    }

    /// Read the cached project index from the `_index/{project}` record.
    pub async fn get_project_index(
        &self,
        project: &str,
    ) -> error::Result<Option<PypiProjectIndex>> {
        let index_path = format!("_index/{}", normalize_name(project));
        let record = service::get_artifact(self.kv, self.repo, &index_path).await?;
        let record = match record {
            Some(r) => r,
            None => return Ok(None),
        };
        let blob_id = match record.blob_id.as_deref() {
            Some(id) if !id.is_empty() => id,
            _ => return Ok(None),
        };
        let data = self.blobs.get(blob_id).await?;
        let data = match data {
            Some(d) => d,
            None => return Ok(None),
        };
        let index: PypiProjectIndex = serde_json::from_slice(&data).map_err(|e| {
            DepotError::DataIntegrity(format!("failed to deserialize project index: {e}"))
        })?;
        Ok(Some(index))
    }

    /// Delete a package.
    pub async fn delete_package(
        &self,
        name: &str,
        version: &str,
        filename: &str,
    ) -> error::Result<bool> {
        let path = package_path(name, version, filename);
        let _store = self.store.to_string();
        let repo = self.repo.to_string();
        Ok(service::delete_artifact(self.kv, &repo, &path)
            .await?
            .is_some())
    }

    // ------------------------------------------------------------------
    // Stale flag + cached proxy index
    // ------------------------------------------------------------------

    /// Set the metadata stale flag on this repo.
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
            kind: ArtifactKind::Raw,
            internal: true,
        };
        let repo = self.repo.to_string();
        service::put_artifact(self.kv, &repo, "_pypi/metadata_stale", &flag).await?;
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
            kind: ArtifactKind::Raw,
            internal: true,
        };
        let repo = self.repo.to_string();
        service::put_artifact_if_absent(self.kv, &repo, "_pypi/metadata_stale", &flag).await?;
        Ok(())
    }

    /// Check if metadata is stale.
    pub async fn is_metadata_stale(&self) -> error::Result<bool> {
        let repo = self.repo.to_string();
        Ok(
            service::get_artifact(self.kv, &repo, "_pypi/metadata_stale")
                .await?
                .is_some(),
        )
    }

    /// Clear the stale flag.
    pub async fn clear_stale_flag(&self) -> error::Result<()> {
        let repo = self.repo.to_string();
        service::delete_artifact(self.kv, &repo, "_pypi/metadata_stale").await?;
        Ok(())
    }

    /// Store the cached /simple/ index HTML on a proxy repo.
    pub async fn store_simple_index(&self, html: &str) -> error::Result<()> {
        let data = html.as_bytes();
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&blob_id, data).await?;

        let now = depot_core::repo::now_utc();
        let blob_rec = depot_core::store::kv::BlobRecord {
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
            content_type: "text/html".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(blob_id),
            etag: Some(blake3_hash.clone()),
            content_hash: Some(blake3_hash),
            kind: ArtifactKind::Raw,
            internal: true,
        };
        let repo = self.repo.to_string();
        service::put_artifact(self.kv, &repo, "_pypi/simple_index", &record).await?;
        Ok(())
    }

    /// Get the cached /simple/ index HTML.
    pub async fn get_simple_index(&self) -> error::Result<Option<String>> {
        let repo = self.repo.to_string();
        let record = service::get_artifact(self.kv, &repo, "_pypi/simple_index").await?;
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
}

/// Guess filetype from filename extension.
/// Public wrapper for guess_filetype.
pub fn guess_filetype_pub(filename: &str) -> String {
    guess_filetype(filename).to_string()
}

fn guess_filetype(filename: &str) -> &str {
    if filename.ends_with(".whl") {
        "bdist_wheel"
    } else if filename.ends_with(".tar.gz") || filename.ends_with(".zip") {
        "sdist"
    } else {
        "unknown"
    }
}

/// Extract version from a PyPI filename heuristic.
/// For cache repos where we parse filenames from upstream HTML.
pub fn extract_version_from_filename(project: &str, filename: &str) -> String {
    // Wheel: {name}-{version}(-{build})?-{python}-{abi}-{platform}.whl
    // Sdist: {name}-{version}.tar.gz or {name}-{version}.zip
    let norm = normalize_name(project);
    let norm_fn = normalize_name(
        filename
            .strip_suffix(".whl")
            .or_else(|| filename.strip_suffix(".tar.gz"))
            .or_else(|| filename.strip_suffix(".zip"))
            .unwrap_or(filename),
    );
    // After normalizing, the filename should start with the normalized project name
    // followed by a dash and the version.
    if let Some(rest) = norm_fn.strip_prefix(&norm) {
        if let Some(ver_str) = rest.strip_prefix('-') {
            // For wheels, version is the second segment before the python tag
            // For sdists, the rest is just the version
            if filename.ends_with(".whl") {
                // version is everything up to the next dash after which we see python tag
                // e.g., "1.0.0-py3-none-any" -> "1.0.0"
                return ver_str.split('-').next().unwrap_or(ver_str).to_string();
            } else {
                return ver_str.to_string();
            }
        }
    }
    // Fallback: try to extract from metadata or just use "unknown"
    "unknown".to_string()
}

/// A parsed link from a PEP 503 simple project page.
#[derive(Debug, Clone)]
pub struct PackageLink {
    pub filename: String,
    pub url: String,
    pub sha256: Option<String>,
    pub requires_python: Option<String>,
}

/// Parse a PEP 503 simple HTML page to extract package links.
/// These pages have a rigid structure: `<a href="...#sha256=..." data-requires-python="...">filename</a>`
pub fn parse_simple_html(html: &str) -> Vec<PackageLink> {
    let mut links = Vec::new();
    // Find all <a> tags
    let mut remaining = html;
    while let Some(start) = remaining.find("<a ") {
        #[allow(clippy::string_slice)] // start comes from .find(), points to valid char boundary
        {
            remaining = &remaining[start..];
        }
        let end = match remaining.find("</a>") {
            Some(e) => e,
            None => break,
        };
        #[allow(clippy::string_slice)]
        // end comes from .find(), end+4 points to valid char boundary
        let tag = &remaining[..end + 4];

        // Extract href
        let href = extract_attr(tag, "href");
        // Extract data-requires-python
        let requires_python = extract_attr(tag, "data-requires-python");

        // Extract link text (filename) - between > and </a>
        let filename = if let Some(gt) = tag.find('>') {
            #[allow(clippy::string_slice)] // gt and end come from .find(), valid char boundaries
            let text = &tag[gt + 1..end];
            text.trim().to_string()
        } else {
            String::new()
        };

        if let Some(href) = href {
            // Parse sha256 from fragment
            let (url, sha256) = if let Some(hash_pos) = href.find("#sha256=") {
                #[allow(clippy::string_slice)] // hash_pos from .find(), "#sha256=" is 8 ASCII bytes
                let url = href[..hash_pos].to_string();
                #[allow(clippy::string_slice)] // hash_pos from .find(), "#sha256=" is 8 ASCII bytes
                let sha256 = href[hash_pos + 8..].to_string();
                (url, Some(sha256))
            } else {
                (href, None)
            };

            if !filename.is_empty() {
                links.push(PackageLink {
                    filename,
                    url,
                    sha256,
                    requires_python,
                });
            }
        }

        #[allow(clippy::string_slice)]
        // end comes from .find(), end+4 points to valid char boundary
        {
            remaining = &remaining[end + 4..];
        }
    }
    links
}

/// Extract an attribute value from an HTML tag.
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    // Try attr="value" and attr='value'
    for quote in ['"', '\''] {
        let pattern = format!("{attr}={quote}");
        if let Some(start) = tag.find(&pattern) {
            let val_start = start + pattern.len();
            #[allow(clippy::string_slice)] // val_start comes from .find() + ASCII pattern len
            if let Some(val_end) = tag[val_start..].find(quote) {
                #[allow(clippy::string_slice)] // val_start and val_end come from .find()
                let val = &tag[val_start..val_start + val_end];
                // Decode HTML entities
                let decoded = val
                    .replace("&amp;", "&")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&#39;", "'");
                return Some(decoded);
            }
        }
    }
    None
}

/// Hex-encode helper (avoids pulling in the `hex` crate).
mod hex {
    pub fn encode(data: impl AsRef<[u8]>) -> String {
        data.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Fetch an upstream PEP 503 project page.
pub async fn fetch_upstream_project_page(
    http: &reqwest::Client,
    upstream_url: &str,
    project: &str,
    upstream_auth: Option<&UpstreamAuth>,
) -> error::Result<Option<String>> {
    let norm = normalize_name(project);
    let url = format!("{}/{norm}/", upstream_url.trim_end_matches('/'));
    tracing::info!(%url, "fetching upstream PyPI project page");

    let upstream_start = std::time::Instant::now();
    let resp = apply_upstream_auth(http.get(&url).header("Accept", "text/html"), upstream_auth)
        .send()
        .await?;

    emit_upstream_event(
        "GET",
        &url,
        resp.status().as_u16(),
        upstream_start.elapsed(),
        0,
        resp.content_length().unwrap_or(0),
        "upstream.pypi.simple",
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

    Ok(Some(resp.text().await?))
}

/// Fetch a package file from an upstream URL, returning the response for streaming.
pub async fn fetch_upstream_response(
    http: &reqwest::Client,
    url: &str,
    upstream_auth: Option<&UpstreamAuth>,
) -> error::Result<reqwest::Response> {
    tracing::info!(%url, "fetching upstream PyPI package");
    let upstream_start = std::time::Instant::now();
    let resp = apply_upstream_auth(http.get(url), upstream_auth)
        .send()
        .await?;
    emit_upstream_event(
        "GET",
        url,
        resp.status().as_u16(),
        upstream_start.elapsed(),
        0,
        resp.content_length().unwrap_or(0),
        "upstream.pypi.download",
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
