// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use depot_core::error::{self, DepotError, Retryability};
use depot_core::service;
use depot_core::store::blob::BlobStore;
use depot_core::store::kv::{
    AptPackageMeta, ArtifactKind, ArtifactRecord, BlobRecord, KvStore, CURRENT_RECORD_VERSION,
};
use depot_core::update::UpdateSender;

/// Extract the directory portion of a path (everything before the last '/').
fn path_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Build the by-hash KV path for a metadata file.
fn by_hash_path(distribution: &str, rel_path: &str, sha256: &str) -> String {
    let dir = path_dir(rel_path);
    if dir.is_empty() {
        format!("dists/{distribution}/by-hash/SHA256/{sha256}")
    } else {
        format!("dists/{distribution}/{dir}/by-hash/SHA256/{sha256}")
    }
}

use super::hex;
use super::metadata::{
    adjust_release_suite, extract_release_from_inrelease, generate_packages_stanza,
    generate_release, parse_release_sha256, ReleaseFileEntry,
};
use super::parse::{parse_deb, pool_path, DebControl};
use super::signing::{sign_inrelease, sign_release_detached};

/// APT storage operations for a repository.
pub struct AptStore<'a> {
    pub repo: &'a str,
    pub kv: &'a dyn KvStore,
    pub blobs: &'a dyn BlobStore,
    pub store: &'a str,
    pub updater: &'a UpdateSender,
}

impl<'a> AptStore<'a> {
    /// Store a `.deb` package, parse its control file, and set the stale flag.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_package(
        &self,
        data: &[u8],
        distribution: &str,
        component: &str,
        filename: &str,
    ) -> error::Result<ArtifactRecord> {
        // Parse .deb to extract control info
        let control = parse_deb(std::io::Cursor::new(data))?;

        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let sha256_hash = hex::encode(Sha256::digest(data));

        let now = depot_core::repo::now_utc();

        // Store blob under UUID key (async I/O, outside txn)
        let blob_id = uuid::Uuid::new_v4().to_string();
        self.blobs.put(&blob_id, data).await?;

        let path = pool_path(component, &control.package, filename);

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
            content_type: "application/vnd.debian.binary-package".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(blob_id),
            content_hash: Some(blake3_hash.clone()),
            etag: Some(blake3_hash.clone()),
            kind: ArtifactKind::AptPackage {
                apt: AptPackageMeta {
                    package: control.package.clone(),
                    version: control.version.clone(),
                    architecture: control.architecture.clone(),
                    distribution: distribution.to_string(),
                    component: component.to_string(),
                    maintainer: control.maintainer.clone(),
                    depends: control.depends.clone(),
                    section: control.section.clone(),
                    priority: control.priority.clone(),
                    installed_size: control.installed_size.clone(),
                    description: control.description.clone(),
                    sha256: sha256_hash,
                    control: control.full_control.clone(),
                },
            },
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
        self.set_stale_flag(distribution).await?;
        Ok(record)
    }

    /// Commit a `.deb` package whose blob has already been written via `BlobWriter`.
    /// Reads the blob back to parse control metadata, then writes KV records.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit_package(
        &self,
        blob_id: &str,
        blake3_hash: &str,
        sha256_hash: &str,
        size: u64,
        distribution: &str,
        component: &str,
        filename: &str,
    ) -> error::Result<ArtifactRecord> {
        // Read the blob back to parse .deb control metadata.
        let (reader, _) = self.blobs.open_read(blob_id).await?.ok_or_else(|| {
            DepotError::DataIntegrity(format!("blob {blob_id} missing after write"))
        })?;
        let control = tokio::task::spawn_blocking(move || {
            let sync_reader = tokio_util::io::SyncIoBridge::new(reader);
            parse_deb(sync_reader)
        })
        .await
        .map_err(|e| DepotError::Storage(Box::new(e), Retryability::Permanent))??;

        let now = depot_core::repo::now_utc();
        let path = pool_path(component, &control.package, filename);

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
            content_type: "application/vnd.debian.binary-package".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(blob_id),
            content_hash: Some(blake3_hash.to_string()),
            etag: Some(blake3_hash.to_string()),
            kind: ArtifactKind::AptPackage {
                apt: AptPackageMeta {
                    package: control.package.clone(),
                    version: control.version.clone(),
                    architecture: control.architecture.clone(),
                    distribution: distribution.to_string(),
                    component: component.to_string(),
                    maintainer: control.maintainer.clone(),
                    depends: control.depends.clone(),
                    section: control.section.clone(),
                    priority: control.priority.clone(),
                    installed_size: control.installed_size.clone(),
                    description: control.description.clone(),
                    sha256: sha256_hash.to_string(),
                    control: control.full_control.clone(),
                },
            },
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

        self.set_stale_flag(distribution).await?;
        Ok(record)
    }

    /// Store a `.deb` package whose blob has already been written to the blob store.
    /// Used by cache repos after streaming the upstream response to a blob writer.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_cached_package(
        &self,
        blob_id: &str,
        blake3_hash: &str,
        sha256_hash: &str,
        size: u64,
        distribution: &str,
        component: &str,
        filename: &str,
        control: &DebControl,
    ) -> error::Result<ArtifactRecord> {
        let now = depot_core::repo::now_utc();

        let path = pool_path(component, &control.package, filename);

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
            content_type: "application/vnd.debian.binary-package".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(blob_id),
            content_hash: Some(blake3_hash.to_string()),
            etag: Some(blake3_hash.to_string()),
            kind: ArtifactKind::AptPackage {
                apt: AptPackageMeta {
                    package: control.package.clone(),
                    version: control.version.clone(),
                    architecture: control.architecture.clone(),
                    distribution: distribution.to_string(),
                    component: component.to_string(),
                    maintainer: control.maintainer.clone(),
                    depends: control.depends.clone(),
                    section: control.section.clone(),
                    priority: control.priority.clone(),
                    installed_size: control.installed_size.clone(),
                    description: control.description.clone(),
                    sha256: sha256_hash.to_string(),
                    control: control.full_control.clone(),
                },
            },
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
        self.set_stale_flag(distribution).await?;
        Ok(record)
    }

    /// Set the metadata stale flag for a distribution (unconditional overwrite).
    /// Also propagates to any parent proxy repos.
    pub async fn set_metadata_stale(&self, distribution: &str) -> error::Result<()> {
        self.set_stale_flag_inner(distribution, false).await
    }

    /// Set the stale flag for a distribution only if not already present.
    /// Also propagates to any parent proxy repos.
    async fn set_stale_flag(&self, distribution: &str) -> error::Result<()> {
        self.set_stale_flag_inner(distribution, true).await
    }

    async fn set_stale_flag_inner(&self, distribution: &str, if_absent: bool) -> error::Result<()> {
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
            kind: ArtifactKind::AptMetadata,
            internal: true,
        };
        let repo = self.repo.to_string();
        let path = format!("_apt/metadata_stale/{distribution}");
        if if_absent {
            service::put_artifact_if_absent(self.kv, &repo, &path, &flag).await?;
        } else {
            service::put_artifact(self.kv, &repo, &path, &flag).await?;
        }
        // Propagation to parent proxies is handled by the API layer via
        // service::propagate_staleness() spawned as an async task.
        Ok(())
    }

    /// Check if metadata is stale for a distribution.
    pub async fn is_metadata_stale(&self, distribution: &str) -> error::Result<bool> {
        let repo = self.repo.to_string();
        let path = format!("_apt/metadata_stale/{distribution}");
        Ok(service::get_artifact(self.kv, &repo, &path)
            .await?
            .is_some())
    }

    /// Clear the stale flag for a distribution.
    async fn clear_stale_flag(&self, distribution: &str) -> error::Result<()> {
        let repo = self.repo.to_string();
        let path = format!("_apt/metadata_stale/{distribution}");
        service::delete_artifact(self.kv, &repo, &path).await?;
        Ok(())
    }

    /// Get a metadata file's content, routing by path type.
    pub async fn get_metadata(
        &self,
        distribution: &str,
        meta_path: &str,
    ) -> error::Result<Option<Vec<u8>>> {
        let result = match meta_path {
            "InRelease" => {
                self.read_artifact(&format!("dists/{distribution}/InRelease"))
                    .await?
            }
            "Release" => {
                let inrelease = self
                    .read_artifact(&format!("dists/{distribution}/InRelease"))
                    .await?;
                inrelease.map(|data| {
                    let text = String::from_utf8_lossy(&data);
                    extract_release_from_inrelease(&text).into_bytes()
                })
            }
            "Release.gpg" => self.get_release_gpg(distribution).await?,
            p if p.contains("/by-hash/SHA256/") => {
                self.read_artifact(&format!("dists/{distribution}/{p}"))
                    .await?
            }
            other => self.resolve_via_release(distribution, other).await?,
        };
        Ok(result)
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

    /// Generate Release.gpg on demand from the signing key.
    async fn get_release_gpg(&self, distribution: &str) -> error::Result<Option<Vec<u8>>> {
        let inrelease = match self
            .read_artifact(&format!("dists/{distribution}/InRelease"))
            .await?
        {
            Some(d) => d,
            None => return Ok(None),
        };
        let release_text = extract_release_from_inrelease(&String::from_utf8_lossy(&inrelease));
        if let Some(key) = self.get_signing_key().await? {
            match sign_release_detached(&key, &release_text) {
                Ok(gpg) => Ok(Some(gpg.into_bytes())),
                Err(_) => Ok(Some(Vec::new())),
            }
        } else {
            Ok(Some(Vec::new()))
        }
    }

    /// Resolve a mutable metadata path (e.g. "main/binary-amd64/Packages.gz")
    /// by looking up its SHA256 from the current InRelease and serving from
    /// the by-hash artifact.
    async fn resolve_via_release(
        &self,
        distribution: &str,
        meta_path: &str,
    ) -> error::Result<Option<Vec<u8>>> {
        let inrelease = match self
            .read_artifact(&format!("dists/{distribution}/InRelease"))
            .await?
        {
            Some(d) => d,
            None => return Ok(None),
        };
        let release_text = extract_release_from_inrelease(&String::from_utf8_lossy(&inrelease));
        let entries = parse_release_sha256(&release_text);
        for (path, sha256, _size) in &entries {
            if path == meta_path {
                let bhp = by_hash_path(distribution, meta_path, sha256);
                return self.read_artifact(&bhp).await;
            }
        }
        Ok(None)
    }

    /// Rebuild APT metadata: store each Packages file at its by-hash path,
    /// then write InRelease as the single mutable commit point.
    ///
    /// Components and architectures are discovered from the pool contents.
    pub async fn rebuild_metadata(
        &self,
        distribution: &str,
        signing_key_armor: Option<&str>,
    ) -> error::Result<()> {
        // Delete stale flag first — any upload that arrives during our rebuild
        // will re-set it, guaranteeing another rebuild on the next metadata fetch.
        self.clear_stale_flag(distribution).await?;

        // Scan pool artifacts (both legacy _pool/ and new pool/ prefixes),
        // filter by distribution, group by (component, architecture).
        let target_dist = distribution.to_string();
        let repo = self.repo.to_string();

        type ScanAcc = (
            HashMap<(String, String), Vec<(String, ArtifactRecord)>>,
            std::collections::HashSet<String>,
            std::collections::HashSet<String>,
        );

        let scan_prefix = |kv: &'a dyn KvStore, repo: &str, prefix: &str, target_dist: &str| {
            let repo = repo.to_string();
            let target_dist = target_dist.to_string();
            let prefix = prefix.to_string();
            async move {
                service::fold_all_artifacts(
                    kv,
                    &repo,
                    &prefix,
                    || -> ScanAcc {
                        (
                            HashMap::new(),
                            std::collections::HashSet::new(),
                            std::collections::HashSet::new(),
                        )
                    },
                    |acc, sk, record| {
                        let (dist, component, arch) = match &record.kind {
                            ArtifactKind::AptPackage { apt, .. } => (
                                apt.distribution.clone(),
                                apt.component.clone(),
                                apt.architecture.clone(),
                            ),
                            _ => return Ok(()),
                        };
                        if dist != target_dist {
                            return Ok(());
                        }
                        acc.1.insert(arch.clone());
                        acc.2.insert(component.clone());
                        if arch == "all" {
                            acc.0
                                .entry((component.clone(), "all".to_string()))
                                .or_default()
                                .push((sk.to_string(), record.clone()));
                        } else {
                            acc.0
                                .entry((component.clone(), arch))
                                .or_default()
                                .push((sk.to_string(), record.clone()));
                        }
                        Ok(())
                    },
                    |(mut groups_a, mut archs_a, mut comps_a): ScanAcc,
                     (groups_b, archs_b, comps_b): ScanAcc| {
                        for (key, mut vals) in groups_b {
                            groups_a.entry(key).or_default().append(&mut vals);
                        }
                        archs_a.extend(archs_b);
                        comps_a.extend(comps_b);
                        (groups_a, archs_a, comps_a)
                    },
                )
                .await
            }
        };

        // Scan both prefixes and merge results.
        let (mut groups, mut all_archs, mut discovered_components) =
            scan_prefix(self.kv, &repo, "pool/", &target_dist).await?;
        let (groups2, archs2, comps2) = scan_prefix(self.kv, &repo, "_pool/", &target_dist).await?;
        for (key, mut vals) in groups2 {
            groups.entry(key).or_default().append(&mut vals);
        }
        all_archs.extend(archs2);
        discovered_components.extend(comps2);

        // Discover components from pool contents; default to ["main"] if empty
        let components: Vec<String> = if discovered_components.is_empty() {
            vec!["main".to_string()]
        } else {
            let mut v: Vec<String> = discovered_components.into_iter().collect();
            v.sort();
            v
        };

        if all_archs.is_empty() {
            all_archs.insert("amd64".to_string());
        }

        let concrete_archs: Vec<String> =
            all_archs.iter().filter(|a| *a != "all").cloned().collect();
        let concrete_archs = if concrete_archs.is_empty() {
            vec!["amd64".to_string()]
        } else {
            concrete_archs
        };

        let now = depot_core::repo::now_utc();
        let mut release_checksums: Vec<ReleaseFileEntry> = Vec::new();

        // Generate Packages and Packages.gz for each (component, arch),
        // storing each at its by-hash path.
        for component in &components {
            for arch in &concrete_archs {
                let mut packages_text = String::new();

                if let Some(entries) = groups.get(&(component.clone(), arch.clone())) {
                    for (path, record) in entries {
                        let stanza = generate_packages_stanza(record, path, distribution);
                        packages_text.push_str(&stanza);
                        packages_text.push('\n');
                    }
                }

                if let Some(entries) = groups.get(&(component.clone(), "all".to_string())) {
                    for (path, record) in entries {
                        let stanza = generate_packages_stanza(record, path, distribution);
                        packages_text.push_str(&stanza);
                        packages_text.push('\n');
                    }
                }

                let packages_bytes = packages_text.into_bytes();

                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                std::io::Write::write_all(&mut encoder, &packages_bytes)?;
                let packages_gz = encoder.finish()?;

                let rel_path_plain = format!("{component}/binary-{arch}/Packages");
                let rel_path_gz = format!("{component}/binary-{arch}/Packages.gz");

                let sha_plain = self
                    .store_metadata_file(distribution, &rel_path_plain, &packages_bytes, now)
                    .await?;
                let sha_gz = self
                    .store_metadata_file(distribution, &rel_path_gz, &packages_gz, now)
                    .await?;

                release_checksums.push(ReleaseFileEntry {
                    path: rel_path_plain,
                    sha256: sha_plain,
                    size: packages_bytes.len() as u64,
                });
                release_checksums.push(ReleaseFileEntry {
                    path: rel_path_gz,
                    sha256: sha_gz,
                    size: packages_gz.len() as u64,
                });
            }
        }

        // Generate Release file
        let release_text = generate_release(
            distribution,
            &components,
            &concrete_archs,
            &release_checksums,
        );

        // Produce InRelease (commit point — written last)
        let inrelease_text = if let Some(key_armor) = signing_key_armor {
            match sign_inrelease(key_armor, &release_text) {
                Ok(signed) => signed,
                Err(e) => {
                    tracing::warn!(repo = self.repo, error = %e, "GPG signing failed, serving unsigned metadata only");
                    release_text.clone()
                }
            }
        } else {
            release_text.clone()
        };

        self.store_inrelease(distribution, inrelease_text.as_bytes(), now)
            .await?;

        Ok(())
    }

    /// Store a metadata file at its by-hash path. Returns the SHA256 hex string.
    async fn store_metadata_file(
        &self,
        distribution: &str,
        rel_path: &str,
        data: &[u8],
        now: chrono::DateTime<chrono::Utc>,
    ) -> error::Result<String> {
        let sha256 = hex::encode(Sha256::digest(data));
        let blake3_hash = blake3::hash(data).to_hex().to_string();

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
            content_hash: Some(sha256.clone()),
            kind: ArtifactKind::AptMetadata,
            internal: true,
        };

        // Store at by-hash path (immutable — only written once).
        let bhp = by_hash_path(distribution, rel_path, &sha256);
        let repo = self.repo.to_string();
        let written = service::put_artifact_if_absent(self.kv, &repo, &bhp, &record).await?;
        if written {
            self.updater
                .dir_changed(self.repo, &bhp, 1, record.size as i64)
                .await;
            self.updater
                .store_changed(self.store, 1, record.size as i64)
                .await;
        }

        Ok(sha256)
    }

    /// Store InRelease at its mutable path (the single commit point per distribution).
    async fn store_inrelease(
        &self,
        distribution: &str,
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
            content_type: "text/plain".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(blob_id),
            etag: Some(blake3_hash.clone()),
            content_hash: Some(sha256),
            kind: ArtifactKind::AptMetadata,
            internal: true,
        };

        let repo = self.repo.to_string();
        let path = format!("dists/{distribution}/InRelease");
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

    /// Store a by-hash artifact record pointing to an existing blob_id.
    /// Used by snapshots to share blobs without copying data.
    async fn store_by_hash_ref(
        &self,
        distribution: &str,
        rel_path: &str,
        sha256: &str,
        source_blob_id: &str,
        source_size: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> error::Result<()> {
        let bhp = by_hash_path(distribution, rel_path, sha256);
        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: source_size,
            content_type: "application/octet-stream".to_string(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            blob_id: Some(source_blob_id.to_string()),
            etag: None,
            content_hash: Some(sha256.to_string()),
            kind: ArtifactKind::AptMetadata,
            internal: true,
        };
        let repo = self.repo.to_string();
        let written = service::put_artifact_if_absent(self.kv, &repo, &bhp, &record).await?;
        if written {
            self.updater
                .dir_changed(self.repo, &bhp, 1, record.size as i64)
                .await;
            self.updater
                .store_changed(self.store, 1, record.size as i64)
                .await;
        }
        Ok(())
    }

    /// Get a pool package by path. Tries new `pool/` prefix then legacy `_pool/`.
    pub async fn get_package(
        &self,
        component: &str,
        prefix: &str,
        package: &str,
        filename: &str,
    ) -> error::Result<Option<(depot_core::store::blob::BlobReader, u64, ArtifactRecord)>> {
        let repo = self.repo.to_string();
        let new_path = format!("pool/{component}/{prefix}/{package}/{filename}");
        let legacy_path = format!("_pool/{component}/{prefix}/{package}/{filename}");
        let record = match service::get_artifact(self.kv, &repo, &new_path).await? {
            Some(r) => r,
            None => match service::get_artifact(self.kv, &repo, &legacy_path).await? {
                Some(r) => r,
                None => return Ok(None),
            },
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
            service::claim_or_reuse_blob(self.kv, self.blobs, self.store, &priv_blob_rec).await?;
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
            kind: ArtifactKind::AptMetadata,
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
            service::claim_or_reuse_blob(self.kv, self.blobs, self.store, &pub_blob_rec).await?;
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
            kind: ArtifactKind::AptMetadata,
            internal: true,
        };

        let repo = self.repo.to_string();
        let old_priv =
            service::put_artifact(self.kv, &repo, "_apt/signing_key", &priv_record).await?;
        let (cd, bd) = match &old_priv {
            Some(old) => (0i64, priv_record.size as i64 - old.size as i64),
            None => (1i64, priv_record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, "_apt/signing_key", cd, bd)
            .await;
        self.updater.store_changed(self.store, cd, bd).await;

        let old_pub = service::put_artifact(self.kv, &repo, "_apt/public_key", &pub_record).await?;
        let (cd, bd) = match &old_pub {
            Some(old) => (0i64, pub_record.size as i64 - old.size as i64),
            None => (1i64, pub_record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, "_apt/public_key", cd, bd)
            .await;
        self.updater.store_changed(self.store, cd, bd).await;
        Ok(())
    }

    /// Get the signing private key armor.
    pub async fn get_signing_key(&self) -> error::Result<Option<String>> {
        let repo = self.repo.to_string();
        let record = service::get_artifact(self.kv, &repo, "_apt/signing_key").await?;
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

    /// Generate a fresh signing keypair, store it, and return the armored public key.
    pub async fn generate_and_store_signing_key(&self, repo_name: &str) -> error::Result<String> {
        let (private_armor, public_armor) = super::signing::generate_gpg_keypair(repo_name)?;
        self.store_signing_key(&private_armor, &public_armor)
            .await?;
        Ok(public_armor)
    }

    /// Ensure a signing key exists, generating one if absent. Returns the armored
    /// public key when a new key is created, or `None` if one already existed.
    pub async fn ensure_signing_key(&self, repo_name: &str) -> error::Result<Option<String>> {
        if self.get_signing_key().await?.is_some() {
            return Ok(None);
        }
        Ok(Some(self.generate_and_store_signing_key(repo_name).await?))
    }

    /// Import an externally-supplied armored secret key, deriving and storing its
    /// public half. Returns the armored public key.
    pub async fn import_signing_key(&self, secret_key_armor: &str) -> error::Result<String> {
        let public_armor = super::signing::public_key_from_secret(secret_key_armor)?;
        self.store_signing_key(secret_key_armor, &public_armor)
            .await?;
        Ok(public_armor)
    }

    /// List distributions that currently have published metadata (an `InRelease`).
    pub async fn list_distributions(&self) -> error::Result<Vec<String>> {
        let repo = self.repo.to_string();
        service::fold_all_artifacts(
            self.kv,
            &repo,
            "dists/",
            Vec::new,
            |acc, sk, _record| {
                if let Some(rest) = sk.strip_prefix("dists/") {
                    if let Some(dist) = rest.strip_suffix("/InRelease") {
                        if !dist.is_empty() && !dist.contains('/') {
                            acc.push(dist.to_string());
                        }
                    }
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

    /// Mark every published distribution stale so its `InRelease`/`Release.gpg`
    /// are rebuilt — and re-signed with the current key — on the next metadata
    /// fetch. Called after the signing key is rotated or imported.
    pub async fn resign_all_distributions(&self) -> error::Result<()> {
        for dist in self.list_distributions().await? {
            self.set_metadata_stale(&dist).await?;
        }
        Ok(())
    }

    /// Store a metadata file fetched from an upstream cache.
    /// Public wrapper around `store_metadata_file` for use by cache repos.
    pub async fn store_cached_metadata(
        &self,
        distribution: &str,
        rel_path: &str,
        data: &[u8],
    ) -> error::Result<String> {
        let now = depot_core::repo::now_utc();
        self.store_metadata_file(distribution, rel_path, data, now)
            .await
    }

    /// Store an InRelease file from an upstream cache.
    pub async fn store_cached_inrelease(
        &self,
        distribution: &str,
        data: &[u8],
    ) -> error::Result<()> {
        let now = depot_core::repo::now_utc();
        self.store_inrelease(distribution, data, now).await
    }

    /// Public wrapper for clear_stale_flag (for proxy rebuild).
    pub async fn clear_stale_flag_pub(&self, distribution: &str) -> error::Result<()> {
        self.clear_stale_flag(distribution).await
    }

    /// Public wrapper for store_metadata_file (for proxy rebuild).
    pub async fn store_metadata_file_pub(
        &self,
        distribution: &str,
        rel_path: &str,
        data: &[u8],
        now: chrono::DateTime<chrono::Utc>,
    ) -> error::Result<String> {
        self.store_metadata_file(distribution, rel_path, data, now)
            .await
    }

    /// Public wrapper for store_inrelease (for proxy rebuild).
    pub async fn store_inrelease_pub(
        &self,
        distribution: &str,
        data: &[u8],
        now: chrono::DateTime<chrono::Utc>,
    ) -> error::Result<()> {
        self.store_inrelease(distribution, data, now).await
    }

    /// Get the public key armor.
    pub async fn get_public_key(&self) -> error::Result<Option<String>> {
        let repo = self.repo.to_string();
        let record = service::get_artifact(self.kv, &repo, "_apt/public_key").await?;
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

    /// Create a snapshot of a distribution's metadata under a new name.
    ///
    /// Reads the source distribution's InRelease, copies by-hash artifact
    /// records (pointing to the same blobs), adjusts Suite/Codename, re-signs,
    /// and stores a new InRelease. No blob data is copied.
    pub async fn create_snapshot(
        &self,
        source_dist: &str,
        snapshot_name: &str,
        signing_key: Option<&str>,
    ) -> error::Result<()> {
        // Read source InRelease.
        let inrelease_data = self
            .read_artifact(&format!("dists/{source_dist}/InRelease"))
            .await?
            .ok_or_else(|| {
                DepotError::NotFound(format!(
                    "no metadata found for distribution '{source_dist}'"
                ))
            })?;
        let release_text =
            extract_release_from_inrelease(&String::from_utf8_lossy(&inrelease_data));

        let now = depot_core::repo::now_utc();

        // Copy by-hash artifact records from source to snapshot.
        let entries = parse_release_sha256(&release_text);
        for (rel_path, sha256, size) in &entries {
            // Look up the source by-hash artifact to get the blob_id.
            let source_bhp = by_hash_path(source_dist, rel_path, sha256);
            let repo = self.repo.to_string();
            if let Some(record) = service::get_artifact(self.kv, &repo, &source_bhp).await? {
                if let Some(blob_id) = record.blob_id.as_deref() {
                    self.store_by_hash_ref(snapshot_name, rel_path, sha256, blob_id, *size, now)
                        .await?;
                }
            }
        }

        // Adjust Suite/Codename and re-sign.
        let adjusted = adjust_release_suite(&release_text, snapshot_name);
        let inrelease_text = if let Some(key) = signing_key {
            match sign_inrelease(key, &adjusted) {
                Ok(signed) => signed,
                Err(_) => adjusted.clone(),
            }
        } else {
            adjusted.clone()
        };

        self.store_inrelease(snapshot_name, inrelease_text.as_bytes(), now)
            .await?;
        Ok(())
    }
}
