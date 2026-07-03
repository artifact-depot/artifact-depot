// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Declarative first-start initialization from the TOML config.
//!
//! When the `[initialization]` section is present AND the KV store is fresh
//! (no users yet), the server creates the listed roles, stores, repositories,
//! and users, and overrides settings, at startup — before listeners bind and
//! before any background workers run.
//!
//! On already-initialized KVs the section is ignored entirely. Init-declared
//! names overwrite default-bootstrap records on conflict (the operator's
//! intent wins); a declared `admin` user also suppresses the random-password
//! default so no throwaway password is printed.
//!
//! Order of application: roles → stores (with health check) → repos → users
//! → settings. Settings are applied last so that a transient failure earlier
//! leaves the KV "fresh" (users table empty) and init is retried cleanly on
//! the next start. Once any user has been successfully written, the next
//! start will skip init; recover by wiping the KV and restarting.
//!
//! Distributed coordination: on multi-instance DynamoDB deployments this
//! inherits the same race as the existing `bootstrap_roles` /
//! `bootstrap_users` — first-boot concurrency is not protected by a lease.
//! Edge caches and demos are expected to be single-instance.

use std::sync::Arc;

use serde::Deserialize;

use crate::server::api::conversions::{
    build_repo_kind_and_format, build_store_kind, validate_yum_proxy_member_depth,
};
use crate::server::api::repositories::CreateRepoRequest;
use crate::server::api::roles::CreateRoleRequest;
use crate::server::api::stores::CreateStoreRequest;
use crate::server::api::users::CreateUserRequest;
use crate::server::auth::{self, hash_password};
use crate::server::config::settings::{save_settings_to_kv, Settings};
use crate::server::infra::store_registry::instantiate_store;
use crate::server::AppState;
use depot_core::service;
use depot_core::store::kv::{
    ArtifactFormat, KvStore, RepoConfig, RepoKind, RoleRecord, StoreRecord, StoreStatsRecord,
    UserRecord, CURRENT_RECORD_VERSION,
};

/// Declarative first-start configuration. Every list is optional and
/// independently applied. Field names match the REST API's create-request
/// types so an operator who has used the REST API can transcribe values
/// directly.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InitializationConfig {
    #[serde(default)]
    pub roles: Vec<CreateRoleRequest>,
    #[serde(default)]
    pub stores: Vec<CreateStoreRequest>,
    #[serde(default)]
    pub repositories: Vec<CreateRepoRequest>,
    #[serde(default)]
    pub users: Vec<CreateUserRequest>,
    #[serde(default)]
    pub settings: Option<Settings>,
}

impl InitializationConfig {
    /// True when no sections are populated.
    pub fn is_empty(&self) -> bool {
        self.roles.is_empty()
            && self.stores.is_empty()
            && self.repositories.is_empty()
            && self.users.is_empty()
            && self.settings.is_none()
    }
}

/// Counts of what was created during `apply_initialization`, for the
/// one-line summary in the startup log.
#[derive(Debug, Default)]
pub struct InitSummary {
    pub roles_created: usize,
    pub stores_created: usize,
    pub repos_created: usize,
    pub users_created: usize,
    pub settings_overridden: bool,
}

/// A KV store is "fresh" when the users table is empty. Call this BEFORE
/// running the default `bootstrap_users`, which would otherwise make the
/// table non-empty.
pub async fn kv_is_fresh(kv: &dyn KvStore) -> depot_core::error::Result<bool> {
    Ok(service::list_users(kv).await?.is_empty())
}

/// Apply the `[initialization]` section against a fresh KV. The caller is
/// responsible for deciding whether to invoke this — typically after
/// [`kv_is_fresh`] returned `true`. All `put_*` calls are upserts, so
/// re-running on a partially-initialized KV is safe and idempotent.
pub async fn apply_initialization(
    state: &AppState,
    init: &InitializationConfig,
) -> anyhow::Result<InitSummary> {
    let mut summary = InitSummary::default();
    let now = crate::server::repo::now_utc();

    // 1. Roles — upserted; any collision with the default-bootstrap `admin` /
    //    `read-only` records replaces them with the operator's definition.
    for req in &init.roles {
        let record = RoleRecord {
            schema_version: CURRENT_RECORD_VERSION,
            name: req.name.clone(),
            description: req.description.clone(),
            capabilities: req.capabilities.clone(),
            created_at: now,
            updated_at: now,
        };
        service::put_role(state.repo.kv.as_ref(), &record).await?;
        state.auth.backend.invalidate_role(&record.name);
        summary.roles_created += 1;
    }

    // 2. Stores — instantiate + health-check before persisting so misconfig
    //    fails fast instead of silently. Newly-created stores are added to
    //    the in-memory registry so repos created in step 3 can reference
    //    them immediately, and requests served after startup Just Work.
    for req in &init.stores {
        let kind = build_store_kind(req)
            .map_err(|e| anyhow::anyhow!("initialization.stores.{}: {}", req.name, e))?;
        let blob_store = instantiate_store(&kind).await.map_err(|e| {
            anyhow::anyhow!(
                "initialization.stores.{}: failed to instantiate: {}",
                req.name,
                e
            )
        })?;
        blob_store.health_check().await.map_err(|e| {
            anyhow::anyhow!(
                "initialization.stores.{}: health check failed: {}",
                req.name,
                e
            )
        })?;

        let record = StoreRecord {
            schema_version: CURRENT_RECORD_VERSION,
            name: req.name.clone(),
            kind,
            created_at: now,
        };
        service::put_store(state.repo.kv.as_ref(), &record).await?;
        let stats = StoreStatsRecord {
            blob_count: 0,
            total_bytes: 0,
            updated_at: now,
        };
        service::put_store_stats(state.repo.kv.as_ref(), &req.name, &stats).await?;
        state.repo.stores.add(&req.name, blob_store).await;
        summary.stores_created += 1;
    }

    // 3. Repositories.
    for req in &init.repositories {
        let (kind, format_config) = build_repo_kind_and_format(req)
            .map_err(|e| anyhow::anyhow!("initialization.repositories.{}: {}", req.name, e))?;

        if service::get_store(state.repo.kv.as_ref(), &req.store)
            .await?
            .is_none()
        {
            anyhow::bail!(
                "initialization.repositories.{}: references unknown store '{}'",
                req.name,
                req.store,
            );
        }

        if format_config.artifact_format() == ArtifactFormat::Yum {
            if let RepoKind::Proxy { ref members, .. } = kind {
                validate_yum_proxy_member_depth(state.repo.kv.as_ref(), members)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("initialization.repositories.{}: {}", req.name, e)
                    })?;
            }
        }

        let cfg = RepoConfig {
            schema_version: CURRENT_RECORD_VERSION,
            name: req.name.clone(),
            kind,
            format_config,
            store: req.store.clone(),
            created_at: now,
            cleanup_max_unaccessed_days: req.cleanup_max_unaccessed_days,
            cleanup_max_age_days: req.cleanup_max_age_days,
            deleting: false,
        };
        service::put_repo(state.repo.kv.as_ref(), &cfg).await?;

        // Provision the apt signing key declaratively: import the operator's key
        // if one was supplied, otherwise auto-generate for hosted repos. Unlike
        // the REST create path, failures here fail initialization so a misconfig
        // surfaces at boot rather than silently leaving an unsigned repo.
        if cfg.format_config.artifact_format() == ArtifactFormat::Apt {
            let fs = state.format_state();
            match &req.signing_key {
                Some(key) => {
                    depot_format_apt::api::import_repo_signing_key(&fs, &cfg, key)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "initialization.repositories.{}: failed to import signing key: {}",
                                req.name,
                                e
                            )
                        })?;
                }
                None => {
                    depot_format_apt::api::ensure_repo_signing_key(&fs, &cfg)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "initialization.repositories.{}: failed to generate signing key: {}",
                                req.name,
                                e
                            )
                        })?;
                }
            }
        }
        summary.repos_created += 1;
    }

    // 4. Users — each password is hashed fresh; `admin` overrides the
    //    default-bootstrap admin because `put_user` is upsert.
    for req in &init.users {
        if req.username.is_empty() {
            anyhow::bail!("initialization.users: empty username");
        }
        let password_hash = hash_password(req.password.clone())
            .await
            .map_err(|e| anyhow::anyhow!("initialization.users.{}: {}", req.username, e))?;
        let record = UserRecord {
            schema_version: CURRENT_RECORD_VERSION,
            username: req.username.clone(),
            password_hash,
            roles: req.roles.clone(),
            auth_source: "builtin".to_string(),
            token_secret: auth::generate_token_secret(),
            must_change_password: req.must_change_password,
            created_at: now,
            updated_at: now,
        };
        service::put_user(state.repo.kv.as_ref(), &record).await?;
        state.auth.backend.invalidate_user(&record.username);
        summary.users_created += 1;
    }

    // 5. Settings — applied last so earlier-step failures leave the KV
    //    "fresh" (users empty) and init is retried cleanly next start.
    if let Some(ref new_settings) = init.settings {
        new_settings
            .validate()
            .map_err(|errs| anyhow::anyhow!("initialization.settings: {}", errs.join(", ")))?;
        save_settings_to_kv(state.repo.kv.as_ref(), new_settings).await?;
        state.settings.store(Arc::new(new_settings.clone()));
        state.rate_limiter.update(new_settings.rate_limit.as_ref());
        summary.settings_overridden = true;
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::auth::backend::{AuthBackend, BuiltinAuthBackend, CachedAuthBackend};
    use crate::server::auth::JwtSecretState;
    use crate::server::config::settings::SettingsHandle;
    use crate::server::infra::event_bus::{EventBus, MaterializedModel, ModelHandle};
    use crate::server::infra::rate_limit::DynamicRateLimiter;
    use crate::server::infra::task::TaskManager;
    use crate::server::store::KvStore;
    use crate::server::{AuthServices, BackgroundServices, RepoServices};
    use depot_core::inflight::InflightMap;
    use depot_core::store::kv::{Capability, CapabilityGrant};
    use depot_core::store_registry::StoreRegistry;
    use depot_core::update::UpdateSender;
    use depot_kv_redb::ShardedRedbKvStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Build a minimal AppState backed by a fresh redb KV — no roles, no
    /// users, no stores. Used for testing apply_initialization in isolation.
    async fn make_fresh_state() -> (TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let kv: Arc<dyn KvStore> = Arc::new(
            ShardedRedbKvStore::open(&dir.path().join("kv"), 2, 0)
                .await
                .unwrap(),
        );
        let auth: Arc<dyn AuthBackend> = Arc::new(CachedAuthBackend::new(
            BuiltinAuthBackend::new(kv.clone()),
            std::time::Duration::from_secs(5),
        ));
        let stores = Arc::new(StoreRegistry::new());
        let instance_id = "test-instance".to_string();
        let event_bus = Arc::new(EventBus::new(64));
        let tasks = Arc::new(TaskManager::new(kv.clone(), instance_id.clone()));
        let state = AppState {
            repo: RepoServices {
                kv,
                stores,
                http: reqwest::Client::new(),
                inflight: InflightMap::new(),
                updater: UpdateSender::noop(),
            },
            auth: AuthServices {
                backend: auth,
                jwt_secret: Arc::new(arc_swap::ArcSwap::from_pointee(JwtSecretState {
                    current: b"test-secret-key-for-jwt-signing!".to_vec(),
                    previous: None,
                    rotated_at: 0,
                })),
                #[cfg(feature = "ldap")]
                ldap_config: Arc::new(crate::server::auth::ldap::LdapConfigHandle::new(None)),
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
            settings: Arc::new(SettingsHandle::new(Settings::default())),
            rate_limiter: Arc::new(DynamicRateLimiter::new(None)),
            default_scheme: "https",
        };
        (dir, state)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_kv_is_fresh_true_on_empty_kv() {
        let (_dir, state) = make_fresh_state().await;
        assert!(kv_is_fresh(state.repo.kv.as_ref()).await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_kv_is_fresh_false_when_user_exists() {
        let (_dir, state) = make_fresh_state().await;
        let now = crate::server::repo::now_utc();
        depot_core::service::put_user(
            state.repo.kv.as_ref(),
            &depot_core::store::kv::UserRecord {
                schema_version: CURRENT_RECORD_VERSION,
                username: "alice".into(),
                password_hash: String::new(),
                roles: vec![],
                auth_source: "builtin".into(),
                token_secret: vec![],
                must_change_password: false,
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .unwrap();
        assert!(!kv_is_fresh(state.repo.kv.as_ref()).await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_initialization_config_is_empty() {
        assert!(InitializationConfig::default().is_empty());
        let mut cfg = InitializationConfig::default();
        cfg.settings = Some(Settings::default());
        assert!(!cfg.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_apply_initialization_full_pipeline() {
        let (dir, state) = make_fresh_state().await;
        let blob_root = dir.path().join("blobs");
        std::fs::create_dir_all(&blob_root).unwrap();

        let init = InitializationConfig {
            roles: vec![CreateRoleRequest {
                name: "developer".into(),
                description: "rw".into(),
                capabilities: vec![CapabilityGrant {
                    capability: Capability::Write,
                    repo: "*".into(),
                }],
            }],
            stores: vec![CreateStoreRequest {
                name: "default".into(),
                store_type: "file".into(),
                root: Some(blob_root.to_string_lossy().into()),
                sync: false,
                io_size: 1024,
                direct_io: false,
                bucket: None,
                endpoint: None,
                region: "us-east-1".into(),
                prefix: None,
                access_key: None,
                secret_key: None,
                max_retries: 3,
                connect_timeout_secs: 3,
                read_timeout_secs: 30,
                retry_mode: "standard".into(),
            }],
            repositories: vec![CreateRepoRequest {
                name: "hosted-raw".into(),
                repo_type: depot_core::store::kv::RepoType::Hosted,
                format: ArtifactFormat::Raw,
                store: "default".into(),
                upstream_url: None,
                cache_ttl_secs: None,
                members: None,
                write_member: None,
                listen: None,
                cleanup_max_unaccessed_days: None,
                cleanup_max_age_days: None,
                cleanup_untagged_manifests: None,
                upstream_auth: None,
                content_disposition: None,
                repodata_depth: None,
                signing_key: None,
            }],
            users: vec![CreateUserRequest {
                username: "ci".into(),
                password: "hunter2".into(),
                roles: vec!["developer".into()],
                must_change_password: false,
            }],
            settings: Some(Settings {
                access_log: false,
                max_artifact_bytes: 12345,
                ..Settings::default()
            }),
        };

        let summary = apply_initialization(&state, &init).await.unwrap();
        assert_eq!(summary.roles_created, 1);
        assert_eq!(summary.stores_created, 1);
        assert_eq!(summary.repos_created, 1);
        assert_eq!(summary.users_created, 1);
        assert!(summary.settings_overridden);

        // Roles, stores, repos, users persisted in KV.
        let roles = depot_core::service::list_roles(state.repo.kv.as_ref())
            .await
            .unwrap();
        assert!(roles.iter().any(|r| r.name == "developer"));

        let stores = depot_core::service::list_stores(state.repo.kv.as_ref())
            .await
            .unwrap();
        assert!(stores.iter().any(|s| s.name == "default"));

        // Newly-created store is in the in-memory registry too.
        assert!(state.repo.stores.get("default").await.is_some());

        let repos = depot_core::service::list_repos(state.repo.kv.as_ref())
            .await
            .unwrap();
        assert!(repos.iter().any(|r| r.name == "hosted-raw"));

        let users = depot_core::service::list_users(state.repo.kv.as_ref())
            .await
            .unwrap();
        let ci = users.iter().find(|u| u.username == "ci").unwrap();
        assert_eq!(ci.roles, vec!["developer".to_string()]);
        // Password hash is non-empty (not the literal plaintext).
        assert!(!ci.password_hash.is_empty());
        assert_ne!(ci.password_hash, "hunter2");

        // Settings were swapped both in KV and in-memory.
        let stored = crate::server::config::settings::load_settings_from_kv(state.repo.kv.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.max_artifact_bytes, 12345);
        assert!(!state.settings.load().access_log);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_apply_initialization_admin_role_overwrites_default() {
        let (_dir, state) = make_fresh_state().await;
        // Pre-populate the default 3-cap admin role.
        crate::server::config::bootstrap::bootstrap_roles(state.repo.kv.as_ref())
            .await
            .unwrap();

        let init = InitializationConfig {
            roles: vec![CreateRoleRequest {
                name: "admin".into(),
                description: "scoped admin".into(),
                capabilities: vec![CapabilityGrant {
                    capability: Capability::Read,
                    repo: "edge-cache".into(),
                }],
            }],
            ..InitializationConfig::default()
        };
        apply_initialization(&state, &init).await.unwrap();

        let admin = depot_core::service::get_role(state.repo.kv.as_ref(), "admin")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(admin.capabilities.len(), 1);
        assert_eq!(admin.capabilities[0].repo, "edge-cache");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_apply_initialization_repo_references_missing_store() {
        let (_dir, state) = make_fresh_state().await;
        let init = InitializationConfig {
            repositories: vec![CreateRepoRequest {
                name: "broken".into(),
                repo_type: depot_core::store::kv::RepoType::Hosted,
                format: ArtifactFormat::Raw,
                store: "does-not-exist".into(),
                upstream_url: None,
                cache_ttl_secs: None,
                members: None,
                write_member: None,
                listen: None,
                cleanup_max_unaccessed_days: None,
                cleanup_max_age_days: None,
                cleanup_untagged_manifests: None,
                upstream_auth: None,
                content_disposition: None,
                repodata_depth: None,
                signing_key: None,
            }],
            ..InitializationConfig::default()
        };
        let err = apply_initialization(&state, &init).await.unwrap_err();
        assert!(
            err.to_string().contains("does-not-exist"),
            "error should mention missing store: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_apply_initialization_settings_only() {
        let (_dir, state) = make_fresh_state().await;
        let init = InitializationConfig {
            settings: Some(Settings {
                jwt_expiry_secs: 600,
                ..Settings::default()
            }),
            ..InitializationConfig::default()
        };
        let summary = apply_initialization(&state, &init).await.unwrap();
        assert!(summary.settings_overridden);
        assert_eq!(summary.roles_created, 0);
        assert_eq!(summary.stores_created, 0);
        assert_eq!(state.settings.load().jwt_expiry_secs, 600);
    }
}
