// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use futures::StreamExt;
use metrics::counter;

use super::{
    apply_upstream_auth, emit_upstream_event, now_utc, ArtifactContent, ArtifactData, RawRepo,
};
use crate::error::{self, DepotError};
use crate::inflight::InflightMap;
use crate::service;
use crate::store::blob::BlobStore;
use crate::store::kv::{
    ArtifactKind, ArtifactRecord, BlobRecord, KvStore, PaginatedResult, Pagination, UpstreamAuth,
    CURRENT_RECORD_VERSION,
};
use crate::update::UpdateSender;

/// A cache repository that proxies an upstream URL with local write-through caching.
///
/// On GET:
///   1. Check local store for the artifact.
///   2. If found and not expired, return it.
///   3. If missing or expired, fetch from upstream, tee-stream to client and blob store.
pub struct CacheRepo {
    pub name: String,
    pub upstream_url: String,
    pub cache_ttl_secs: u64,
    pub kv: Arc<dyn KvStore>,
    pub blobs: Arc<dyn BlobStore>,
    pub store: String,
    pub http: reqwest::Client,
    pub upstream_auth: Option<UpstreamAuth>,
    pub inflight: InflightMap,
    pub updater: UpdateSender,
    pub format: String,
}

/// Extract an upstream BLAKE3 content hash, if advertised via `X-Checksum-Blake3`.
///
/// The value must be exactly 64 lowercase hex chars to be accepted (the canonical
/// BLAKE3 hex representation). Returns `None` for missing, malformed, or
/// wrong-length values — callers fall back to computing the hash from the body.
fn extract_upstream_blake3(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let val = headers.get("x-checksum-blake3")?.to_str().ok()?.trim();
    if val.len() == 64
        && val
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Some(val.to_string())
    } else {
        None
    }
}

/// Extract an upstream ETag or synthesize one from available metadata.
///
/// Checks headers in priority order:
///   1. ETag (standard HTTP)
///   2. Content-Digest (RFC 9530, e.g. `sha-256=:base64:`)
///   3. Digest (RFC 3230, e.g. `sha-256=base64`)
///   4. X-Checksum-SHA256 / SHA1 / MD5 (Nexus/Artifactory convention)
///   5. Content-MD5 (RFC 1864, base64)
///   6. Synthesize from Last-Modified + size + path
fn extract_or_synthesize_etag(
    headers: &reqwest::header::HeaderMap,
    path: &str,
    size: u64,
) -> String {
    // 1. Standard ETag header.
    if let Some(etag) = headers
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
    {
        let etag = etag.trim_matches('"');
        if !etag.is_empty() {
            return etag.to_string();
        }
    }
    // 2. Content-Digest (RFC 9530): `sha-256=:base64:` or `sha-512=:base64:`
    if let Some(val) = headers.get("content-digest").and_then(|v| v.to_str().ok()) {
        if !val.is_empty() {
            return val.to_string();
        }
    }
    // 3. Digest (RFC 3230, deprecated but widely used): `sha-256=base64`
    if let Some(val) = headers.get("digest").and_then(|v| v.to_str().ok()) {
        if !val.is_empty() {
            return val.to_string();
        }
    }
    // 4. Nexus/Artifactory-style checksum headers.
    for name in ["x-checksum-sha256", "x-checksum-sha1", "x-checksum-md5"] {
        if let Some(val) = headers.get(name).and_then(|v| v.to_str().ok()) {
            if !val.is_empty() {
                return val.to_string();
            }
        }
    }
    // 5. Content-MD5 (RFC 1864, base64-encoded).
    if let Some(val) = headers.get("content-md5").and_then(|v| v.to_str().ok()) {
        if !val.is_empty() {
            return val.to_string();
        }
    }
    // 6. Synthesize from available metadata.
    let last_mod = headers
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    blake3::hash(format!("{}:{}:{}", last_mod, size, path).as_bytes())
        .to_hex()
        .to_string()
}

impl CacheRepo {
    #[tracing::instrument(level = "debug", skip(self), fields(repo = %self.name, path))]
    pub async fn head(&self, path: &str) -> error::Result<Option<ArtifactRecord>> {
        let now = now_utc();

        // Check local cache first.
        let cached = service::get_artifact(self.kv.as_ref(), &self.name, path).await?;

        let mut stale_record: Option<ArtifactRecord> = None;

        if let Some(record) = cached {
            let age = (now - record.updated_at).num_seconds();
            if self.cache_ttl_secs == 0 || age < self.cache_ttl_secs as i64 {
                return Ok(Some(record));
            }
            stale_record = Some(record);
        }

        // Fetch HEAD from upstream — no blob write, no caching.
        let url = format!("{}/{}", self.upstream_url.trim_end_matches('/'), path);
        tracing::debug!(repo = %self.name, %url, "HEAD request to upstream");

        let upstream_start = std::time::Instant::now();
        let resp = match apply_upstream_auth(self.http.head(&url), self.upstream_auth.as_ref())
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                if let Some(record) = stale_record {
                    tracing::warn!(
                        repo = %self.name,
                        path,
                        error = %e,
                        "upstream unreachable on HEAD, serving stale cache"
                    );
                    return Ok(Some(record));
                }
                return Err(e.into());
            }
        };

        emit_upstream_event(
            "HEAD",
            &url,
            resp.status().as_u16(),
            upstream_start.elapsed(),
            0,
            resp.content_length().unwrap_or(0),
            "upstream.fetch",
        );

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if let Some(record) = stale_record {
                tracing::warn!(
                    repo = %self.name,
                    path,
                    status = %resp.status(),
                    "upstream error on HEAD, serving stale cache"
                );
                return Ok(Some(record));
            }
            return Err(DepotError::Upstream(format!(
                "upstream returned {}: {}",
                resp.status(),
                url
            )));
        }

        // Build a synthetic record from upstream response headers.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let size = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);

        let etag = extract_or_synthesize_etag(resp.headers(), path, size);
        let upstream_blake3 = extract_upstream_blake3(resp.headers());

        Ok(Some(ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            kind: ArtifactKind::Raw,
            blob_id: None,
            content_hash: upstream_blake3,
            etag: Some(etag),
            size,
            content_type,
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
        }))
    }

    #[tracing::instrument(level = "debug", skip(self), fields(repo = %self.name, path))]
    pub async fn get(&self, path: &str) -> error::Result<Option<ArtifactContent>> {
        let now = now_utc();

        // Check local cache first.
        let cached = service::get_artifact(self.kv.as_ref(), &self.name, path).await?;

        let mut stale_record: Option<ArtifactRecord> = None;

        if let Some(record) = cached {
            let age = (now - record.updated_at).num_seconds();
            if self.cache_ttl_secs == 0 || age < self.cache_ttl_secs as i64 {
                // Cache hit, not expired.
                self.updater.touch(&self.name, path, now);
                counter!("cache_requests_total", "result" => "hit", "format" => self.format.clone()).increment(1);
                return Ok(Some(super::open_artifact_blob(&*self.blobs, record).await?));
            }
            // Expired -- fall through to re-fetch.
            tracing::debug!(
                repo = %self.name,
                path,
                age,
                ttl = self.cache_ttl_secs,
                "cache expired, re-fetching"
            );
            stale_record = Some(record);
        }

        // Inflight dedup: only one caller fetches from upstream per (repo, path).
        // The guard is moved into the background task so it is held until the
        // KV commit finishes — not dropped when streaming starts.
        let inflight_key = format!("raw:{}:{}", self.name, path);
        let inflight_guard = match self.inflight.try_claim(&inflight_key) {
            Ok(guard) => guard,
            Err(notify) => {
                // Another task is already fetching this. Wait for it, then re-check cache.
                notify.notified().await;
                let record = service::get_artifact(self.kv.as_ref(), &self.name, path).await?;
                if let Some(record) = record {
                    counter!("cache_requests_total", "result" => "hit", "format" => self.format.clone()).increment(1);
                    return Ok(Some(super::open_artifact_blob(&*self.blobs, record).await?));
                }
                return Ok(None);
            }
        };

        // Fetch from upstream.
        let url = format!("{}/{}", self.upstream_url.trim_end_matches('/'), path);
        tracing::info!(repo = %self.name, %url, "fetching from upstream");

        let upstream_start = std::time::Instant::now();
        let resp = match apply_upstream_auth(self.http.get(&url), self.upstream_auth.as_ref())
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                if let Some(record) = stale_record {
                    tracing::warn!(
                        repo = %self.name,
                        path,
                        error = %e,
                        "upstream unreachable, serving stale cache"
                    );
                    counter!("cache_requests_total", "result" => "stale_serve", "format" => self.format.clone()).increment(1);
                    return Ok(Some(super::open_artifact_blob(&*self.blobs, record).await?));
                }
                counter!("cache_requests_total", "result" => "error", "format" => self.format.clone()).increment(1);
                return Err(e.into());
            }
        };

        emit_upstream_event(
            "GET",
            &url,
            resp.status().as_u16(),
            upstream_start.elapsed(),
            0,
            resp.content_length().unwrap_or(0),
            "upstream.fetch",
        );

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                counter!("cache_requests_total", "result" => "miss", "format" => self.format.clone()).increment(1);
                return Ok(None);
            }
            if let Some(record) = stale_record {
                tracing::warn!(
                    repo = %self.name,
                    path,
                    status = %resp.status(),
                    "upstream unreachable, serving stale cache"
                );
                counter!("cache_requests_total", "result" => "stale_serve", "format" => self.format.clone()).increment(1);
                return Ok(Some(super::open_artifact_blob(&*self.blobs, record).await?));
            }
            counter!("cache_requests_total", "result" => "error", "format" => self.format.clone())
                .increment(1);
            return Err(DepotError::Upstream(format!(
                "upstream returned {}: {}",
                resp.status(),
                url
            )));
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let upstream_size = resp.content_length().unwrap_or(0);
        let upstream_etag = extract_or_synthesize_etag(resp.headers(), path, upstream_size);
        let upstream_blake3 = extract_upstream_blake3(resp.headers());

        // Tee-stream via mpsc channel: the background task streams chunks from upstream,
        // caches them to blob store, and forwards each chunk through the channel.
        // The client stream only completes successfully after the KV commit succeeds.
        // If anything fails, an error is sent through the channel so the client sees
        // a broken transfer (fewer bytes than Content-Length) rather than silent truncation.
        let blob_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);

        // Build a preliminary record using the upstream Content-Length (when available)
        // so the response can include a Content-Length header. The background task
        // independently records the actual size in KV once the download completes.
        // The upstream-asserted BLAKE3 (if any) flows into the preliminary record
        // so the streaming response emits `X-Checksum-Blake3` before the body finishes.
        let record = ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            kind: ArtifactKind::Raw,
            blob_id: Some(blob_id.clone()),
            content_hash: upstream_blake3.clone(),
            etag: Some(upstream_etag.clone()), // available immediately
            size: upstream_size,               // upstream Content-Length, 0 if unknown
            content_type: content_type.clone(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
        };

        // Clone Arcs for the background commit task.
        let blobs = Arc::clone(&self.blobs);
        let kv = Arc::clone(&self.kv);
        let repo_name = self.name.clone();
        let store_bg = self.store.clone();
        let path_bg = path.to_string();
        let content_type_bg = content_type.clone();
        let blob_id_bg = blob_id.clone();
        let updater_bg = self.updater.clone();
        let etag_bg = upstream_etag;
        let upstream_blake3_bg = upstream_blake3;

        tokio::spawn(async move {
            let _guard = inflight_guard; // hold until KV commit finishes
            let result = async {
                let mut blob_writer = blobs.create_writer(&blob_id_bg, None).await?;
                let mut hasher = blake3::Hasher::new();
                let mut size = 0u64;
                let mut client_gone = false;

                let mut stream = resp.bytes_stream();
                while let Some(chunk_result) = stream.next().await {
                    let chunk: bytes::Bytes = chunk_result
                        .map_err(|e| DepotError::Upstream(format!("upstream stream error: {e}")))?;
                    if chunk.is_empty() {
                        continue;
                    }
                    hasher.update(&chunk);
                    blob_writer.write_chunk(&chunk).await?;
                    size += chunk.len() as u64;
                    if !client_gone && tx.send(Ok(chunk)).await.is_err() {
                        client_gone = true; // client disconnected, keep caching
                    }
                }

                blob_writer.finish().await?;

                let content_hash = hasher.finalize().to_hex().to_string();

                // If upstream advertised a BLAKE3 via X-Checksum-Blake3, verify it
                // matches what we actually streamed. A mismatch means the upstream
                // lied about the content (or the bytes were corrupted in transit);
                // refuse to cache it. The client gets a stream error via the
                // outer error path so the transfer fails detectably.
                if let Some(claimed) = upstream_blake3_bg.as_deref() {
                    if claimed != content_hash {
                        return Err(DepotError::Upstream(format!(
                            "upstream BLAKE3 mismatch for {}: claimed {}, computed {}",
                            path_bg, claimed, content_hash
                        )));
                    }
                }

                // Create KV records for the blob and artifact. On a dedup hit
                // (re-fetch of unchanged upstream content) this deletes the
                // duplicate blob we just wrote and reuses the existing one.
                let blob_rec = BlobRecord {
                    schema_version: CURRENT_RECORD_VERSION,
                    blob_id: blob_id_bg.clone(),
                    hash: content_hash.clone(),
                    size,
                    created_at: now,
                    store: store_bg.clone(),
                };
                // Credits the store iff a new blob record was created, and drops
                // the duplicate on a dedup hit (the cache-refetch leak fix).
                let effective_blob_id = service::claim_or_reuse_blob_counted(
                    kv.as_ref(),
                    blobs.as_ref(),
                    &store_bg,
                    &blob_rec,
                    &updater_bg,
                )
                .await?;

                let artifact = ArtifactRecord {
                    schema_version: CURRENT_RECORD_VERSION,
                    id: String::new(),
                    kind: ArtifactKind::Raw,
                    blob_id: Some(effective_blob_id),
                    content_hash: Some(content_hash),
                    etag: Some(etag_bg.clone()),
                    size,
                    content_type: content_type_bg,
                    created_at: now,
                    updated_at: now,
                    last_accessed_at: now,
                    path: String::new(),
                    internal: false,
                };
                let old_record =
                    service::put_artifact(kv.as_ref(), &repo_name, &path_bg, &artifact).await?;
                let (count_delta, bytes_delta) = match &old_record {
                    Some(old) => (0i64, artifact.size as i64 - old.size as i64),
                    None => (1i64, artifact.size as i64),
                };
                updater_bg
                    .dir_changed(&repo_name, &path_bg, count_delta, bytes_delta)
                    .await;
                // Store credit handled by claim_or_reuse_blob_counted above.

                // tx is dropped here after successful commit → clean stream end
                Ok::<(), DepotError>(())
            }
            .await;

            if let Err(e) = result {
                tracing::error!(error = %e, "background cache commit failed");
                // Send error to client so the chunked transfer is aborted
                let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
            }
        });

        counter!("cache_requests_total", "result" => "miss", "format" => self.format.clone())
            .increment(1);
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Some(ArtifactContent {
            data: ArtifactData::Stream(Box::pin(stream)),
            record,
        }))
    }
    pub async fn list(
        &self,
        prefix: &str,
        pagination: &Pagination,
    ) -> error::Result<PaginatedResult<(String, ArtifactRecord)>> {
        service::list_artifacts(self.kv.as_ref(), &self.name, prefix, pagination).await
    }

    pub async fn search(
        &self,
        query: &str,
        pagination: &Pagination,
    ) -> error::Result<PaginatedResult<(String, ArtifactRecord)>> {
        service::search_artifacts(self.kv.as_ref(), &self.name, query, pagination).await
    }

    pub async fn list_children(
        &self,
        prefix: &str,
    ) -> error::Result<crate::store::kv::ChildrenResult> {
        service::list_children(self.kv.as_ref(), &self.name, prefix).await
    }
}

#[async_trait::async_trait]
impl RawRepo for CacheRepo {
    async fn head(&self, path: &str) -> error::Result<Option<ArtifactRecord>> {
        self.head(path).await
    }

    async fn get(&self, path: &str) -> error::Result<Option<ArtifactContent>> {
        self.get(path).await
    }

    async fn search(
        &self,
        query: &str,
        pagination: &Pagination,
    ) -> error::Result<PaginatedResult<(String, ArtifactRecord)>> {
        self.search(query, pagination).await
    }

    async fn list(
        &self,
        prefix: &str,
        pagination: &Pagination,
    ) -> error::Result<PaginatedResult<(String, ArtifactRecord)>> {
        self.list(prefix, pagination).await
    }

    async fn list_children(&self, prefix: &str) -> error::Result<crate::store::kv::ChildrenResult> {
        self.list_children(prefix).await
    }
}
