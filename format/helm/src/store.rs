// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Helm chart repository storage logic.
//!
//! Maps Helm concepts onto our generic KV + blob store:
//!
//! - Chart packages (.tgz) stored at the repo root — `{name}-{version}.tgz` —
//!   matching the canonical helm repository layout: charts and `index.yaml`
//!   live side-by-side at the repo URL root. The same string appears in the
//!   generated `index.yaml` `urls` field, so storage and download URL are
//!   unified.
//! - Metadata artifacts at `_helm/meta/index.yaml` (inline content), served
//!   at the URL `/index.yaml`.
//! - Stale flag at `_helm/metadata_stale` triggers atomic rebuild on next index fetch.
//!
//! Historical note: charts have lived at two earlier paths —
//! `_charts/{name}/{name}-{version}.tgz` and later `charts/{name}-{version}.tgz`.
//! The integrity-check task relocates any leftover records from either prefix
//! to the canonical root path and sets the stale flag so the next index fetch
//! picks them up. The on-fetch path itself is purely "rebuild index when
//! stale" — it does not migrate.

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;

use depot_core::error::{self, DepotError, Retryability};
use depot_core::service;
use depot_core::store::blob::BlobStore;
use depot_core::store::kv::KvStore;
use depot_core::store::kv::{ArtifactKind, ArtifactRecord, HelmChartMeta, CURRENT_RECORD_VERSION};
use depot_core::update::UpdateSender;

/// Hex-encode helper.
pub(crate) mod hex {
    pub fn encode(data: impl AsRef<[u8]>) -> String {
        data.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Helm storage operations for a repository.
pub struct HelmStore<'a> {
    pub repo: &'a str,
    pub kv: &'a dyn KvStore,
    pub blobs: &'a dyn BlobStore,
    pub store: &'a str,
    pub updater: &'a UpdateSender,
}

/// Deserialization struct for Chart.yaml.
#[derive(serde::Deserialize)]
struct ChartYaml {
    name: Option<String>,
    version: Option<String>,
    #[serde(rename = "appVersion")]
    app_version: Option<String>,
    description: Option<String>,
    #[serde(rename = "apiVersion")]
    api_version: Option<String>,
    #[serde(rename = "type")]
    chart_type: Option<String>,
    keywords: Option<Vec<String>>,
    home: Option<String>,
    sources: Option<Vec<String>>,
    icon: Option<String>,
    deprecated: Option<bool>,
}

/// Serialization struct for the Helm index.yaml.
#[derive(serde::Serialize)]
struct HelmIndex {
    #[serde(rename = "apiVersion")]
    api_version: String,
    entries: BTreeMap<String, Vec<HelmIndexEntry>>,
    generated: String,
}

/// A single entry in the Helm index.yaml `entries` map.
#[derive(serde::Serialize)]
struct HelmIndexEntry {
    name: String,
    version: String,
    #[serde(rename = "appVersion", skip_serializing_if = "String::is_empty")]
    app_version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    #[serde(rename = "apiVersion")]
    api_version: String,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    chart_type: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    keywords: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    home: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sources: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    icon: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    deprecated: bool,
    digest: String,
    urls: Vec<String>,
    created: String,
}

/// Parse a `.tgz` chart archive and extract metadata from `Chart.yaml`.
/// The SHA-256 hash must be provided by the caller (computed during streaming).
pub fn parse_chart(reader: impl Read, sha256: &str) -> error::Result<HelmChartMeta> {
    let decoder = GzDecoder::new(reader);
    let mut archive = tar::Archive::new(decoder);

    let mut chart_yaml_content: Option<String> = None;

    let entries = archive
        .entries()
        .map_err(|e| DepotError::BadRequest(format!("invalid chart archive: {e}")))?;
    for entry_result in entries {
        let mut entry = entry_result
            .map_err(|e| DepotError::BadRequest(format!("invalid chart archive entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| DepotError::BadRequest(format!("invalid chart archive path: {e}")))?
            .to_string_lossy()
            .to_string();

        // Chart.yaml is at {chartdir}/Chart.yaml — first path segment is the chart directory
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        if parts.get(1) == Some(&"Chart.yaml") {
            let mut content = String::new();
            entry.read_to_string(&mut content)?;
            chart_yaml_content = Some(content);
            break;
        }
    }

    let yaml_content = chart_yaml_content.ok_or_else(|| {
        DepotError::BadRequest("Chart.yaml not found in chart archive".to_string())
    })?;

    let chart: ChartYaml = serde_yml::from_str(&yaml_content)
        .map_err(|e| DepotError::BadRequest(format!("failed to parse Chart.yaml: {e}")))?;

    let name = chart
        .name
        .ok_or_else(|| DepotError::BadRequest("missing 'name' field in Chart.yaml".to_string()))?;
    let version = chart.version.ok_or_else(|| {
        DepotError::BadRequest("missing 'version' field in Chart.yaml".to_string())
    })?;

    if name.is_empty() {
        return Err(DepotError::BadRequest(
            "empty 'name' field in Chart.yaml".to_string(),
        ));
    }
    if version.is_empty() {
        return Err(DepotError::BadRequest(
            "empty 'version' field in Chart.yaml".to_string(),
        ));
    }

    Ok(HelmChartMeta {
        name,
        version,
        app_version: chart.app_version.unwrap_or_default(),
        description: chart.description.unwrap_or_default(),
        api_version: chart.api_version.unwrap_or_default(),
        chart_type: chart.chart_type.unwrap_or_default(),
        keywords: chart.keywords.unwrap_or_default(),
        home: chart.home.unwrap_or_default(),
        sources: chart.sources.unwrap_or_default(),
        icon: chart.icon.unwrap_or_default(),
        deprecated: chart.deprecated.unwrap_or(false),
        sha256: sha256.to_string(),
    })
}

/// Returns the KV path for a chart package.
///
/// Charts live at the repo root — `{name}-{version}.tgz` — matching the
/// canonical helm repository layout (charts and `index.yaml` side-by-side
/// at the repo URL root). This is also exactly what `index.yaml`'s `urls`
/// field advertises, so storage and download URL are unified.
pub fn chart_path(name: &str, version: &str) -> String {
    format!("{name}-{version}.tgz")
}

/// Prefixes where charts have been stored historically. The integrity-check
/// helm canonicalization phase relocates any leftover records to the root.
/// In order from oldest to newest:
///   1. `_charts/{name}/{name}-{version}.tgz` (per-chart subdirectory)
///   2. `charts/{name}-{version}.tgz` (flat under `charts/`)
pub(crate) const LEGACY_CHART_PREFIXES: &[&str] = &["_charts/", "charts/"];

/// Enumerate chart records still at a legacy storage prefix that should be
/// relocated to the canonical root path `{name}-{version}.tgz`.
///
/// Returns `(old_path, new_path, record)` for each chart that needs to move.
/// Cheap when there's nothing to move: the underlying scans return empty.
pub async fn enumerate_legacy_chart_relocations(
    kv: &dyn KvStore,
    repo: &str,
) -> error::Result<Vec<(String, String, ArtifactRecord)>> {
    let mut all = Vec::new();
    for prefix in LEGACY_CHART_PREFIXES {
        let chunk: Vec<(String, String, ArtifactRecord)> = service::fold_all_artifacts(
            kv,
            repo,
            prefix,
            Vec::<(String, String, ArtifactRecord)>::new,
            |acc, old_path, record| {
                if let ArtifactKind::HelmChart { ref helm } = record.kind {
                    let new_path = chart_path(&helm.name, &helm.version);
                    if new_path != old_path {
                        acc.push((old_path.to_string(), new_path, record.clone()));
                    }
                }
                Ok(())
            },
            |mut a, mut b| {
                a.append(&mut b);
                a
            },
        )
        .await?;
        all.extend(chunk);
    }
    Ok(all)
}

/// Migrate any chart records stored at a legacy prefix (`_charts/...` or
/// `charts/...`) to the canonical root path `{name}-{version}.tgz`.
///
/// Ordering: put at new path first, then delete old. If a crash occurs between
/// the two, the next call re-runs this step idempotently (same content
/// overwrites at new; delete at old is idempotent).
pub async fn migrate_legacy_chart_paths(kv: &dyn KvStore, repo: &str) -> error::Result<()> {
    let legacy = enumerate_legacy_chart_relocations(kv, repo).await?;
    if legacy.is_empty() {
        return Ok(());
    }

    tracing::info!(
        repo,
        count = legacy.len(),
        "helm: relocating legacy chart records to canonical root path"
    );

    for (old_path, new_path, record) in legacy {
        service::put_artifact(kv, repo, &new_path, &record).await?;
        service::delete_artifact(kv, repo, &old_path).await?;
    }
    Ok(())
}

/// Set the metadata stale flag on a single Helm repo (no propagation).
pub async fn set_stale_flag(kv: &dyn KvStore, repo: &str) -> error::Result<()> {
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
        kind: ArtifactKind::HelmMetadata,
        internal: true,
        blob_id: None,
        content_hash: None,
        etag: None,
    };
    service::put_artifact_if_absent(kv, repo, "_helm/metadata_stale", &flag).await?;
    Ok(())
}

// Staleness propagation to parent proxies is now handled generically by
// service::propagate_staleness(), called from the API layer.

impl<'a> HelmStore<'a> {
    /// Store a chart package (.tgz), parse its metadata, and set the stale flag.
    pub async fn put_chart(&self, data: &[u8]) -> error::Result<ArtifactRecord> {
        let sha256_hash = hex::encode(Sha256::digest(data));
        let meta = parse_chart(std::io::Cursor::new(data), &sha256_hash)?;

        let blake3_hash = blake3::hash(data).to_hex().to_string();

        let now = depot_core::repo::now_utc();

        // Store blob under UUID key (async I/O, outside txn)
        let blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&blob_id, data).await?;

        let path = chart_path(&meta.name, &meta.version);

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: data.len() as u64,
            content_type: "application/gzip".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            kind: ArtifactKind::HelmChart { helm: meta },
            internal: false,
            blob_id: Some(blob_id.clone()),
            content_hash: Some(blake3_hash.clone()),
            etag: Some(blake3_hash.clone()),
        };

        // Store blob record and artifact
        service::put_blob(
            self.kv,
            self.store,
            &depot_core::store::kv::BlobRecord {
                schema_version: CURRENT_RECORD_VERSION,
                blob_id,
                hash: blake3_hash,
                size: data.len() as u64,
                created_at: now,
                store: self.store.to_string(),
            },
        )
        .await?;
        let old_record = service::put_artifact(self.kv, self.repo, &path, &record).await?;
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, &path, count_delta, bytes_delta)
            .await;
        self.set_metadata_stale().await?;

        Ok(record)
    }

    /// Store a chart package from in-memory data with pre-computed hashes.
    /// Used by cache repos after fetching the upstream response into memory.
    pub async fn put_cached_chart(
        &self,
        data: &[u8],
        blake3_hash: &str,
        sha256_hash: &str,
    ) -> error::Result<ArtifactRecord> {
        let meta = parse_chart(std::io::Cursor::new(data), sha256_hash)?;
        let size = data.len() as u64;

        let now = depot_core::repo::now_utc();

        let blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&blob_id, data).await?;

        let path = chart_path(&meta.name, &meta.version);

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size,
            content_type: "application/gzip".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            kind: ArtifactKind::HelmChart {
                helm: HelmChartMeta {
                    sha256: sha256_hash.to_string(),
                    ..meta
                },
            },
            internal: false,
            blob_id: Some(blob_id.clone()),
            content_hash: Some(blake3_hash.to_string()),
            etag: Some(blake3_hash.to_string()),
        };

        // Store blob record and artifact
        service::put_blob(
            self.kv,
            self.store,
            &depot_core::store::kv::BlobRecord {
                schema_version: CURRENT_RECORD_VERSION,
                blob_id,
                hash: blake3_hash.to_string(),
                size,
                created_at: now,
                store: self.store.to_string(),
            },
        )
        .await?;
        let old_record = service::put_artifact(self.kv, self.repo, &path, &record).await?;
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, &path, count_delta, bytes_delta)
            .await;
        self.set_metadata_stale().await?;

        Ok(record)
    }

    /// Commit a chart whose blob has already been written via `BlobWriter`.
    /// Reads the blob back to parse Chart.yaml, then writes KV records.
    pub async fn commit_chart(
        &self,
        blob_id: &str,
        blake3_hash: &str,
        sha256_hash: &str,
        size: u64,
    ) -> error::Result<ArtifactRecord> {
        // Read the blob back to parse chart metadata.
        let (reader, _) = self.blobs.open_read(blob_id).await?.ok_or_else(|| {
            DepotError::DataIntegrity(format!("blob {blob_id} missing after write"))
        })?;
        let sha256_owned = sha256_hash.to_string();
        let meta = tokio::task::spawn_blocking(move || {
            let sync_reader = tokio_util::io::SyncIoBridge::new(reader);
            parse_chart(sync_reader, &sha256_owned)
        })
        .await
        .map_err(|e| DepotError::Storage(Box::new(e), Retryability::Permanent))??;

        let now = depot_core::repo::now_utc();
        let path = chart_path(&meta.name, &meta.version);

        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size,
            content_type: "application/gzip".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            kind: ArtifactKind::HelmChart { helm: meta },
            internal: false,
            blob_id: Some(blob_id.to_string()),
            content_hash: Some(blake3_hash.to_string()),
            etag: Some(blake3_hash.to_string()),
        };

        service::put_blob(
            self.kv,
            self.store,
            &depot_core::store::kv::BlobRecord {
                schema_version: CURRENT_RECORD_VERSION,
                blob_id: blob_id.to_string(),
                hash: blake3_hash.to_string(),
                size,
                created_at: now,
                store: self.store.to_string(),
            },
        )
        .await?;
        let old_record = service::put_artifact(self.kv, self.repo, &path, &record).await?;
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, &path, count_delta, bytes_delta)
            .await;
        self.set_metadata_stale().await?;

        Ok(record)
    }

    /// Method wrapper around the free `enumerate_legacy_chart_relocations`.
    pub async fn enumerate_legacy_chart_relocations(
        &self,
    ) -> error::Result<Vec<(String, String, ArtifactRecord)>> {
        enumerate_legacy_chart_relocations(self.kv, self.repo).await
    }

    /// Method wrapper around the free `migrate_legacy_chart_paths`.
    pub async fn migrate_legacy_chart_paths(&self) -> error::Result<()> {
        migrate_legacy_chart_paths(self.kv, self.repo).await
    }

    /// Rebuild the `index.yaml` metadata file from all chart artifacts.
    ///
    /// Lockless: index.yaml is a single inline artifact (atomic KV put).
    /// The stale flag is deleted first so uploads during rebuild re-set it.
    /// Returns `true` if the index content changed.
    ///
    /// This does **not** relocate legacy chart records — that's handled by
    /// the integrity check, which sets the stale flag after relocating.
    pub async fn rebuild_index(&self) -> error::Result<bool> {
        // Delete stale flag first — any upload during rebuild re-sets it.
        self.clear_stale_flag().await?;

        // Charts live at the repo root, so scan all artifacts and filter by
        // kind. In a helm repo the only non-chart records are `_helm/...`
        // metadata, so this stays cheap.
        let entries: BTreeMap<String, Vec<HelmIndexEntry>> = service::fold_all_artifacts(
            self.kv,
            self.repo,
            "",
            BTreeMap::<String, Vec<HelmIndexEntry>>::new,
            |acc, _sk, record| {
                let helm = match &record.kind {
                    ArtifactKind::HelmChart { helm } => helm,
                    _ => return Ok(()),
                };

                let entry = HelmIndexEntry {
                    name: helm.name.clone(),
                    version: helm.version.clone(),
                    app_version: helm.app_version.clone(),
                    description: helm.description.clone(),
                    api_version: helm.api_version.clone(),
                    chart_type: helm.chart_type.clone(),
                    keywords: helm.keywords.clone(),
                    home: helm.home.clone(),
                    sources: helm.sources.clone(),
                    icon: helm.icon.clone(),
                    deprecated: helm.deprecated,
                    digest: format!("sha256:{}", helm.sha256),
                    urls: vec![chart_path(&helm.name, &helm.version)],
                    created: record.created_at.to_rfc3339(),
                };

                acc.entry(helm.name.clone()).or_default().push(entry);
                Ok(())
            },
            |mut a, b| {
                for (k, mut v) in b {
                    a.entry(k).or_default().append(&mut v);
                }
                a
            },
        )
        .await?;

        let now = depot_core::repo::now_utc();

        let index = HelmIndex {
            api_version: "v1".to_string(),
            entries,
            generated: now.to_rfc3339(),
        };

        let index_yaml = serde_yml::to_string(&index)
            .map_err(|e| DepotError::Storage(Box::new(e), Retryability::Permanent))?;

        // Store as HelmMetadata blob at _helm/meta/index.yaml
        let index_data = index_yaml.as_bytes();
        let blake3_hash = blake3::hash(index_data).to_hex().to_string();
        let blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&blob_id, index_data).await?;

        let blob_rec = depot_core::store::kv::BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.clone(),
            hash: blake3_hash.clone(),
            size: index_data.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let blob_id = service::claim_or_reuse_blob_counted(
            self.kv,
            self.blobs,
            self.store,
            &blob_rec,
            self.updater,
        )
        .await?;
        let index_record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: index_data.len() as u64,
            content_type: "application/x-yaml".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            kind: ArtifactKind::HelmMetadata,
            internal: true,
            blob_id: Some(blob_id),
            etag: Some(blake3_hash.clone()),
            content_hash: Some(blake3_hash.clone()),
        };
        let old_record =
            service::put_artifact(self.kv, self.repo, "_helm/meta/index.yaml", &index_record)
                .await?;
        let content_changed = match &old_record {
            Some(old) => old.content_hash.as_deref() != Some(&blake3_hash),
            None => true,
        };
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, index_record.size as i64 - old.size as i64),
            None => (1i64, index_record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, "_helm/meta/index.yaml", count_delta, bytes_delta)
            .await;

        Ok(content_changed)
    }

    /// Get content for a metadata path under `_helm/meta/`.
    pub async fn get_metadata(&self, path: &str) -> error::Result<Option<String>> {
        let full_path = format!("_helm/meta/{path}");
        let record = service::get_artifact(self.kv, self.repo, &full_path).await?;
        if let Some(record) = record {
            if matches!(&record.kind, ArtifactKind::HelmMetadata) {
                let blob_id = record.blob_id.as_deref().unwrap_or("");
                if !blob_id.is_empty() {
                    if let Some(data) = self.blobs.get(blob_id).await? {
                        return Ok(Some(String::from_utf8_lossy(&data).to_string()));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Check if metadata is stale.
    ///
    /// Returns `true` when the explicit `_helm/metadata_stale` flag is set.
    /// Uploads, deletes, and the integrity-check helm canonicalization phase
    /// all set this flag; the on-fetch path's only job is to rebuild from it.
    pub async fn is_metadata_stale(&self) -> error::Result<bool> {
        Ok(
            service::get_artifact(self.kv, self.repo, "_helm/metadata_stale")
                .await?
                .is_some(),
        )
    }

    /// Set the metadata stale flag on this repo.
    /// Propagation to parent proxies is handled by the API layer.
    pub async fn set_metadata_stale(&self) -> error::Result<()> {
        set_stale_flag(self.kv, self.repo).await
    }

    /// Clear the stale flag.
    pub async fn clear_stale_flag(&self) -> error::Result<()> {
        service::delete_artifact(self.kv, self.repo, "_helm/metadata_stale").await?;
        Ok(())
    }

    /// Convenience: get the index.yaml content as bytes.
    pub async fn get_index(&self) -> error::Result<Option<Vec<u8>>> {
        Ok(self
            .get_metadata("index.yaml")
            .await?
            .map(|s| s.into_bytes()))
    }

    /// Convenience: check if the index is stale (alias for is_metadata_stale).
    pub async fn is_index_stale(&self) -> error::Result<bool> {
        self.is_metadata_stale().await
    }

    /// Store an index.yaml as a blob (used by cache and proxy repos).
    /// Returns `true` if the content changed compared to the previous stored index.
    pub async fn store_index(
        &self,
        content: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> error::Result<bool> {
        let data = content.as_bytes();
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&blob_id, data).await?;

        let blob_rec = depot_core::store::kv::BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.clone(),
            hash: blake3_hash.clone(),
            size: data.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let blob_id = service::claim_or_reuse_blob_counted(
            self.kv,
            self.blobs,
            self.store,
            &blob_rec,
            self.updater,
        )
        .await?;
        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: data.len() as u64,
            content_type: "application/x-yaml".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            kind: ArtifactKind::HelmMetadata,
            internal: true,
            blob_id: Some(blob_id),
            etag: Some(blake3_hash.clone()),
            content_hash: Some(blake3_hash.clone()),
        };
        let old_record =
            service::put_artifact(self.kv, self.repo, "_helm/meta/index.yaml", &record).await?;
        let content_changed = match &old_record {
            Some(old) => old.content_hash.as_deref() != Some(&blake3_hash),
            None => true,
        };
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, "_helm/meta/index.yaml", count_delta, bytes_delta)
            .await;
        Ok(content_changed)
    }

    /// Get a chart package by URL path — direct lookup at the repo root.
    ///
    /// Avoids the brittle "parse `name-version.tgz`" heuristic on the way in.
    /// Charts live at the root, so the path received from the client is the
    /// storage path verbatim — exactly what `index.yaml` advertises in `urls`.
    pub async fn get_chart_by_filename(
        &self,
        filename: &str,
    ) -> error::Result<Option<(depot_core::store::blob::BlobReader, u64, ArtifactRecord)>> {
        let record = match service::get_artifact(self.kv, self.repo, filename).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        let blob_id = record.blob_id.as_deref().unwrap_or("");
        let (reader, size) = self.blobs.open_read(blob_id).await?.ok_or_else(|| {
            DepotError::DataIntegrity(format!("blob missing for blob_id {}", blob_id))
        })?;
        Ok(Some((reader, size, record)))
    }
}

/// Build a minimal synthetic `.tgz` chart for tests and demo.
///
/// The archive contains `{name}/Chart.yaml` with the given metadata
/// and `{name}/templates/NOTES.txt` with a placeholder.
pub fn build_synthetic_chart(
    name: &str,
    version: &str,
    description: &str,
) -> error::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let chart_yaml = format!(
        "apiVersion: v2\n\
         name: {name}\n\
         version: {version}\n\
         description: {description}\n\
         type: application\n"
    );

    let notes_txt = format!("Thank you for installing {name}.\n");

    // Build tar archive
    let mut tar_builder = tar::Builder::new(Vec::new());

    // Add Chart.yaml
    let chart_yaml_bytes = chart_yaml.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_path(format!("{name}/Chart.yaml"))?;
    header.set_size(chart_yaml_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar_builder.append(&header, chart_yaml_bytes)?;

    // Add templates/NOTES.txt
    let notes_bytes = notes_txt.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_path(format!("{name}/templates/NOTES.txt"))?;
    header.set_size(notes_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar_builder.append(&header, notes_bytes)?;

    let tar_data = tar_builder.into_inner()?;

    // Gzip compress
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut encoder, &tar_data)?;
    let tgz_data = encoder.finish()?;

    Ok(tgz_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chart_path() {
        assert_eq!(chart_path("nginx", "1.0.0"), "nginx-1.0.0.tgz");
        assert_eq!(chart_path("my-app", "2.1.3"), "my-app-2.1.3.tgz");
        assert_eq!(
            chart_path("tigera-operator", "v3.28.0"),
            "tigera-operator-v3.28.0.tgz"
        );
    }

    #[test]
    fn test_build_and_parse_synthetic_chart() {
        let tgz = build_synthetic_chart("my-chart", "0.1.0", "A test chart").unwrap();
        let meta = parse_chart(std::io::Cursor::new(&tgz), "test").unwrap();
        assert_eq!(meta.name, "my-chart");
        assert_eq!(meta.version, "0.1.0");
        assert_eq!(meta.description, "A test chart");
        assert_eq!(meta.api_version, "v2");
        assert_eq!(meta.chart_type, "application");
        assert!(!meta.sha256.is_empty());
    }

    #[test]
    fn test_parse_chart_missing_name() {
        // Build a chart with no name field
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let yaml = "version: 1.0.0\ndescription: no name\n";
        let mut tar_builder = tar::Builder::new(Vec::new());
        let yaml_bytes = yaml.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_path("test/Chart.yaml").unwrap();
        header.set_size(yaml_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar_builder.append(&header, yaml_bytes).unwrap();
        let tar_data = tar_builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_data).unwrap();
        let tgz = encoder.finish().unwrap();

        let err = parse_chart(std::io::Cursor::new(&tgz), "test").unwrap_err();
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn test_parse_chart_no_chart_yaml() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let mut tar_builder = tar::Builder::new(Vec::new());
        let data = b"hello";
        let mut header = tar::Header::new_gnu();
        header.set_path("test/values.yaml").unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar_builder.append(&header, &data[..]).unwrap();
        let tar_data = tar_builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_data).unwrap();
        let tgz = encoder.finish().unwrap();

        let err = parse_chart(std::io::Cursor::new(&tgz), "test").unwrap_err();
        assert!(err.to_string().contains("Chart.yaml not found"));
    }

    #[test]
    fn test_parse_chart_full_metadata() {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let yaml = "\
apiVersion: v2
name: full-chart
version: 3.2.1
appVersion: \"1.16.0\"
description: A full-featured chart
type: application
keywords:
  - web
  - proxy
home: https://example.com
sources:
  - https://github.com/example/chart
icon: https://example.com/icon.png
deprecated: true
";
        let mut tar_builder = tar::Builder::new(Vec::new());
        let yaml_bytes = yaml.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_path("full-chart/Chart.yaml").unwrap();
        header.set_size(yaml_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar_builder.append(&header, yaml_bytes).unwrap();
        let tar_data = tar_builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_data).unwrap();
        let tgz = encoder.finish().unwrap();

        let meta = parse_chart(std::io::Cursor::new(&tgz), "test").unwrap();
        assert_eq!(meta.name, "full-chart");
        assert_eq!(meta.version, "3.2.1");
        assert_eq!(meta.app_version, "1.16.0");
        assert_eq!(meta.description, "A full-featured chart");
        assert_eq!(meta.api_version, "v2");
        assert_eq!(meta.chart_type, "application");
        assert_eq!(meta.keywords, vec!["web", "proxy"]);
        assert_eq!(meta.home, "https://example.com");
        assert_eq!(meta.sources, vec!["https://github.com/example/chart"]);
        assert_eq!(meta.icon, "https://example.com/icon.png");
        assert!(meta.deprecated);
        assert!(!meta.sha256.is_empty());
    }
}
