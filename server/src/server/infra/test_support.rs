// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Test support: start a real depotd instance on a random port with temp storage.
//!
//! All temporary files (databases, blobs, logs) are kept under `build/test/`
//! in the workspace root so that concurrent worktree runs stay isolated and
//! nothing leaks into `/tmp`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::task::JoinHandle;

use crate::server::auth::backend::{AuthBackend, BuiltinAuthBackend, CachedAuthBackend};
use crate::server::config::settings::{Settings, SettingsHandle};
use crate::server::config::{Config, HttpConfig, KvStoreConfig};
use crate::server::infra::rate_limit::DynamicRateLimiter;
use crate::server::infra::router::build_router;
use crate::server::infra::state::{AppState, AuthServices, BackgroundServices, RepoServices};
use crate::server::infra::task::TaskManager;
use crate::server::store::KvStore;
use depot_blob_file::FileBlobStore;
use depot_core::inflight::InflightMap;
use depot_core::store::kv::CURRENT_RECORD_VERSION;
use depot_core::store_registry::StoreRegistry;
use depot_core::update::UpdateSender;
use depot_kv_redb::ShardedRedbKvStore;

/// Return the `build/test/` directory inside the workspace root.
/// Created once and reused for all temp dirs, temp files, and logs.
fn test_build_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        // Walk up from CARGO_MANIFEST_DIR (the consuming crate's dir) to find
        // the workspace root — the first ancestor whose Cargo.toml declares
        // `[workspace]`. Crates can now sit at varying depths below the root
        // (e.g. `format/apt/`, `kv/redb/`), so we can't assume a fixed depth.
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let root = find_workspace_root(&PathBuf::from(&manifest))
            .unwrap_or_else(|| PathBuf::from(&manifest));
        let dir = root.join("build").join("test");
        std::fs::create_dir_all(&dir).expect("failed to create build/test/ directory");
        dir
    })
}

fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        let cargo_toml = ancestor.join("Cargo.toml");
        if let Ok(contents) = std::fs::read_to_string(&cargo_toml) {
            if contents.contains("[workspace]") {
                return Some(ancestor.to_path_buf());
            }
        }
    }
    None
}

/// Create a `TempDir` under `build/test/` instead of the system `/tmp`.
pub fn test_tempdir() -> TempDir {
    TempDir::new_in(test_build_dir()).expect("failed to create temp dir in build/test/")
}

/// Initialise a file-based tracing subscriber once per test process.
/// All tracing output (server logs, demo/bench progress) goes to a file
/// under `build/test/` whose path is printed to stderr so the user can
/// inspect it if needed.
fn init_test_tracing() {
    static LOG_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    LOG_PATH.get_or_init(|| {
        let path = test_build_dir().join(format!("depot-test-{}.log", std::process::id()));
        let file = std::fs::File::create(&path).expect("failed to create test log file");
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "depot=info,tower_http=info".into()),
            )
            .with_writer(file)
            .with_ansi(false)
            .init();
        eprintln!("depot test log: {}", path.display());
        path
    });
}

/// A running test server with its URL, background task handle, and temp directory.
pub struct TestServer {
    pub url: String,
    pub handle: JoinHandle<()>,
    pub _temp_dir: TempDir,
}

impl TestServer {
    /// Start a depotd instance on a random port with temp storage.
    /// Default admin user: admin/admin.
    pub async fn start() -> Self {
        Self::start_with_kv(None).await
    }

    /// Start a depotd instance with an optional KV store override.
    /// When `kv_config` is `None`, uses the default redb backend.
    pub async fn start_with_kv(kv_config: Option<KvStoreConfig>) -> Self {
        init_test_tracing();
        let temp_dir = test_tempdir();
        let db_path = temp_dir.path().join("kv");
        let blob_root = temp_dir.path().join("blobs");

        let cfg = Config {
            http: Some(HttpConfig {
                listen: "127.0.0.1:0".to_string(),
            }),
            kv_store: kv_config.unwrap_or(KvStoreConfig::Redb {
                path: db_path.clone(),
                scaling_factor: 2,
                cache_size: 0,
            }),
            default_admin_password: Some("admin".to_string()),
            ..Config::default()
        };

        // Build KV store based on config.
        let kv: Arc<dyn KvStore> = match &cfg.kv_store {
            KvStoreConfig::Redb {
                path,
                scaling_factor,
                cache_size,
            } => Arc::new(
                ShardedRedbKvStore::open(path, *scaling_factor, *cache_size)
                    .await
                    .expect("failed to open redb"),
            ),
            #[cfg(feature = "dynamodb")]
            KvStoreConfig::DynamoDB {
                table_prefix,
                region,
                endpoint_url,
                max_retries,
                connect_timeout_secs,
                read_timeout_secs,
                retry_mode,
            } => Arc::new(
                depot_kv_dynamo::DynamoKvStore::connect(
                    table_prefix,
                    region,
                    endpoint_url.as_deref(),
                    *max_retries,
                    *connect_timeout_secs,
                    *read_timeout_secs,
                    retry_mode,
                )
                .await
                .expect("failed to connect to DynamoDB"),
            ),
        };
        let blobs: Arc<dyn depot_core::store::blob::BlobStore> = Arc::new(
            FileBlobStore::new(&blob_root, false, 1_048_576, false)
                .expect("failed to create blob store"),
        );
        let stores = Arc::new(StoreRegistry::new());
        stores.add("default", blobs).await;
        let http = reqwest::Client::builder()
            .user_agent("depot-test/0.1.0")
            .build()
            .expect("failed to build HTTP client");

        let auth: Arc<dyn AuthBackend> = Arc::new(CachedAuthBackend::new(
            BuiltinAuthBackend::new(kv.clone()),
            std::time::Duration::from_secs(5),
        ));
        let instance_id = uuid::Uuid::new_v4().to_string();
        let event_bus = Arc::new(crate::server::infra::event_bus::EventBus::new(1024));
        let tasks = Arc::new(TaskManager::new(kv.clone(), instance_id.clone()));
        let state = AppState {
            repo: RepoServices {
                kv,
                stores,
                http,
                inflight: InflightMap::new(),
                updater: UpdateSender::noop(),
            },
            auth: AuthServices {
                backend: auth,
                jwt_secret: Arc::new(arc_swap::ArcSwap::from_pointee(
                    crate::server::auth::JwtSecretState {
                        current: b"test-secret-key-for-jwt-signing!".to_vec(),
                        previous: None,
                        rotated_at: 0,
                    },
                )),
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
                model: Arc::new(crate::server::infra::event_bus::ModelHandle::new(
                    crate::server::infra::event_bus::MaterializedModel::empty(),
                )),
                scan_trigger: Arc::new(tokio::sync::Notify::new()),
                request_stream: Arc::new(crate::server::infra::request_stream::RequestStream::new()),
            },
            settings: Arc::new(SettingsHandle::new(Settings {
                access_log: false,
                ..Settings::default()
            })),
            rate_limiter: Arc::new(DynamicRateLimiter::new(None)),
            default_scheme: "https",
        };

        // Bootstrap default store record.
        let now = crate::server::repo::now_utc();
        let blob_root_str = blob_root.to_string_lossy().to_string();
        depot_core::service::put_store(
            state.repo.kv.as_ref(),
            &depot_core::store::kv::StoreRecord {
                schema_version: CURRENT_RECORD_VERSION,
                name: "default".to_string(),
                kind: depot_core::store::kv::StoreKind::File {
                    root: blob_root_str,
                    sync: false,
                    io_size: 1024,
                    direct_io: false,
                },
                created_at: now,
            },
        )
        .await
        .expect("failed to bootstrap store record");

        // Bootstrap roles and users.
        crate::server::config::bootstrap::bootstrap_roles(state.repo.kv.as_ref())
            .await
            .expect("failed to bootstrap roles");
        crate::server::config::bootstrap::bootstrap_users(
            state.repo.kv.as_ref(),
            Some("admin".to_string()),
            false,
        )
        .await
        .expect("failed to bootstrap users");

        let app = build_router(state, None);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind");
        let addr = listener.local_addr().expect("failed to get local addr");
        let url = format!("http://{}", addr);

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        // Poll health endpoint until the server is ready.
        let health_url = format!("http://{}/api/v1/health", addr);
        let client = reqwest::Client::new();
        let mut delay = std::time::Duration::from_millis(10);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if client.get(&health_url).send().await.is_ok() {
                break;
            }
            if tokio::time::Instant::now() + delay > deadline {
                panic!("TestServer failed to become ready within 5s");
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(std::time::Duration::from_millis(500));
        }

        TestServer {
            url,
            handle,
            _temp_dir: temp_dir,
        }
    }
}
