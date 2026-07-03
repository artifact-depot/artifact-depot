// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Go module proxy storage logic.
//!
//! Maps Go module proxy concepts onto our generic KV + blob store:
//!
//! - Module files stored as artifacts at path `_modules/{encoded_module}/@v/{version}.{ext}`
//! - BLAKE3 is the blob store key, SHA-256 stored in metadata for go.sum compatibility.
//! - Module path encoding: uppercase -> `!` + lowercase (per GOPROXY spec).

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

use depot_core::error::{self, DepotError};
use depot_core::repo::apply_upstream_auth;
use depot_core::service;
use depot_core::store::blob::BlobStore;
use depot_core::store::kv::{
    ArtifactKind, ArtifactRecord, BlobRecord, GoModuleMeta, KvStore, UpstreamAuth,
    CURRENT_RECORD_VERSION,
};
use depot_core::update::UpdateSender;

/// Encode a module path per GOPROXY spec: uppercase letters become `!` + lowercase.
pub fn encode_module_path(module: &str) -> String {
    let mut encoded = String::with_capacity(module.len());
    for c in module.chars() {
        if c.is_ascii_uppercase() {
            encoded.push('!');
            encoded.push(c.to_ascii_lowercase());
        } else {
            encoded.push(c);
        }
    }
    encoded
}

/// Decode a module path: `!x` becomes `X`.
pub fn decode_module_path(encoded: &str) -> String {
    let mut decoded = String::with_capacity(encoded.len());
    let mut chars = encoded.chars();
    while let Some(c) = chars.next() {
        if c == '!' {
            if let Some(next) = chars.next() {
                decoded.push(next.to_ascii_uppercase());
            }
        } else {
            decoded.push(c);
        }
    }
    decoded
}

/// Build the KV path for a module file.
pub fn module_file_path(module: &str, version: &str, ext: &str) -> String {
    let encoded = encode_module_path(module);
    format!("_modules/{encoded}/@v/{version}.{ext}")
}

/// Go module storage operations for a repository.
pub struct GolangStore<'a> {
    pub repo: &'a str,
    pub kv: &'a dyn KvStore,
    pub blobs: &'a dyn BlobStore,
    pub store: &'a str,
    pub updater: &'a UpdateSender,
}

impl<'a> GolangStore<'a> {
    /// Store a module file (info, mod, or zip), computing BLAKE3 + SHA-256.
    pub async fn put_module_file(
        &self,
        module: &str,
        version: &str,
        file_type: &str,
        data: &[u8],
    ) -> error::Result<ArtifactRecord> {
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let sha256_hash = hex_encode(Sha256::digest(data));

        let blob_id = uuid::Uuid::new_v4().to_string();
        let now = depot_core::repo::now_utc();

        self.blobs.put(&blob_id, data).await?;

        let path = module_file_path(module, version, file_type);

        let content_type = match file_type {
            "info" => "application/json",
            "mod" => "text/plain",
            "zip" => "application/zip",
            _ => "application/octet-stream",
        };

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
            size: data.len() as u64,
            content_type: content_type.to_string(),
            kind: ArtifactKind::GoModule {
                golang: GoModuleMeta {
                    module: module.to_string(),
                    version: version.to_string(),
                    file_type: file_type.to_string(),
                    sha256: sha256_hash,
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

        Ok(record)
    }

    /// Commit a module file whose blob has already been written via `BlobWriter`.
    pub async fn commit_module_file(
        &self,
        module: &str,
        version: &str,
        file_type: &str,
        blob: &depot_core::repo::BlobInfo,
    ) -> error::Result<ArtifactRecord> {
        let now = depot_core::repo::now_utc();
        let path = module_file_path(module, version, file_type);

        let content_type = match file_type {
            "info" => "application/json",
            "mod" => "text/plain",
            "zip" => "application/zip",
            _ => "application/octet-stream",
        };

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
            size: blob.size,
            content_type: content_type.to_string(),
            kind: ArtifactKind::GoModule {
                golang: GoModuleMeta {
                    module: module.to_string(),
                    version: version.to_string(),
                    file_type: file_type.to_string(),
                    sha256: blob.sha256_hash.clone(),
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

        Ok(record)
    }

    /// Look up a module file and return a blob reader.
    pub async fn get_module_file(
        &self,
        module: &str,
        version: &str,
        file_type: &str,
    ) -> error::Result<Option<(depot_core::store::blob::BlobReader, u64, ArtifactRecord)>> {
        let path = module_file_path(module, version, file_type);
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

    /// List all versions of a module by scanning for `.info` keys.
    pub async fn list_versions(&self, module: &str) -> error::Result<Vec<String>> {
        let encoded = encode_module_path(module);
        let prefix = format!("_modules/{encoded}/@v/");
        let repo = self.repo.to_string();
        let versions = service::fold_all_artifacts(
            self.kv,
            &repo,
            &prefix,
            BTreeSet::new,
            |acc, sk, _record| {
                // sk = _modules/{encoded}/@v/{version}.{ext}
                if let Some(file) = sk.rsplit('/').next() {
                    if let Some(ver) = file.strip_suffix(".info") {
                        acc.insert(ver.to_string());
                    }
                }
                Ok(())
            },
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .await?;
        Ok(versions.into_iter().collect())
    }

    /// Get the latest version (highest semver).
    pub async fn get_latest(&self, module: &str) -> error::Result<Option<String>> {
        let versions = self.list_versions(module).await?;
        // Simple semver: sort and take last. Versions are like v1.0.0.
        let mut sorted = versions;
        sorted.sort_by(|a, b| compare_semver(a, b));
        Ok(sorted.last().cloned())
    }

    /// Delete all files for a module version (.info, .mod, .zip).
    pub async fn delete_module_version(&self, module: &str, version: &str) -> error::Result<bool> {
        let mut deleted = false;
        for ext in &["info", "mod", "zip"] {
            let path = module_file_path(module, version, ext);
            let _store = self.store.to_string();
            let repo = self.repo.to_string();
            let did_delete = service::delete_artifact(self.kv, &repo, &path)
                .await?
                .is_some();
            if did_delete {
                deleted = true;
            }
        }
        Ok(deleted)
    }
}

/// Simple semver comparison for Go versions (v1.2.3).
fn compare_semver(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> (u64, u64, u64) {
        let s = s.strip_prefix('v').unwrap_or(s);
        let parts: Vec<&str> = s.splitn(3, '.').collect();
        let major = parts.first().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    };
    parse(a).cmp(&parse(b))
}

/// Hex-encode helper.
fn hex_encode(data: impl AsRef<[u8]>) -> String {
    data.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Emit a structured upstream request event for the dashboard/logging pipeline.
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

/// Fetch an upstream module file (for cache repos).
pub async fn fetch_upstream_file(
    http: &reqwest::Client,
    upstream_url: &str,
    module: &str,
    path_suffix: &str,
    upstream_auth: Option<&UpstreamAuth>,
) -> error::Result<Option<Vec<u8>>> {
    let encoded = encode_module_path(module);
    let url = format!(
        "{}/{}/{}",
        upstream_url.trim_end_matches('/'),
        encoded,
        path_suffix
    );
    tracing::info!(%url, "fetching upstream Go module file");

    let upstream_start = std::time::Instant::now();
    let resp = apply_upstream_auth(http.get(&url), upstream_auth)
        .send()
        .await?;

    emit_upstream_event(
        "GET",
        &url,
        resp.status().as_u16(),
        upstream_start.elapsed(),
        0,
        resp.content_length().unwrap_or(0),
        "upstream.golang.file",
    );

    if resp.status() == reqwest::StatusCode::NOT_FOUND || resp.status() == reqwest::StatusCode::GONE
    {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(DepotError::Upstream(format!(
            "upstream returned {}: {}",
            resp.status(),
            url
        )));
    }

    Ok(Some(resp.bytes().await?.to_vec()))
}
