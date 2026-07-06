// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Dynamic cluster-wide settings backed by the shared KV store.
//!
//! Settings are cached in-memory via ArcSwap and refreshed every 30 seconds
//! from KV, so changes made via the API propagate to all instances within
//! one refresh cycle.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

#[cfg(feature = "ldap")]
use crate::server::config::LdapConfig;
use crate::server::config::{CorsConfig, RateLimitConfig};
use depot_core::service;
use depot_core::store::kv::KvStore;

/// Operational settings shared across all cluster instances.
///
/// Stored as a single msgpack-encoded blob under `TAG_META` key `"settings"`.
///
/// `#[serde(default)]` at the container level lets the TOML
/// `[initialization.settings]` section omit any field — missing values fall
/// back to [`Settings::default`]. KV deserialization is unaffected because
/// stored records always contain every field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(default)]
pub struct Settings {
    /// Log all HTTP requests (method, path, status, duration).
    pub access_log: bool,
    /// Maximum upload size in bytes for artifact content.
    pub max_artifact_bytes: u64,
    /// **Deprecated**: cleanup now runs as part of garbage collection.
    /// Retained for backward-compatible deserialization of existing KV
    /// records; the value is ignored at runtime.
    pub cleanup_interval_secs: Option<u64>,
    /// Default Docker repository name for the main listener.
    pub default_docker_repo: Option<String>,
    /// CORS configuration. `None` means no CORS headers.
    pub cors: Option<CorsConfig>,
    /// Rate limiting configuration. `None` disables rate limiting.
    pub rate_limit: Option<RateLimitConfig>,
    /// GC interval in seconds. `None` disables GC. Default: 86400 (24h).
    #[serde(default = "default_gc_interval_secs")]
    pub gc_interval_secs: Option<u64>,
    /// Maximum number of chunks allowed per Docker chunked upload session.
    #[serde(default = "default_max_docker_chunk_count")]
    pub max_docker_chunk_count: u32,
    /// JWT token expiry in seconds. Default: 86400 (24h).
    #[serde(default = "default_jwt_expiry_secs")]
    pub jwt_expiry_secs: u64,
    /// JWT secret rotation interval in seconds. Default: 604800 (7d).
    #[serde(default = "default_jwt_rotation_interval_secs")]
    pub jwt_rotation_interval_secs: u64,
    /// Maximum number of orphan candidates tracked across GC passes.
    /// `None` uses the default of 1,000,000.
    #[serde(default = "default_gc_max_orphan_candidates")]
    pub gc_max_orphan_candidates: Option<usize>,
    /// Minimum seconds between GC runs (manual or automatic).
    /// Prevents rapid consecutive GC passes from prematurely deleting blobs
    /// still in the two-pass grace period. Default: 3600 (1h).
    #[serde(default = "default_gc_min_interval_secs")]
    pub gc_min_interval_secs: Option<u64>,

    /// Optional fixed start time for automatic GC passes, as a UTC time of
    /// day in `HH:MM` form (e.g. `"07:00"`). When set, passes anchor to this
    /// hour daily instead of running `gc_interval_secs` after the previous
    /// pass, so the schedule does not drift with pass duration or manual
    /// runs. `gc_min_interval_secs` still applies. When unset (the default),
    /// interval-based scheduling is used.
    #[serde(default)]
    pub gc_start_time: Option<String>,

    /// How often the state scanner polls KV for cross-instance changes.
    /// Default: 1000 (1 second).
    #[serde(default = "default_state_scan_interval_ms")]
    pub state_scan_interval_ms: u64,

    /// Retention for completed / failed / cancelled tasks before they are
    /// auto-reaped from KV. Default: 86400 (24 hours).
    #[serde(default = "default_task_retention_secs")]
    pub task_retention_secs: u64,

    /// Logging endpoint configuration (overrides TOML on next restart).
    #[serde(default)]
    pub logging: Option<LoggingSettingsConfig>,

    /// OTLP endpoint for trace export (overrides TOML tracing.otlp_endpoint on restart).
    #[serde(default)]
    pub tracing_endpoint: Option<String>,

    /// External base URL (`scheme://host[:port]`) used to build absolute URLs
    /// in artifact metadata (npm `dist.tarball`, Docker `WWW-Authenticate`
    /// realm, Nexus-compat asset URLs, …).
    ///
    /// When set, it is a hard override applied regardless of request headers —
    /// the equivalent of Nexus's "Base URL" capability. Use it to pin a
    /// canonical public URL, e.g. to force `http://` while a TLS certificate is
    /// being fixed. When unset (the default), the origin is derived per-request
    /// from `X-Forwarded-Proto`/`Host`, falling back to the scheme of the
    /// listener that accepted the request.
    #[serde(default)]
    pub base_url: Option<String>,
}

/// Logging endpoint configuration stored in dynamic settings.
/// Mirrors `LoggingConfig` in the static TOML config. Changes take effect
/// after restart. Unknown keys in older KV records (capacity, file_path,
/// s3, splunk_hec) are silently ignored so upgrades don't break.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct LoggingSettingsConfig {
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
}

fn default_gc_interval_secs() -> Option<u64> {
    Some(86400)
}

fn default_max_docker_chunk_count() -> u32 {
    10_000
}

fn default_jwt_expiry_secs() -> u64 {
    86400
}

fn default_jwt_rotation_interval_secs() -> u64 {
    7 * 24 * 3600
}

fn default_gc_min_interval_secs() -> Option<u64> {
    Some(3600)
}

fn default_gc_max_orphan_candidates() -> Option<usize> {
    Some(1_000_000)
}

fn default_state_scan_interval_ms() -> u64 {
    1000
}

fn default_task_retention_secs() -> u64 {
    86_400
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            access_log: true,
            max_artifact_bytes: 32 * 1024 * 1024 * 1024, // 32 GiB
            cleanup_interval_secs: None,
            default_docker_repo: None,
            cors: None,
            rate_limit: None,
            gc_interval_secs: default_gc_interval_secs(),
            max_docker_chunk_count: 10_000,
            jwt_expiry_secs: default_jwt_expiry_secs(),
            jwt_rotation_interval_secs: default_jwt_rotation_interval_secs(),
            gc_max_orphan_candidates: default_gc_max_orphan_candidates(),
            gc_min_interval_secs: default_gc_min_interval_secs(),
            gc_start_time: None,
            state_scan_interval_ms: default_state_scan_interval_ms(),
            task_retention_secs: default_task_retention_secs(),
            logging: None,
            tracing_endpoint: None,
            base_url: None,
        }
    }
}

impl Settings {
    /// Validate settings values, returning all errors at once.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if let Some(ref rl) = self.rate_limit {
            if rl.requests_per_second == 0 {
                errors.push("rate_limit.requests_per_second must be > 0".to_string());
            }
            if rl.burst_size == 0 {
                errors.push("rate_limit.burst_size must be > 0".to_string());
            }
        }

        if let Some(ref cors) = self.cors {
            for method in &cors.allowed_methods {
                if method.parse::<axum::http::Method>().is_err() {
                    errors.push(format!(
                        "cors.allowed_methods: {:?} is not a valid HTTP method",
                        method
                    ));
                }
            }
        }

        if self.jwt_expiry_secs < 60 {
            errors.push("jwt_expiry_secs must be >= 60".to_string());
        }
        if self.jwt_rotation_interval_secs < 3600 {
            errors.push("jwt_rotation_interval_secs must be >= 3600".to_string());
        }
        if self.jwt_rotation_interval_secs < self.jwt_expiry_secs {
            errors.push("jwt_rotation_interval_secs should be >= jwt_expiry_secs".to_string());
        }

        if let Some(min) = self.gc_min_interval_secs {
            if min < 60 {
                errors.push("gc_min_interval_secs must be >= 60".to_string());
            }
        }
        if let (Some(interval), Some(min)) = (self.gc_interval_secs, self.gc_min_interval_secs) {
            if interval < min {
                errors.push("gc_interval_secs must be >= gc_min_interval_secs".to_string());
            }
        }
        if let Some(ref t) = self.gc_start_time {
            if chrono::NaiveTime::parse_from_str(t, "%H:%M").is_err() {
                errors.push(format!(
                    "gc_start_time must be a UTC time of day in HH:MM form (e.g. \"07:00\"), got {t:?}"
                ));
            }
        }

        if let Some(ref base) = self.base_url {
            let trimmed = base.trim();
            let valid_scheme = trimmed.starts_with("http://") || trimmed.starts_with("https://");
            let host = trimmed
                .split_once("://")
                .map(|(_, rest)| rest.trim_end_matches('/'))
                .unwrap_or("");
            if !valid_scheme || host.is_empty() || host.contains('/') {
                errors.push(
                    "base_url must be an absolute origin like https://depot.example.com \
                     (scheme://host[:port], no path)"
                        .to_string(),
                );
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Thread-safe handle to the current settings, readable without locking.
pub struct SettingsHandle(ArcSwap<Settings>);

impl SettingsHandle {
    pub fn new(initial: Settings) -> Self {
        Self(ArcSwap::from_pointee(initial))
    }

    /// Read the current settings snapshot.
    pub fn load(&self) -> arc_swap::Guard<Arc<Settings>> {
        self.0.load()
    }

    /// Replace the current settings.
    pub fn store(&self, new: Arc<Settings>) {
        self.0.store(new);
    }
}

/// Load settings from KV, returning `None` if no settings key exists.
pub async fn load_settings_from_kv(kv: &dyn KvStore) -> anyhow::Result<Option<Settings>> {
    let (table, pk, sk) = depot_core::store::keys::meta_key("settings");
    Ok(service::typed_get(kv, table, pk, sk).await?)
}

/// Write settings to KV.
pub async fn save_settings_to_kv(kv: &dyn KvStore, settings: &Settings) -> anyhow::Result<()> {
    let (table, pk, sk) = depot_core::store::keys::meta_key("settings");
    service::typed_put(kv, table, pk, sk, settings).await?;
    Ok(())
}

/// Load settings from KV, or initialize with defaults on first boot.
/// Re-saves on every startup to migrate the serialization format (e.g.
/// positional msgpack → named msgpack) when struct fields are added.
pub async fn ensure_settings(kv: &dyn KvStore) -> anyhow::Result<Settings> {
    match load_settings_from_kv(kv).await? {
        Some(existing) => {
            // Re-save to upgrade the on-disk format (e.g. positional → named
            // msgpack) so newly added fields are persisted correctly.
            save_settings_to_kv(kv, &existing).await?;
            Ok(existing)
        }
        None => {
            let settings = Settings::default();
            save_settings_to_kv(kv, &settings).await?;
            tracing::info!("initialized default cluster settings in KV store");
            Ok(settings)
        }
    }
}

// =============================================================================
// LDAP config persistence
// =============================================================================

/// Load LDAP config from KV, returning `None` if not configured.
#[cfg(feature = "ldap")]
pub async fn load_ldap_config(kv: &dyn KvStore) -> anyhow::Result<Option<LdapConfig>> {
    let (table, pk, sk) = depot_core::store::keys::meta_key("ldap_config");
    Ok(service::typed_get(kv, table, pk, sk).await?)
}

/// Save LDAP config to KV.
#[cfg(feature = "ldap")]
pub async fn save_ldap_config(kv: &dyn KvStore, config: &LdapConfig) -> anyhow::Result<()> {
    let (table, pk, sk) = depot_core::store::keys::meta_key("ldap_config");
    service::typed_put(kv, table, pk, sk, config).await?;
    Ok(())
}

/// Delete LDAP config from KV (switch back to builtin auth).
#[cfg(feature = "ldap")]
pub async fn delete_ldap_config(kv: &dyn KvStore) -> anyhow::Result<()> {
    service::delete_meta(kv, "ldap_config").await?;
    Ok(())
}

/// Validate LDAP config fields, returning all errors at once.
#[cfg(feature = "ldap")]
pub fn validate_ldap_config(ldap: &LdapConfig) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    if ldap.url.is_empty() {
        errors.push("url must not be empty".to_string());
    }
    if ldap.bind_dn.is_empty() {
        errors.push("bind_dn must not be empty".to_string());
    }
    if ldap.user_base_dn.is_empty() {
        errors.push("user_base_dn must not be empty".to_string());
    }
    if ldap.group_base_dn.is_empty() {
        errors.push("group_base_dn must not be empty".to_string());
    }
    if !ldap.user_filter.contains("{username}") {
        errors.push("user_filter must contain {username} placeholder".to_string());
    }
    if !ldap.group_filter.contains("{dn}") {
        errors.push("group_filter must contain {dn} placeholder".to_string());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Background task that refreshes settings from KV every 30 seconds.
pub async fn run_settings_refresh(
    kv: Arc<dyn KvStore>,
    handle: Arc<SettingsHandle>,
    rate_limiter: Arc<crate::server::infra::rate_limit::DynamicRateLimiter>,
    jwt_secret: Arc<ArcSwap<crate::server::auth::JwtSecretState>>,
    cancel: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.tick().await; // Skip the immediate first tick.

    loop {
        tokio::select! {
            _ = interval.tick() => {
                async {
                    match load_settings_from_kv(&*kv).await {
                        Ok(Some(s)) => {
                            rate_limiter.update(s.rate_limit.as_ref());
                            handle.store(Arc::new(s));
                        }
                        Ok(None) => {} // No settings in KV yet, keep current.
                        Err(e) => tracing::warn!(error = %e, "settings refresh failed"),
                    }

                    // Reload JWT secret state so multi-instance deployments converge
                    // within one refresh cycle after a rotation or revoke-all.
                    match depot_core::service::get_meta(kv.as_ref(), "jwt_secret").await {
                        Ok(Some(bytes)) => {
                            if let Ok(state) = rmp_serde::from_slice::<crate::server::auth::JwtSecretState>(&bytes) {
                                jwt_secret.store(Arc::new(state));
                            }
                        }
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, "JWT secret refresh failed"),
                    }
                }
                .instrument(tracing::debug_span!("settings_refresh"))
                .await;
            }
            _ = cancel.cancelled() => {
                tracing::info!("settings refresh worker shutting down");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_settings() {
        let s = Settings::default();
        assert!(s.access_log);
        assert_eq!(s.max_artifact_bytes, 32 * 1024 * 1024 * 1024);
        assert!(s.cleanup_interval_secs.is_none());
        assert!(s.default_docker_repo.is_none());
        assert!(s.cors.is_none());
        assert!(s.rate_limit.is_none());
    }

    #[test]
    fn test_validate_valid() {
        Settings::default().validate().unwrap();
    }

    #[test]
    fn test_validate_bad_rate_limit() {
        let mut s = Settings::default();
        s.rate_limit = Some(RateLimitConfig {
            requests_per_second: 0,
            burst_size: 0,
        });
        let errs = s.validate().unwrap_err();
        assert_eq!(errs.len(), 2);
    }

    #[test]
    fn test_validate_bad_cors_method() {
        let mut s = Settings::default();
        s.cors = Some(CorsConfig {
            allowed_origins: vec![],
            allowed_methods: vec!["NOT A METHOD".to_string()],
            allowed_headers: vec![],
            max_age_secs: 3600,
        });
        let errs = s.validate().unwrap_err();
        assert!(errs[0].contains("NOT A METHOD"));
    }

    #[test]
    fn test_validate_base_url_ok() {
        for ok in [
            "https://depot.example.com",
            "http://depot-dev.lab.example.com",
            "https://depot.example.com:8443",
            "https://depot.example.com/", // trailing slash tolerated
        ] {
            let mut s = Settings::default();
            s.base_url = Some(ok.to_string());
            s.validate()
                .unwrap_or_else(|e| panic!("{ok:?} should be valid, got {e:?}"));
        }
    }

    #[test]
    fn test_validate_base_url_bad() {
        for bad in [
            "depot.example.com",                    // no scheme
            "ftp://depot.example.com",              // wrong scheme
            "https://",                             // empty host
            "https://depot.example.com/repository", // path not allowed
        ] {
            let mut s = Settings::default();
            s.base_url = Some(bad.to_string());
            let errs = s.validate().unwrap_err();
            assert!(
                errs.iter().any(|e| e.contains("base_url")),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn test_validate_gc_start_time_ok() {
        for ok in ["00:00", "07:00", "7:05", "23:59"] {
            let mut s = Settings::default();
            s.gc_start_time = Some(ok.to_string());
            s.validate()
                .unwrap_or_else(|e| panic!("{ok:?} should be valid, got {e:?}"));
        }
    }

    #[test]
    fn test_validate_gc_start_time_bad() {
        for bad in ["24:00", "07:60", "0700", "7pm", "07:00:00", ""] {
            let mut s = Settings::default();
            s.gc_start_time = Some(bad.to_string());
            let errs = s.validate().unwrap_err();
            assert!(
                errs.iter().any(|e| e.contains("gc_start_time")),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn test_settings_without_base_url_deserializes() {
        // A settings record persisted by an older build (no `base_url` field)
        // must load with `base_url = None` thanks to the container-level
        // `#[serde(default)]`. Encode an empty named map to simulate that.
        let empty = std::collections::BTreeMap::<String, u8>::new();
        let bytes = rmp_serde::to_vec_named(&empty).unwrap();
        let s: Settings = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(s.base_url, None);
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn test_settings_handle_load_store() {
        let handle = SettingsHandle::new(Settings::default());
        assert!(handle.load().access_log);

        let mut new_settings = Settings::default();
        new_settings.access_log = false;
        handle.store(Arc::new(new_settings));
        assert!(!handle.load().access_log);
    }

    #[test]
    fn test_settings_roundtrip_msgpack() {
        let s = Settings::default();
        let bytes = rmp_serde::to_vec(&s).unwrap();
        let s2: Settings = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(s, s2);
    }

    #[test]
    fn test_legacy_logging_settings_deserialize() {
        // Replicates a persisted settings blob from an older build that carried
        // capacity/file_path/s3/splunk_hec. Real records are encoded with
        // rmp_serde::to_vec_named via service::typed_put, so the forward-compat
        // path relies on msgpack named-map deserialization ignoring extras.
        #[derive(Serialize)]
        struct LegacyS3 {
            bucket: String,
            prefix: Option<String>,
            region: Option<String>,
            endpoint: Option<String>,
        }
        #[derive(Serialize)]
        struct LegacySplunk {
            url: String,
            token: String,
            source: String,
            sourcetype: String,
            index: Option<String>,
            tls_skip_verify: bool,
        }
        #[derive(Serialize)]
        struct LegacyLogging {
            capacity: Option<usize>,
            file_path: Option<String>,
            otlp_endpoint: Option<String>,
            s3: Option<LegacyS3>,
            splunk_hec: Option<LegacySplunk>,
        }

        let legacy = LegacyLogging {
            capacity: Some(500),
            file_path: Some("/var/log/depot.log".to_string()),
            otlp_endpoint: Some("http://loki:3100/otlp".to_string()),
            s3: Some(LegacyS3 {
                bucket: "my-bucket".to_string(),
                prefix: Some("logs/".to_string()),
                region: Some("us-east-1".to_string()),
                endpoint: None,
            }),
            splunk_hec: Some(LegacySplunk {
                url: "https://splunk.example.com:8088".to_string(),
                token: "xxx".to_string(),
                source: "depot".to_string(),
                sourcetype: "depot:log".to_string(),
                index: None,
                tls_skip_verify: false,
            }),
        };
        let bytes = rmp_serde::to_vec_named(&legacy).unwrap();
        let cfg: LoggingSettingsConfig = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(cfg.otlp_endpoint.as_deref(), Some("http://loki:3100/otlp"));
    }
}
