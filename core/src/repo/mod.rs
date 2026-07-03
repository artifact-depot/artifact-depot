// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Shared repository infrastructure: `RepoContext`, `RawRepo`, and the
//! shared Hosted/Cache/Proxy implementations used by every format handler.
//!
//! Format-specific repo logic (helm, npm, apt, etc.) lives in the `depot`
//! crate; this module only provides the generic machinery they share.

pub mod cache;
pub mod hosted;
pub mod proxy;

pub use self::cache::CacheRepo;
pub use self::hosted::HostedRepo;
pub use self::proxy::ProxyRepo;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use metrics::{counter, histogram};
use sha2::Digest;

use crate::error::{self, DepotError};
use crate::inflight::InflightMap;
use crate::service;
use crate::store::blob::{BlobReader, BlobStore};
use crate::store::kv::{
    ArtifactFormat, ArtifactKind, ArtifactRecord, BlobRecord, ChildrenResult, KvStore,
    PaginatedResult, Pagination, RepoConfig, RepoKind, RepoType, UpstreamAuth,
    CURRENT_RECORD_VERSION,
};
use crate::update::UpdateSender;

/// Context for artifact ingestion: identifies the storage backend,
/// target repository, and format metadata.
pub struct IngestionCtx<'a> {
    pub kv: &'a dyn KvStore,
    pub blobs: &'a dyn BlobStore,
    pub store: &'a str,
    pub repo: &'a str,
    pub format: &'a str,
    pub repo_type: &'a str,
}

/// Pre-computed blob information from a streaming upload.
/// Bundles the blob_id, hashes, and size that are known after
/// streaming to `BlobWriter` and before writing KV records.
pub struct BlobInfo {
    pub blob_id: String,
    pub blake3_hash: String,
    pub sha256_hash: String,
    pub size: u64,
}

/// Shared infrastructure for constructing repo implementations.
#[derive(Clone)]
pub struct RepoContext {
    pub kv: Arc<dyn KvStore>,
    pub blobs: Arc<dyn BlobStore>,
    pub http: reqwest::Client,
    pub inflight: InflightMap,
    pub updater: UpdateSender,
}

/// A byte-chunk stream for streaming cache miss responses.
pub type BoxByteStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send>>;

/// The data payload of an artifact response: either a seekable blob reader or a streaming channel.
pub enum ArtifactData {
    /// Cache hit / hosted: a BlobReader backed by a file on disk.
    Reader(BlobReader),
    /// Cache miss tee-stream: chunks arrive as the background task caches them.
    Stream(BoxByteStream),
}

/// Result of fetching an artifact.
pub struct ArtifactContent {
    pub data: ArtifactData,
    pub record: ArtifactRecord,
}

/// Open the blob for an artifact record, returning an `ArtifactContent` or a `DataIntegrity` error.
#[tracing::instrument(level = "debug", name = "open_artifact_blob", skip_all, fields(blob_id = record.blob_id.as_deref().unwrap_or("")))]
pub async fn open_artifact_blob(
    blobs: &dyn BlobStore,
    record: ArtifactRecord,
) -> error::Result<ArtifactContent> {
    let (data, _size) = blobs
        .open_read(record.blob_id.as_deref().unwrap_or(""))
        .await?
        .ok_or_else(|| {
            error::DepotError::DataIntegrity(format!(
                "blob missing for hash {}",
                record.blob_id.as_deref().unwrap_or("")
            ))
        })?;
    Ok(ArtifactContent {
        data: ArtifactData::Reader(data),
        record,
    })
}

/// Trait for read-only raw artifact operations, implemented by all repo types.
#[async_trait::async_trait]
pub trait RawRepo: Send + Sync {
    async fn head(&self, path: &str) -> error::Result<Option<ArtifactRecord>>;
    async fn get(&self, path: &str) -> error::Result<Option<ArtifactContent>>;
    async fn search(
        &self,
        query: &str,
        pagination: &Pagination,
    ) -> error::Result<PaginatedResult<(String, ArtifactRecord)>>;
    async fn list(
        &self,
        prefix: &str,
        pagination: &Pagination,
    ) -> error::Result<PaginatedResult<(String, ArtifactRecord)>>;
    async fn list_children(&self, prefix: &str) -> error::Result<ChildrenResult>;
}

/// Construct a `RawRepo` from shared state and a `RepoConfig`.
pub fn make_raw_repo(ctx: RepoContext, config: &RepoConfig) -> Box<dyn RawRepo> {
    make_raw_repo_with_depth(ctx, config, 0)
}

/// Construct a `RawRepo` with an explicit recursion depth (used by ProxyRepo members).
pub fn make_raw_repo_with_depth(
    ctx: RepoContext,
    config: &RepoConfig,
    depth: u8,
) -> Box<dyn RawRepo> {
    match config.kind {
        RepoKind::Hosted => Box::new(HostedRepo {
            name: config.name.clone(),
            kv: ctx.kv,
            blobs: ctx.blobs,
            updater: ctx.updater,
            store: config.store.clone(),
            format: config.format().to_string(),
        }),
        RepoKind::Cache {
            ref upstream_url,
            cache_ttl_secs,
            ref upstream_auth,
        } => Box::new(CacheRepo {
            name: config.name.clone(),
            upstream_url: upstream_url.clone(),
            cache_ttl_secs,
            kv: ctx.kv,
            blobs: ctx.blobs,
            store: config.store.clone(),
            http: ctx.http,
            upstream_auth: upstream_auth.clone(),
            inflight: ctx.inflight,
            updater: ctx.updater,
            format: config.format().to_string(),
        }),
        RepoKind::Proxy { ref members, .. } => Box::new(ProxyRepo {
            name: config.name.clone(),
            members: members.clone(),
            kv: ctx.kv,
            blobs: ctx.blobs,
            store: config.store.clone(),
            http: ctx.http,
            inflight: ctx.inflight,
            updater: ctx.updater,
            depth,
        }),
    }
}

/// Resolve the write target for a repository. Returns the hosted repo name to write to.
pub async fn resolve_write_target(_kv: &dyn KvStore, config: &RepoConfig) -> error::Result<String> {
    match config.kind {
        RepoKind::Hosted => Ok(config.name.clone()),
        RepoKind::Proxy {
            ref write_member, ..
        } => {
            if let Some(wm) = write_member {
                return Ok(wm.clone());
            }
            Err(DepotError::NotAllowed(
                "proxy repos require an explicit write_member for PUT uploads; \
                 configure one in the repository settings"
                    .into(),
            ))
        }
        _ => Err(DepotError::NotAllowed(
            "only hosted and proxy repos accept uploads".into(),
        )),
    }
}

/// Resolve the write target for a format-specific repository.
///
/// Like [`resolve_write_target`] but also checks that proxy members match the
/// expected format, and returns the target config alongside the name so callers
/// don't need a second lookup.
pub async fn resolve_format_write_target(
    kv: &dyn KvStore,
    config: &RepoConfig,
    expected_format: ArtifactFormat,
) -> error::Result<(String, RepoConfig)> {
    match config.kind {
        RepoKind::Hosted => Ok((config.name.clone(), config.clone())),
        RepoKind::Proxy {
            ref members,
            ref write_member,
        } => {
            if let Some(wm) = write_member {
                let wm_config = service::get_repo(kv, wm).await?.ok_or_else(|| {
                    DepotError::NotAllowed("write member repo config missing".into())
                })?;
                return Ok((wm.clone(), wm_config));
            }
            for m in members {
                if let Ok(Some(mc)) = service::get_repo(kv, m).await {
                    if mc.repo_type() == RepoType::Hosted && mc.format() == expected_format {
                        return Ok((m.clone(), mc));
                    }
                }
            }
            Err(DepotError::BadRequest(format!(
                "no hosted {} member in proxy repo",
                expected_format
            )))
        }
        _ => Err(DepotError::BadRequest(
            "only hosted and proxy repos accept uploads".into(),
        )),
    }
}

/// Current time as a UTC DateTime.
pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

/// Emit a structured upstream request event for the dashboard/logging pipeline.
///
/// `action` identifies the kind of upstream traffic (e.g. `"upstream.fetch"`,
/// `"upstream.apt.release"`, `"upstream.yum.package"`).
pub fn emit_upstream_event(
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

/// Apply upstream authentication and inject W3C trace context into a request
/// builder. The trace context injection allows distributed tracing across
/// service boundaries.
pub fn apply_upstream_auth(
    builder: reqwest::RequestBuilder,
    auth: Option<&UpstreamAuth>,
) -> reqwest::RequestBuilder {
    let builder = with_trace_context(builder);
    match auth {
        Some(a) => builder.basic_auth(&a.username, a.password.as_deref()),
        None => builder,
    }
}

/// Inject W3C `traceparent`/`tracestate` headers into an outgoing request so
/// that upstream services can join the current distributed trace.
///
pub fn with_trace_context(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    use opentelemetry::propagation::Injector;

    struct HeaderInjector(reqwest::header::HeaderMap);

    impl Injector for HeaderInjector {
        fn set(&mut self, key: &str, value: String) {
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                reqwest::header::HeaderValue::from_str(&value),
            ) {
                self.0.insert(name, val);
            }
        }
    }

    let mut injector = HeaderInjector(reqwest::header::HeaderMap::new());
    let cx = tracing_opentelemetry::OpenTelemetrySpanExt::context(&tracing::Span::current());
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut injector);
    });
    builder.headers(injector.0)
}

/// Commit an ingestion: create blob/artifact KV records, handle dedup, clean up.
///
/// This is the shared logic for all raw-artifact ingest paths. The caller is
/// responsible for writing blob data to the blob store *before* calling this function.
/// Ingestion result containing the new record and optionally the old one (on overwrite).
pub struct IngestionResult {
    pub record: ArtifactRecord,
    pub old_record: Option<ArtifactRecord>,
    /// True when a genuinely new physical blob was stored (no dedup hit). The
    /// caller must credit store stats `+record.size` iff this is true — store
    /// growth is recorded only on real blob creation.
    pub new_blob: bool,
}

#[tracing::instrument(
    level = "debug",
    name = "commit_ingestion",
    skip(ctx),
    fields(path, blob_id, size)
)]
pub async fn commit_ingestion(
    ctx: &IngestionCtx<'_>,
    path: &str,
    blob_id: &str,
    blake3_hash: &str,
    size: u64,
    content_type: &str,
) -> error::Result<IngestionResult> {
    let ingest_start = std::time::Instant::now();
    let now = now_utc();

    let blob_rec = BlobRecord {
        schema_version: CURRENT_RECORD_VERSION,
        blob_id: blob_id.to_string(),
        hash: blake3_hash.to_string(),
        size,
        created_at: now,
        store: ctx.store.to_string(),
    };

    // Check dedup: put_dedup_record returns the existing blob_id if another
    // blob with the same content hash was already registered, or None if we
    // won the race and our record was created.
    let existing = service::put_dedup_record(ctx.kv, ctx.store, &blob_rec).await?;

    let (effective_blob_id, is_dedup) = match existing {
        Some(ref existing_blob_id) => (existing_blob_id.clone(), true),
        None => (blob_id.to_string(), false),
    };

    let record = ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        kind: ArtifactKind::Raw,
        blob_id: Some(effective_blob_id),
        content_hash: Some(blake3_hash.to_string()),
        etag: Some(blake3_hash.to_string()),
        size,
        content_type: content_type.to_string(),
        created_at: now,
        updated_at: now,
        last_accessed_at: now,
        path: String::new(),
        internal: false,
    };

    // On dedup hit: delete our duplicate blob + write artifact (concurrent).
    // On miss: dedup record already written, just write artifact.
    let old_record = if is_dedup {
        let (_, artifact_result) = tokio::join!(
            ctx.blobs.delete(blob_id),
            service::put_artifact(ctx.kv, ctx.repo, path, &record),
        );
        artifact_result?
    } else {
        service::put_artifact(ctx.kv, ctx.repo, path, &record).await?
    };

    let format_owned = ctx.format.to_string();
    let repo_type_owned = ctx.repo_type.to_string();
    histogram!("ingest_duration_seconds", "format" => format_owned.clone(), "repo_type" => repo_type_owned.clone())
        .record(ingest_start.elapsed().as_secs_f64());
    counter!("ingest_total", "dedup" => if is_dedup { "true" } else { "false" }, "format" => format_owned.clone(), "repo_type" => repo_type_owned.clone())
        .increment(1);
    counter!("ingest_bytes_total", "format" => format_owned, "repo_type" => repo_type_owned)
        .increment(size);

    Ok(IngestionResult {
        record,
        old_record,
        new_blob: !is_dedup,
    })
}

/// Ingest an artifact: hash it, store the blob, write the artifact and blob records.
/// Returns the new record and the old record (if overwriting).
pub async fn ingest_artifact(
    ctx: &IngestionCtx<'_>,
    path: &str,
    content_type: &str,
    data: &[u8],
) -> error::Result<IngestionResult> {
    let hash = blake3::hash(data).to_hex().to_string();
    let blob_id = uuid::Uuid::new_v4().to_string();

    // Store blob under UUID key.
    ctx.blobs.put(&blob_id, data).await?;

    commit_ingestion(ctx, path, &blob_id, &hash, data.len() as u64, content_type).await
}

/// Stream an HTTP response body to a blob writer, computing BLAKE3 (and optionally SHA-256)
/// hashes on the fly. Returns `(blake3_hex, optional_sha256_hex, byte_size)`.
///
/// The caller is responsible for calling `finish()` or `abort()` on the writer.
#[tracing::instrument(level = "debug", name = "blob.stream_upstream", skip_all)]
pub async fn fetch_upstream_to_writer(
    resp: reqwest::Response,
    writer: &mut dyn crate::store::blob::BlobWriter,
    compute_sha256: bool,
) -> anyhow::Result<(String, Option<String>, u64)> {
    let mut blake3_hasher = blake3::Hasher::new();
    let mut sha256_hasher = if compute_sha256 {
        Some(sha2::Sha256::new())
    } else {
        None
    };
    let mut size = 0u64;

    let mut stream = resp.bytes_stream();
    while let Some(result) = stream.next().await {
        let chunk = result?;
        if chunk.is_empty() {
            continue;
        }
        blake3_hasher.update(&chunk);
        if let Some(ref mut h) = sha256_hasher {
            sha2::Digest::update(h, &chunk);
        }
        writer.write_chunk(&chunk).await?;
        size += chunk.len() as u64;
    }

    let blake3_hex = blake3_hasher.finalize().to_hex().to_string();
    let sha256_hex = sha256_hasher.map(|h| {
        sha2::Digest::finalize(h)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    });

    Ok((blake3_hex, sha256_hex, size))
}

/// Collect an HTTP response body fully into memory, computing BLAKE3 (and optionally SHA-256)
/// hashes on the fly. Returns `(data, blake3_hex, optional_sha256_hex)`.
///
/// Use for small artifacts (charts, metadata) where in-memory buffering is acceptable.
pub async fn fetch_upstream_to_bytes(
    resp: reqwest::Response,
    compute_sha256: bool,
) -> anyhow::Result<(Vec<u8>, String, Option<String>)> {
    let mut blake3_hasher = blake3::Hasher::new();
    let mut sha256_hasher = if compute_sha256 {
        Some(sha2::Sha256::new())
    } else {
        None
    };
    let mut data = Vec::new();

    let mut stream = resp.bytes_stream();
    while let Some(result) = stream.next().await {
        let chunk = result?;
        if chunk.is_empty() {
            continue;
        }
        blake3_hasher.update(&chunk);
        if let Some(ref mut h) = sha256_hasher {
            sha2::Digest::update(h, &chunk);
        }
        data.extend_from_slice(&chunk);
    }

    let blake3_hex = blake3_hasher.finalize().to_hex().to_string();
    let sha256_hex = sha256_hasher.map(|h| {
        sha2::Digest::finalize(h)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    });

    Ok((data, blake3_hex, sha256_hex))
}
