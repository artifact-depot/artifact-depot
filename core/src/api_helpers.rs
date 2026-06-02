// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Shared HTTP helpers used by every format handler.
//!
//! These used to live in `depot_server::server::api::mod`; they are extracted into
//! `depot-core` so that per-format crates can share them without depending
//! on the monolithic depot crate.

use std::sync::Arc;

use sha2::Digest;

use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Request};
use axum::http::{header, HeaderMap, Method};
use axum::response::{IntoResponse, Response};

use crate::error::{DepotError, Retryability};
use crate::format_state::FormatState;
use crate::repo;
use crate::service;
use crate::store::blob::BlobStore;
use crate::store::kv::{ArtifactFormat, Capability, RepoConfig};

/// Build an `axum::extract::Multipart` from a raw method/headers/body triple.
///
/// Format write dispatchers are called from the unified `/repository/{repo}/...`
/// route, which hands them the raw body and headers instead of an
/// axum-extracted `Multipart`. This helper reconstructs a `Request` and runs
/// `Multipart::from_request` so each format can reuse its existing multipart
/// parsing logic.
pub async fn multipart_from_body(
    method: &Method,
    headers: &HeaderMap,
    body: Body,
) -> Result<Multipart, Response> {
    let mut req = Request::new(body);
    *req.method_mut() = method.clone();
    if let Ok(uri) = "/".parse::<axum::http::Uri>() {
        *req.uri_mut() = uri;
    }
    for (name, value) in headers.iter() {
        req.headers_mut().insert(name, value.clone());
    }
    Multipart::from_request(req, &())
        .await
        .map_err(|e| e.into_response())
}

/// Buffer a request body into bytes, returning a 400 response if the body is
/// malformed or exceeds the artifact-size cap. Used by JSON / small-body
/// format write dispatchers (npm publish, cargo publish, etc.).
pub async fn body_to_bytes(body: Body, limit: usize) -> Result<axum::body::Bytes, Response> {
    axum::body::to_bytes(body, limit)
        .await
        .map_err(|e| DepotError::BadRequest(format!("failed to read body: {e}")).into_response())
}

/// Generate a `fn $name(config, state, blobs) -> $store_type` factory for format stores
/// that share the standard `{ repo, kv, blobs, store, updater }` layout.
#[macro_export]
macro_rules! store_from_config_fn {
    ($name:ident, $store_type:ident) => {
        fn $name<'a>(
            config: &'a $crate::store::kv::RepoConfig,
            state: &'a $crate::format_state::FormatState,
            blobs: &'a dyn $crate::store::blob::BlobStore,
        ) -> $store_type<'a> {
            $store_type {
                repo: &config.name,
                kv: state.kv.as_ref(),
                blobs,
                store: &config.store,
                updater: &state.updater,
            }
        }
    };
}

/// Derive the external scheme from proxy headers, defaulting to `"https"`.
pub fn request_scheme(headers: &HeaderMap) -> &str {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https")
}

/// Build the external-facing origin (`scheme://host`) from request headers.
///
/// Combines [`request_scheme`] and [`external_host`] so callers that need a
/// fully-qualified URL prefix (e.g. npm `dist.tarball`) get correct results
/// behind a reverse proxy. Honors `X-Forwarded-Proto`/`X-Forwarded-Port` and
/// the `Host` header, falling back to `uri_authority` then `localhost`.
pub fn external_origin(headers: &HeaderMap, uri_authority: Option<&str>) -> String {
    let scheme = request_scheme(headers);
    let host = external_host(headers, scheme, uri_authority);
    format!("{scheme}://{host}")
}

/// Determine the external-facing host authority from request headers.
///
/// When behind a reverse proxy (e.g., AWS ALB terminating HTTPS on 443 and
/// forwarding HTTP on 80), the `Host` header may contain the internal port
/// (`:80`) while `X-Forwarded-Proto` indicates `https`. Naively combining
/// them produces invalid URLs like `https://host:80/path`.
///
/// This function:
/// 1. Extracts the hostname from the `Host` header (stripping any port),
///    falling back to `uri_authority`, then `"localhost"`.
/// 2. Uses `X-Forwarded-Port` if present; otherwise discards the Host port
///    when proxy headers are detected (the Host port is an internal artifact).
/// 3. Omits the port when it is the default for the scheme (80/http, 443/https).
pub fn external_host(headers: &HeaderMap, scheme: &str, uri_authority: Option<&str>) -> String {
    let raw_host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or(uri_authority)
        .unwrap_or("localhost");

    let (hostname, host_port) = split_host_port(raw_host);

    let has_proxy = headers.get("x-forwarded-proto").is_some();

    let fwd_port: Option<u16> = headers
        .get("x-forwarded-port")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok());

    let effective_port: Option<u16> = if fwd_port.is_some() {
        fwd_port
    } else if has_proxy {
        // Proxy present but no X-Forwarded-Port: the Host port is internal — discard it.
        None
    } else {
        host_port
    };

    let is_default = matches!(
        (scheme, effective_port),
        ("http", Some(80)) | ("https", Some(443))
    );

    match effective_port {
        Some(port) if !is_default => format!("{hostname}:{port}"),
        _ => hostname.to_string(),
    }
}

/// Split a host value into hostname and optional port.
///
/// Handles IPv6 bracket notation: `[::1]:8080` → (`[::1]`, Some(8080)).
fn split_host_port(host: &str) -> (&str, Option<u16>) {
    if let Some(bracket_end) = host.find(']') {
        // IPv6: [::1] or [::1]:port
        let (addr, rest) = host.split_at(bracket_end + 1);
        let port = rest.strip_prefix(':').and_then(|p| p.parse().ok());
        (addr, port)
    } else if let Some(colon) = host.rfind(':') {
        let (name, port_with_colon) = host.split_at(colon);
        let port: Option<u16> = port_with_colon
            .strip_prefix(':')
            .and_then(|p| p.parse().ok());
        if port.is_some() {
            (name, port)
        } else {
            (host, None)
        }
    } else {
        (host, None)
    }
}

/// Propagate staleness to parent proxy repos (fire-and-forget).
///
/// Call this from API handlers after a repo's metadata changes (upload,
/// cache refresh, etc.). Errors are silently ignored.
pub async fn propagate_staleness_to_parents(
    state: &FormatState,
    repo_name: &str,
    format: ArtifactFormat,
    stale_path: &str,
) {
    let _ = service::propagate_staleness(state.kv.as_ref(), repo_name, format, stale_path).await;
}

/// Validate that a repo exists and matches the expected artifact format.
pub async fn validate_format_repo(
    state: &FormatState,
    repo_name: &str,
    expected: ArtifactFormat,
) -> Result<RepoConfig, DepotError> {
    let name = repo_name.to_string();
    let config = service::get_repo(state.kv.as_ref(), &name)
        .await?
        .ok_or_else(|| DepotError::NotFound(format!("repository '{}' not found", repo_name)))?;

    if config.format() != expected {
        return Err(DepotError::BadRequest(format!(
            "repository is not a {} repository",
            expected
        )));
    }

    Ok(config)
}

/// Resolve the blob store for a repo by name, returning both the store and the repo config.
pub async fn resolve_repo_blob_store(
    state: &FormatState,
    repo_name: &str,
) -> Result<(Arc<dyn BlobStore>, RepoConfig), DepotError> {
    let rn = repo_name.to_string();
    let config = service::get_repo(state.kv.as_ref(), &rn)
        .await?
        .ok_or_else(|| DepotError::NotFound(format!("repository '{}' not found", repo_name)))?;
    let blobs = state.blob_store(&config.store).await?;
    Ok((blobs, config))
}

/// Common preamble for upload handlers: validate format, check write permission,
/// resolve write target, and obtain the blob store.
pub async fn upload_preamble(
    state: &FormatState,
    user: &str,
    repo_name: &str,
    format: ArtifactFormat,
) -> Result<(String, RepoConfig, Arc<dyn BlobStore>), DepotError> {
    let config = validate_format_repo(state, repo_name, format).await?;

    state
        .auth
        .check_permission(user, repo_name, Capability::Write)
        .await?;

    let (target_repo, target_config) =
        repo::resolve_format_write_target(state.kv.as_ref(), &config, format).await?;

    let blobs = state.blob_store(&target_config.store).await?;

    Ok((target_repo, target_config, blobs))
}

/// Result of streaming a multipart field to a blob store.
pub struct StreamedBlob {
    pub blob_id: String,
    pub blake3: String,
    pub sha256: String,
    pub size: u64,
    pub filename: Option<String>,
}

/// Stream a single multipart field to a blob writer, computing BLAKE3 and SHA-256
/// hashes on the fly. Captures the field's filename if present.
///
/// On success, the blob is finalized in the store. On error, the blob is aborted
/// and a storage error is returned.
#[tracing::instrument(level = "debug", name = "blob.stream_write", skip_all)]
pub async fn stream_multipart_field_to_blob(
    field: &mut axum::extract::multipart::Field<'_>,
    blobs: &dyn BlobStore,
) -> Result<StreamedBlob, DepotError> {
    let filename = field.file_name().map(|f| f.to_string());
    let blob_id = uuid::Uuid::new_v4().to_string();
    let mut writer = blobs.create_writer(&blob_id, None).await?;
    let mut blake3_hasher = blake3::Hasher::new();
    let mut sha256_hasher = sha2::Sha256::new();
    let mut size = 0u64;

    let stream_err: Option<String> = loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if chunk.is_empty() {
                    continue;
                }
                blake3_hasher.update(&chunk);
                Digest::update(&mut sha256_hasher, &chunk);
                if let Err(e) = writer.write_chunk(&chunk).await {
                    break Some(e.to_string());
                }
                size += chunk.len() as u64;
            }
            Ok(None) => break None,
            Err(e) => break Some(e.to_string()),
        }
    };

    if let Some(err) = stream_err {
        let _ = writer.abort().await;
        return Err(DepotError::Storage(
            Box::new(std::io::Error::other(err)),
            Retryability::Transient,
        ));
    }

    writer
        .finish()
        .await
        .map_err(|e| DepotError::Storage(Box::new(e), Retryability::Transient))?;

    let blake3 = blake3_hasher.finalize().to_hex().to_string();
    let sha256: String = Digest::finalize(sha256_hasher)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    Ok(StreamedBlob {
        blob_id,
        blake3,
        sha256,
        size,
        filename,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                axum::http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn alb_https_with_forwarded_port() {
        let h = headers(&[
            ("host", "example.com:80"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-port", "443"),
        ]);
        assert_eq!(external_host(&h, "https", None), "example.com");
    }

    #[test]
    fn alb_https_no_forwarded_port() {
        let h = headers(&[("host", "example.com:80"), ("x-forwarded-proto", "https")]);
        assert_eq!(external_host(&h, "https", None), "example.com");
    }

    #[test]
    fn direct_access_no_port() {
        let h = headers(&[("host", "example.com")]);
        assert_eq!(external_host(&h, "https", None), "example.com");
    }

    #[test]
    fn direct_access_custom_port() {
        let h = headers(&[("host", "example.com:8080")]);
        assert_eq!(external_host(&h, "https", None), "example.com:8080");
    }

    #[test]
    fn non_standard_forwarded_port() {
        let h = headers(&[
            ("host", "example.com:80"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-port", "8443"),
        ]);
        assert_eq!(external_host(&h, "https", None), "example.com:8443");
    }

    #[test]
    fn http_default_port_stripped() {
        let h = headers(&[("host", "example.com:80")]);
        assert_eq!(external_host(&h, "http", None), "example.com");
    }

    #[test]
    fn https_default_port_stripped() {
        let h = headers(&[("host", "example.com:443")]);
        assert_eq!(external_host(&h, "https", None), "example.com");
    }

    #[test]
    fn ipv6_with_port() {
        let h = headers(&[("host", "[::1]:8080")]);
        assert_eq!(external_host(&h, "https", None), "[::1]:8080");
    }

    #[test]
    fn ipv6_without_port() {
        let h = headers(&[("host", "[::1]")]);
        assert_eq!(external_host(&h, "https", None), "[::1]");
    }

    #[test]
    fn fallback_to_uri_authority() {
        let h = HeaderMap::new();
        assert_eq!(
            external_host(&h, "https", Some("fallback.com:9090")),
            "fallback.com:9090"
        );
    }

    #[test]
    fn fallback_to_localhost() {
        let h = HeaderMap::new();
        assert_eq!(external_host(&h, "https", None), "localhost");
    }

    #[test]
    fn forwarded_port_overrides_host_port() {
        let h = headers(&[
            ("host", "example.com:3000"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-port", "443"),
        ]);
        assert_eq!(external_host(&h, "https", None), "example.com");
    }

    #[test]
    fn request_scheme_from_header() {
        let h = headers(&[("x-forwarded-proto", "http")]);
        assert_eq!(request_scheme(&h), "http");
    }

    #[test]
    fn request_scheme_default() {
        let h = HeaderMap::new();
        assert_eq!(request_scheme(&h), "https");
    }
}
