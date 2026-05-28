// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Docker/OCI registry storage logic.
//!
//! Maps Docker concepts onto our generic KV + blob store:
//!
//! - Blobs (layers, configs) stored in blob store keyed by BLAKE3 hash.
//!   The SHA-256 digest is stored as metadata so we can look up by Docker digest.
//! - Manifests stored as blobs, referenced by ArtifactRecord.
//! - Tags stored as artifacts at path `[{image}/]_tags/<tag>` pointing to a manifest digest.
//! - Blob references stored at path `_blobs/sha256/<digest>` for tracking (shared across images).
//!
//! The optional `image` namespace prefix allows multiple Docker images within a single
//! repository (used by cache and proxy repos).
//!
//! Supports:
//! - Docker Image Manifest V2 Schema 2
//! - Docker Manifest List (fat manifest / multi-arch)
//! - OCI Image Manifest
//! - OCI Image Index (multi-arch)

use depot_core::store::blob::BlobReader;
use futures::StreamExt;
use sha2::{Digest, Sha256};

use depot_core::error::{self, DepotError};
use depot_core::repo::{apply_upstream_auth, emit_upstream_event, now_utc, with_trace_context};
use depot_core::service;
use depot_core::store::blob::BlobStore;
use depot_core::store::kv::{
    ArtifactKind, ArtifactRecord, BlobRecord, KvStore, UpstreamAuth, CURRENT_RECORD_VERSION,
};
use depot_core::update::UpdateSender;

// --- Media types ---

pub const DOCKER_MANIFEST_V2: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_MANIFEST_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const OCI_MANIFEST_V1: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_INDEX_V1: &str = "application/vnd.oci.image.index.v1+json";

/// All manifest media types we accept.
pub const MANIFEST_TYPES: &[&str] = &[
    DOCKER_MANIFEST_V2,
    DOCKER_MANIFEST_LIST,
    OCI_MANIFEST_V1,
    OCI_INDEX_V1,
];

/// Accept header value requesting all manifest types.
pub const MANIFEST_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.oci.image.index.v1+json";

// --- Path helpers ---

fn manifest_path_for(image: Option<&str>, digest: &str) -> String {
    match image {
        Some(img) => format!("{img}/_manifests/{digest}"),
        None => format!("_manifests/{digest}"),
    }
}

fn tag_path_for(image: Option<&str>, tag: &str) -> String {
    match image {
        Some(img) => format!("{img}/_tags/{tag}"),
        None => format!("_tags/{tag}"),
    }
}

fn tag_prefix_for(image: Option<&str>) -> String {
    match image {
        Some(img) => format!("{img}/_tags/"),
        None => "_tags/".to_string(),
    }
}

fn blob_ref_path_for(digest: &str) -> String {
    format!("_blobs/{digest}")
}

// --- Helpers ---

/// Compute sha256 digest in Docker format: "sha256:<hex>"
pub fn sha256_digest(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    format!("sha256:{:x}", hash)
}

/// Docker registry storage operations for a repository.
///
/// When `image` is Some, paths are prefixed with the image name to support
/// multiple Docker images within a single depot repository (used by cache/proxy repos).
/// When `image` is None, paths are flat (single-image hosted repos).
pub struct DockerStore<'a> {
    pub repo: &'a str,
    pub image: Option<&'a str>,
    pub kv: &'a dyn KvStore,
    pub blobs: &'a dyn BlobStore,
    pub updater: &'a UpdateSender,
    pub store: &'a str,
}

impl<'a> DockerStore<'a> {
    // --- Path helpers (image-namespace-aware) ---

    fn manifest_path(&self, digest: &str) -> String {
        manifest_path_for(self.image, digest)
    }

    fn tag_path(&self, tag: &str) -> String {
        tag_path_for(self.image, tag)
    }

    fn tag_prefix(&self) -> String {
        tag_prefix_for(self.image)
    }

    pub fn blob_ref_path(&self, digest: &str) -> String {
        blob_ref_path_for(digest)
    }

    // --- Blob operations ---

    /// Check if a blob exists by Docker digest (sha256:...).
    /// Returns (size, blob_id, content_hash) if found.
    #[tracing::instrument(level = "debug", skip(self), fields(repo = %self.repo, digest))]
    pub async fn blob_exists(&self, digest: &str) -> error::Result<Option<(u64, String, String)>> {
        let path = self.blob_ref_path(digest);
        match service::get_artifact(self.kv, self.repo, &path).await? {
            Some(record) => Ok(Some((
                record.size,
                record.blob_id.as_deref().unwrap_or("").to_string(),
                record.content_hash.as_deref().unwrap_or("").to_string(),
            ))),
            None => Ok(None),
        }
    }

    /// Get blob data by Docker digest.
    pub async fn get_blob(&self, digest: &str) -> error::Result<Option<(Vec<u8>, u64)>> {
        let path = self.blob_ref_path(digest);
        let record = service::get_artifact(self.kv, self.repo, &path).await?;
        let record = match record {
            Some(r) => r,
            None => return Ok(None),
        };
        self.updater
            .touch(self.repo, &self.blob_ref_path(digest), now_utc());
        let blob_id = record.blob_id.as_deref().unwrap_or("");
        let data = self.blobs.get(blob_id).await?.ok_or_else(|| {
            DepotError::DataIntegrity(format!("blob missing for blob_id {}", blob_id))
        })?;
        Ok(Some((data, record.size)))
    }

    /// Open a blob for streaming reads by Docker digest.
    pub async fn open_blob(&self, digest: &str) -> error::Result<Option<(BlobReader, u64)>> {
        let path = self.blob_ref_path(digest);
        let record = service::get_artifact(self.kv, self.repo, &path).await?;
        let record = match record {
            Some(r) => r,
            None => return Ok(None),
        };
        self.updater
            .touch(self.repo, &self.blob_ref_path(digest), now_utc());
        self.blobs
            .open_read(record.blob_id.as_deref().unwrap_or(""))
            .await
    }

    /// Store a blob. Computes both BLAKE3 (for storage) and SHA-256 (for Docker digest).
    /// Returns the Docker digest.
    #[tracing::instrument(level = "debug", skip(self, data), fields(repo = %self.repo))]
    pub async fn put_blob(&self, data: &[u8]) -> error::Result<String> {
        let docker_digest = sha256_digest(data);
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let blob_id = uuid::Uuid::new_v4().to_string();
        let now = now_utc();

        // Store in blob store under UUID key.
        self.blobs.put(&blob_id, data).await?;

        let blob_path = self.blob_ref_path(&docker_digest);
        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.clone(),
            hash: blake3_hash.clone(),
            size: data.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        let existing = service::put_dedup_record(self.kv, self.store, &blob_rec).await?;

        let effective_blob_id = existing.as_ref().unwrap_or(&blob_id).clone();
        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: data.len() as u64,
            content_type: "application/octet-stream".to_string(),
            kind: ArtifactKind::DockerBlob {
                docker_digest: docker_digest.clone(),
            },
            blob_id: Some(effective_blob_id),
            etag: Some(blake3_hash.clone()),
            content_hash: Some(blake3_hash),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
        };
        let old_record = service::put_artifact(self.kv, self.repo, &blob_path, &record).await?;
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, &blob_path, count_delta, bytes_delta)
            .await;
        self.updater
            .store_changed(self.store, count_delta, bytes_delta)
            .await;

        // Dedup: if blob already existed, delete the duplicate we just wrote.
        if existing.is_some() {
            let _ = self.blobs.delete(&blob_id).await;
        }

        Ok(docker_digest)
    }

    /// Register a blob that already exists in the blob store (cross-repo mount).
    /// Only creates KV records; does not touch blob data.
    pub async fn mount_blob(
        &self,
        docker_digest: &str,
        blob_id: &str,
        content_hash: &str,
        size: u64,
    ) -> error::Result<String> {
        let now = now_utc();
        let blob_path = self.blob_ref_path(docker_digest);

        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.to_string(),
            hash: content_hash.to_string(),
            size,
            created_at: now,
            store: self.store.to_string(),
        };
        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size,
            content_type: "application/octet-stream".to_string(),
            kind: ArtifactKind::DockerBlob {
                docker_digest: docker_digest.to_string(),
            },
            blob_id: Some(blob_id.to_string()),
            content_hash: Some(content_hash.to_string()),
            etag: Some(content_hash.to_string()),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
        };
        service::put_dedup_record(self.kv, self.store, &blob_rec).await?;
        let old_record = service::put_artifact(self.kv, self.repo, &blob_path, &record).await?;
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, &blob_path, count_delta, bytes_delta)
            .await;
        self.updater
            .store_changed(self.store, count_delta, bytes_delta)
            .await;

        Ok(docker_digest.to_string())
    }

    /// Store a blob whose data has already been written to the blob store.
    pub async fn put_cached_blob(
        &self,
        blob_id: &str,
        docker_digest: &str,
        blake3_hash: &str,
        size: u64,
    ) -> error::Result<()> {
        let now = now_utc();

        let blob_path = self.blob_ref_path(docker_digest);
        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.to_string(),
            hash: blake3_hash.to_string(),
            size,
            created_at: now,
            store: self.store.to_string(),
        };
        let blob_id_owned = blob_id.to_string();
        let existing = service::put_dedup_record(self.kv, self.store, &blob_rec).await?;

        let effective_blob_id = existing.as_ref().unwrap_or(&blob_id_owned).clone();
        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size,
            content_type: "application/octet-stream".to_string(),
            kind: ArtifactKind::DockerBlob {
                docker_digest: docker_digest.to_string(),
            },
            blob_id: Some(effective_blob_id),
            content_hash: Some(blake3_hash.to_string()),
            etag: Some(blake3_hash.to_string()),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
        };
        let old_record = service::put_artifact(self.kv, self.repo, &blob_path, &record).await?;
        let (count_delta, bytes_delta) = match &old_record {
            Some(old) => (0i64, record.size as i64 - old.size as i64),
            None => (1i64, record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, &blob_path, count_delta, bytes_delta)
            .await;
        self.updater
            .store_changed(self.store, count_delta, bytes_delta)
            .await;

        // Dedup: if blob already existed, delete the duplicate we just wrote.
        if existing.is_some() {
            let _ = self.blobs.delete(blob_id).await;
        }

        Ok(())
    }

    // --- Manifest operations ---

    /// Get a manifest by Docker digest.
    pub async fn get_manifest_by_digest(
        &self,
        digest: &str,
    ) -> error::Result<Option<(Vec<u8>, String)>> {
        let path = self.manifest_path(digest);
        let record = service::get_artifact(self.kv, self.repo, &path).await?;
        let record = match record {
            Some(r) => r,
            None => return Ok(None),
        };
        self.updater
            .touch(self.repo, &self.manifest_path(digest), now_utc());

        let blob_id = record.blob_id.as_deref().unwrap_or("");
        if blob_id.is_empty() {
            return Ok(None);
        }
        match self.blobs.get(blob_id).await? {
            Some(data) => Ok(Some((data, record.content_type.clone()))),
            None => {
                tracing::warn!(
                    repo = self.repo,
                    digest,
                    blob_id,
                    "manifest blob missing, treating as cache miss"
                );
                Ok(None)
            }
        }
    }

    /// Resolve a tag to a digest, then fetch the manifest.
    ///
    /// The tag's `last_accessed_at` is only refreshed when the manifest is
    /// also reachable. Touching the tag on a dangling lookup would let a
    /// tag whose manifest has aged out be kept alive forever by its own
    /// 404s, while the manifest stays dead.
    pub async fn get_manifest_by_tag(
        &self,
        tag: &str,
    ) -> error::Result<Option<(Vec<u8>, String, String)>> {
        let path = self.tag_path(tag);
        let tag_record = match service::get_artifact(self.kv, self.repo, &path).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        let digest = match &tag_record.kind {
            ArtifactKind::DockerTag { digest, .. } => digest.clone(),
            _ => {
                return Err(DepotError::DataIntegrity(format!(
                    "tag {} is not a DockerTag",
                    tag
                )))
            }
        };
        match self.get_manifest_by_digest(&digest).await? {
            Some((data, ct)) => {
                self.updater.touch(self.repo, &path, now_utc());
                Ok(Some((data, ct, digest)))
            }
            None => {
                tracing::warn!(
                    repo = self.repo,
                    tag,
                    digest = %digest,
                    "dangling tag: tag points to non-existent manifest"
                );
                Ok(None)
            }
        }
    }

    /// Get a manifest by reference (tag or digest).
    #[tracing::instrument(level = "debug", skip(self), fields(repo = %self.repo, reference))]
    pub async fn get_manifest(
        &self,
        reference: &str,
    ) -> error::Result<Option<(Vec<u8>, String, String)>> {
        if reference.starts_with("sha256:") {
            self.get_manifest_by_digest(reference)
                .await
                .map(|opt| opt.map(|(data, ct)| (data, ct, reference.to_string())))
        } else {
            self.get_manifest_by_tag(reference).await
        }
    }

    /// Store a manifest. Stores inline (no blob).
    #[tracing::instrument(level = "debug", skip(self, data), fields(repo = %self.repo, reference))]
    pub async fn put_manifest(
        &self,
        reference: &str,
        content_type: &str,
        data: &[u8],
    ) -> error::Result<String> {
        let digest = sha256_digest(data);
        let now = now_utc();

        let manifest_path = self.manifest_path(&digest);

        // Store manifest JSON in blob store
        let blob_id = uuid::Uuid::new_v4().to_string();
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        self.blobs.put(&blob_id, data).await?;

        let blob_rec = BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.clone(),
            hash: blake3_hash.clone(),
            size: data.len() as u64,
            created_at: now,
            store: self.store.to_string(),
        };
        service::put_dedup_record(self.kv, self.store, &blob_rec).await?;

        let manifest_record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: data.len() as u64,
            content_type: content_type.to_string(),
            kind: ArtifactKind::DockerManifest {
                docker_digest: digest.clone(),
            },
            blob_id: Some(blob_id),
            etag: Some(blake3_hash.clone()),
            content_hash: Some(blake3_hash),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
        };

        // Write the tag before the manifest so that GC's untagged-manifest
        // cleanup cannot see an untagged manifest in the window between the two
        // writes.  The brief moment where the tag points to a not-yet-existent
        // manifest is harmless — get_manifest_by_tag already handles that case.
        if !reference.starts_with("sha256:") {
            let tag_record = ArtifactRecord {
                schema_version: CURRENT_RECORD_VERSION,
                id: String::new(),
                size: 0,
                content_type: content_type.to_string(),
                kind: ArtifactKind::DockerTag {
                    digest: digest.clone(),
                    tag: reference.to_string(),
                },
                blob_id: None,
                content_hash: None,
                etag: None,
                created_at: now,
                updated_at: now,
                last_accessed_at: now,
                path: String::new(),
                internal: false,
            };

            let tag_path = tag_path_for(self.image, reference);
            let old_tag = service::put_artifact(self.kv, self.repo, &tag_path, &tag_record).await?;
            let (tc, tb) = match &old_tag {
                Some(old) => (0i64, tag_record.size as i64 - old.size as i64),
                None => (1i64, tag_record.size as i64),
            };
            self.updater.dir_changed(self.repo, &tag_path, tc, tb).await;
            self.updater.store_changed(self.store, tc, tb).await;
        }

        let old_manifest =
            service::put_artifact(self.kv, self.repo, &manifest_path, &manifest_record).await?;
        let (count_delta, bytes_delta) = match &old_manifest {
            Some(old) => (0i64, manifest_record.size as i64 - old.size as i64),
            None => (1i64, manifest_record.size as i64),
        };
        self.updater
            .dir_changed(self.repo, &manifest_path, count_delta, bytes_delta)
            .await;
        self.updater
            .store_changed(self.store, count_delta, bytes_delta)
            .await;

        Ok(digest)
    }

    /// Delete a manifest by Docker digest.
    #[tracing::instrument(level = "debug", skip(self), fields(repo = %self.repo, digest))]
    pub async fn delete_manifest(&self, digest: &str) -> error::Result<bool> {
        let path = self.manifest_path(digest);
        Ok(service::delete_artifact(self.kv, self.repo, &path)
            .await?
            .is_some())
    }

    // --- Tag operations ---

    /// List all tags in this repository (for the current image namespace).
    pub async fn list_tags(&self) -> error::Result<Vec<String>> {
        let prefix = self.tag_prefix();
        let tags = service::fold_all_artifacts(
            self.kv,
            self.repo,
            &prefix,
            Vec::new,
            |acc, sk, _record| {
                if let Some(tag) = sk.strip_prefix(&prefix) {
                    acc.push(tag.to_string());
                }
                Ok(())
            },
            |mut a, b| {
                a.extend(b);
                a
            },
        )
        .await?;
        Ok(tags)
    }
}

// --- Upstream Docker V2 client for cache repos ---

/// Parse a `WWW-Authenticate: Bearer realm="...",service="...",scope="..."`
/// header and fetch an anonymous or authenticated token.
async fn fetch_bearer_token(
    http: &reqwest::Client,
    www_auth: &str,
    upstream_auth: Option<&UpstreamAuth>,
) -> Option<String> {
    // Parse realm, service, scope from the Bearer challenge.
    let bearer = www_auth.strip_prefix("Bearer ")?;
    let mut realm = None;
    let mut params = Vec::new();
    for part in bearer.split(',') {
        let part = part.trim();
        if let Some((key, val)) = part.split_once('=') {
            let val = val.trim_matches('"');
            match key {
                "realm" => realm = Some(val.to_string()),
                _ => params.push((key.to_string(), val.to_string())),
            }
        }
    }
    let realm = realm?;
    let mut token_url = reqwest::Url::parse(&realm).ok()?;
    for (k, v) in &params {
        token_url.query_pairs_mut().append_pair(k, v);
    }

    let req = if let Some(auth) = upstream_auth {
        http.get(token_url)
            .basic_auth(&auth.username, auth.password.as_deref())
    } else {
        http.get(token_url)
    };
    let resp = with_trace_context(req).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("token")
        .or_else(|| body.get("access_token"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Send a GET request with upstream auth. If the response is 401 with a
/// Bearer challenge, perform the Docker token exchange and retry.
async fn upstream_get_with_bearer_retry(
    http: &reqwest::Client,
    url: &str,
    extra_headers: Option<(&str, &str)>,
    upstream_auth: Option<&UpstreamAuth>,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut builder = http.get(url);
    if let Some((k, v)) = extra_headers {
        builder = builder.header(k, v);
    }
    builder = apply_upstream_auth(builder, upstream_auth);
    let resp = builder.send().await?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        if let Some(www_auth) = resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
        {
            if www_auth.starts_with("Bearer ") {
                if let Some(token) = fetch_bearer_token(http, www_auth, upstream_auth).await {
                    let mut retry = http
                        .get(url)
                        .header("Authorization", format!("Bearer {token}"));
                    if let Some((k, v)) = extra_headers {
                        retry = retry.header(k, v);
                    }
                    return with_trace_context(retry).send().await;
                }
            }
        }
    }

    Ok(resp)
}

/// Fetch a manifest from an upstream Docker V2 registry.
pub async fn fetch_upstream_manifest(
    http: &reqwest::Client,
    upstream_url: &str,
    image: &str,
    reference: &str,
    upstream_auth: Option<&UpstreamAuth>,
) -> error::Result<Option<(Vec<u8>, String, String)>> {
    let url = format!(
        "{}/v2/{}/manifests/{}",
        upstream_url.trim_end_matches('/'),
        image,
        reference
    );
    tracing::info!(%url, "fetching manifest from upstream");

    let upstream_start = std::time::Instant::now();
    let resp = upstream_get_with_bearer_retry(
        http,
        &url,
        Some(("Accept", MANIFEST_ACCEPT)),
        upstream_auth,
    )
    .await?;

    emit_upstream_event(
        "GET",
        &url,
        resp.status().as_u16(),
        upstream_start.elapsed(),
        0,
        resp.content_length().unwrap_or(0),
        "upstream.docker.manifest",
    );

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(DepotError::Upstream(format!(
            "upstream manifest fetch returned {}: {}",
            resp.status(),
            url
        )));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(OCI_MANIFEST_V1)
        .to_string();

    let digest = resp
        .headers()
        .get("Docker-Content-Digest")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let data = resp.bytes().await?.to_vec();

    // Compute digest if upstream didn't provide it.
    let digest = digest.unwrap_or_else(|| sha256_digest(&data));

    Ok(Some((data, content_type, digest)))
}

/// Fetch a blob from an upstream Docker V2 registry (buffered in memory).
/// Prefer `fetch_upstream_blob_to_temp` for large blobs.
pub async fn fetch_upstream_blob(
    http: &reqwest::Client,
    upstream_url: &str,
    image: &str,
    digest: &str,
    upstream_auth: Option<&UpstreamAuth>,
) -> error::Result<Option<Vec<u8>>> {
    let url = format!(
        "{}/v2/{}/blobs/{}",
        upstream_url.trim_end_matches('/'),
        image,
        digest
    );
    tracing::info!(%url, "fetching blob from upstream");

    let upstream_start = std::time::Instant::now();
    let resp = upstream_get_with_bearer_retry(http, &url, None, upstream_auth).await?;

    emit_upstream_event(
        "GET",
        &url,
        resp.status().as_u16(),
        upstream_start.elapsed(),
        0,
        resp.content_length().unwrap_or(0),
        "upstream.docker.blob",
    );

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(DepotError::Upstream(format!(
            "upstream blob fetch returned {}: {}",
            resp.status(),
            url
        )));
    }

    let data = resp.bytes().await?.to_vec();
    Ok(Some(data))
}

/// Fetch a blob from an upstream Docker V2 registry, streaming to a blob writer.
/// Returns (docker_digest, blake3_hash, size) on success.
pub async fn fetch_upstream_blob_to_writer(
    http: &reqwest::Client,
    upstream_url: &str,
    image: &str,
    digest: &str,
    upstream_auth: Option<&UpstreamAuth>,
    writer: &mut dyn depot_core::store::blob::BlobWriter,
) -> error::Result<Option<(String, String, u64)>> {
    let url = format!(
        "{}/v2/{}/blobs/{}",
        upstream_url.trim_end_matches('/'),
        image,
        digest
    );
    tracing::info!(%url, "fetching blob from upstream (streaming)");

    let upstream_start = std::time::Instant::now();
    let resp = upstream_get_with_bearer_retry(http, &url, None, upstream_auth).await?;

    emit_upstream_event(
        "GET",
        &url,
        resp.status().as_u16(),
        upstream_start.elapsed(),
        0,
        resp.content_length().unwrap_or(0),
        "upstream.docker.blob",
    );

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(DepotError::Upstream(format!(
            "upstream blob fetch returned {}: {}",
            resp.status(),
            url
        )));
    }

    let mut sha256 = Sha256::new();
    let mut blake3 = blake3::Hasher::new();
    let mut size = 0u64;

    let mut stream = resp.bytes_stream();
    while let Some(result) = stream.next().await {
        let chunk: bytes::Bytes = result?;
        if chunk.is_empty() {
            continue;
        }
        sha256.update(&chunk);
        blake3.update(&chunk);
        writer.write_chunk(&chunk).await?;
        size += chunk.len() as u64;
    }

    let docker_digest = format!("sha256:{:x}", sha256.finalize());
    let blake3_hash = blake3.finalize().to_hex().to_string();

    Ok(Some((docker_digest, blake3_hash, size)))
}
