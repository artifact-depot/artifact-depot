// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use axum::extract::FromRef;

#[cfg(not(feature = "ldap"))]
use crate::server::auth::backend::BuiltinAuthBackend;
use crate::server::auth::backend::{AuthBackend, CachedAuthBackend};
use crate::server::auth::JwtSecretState;
use crate::server::config::settings::SettingsHandle;
use crate::server::config::{Config, KvStoreConfig};
use crate::server::infra::event_bus::{EventBus, MaterializedModel, ModelHandle};
use crate::server::infra::rate_limit::DynamicRateLimiter;
use crate::server::infra::store_registry::instantiate_store;
use crate::server::infra::task::TaskManager;
use crate::server::store::encrypted::{self, EncryptedKvStore};
use crate::server::store::{InstrumentedKvStore, KvStore};
use depot_core::error::Result;
use depot_core::inflight::InflightMap;
use depot_core::store::kv::Capability;
use depot_core::store_registry::StoreRegistry;
use depot_core::update::UpdateSender;
use depot_kv_redb::ShardedRedbKvStore;

/// Core repository services: KV store, blob stores, HTTP client, and helpers
/// needed by artifact/format handlers on the hot path.
#[derive(Clone)]
pub struct RepoServices {
    pub kv: Arc<dyn KvStore>,
    pub stores: Arc<StoreRegistry>,
    pub http: reqwest::Client,
    pub inflight: InflightMap,
    pub updater: UpdateSender,
}

impl RepoServices {
    /// Resolve the blob store for a given store name.
    pub async fn blob_store(
        &self,
        store_name: &str,
    ) -> depot_core::error::Result<Arc<dyn depot_core::store::blob::BlobStore>> {
        crate::server::infra::store_registry::resolve_blob_store(&self.stores, store_name).await
    }

    /// Build a `RepoContext` with a resolved blob store.
    pub fn repo_context(
        &self,
        blobs: Arc<dyn depot_core::store::blob::BlobStore>,
    ) -> crate::server::repo::RepoContext {
        crate::server::repo::RepoContext {
            kv: Arc::clone(&self.kv),
            blobs,
            http: self.http.clone(),
            inflight: self.inflight.clone(),
            updater: self.updater.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// FormatState — the subset of AppState available to format crate handlers.
// ---------------------------------------------------------------------------

use depot_core::error::{DepotError, Retryability};
use depot_core::format_state::{FormatSettings, FormatState, PermissionChecker};
use depot_core::store::kv::{RoleRecord, UserRecord};

/// Bridges the full auth stack (AuthBackend + JWT secret + settings) into the
/// thin [`PermissionChecker`] trait that format crates use.
struct AuthPermissionChecker {
    backend: Arc<dyn AuthBackend>,
    jwt_secret: Arc<arc_swap::ArcSwap<JwtSecretState>>,
    settings: Arc<SettingsHandle>,
}

/// Bridges the depot `SettingsHandle` to the thin [`FormatSettings`] trait.
struct SettingsAdapter {
    settings: Arc<SettingsHandle>,
}

impl FormatSettings for SettingsAdapter {
    fn default_docker_repo(&self) -> Option<String> {
        self.settings.load().default_docker_repo.clone()
    }

    fn max_docker_chunk_count(&self) -> u32 {
        self.settings.load().max_docker_chunk_count
    }

    fn base_url(&self) -> Option<String> {
        self.settings.load().base_url.clone()
    }
}

#[async_trait::async_trait]
impl PermissionChecker for AuthPermissionChecker {
    async fn check_permission(&self, user: &str, repo: &str, required: Capability) -> Result<()> {
        self.backend.check_permission(user, repo, required).await
    }

    async fn authenticate(&self, username: &str, password: &str) -> Result<Option<UserRecord>> {
        self.backend
            .authenticate(username, password)
            .await
            .map_err(|e| DepotError::Storage(e.into(), Retryability::Transient))
    }

    async fn resolve_user_roles(
        &self,
        username: &str,
    ) -> Result<Option<(UserRecord, Vec<RoleRecord>)>> {
        self.backend.resolve_user_roles(username).await
    }

    async fn create_user_token(&self, user: &UserRecord) -> Result<String> {
        let jwt = self.jwt_secret.load();
        let expiry_secs = self.settings.load().jwt_expiry_secs;
        crate::server::auth::create_user_token(
            &user.username,
            &jwt.current,
            &user.token_secret,
            expiry_secs,
            user.roles.clone(),
        )
        .map_err(|e| DepotError::Storage(Box::new(e), Retryability::Transient))
    }
}

impl AppState {
    /// Extract a `FormatState` for calling format-handler functions from admin code.
    pub fn format_state(&self) -> FormatState {
        FormatState::from_ref(self)
    }

    /// Clone this state with a different listener [`default_scheme`]. Used by
    /// the server to give the plain-HTTP and TLS listeners distinct fallback
    /// schemes from a single shared `AppState`. Cheap — every heavy field is
    /// behind an `Arc`.
    ///
    /// [`default_scheme`]: AppState::default_scheme
    pub fn with_scheme(&self, default_scheme: &'static str) -> Self {
        let mut s = self.clone();
        s.default_scheme = default_scheme;
        s
    }
}

impl FromRef<AppState> for FormatState {
    fn from_ref(state: &AppState) -> Self {
        Self {
            kv: Arc::clone(&state.repo.kv),
            stores: Arc::clone(&state.repo.stores),
            http: state.repo.http.clone(),
            inflight: state.repo.inflight.clone(),
            updater: state.repo.updater.clone(),
            auth: Arc::new(AuthPermissionChecker {
                backend: Arc::clone(&state.auth.backend),
                jwt_secret: Arc::clone(&state.auth.jwt_secret),
                settings: Arc::clone(&state.settings),
            }),
            settings: Arc::new(SettingsAdapter {
                settings: Arc::clone(&state.settings),
            }),
            default_scheme: state.default_scheme,
        }
    }
}

// ---------------------------------------------------------------------------

/// Authentication and authorization infrastructure.
#[derive(Clone)]
pub struct AuthServices {
    pub backend: Arc<dyn AuthBackend>,
    pub jwt_secret: Arc<arc_swap::ArcSwap<crate::server::auth::JwtSecretState>>,
    #[cfg(feature = "ldap")]
    pub ldap_config: Arc<crate::server::auth::ldap::LdapConfigHandle>,
}

/// Background task and maintenance infrastructure.
#[derive(Clone)]
pub struct BackgroundServices {
    pub tasks: Arc<TaskManager>,
    pub instance_id: String,
    /// Shared GC state so orphan candidates persist across manual and
    /// scheduled GC passes.
    pub gc_state: Arc<tokio::sync::Mutex<crate::server::worker::blob_reaper::GcState>>,
    pub event_bus: Arc<EventBus>,
    pub model: Arc<ModelHandle>,
    /// Wakes the state scanner immediately on user-initiated KV writes so
    /// the UI doesn't have to wait for the next periodic scan tick.
    pub scan_trigger: Arc<tokio::sync::Notify>,
}

#[derive(Clone)]
pub struct AppState {
    pub repo: RepoServices,
    pub auth: AuthServices,
    pub bg: BackgroundServices,
    pub settings: Arc<SettingsHandle>,
    pub rate_limiter: Arc<DynamicRateLimiter>,
    /// Scheme of the listener this state serves (`"http"` or `"https"`). Used
    /// as the fallback scheme when building absolute external URLs and no
    /// `X-Forwarded-Proto` header is present, so a direct HTTP client receives
    /// HTTP URLs. Set per-listener via [`AppState::with_scheme`]; defaults to
    /// `"https"`.
    pub default_scheme: &'static str,
}

impl AppState {
    pub async fn new(cfg: &Config, instance_id: String) -> anyhow::Result<Self> {
        let startup = std::time::Instant::now();
        // Build the raw KV backend, wrap with metrics, then encryption.
        let kv: Arc<dyn KvStore> = match &cfg.kv_store {
            KvStoreConfig::Redb {
                path,
                scaling_factor,
                cache_size,
            } => {
                let t = std::time::Instant::now();
                let store = ShardedRedbKvStore::open(path, *scaling_factor, *cache_size).await?;
                tracing::info!(
                    elapsed_ms = t.elapsed().as_millis(),
                    cache_mb = *cache_size / (1024 * 1024),
                    "redb shards opened"
                );
                let instrumented = InstrumentedKvStore::new(store, "redb");
                match &cfg.server_secret {
                    Some(secret) => {
                        let enc = EncryptedKvStore::with_secret(instrumented, secret);
                        Self::verify_encryption_canary(&enc).await?;
                        Arc::new(enc)
                    }
                    None => {
                        tracing::info!(
                            "no server_secret configured — KV values will be stored unencrypted"
                        );
                        Arc::new(instrumented)
                    }
                }
            }
            #[cfg(feature = "dynamodb")]
            KvStoreConfig::DynamoDB {
                table_prefix,
                region,
                endpoint_url,
                max_retries,
                connect_timeout_secs,
                read_timeout_secs,
                retry_mode,
            } => {
                let instrumented = InstrumentedKvStore::new(
                    depot_kv_dynamo::DynamoKvStore::connect(
                        table_prefix,
                        region,
                        endpoint_url.as_deref(),
                        *max_retries,
                        *connect_timeout_secs,
                        *read_timeout_secs,
                        retry_mode,
                    )
                    .await?,
                    "dynamodb",
                );
                match &cfg.server_secret {
                    Some(secret) => {
                        let enc = EncryptedKvStore::with_secret(instrumented, secret);
                        Self::verify_encryption_canary(&enc).await?;
                        Arc::new(enc)
                    }
                    None => {
                        tracing::info!(
                            "no server_secret configured — KV values will be stored unencrypted"
                        );
                        Arc::new(instrumented)
                    }
                }
            }
        };

        let stores = init_stores(kv.as_ref()).await?;

        let http = reqwest::Client::builder()
            .user_agent("depotd/0.1.0")
            .build()?;
        let jwt_secret = init_jwt_secret(kv.as_ref()).await?;
        #[cfg(feature = "ldap")]
        let (auth, ldap_config) = init_auth(kv.clone()).await?;
        #[cfg(not(feature = "ldap"))]
        let auth = init_auth(kv.clone()).await?;

        let settings = crate::server::config::settings::ensure_settings(kv.as_ref()).await?;
        let rate_limiter = Arc::new(DynamicRateLimiter::new(settings.rate_limit.as_ref()));

        tracing::info!(
            elapsed_ms = startup.elapsed().as_millis(),
            "app state initialized"
        );
        let event_bus = Arc::new(EventBus::new(1024));
        let tasks = Arc::new(TaskManager::new(Arc::clone(&kv), instance_id.clone()));
        Ok(Self {
            repo: RepoServices {
                kv,
                stores,
                http,
                inflight: InflightMap::new(),
                updater: UpdateSender::noop(),
            },
            auth: AuthServices {
                backend: auth,
                jwt_secret,
                #[cfg(feature = "ldap")]
                ldap_config,
            },
            bg: BackgroundServices {
                tasks,
                instance_id,
                gc_state: Arc::new(tokio::sync::Mutex::new(
                    crate::server::worker::blob_reaper::GcState::new(),
                )),
                event_bus,
                model: Arc::new(ModelHandle::new(MaterializedModel::empty())),
                scan_trigger: Arc::new(tokio::sync::Notify::new()),
            },
            settings: Arc::new(SettingsHandle::new(settings)),
            default_scheme: "https",
            rate_limiter,
        })
    }

    /// Check the encryption canary to verify the server_secret matches
    /// the key used to encrypt existing data. On a fresh database, writes
    /// the canary for future verification.
    async fn verify_encryption_canary<T: KvStore>(enc: &EncryptedKvStore<T>) -> anyhow::Result<()> {
        match encrypted::verify_canary(enc).await {
            Ok(true) => {
                tracing::info!("encryption canary verified — server_secret matches");
                Ok(())
            }
            Ok(false) => {
                // Fresh database — write the canary.
                encrypted::write_canary(enc).await?;
                tracing::info!("encryption canary written for future verification");
                Ok(())
            }
            Err(e) => {
                anyhow::bail!(
                    "KV encryption check failed — the server_secret does not match \
                     the key used to encrypt existing data. If you changed or removed \
                     the server_secret, restore the original value. Error: {}",
                    e
                );
            }
        }
    }
}

#[cfg(feature = "ldap")]
async fn init_auth(
    kv: Arc<dyn KvStore>,
) -> anyhow::Result<(
    Arc<dyn AuthBackend>,
    Arc<crate::server::auth::ldap::LdapConfigHandle>,
)> {
    let ldap_cfg = crate::server::config::settings::load_ldap_config(kv.as_ref()).await?;
    let ldap_config = Arc::new(crate::server::auth::ldap::LdapConfigHandle::new(ldap_cfg));
    let auth: Arc<dyn AuthBackend> = Arc::new(CachedAuthBackend::new(
        crate::server::auth::ldap::LdapAuthBackend::new(ldap_config.clone(), kv),
        std::time::Duration::from_secs(5),
    ));
    Ok((auth, ldap_config))
}

#[cfg(not(feature = "ldap"))]
async fn init_auth(kv: Arc<dyn KvStore>) -> anyhow::Result<Arc<dyn AuthBackend>> {
    Ok(Arc::new(CachedAuthBackend::new(
        BuiltinAuthBackend::new(kv),
        std::time::Duration::from_secs(5),
    )))
}

async fn init_stores(kv: &dyn KvStore) -> anyhow::Result<Arc<StoreRegistry>> {
    let stores = Arc::new(StoreRegistry::new());

    // Load all store records from KV and instantiate.
    let store_records = depot_core::service::list_stores(kv).await?;
    if store_records.is_empty() {
        tracing::warn!(
            "no blob stores configured; create stores via the API before creating repositories"
        );
    }
    for record in &store_records {
        match instantiate_store(&record.kind).await {
            Ok(blob_store) => {
                stores.add(&record.name, blob_store).await;
                tracing::info!(store = %record.name, kind = record.kind.type_name(), "loaded blob store");
            }
            Err(e) => {
                tracing::error!(store = %record.name, error = %e, "failed to instantiate blob store");
                anyhow::bail!("failed to instantiate store '{}': {}", record.name, e);
            }
        }
    }

    Ok(stores)
}

async fn init_jwt_secret(
    kv: &dyn KvStore,
) -> anyhow::Result<Arc<arc_swap::ArcSwap<JwtSecretState>>> {
    let existing = depot_core::service::get_meta(kv, "jwt_secret").await?;
    let state = match existing {
        Some(bytes) => match rmp_serde::from_slice::<JwtSecretState>(&bytes) {
            Ok(s) => s,
            Err(_) => {
                let s = JwtSecretState {
                    current: crate::server::auth::generate_token_secret(),
                    previous: None,
                    rotated_at: chrono::Utc::now().timestamp(),
                };
                depot_core::service::put_meta(kv, "jwt_secret", &rmp_serde::to_vec(&s)?).await?;
                s
            }
        },
        None => {
            let s = JwtSecretState {
                current: crate::server::auth::generate_token_secret(),
                previous: None,
                rotated_at: chrono::Utc::now().timestamp(),
            };
            depot_core::service::put_meta(kv, "jwt_secret", &rmp_serde::to_vec(&s)?).await?;
            s
        }
    };
    Ok(Arc::new(arc_swap::ArcSwap::from_pointee(state)))
}
