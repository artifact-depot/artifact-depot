// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Test support for depot-format-* crates and depot.
//!
//! Provides the shared `TestApp` harness plus generic request helpers. This
//! crate replaces the old `depot_server::server::tests::helpers` module so that
//! format crates can exercise their HTTP handlers through a real Axum router
//! without needing their own test server.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc
)]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;

use depot_blob_file::FileBlobStore;
use depot_core::service;
use depot_core::store::kv::{Capability, CapabilityGrant, CURRENT_RECORD_VERSION};
use depot_core::store::kv::{KvStore, RoleRecord, UserRecord};
use depot_kv_redb::ShardedRedbKvStore;
use depot_server::server::auth;
use depot_server::server::auth::backend::{AuthBackend, BuiltinAuthBackend, CachedAuthBackend};
use depot_server::server::config::settings::{Settings, SettingsHandle};
use depot_server::server::{build_docker_port_router, build_router, AppState};

pub use tower::ServiceExt; // for oneshot

// ---------------------------------------------------------------------------
// Tempdir / build directory helpers
// ---------------------------------------------------------------------------

pub use depot_server::server::infra::test_support::test_tempdir;

// ---------------------------------------------------------------------------
// TestApp helper
// ---------------------------------------------------------------------------

pub struct TestApp {
    pub router: Router,
    pub jwt_secret: Vec<u8>,
    pub admin_token_secret: Vec<u8>,
    pub state: AppState,
    pub _dir: tempfile::TempDir,
}

/// Intermediate result from the shared test-app initialization.
struct TestAppSetup {
    dir: tempfile::TempDir,
    state: AppState,
    jwt_secret: Vec<u8>,
    admin_token_secret: Vec<u8>,
}

/// Common initialization shared between `TestApp::new()` and `new_with_docker_port()`.
async fn test_app_setup() -> TestAppSetup {
    let dir = test_tempdir();
    let db_path = dir.path().join("kv");
    let blob_root = dir.path().join("blobs");
    std::fs::create_dir_all(&blob_root).unwrap();

    let kv = ShardedRedbKvStore::open(&db_path, 2, 0)
        .await
        .expect("failed to open redb");
    let blobs =
        FileBlobStore::new(&blob_root, false, 1_048_576, false).expect("failed to open blob store");

    let jwt_secret = b"test-secret-key-for-jwt-signing!".to_vec();

    let now = depot_server::server::repo::now_utc();

    // Bootstrap roles.
    let admin_role = RoleRecord {
        schema_version: CURRENT_RECORD_VERSION,
        name: "admin".to_string(),
        description: "Full access to all repositories and admin operations".to_string(),
        capabilities: vec![
            CapabilityGrant {
                capability: Capability::Read,
                repo: "*".to_string(),
            },
            CapabilityGrant {
                capability: Capability::Write,
                repo: "*".to_string(),
            },
            CapabilityGrant {
                capability: Capability::Delete,
                repo: "*".to_string(),
            },
        ],
        created_at: now,
        updated_at: now,
    };
    let readonly_role = RoleRecord {
        schema_version: CURRENT_RECORD_VERSION,
        name: "read-only".to_string(),
        description: "Read-only access to all repositories".to_string(),
        capabilities: vec![CapabilityGrant {
            capability: Capability::Read,
            repo: "*".to_string(),
        }],
        created_at: now,
        updated_at: now,
    };

    // Bootstrap users.
    let admin_token_secret = auth::generate_token_secret();
    let admin_user = UserRecord {
        schema_version: CURRENT_RECORD_VERSION,
        username: "admin".to_string(),
        password_hash: auth::hash_password("password".to_string()).await.unwrap(),
        roles: vec!["admin".to_string()],
        auth_source: "builtin".to_string(),
        token_secret: admin_token_secret.clone(),
        must_change_password: false,
        created_at: now,
        updated_at: now,
    };

    let anon_user = UserRecord {
        schema_version: CURRENT_RECORD_VERSION,
        username: "anonymous".to_string(),
        password_hash: String::new(),
        roles: vec![],
        auth_source: "builtin".to_string(),
        token_secret: vec![],
        must_change_password: false,
        created_at: now,
        updated_at: now,
    };

    let default_store = depot_core::store::kv::StoreRecord {
        schema_version: CURRENT_RECORD_VERSION,
        name: "default".to_string(),
        kind: depot_core::store::kv::StoreKind::File {
            root: blob_root.to_string_lossy().to_string(),
            sync: false,
            io_size: 1024,
            direct_io: false,
        },
        created_at: now,
    };

    service::put_role(&kv, &admin_role).await.unwrap();
    service::put_role(&kv, &readonly_role).await.unwrap();
    service::put_user(&kv, &admin_user).await.unwrap();
    service::put_user(&kv, &anon_user).await.unwrap();
    service::put_store(&kv, &default_store).await.unwrap();

    let kv: Arc<dyn KvStore> = Arc::new(kv);
    let auth_backend: Arc<dyn AuthBackend> = Arc::new(CachedAuthBackend::new(
        BuiltinAuthBackend::new(kv.clone()),
        std::time::Duration::from_secs(5),
    ));
    let stores = Arc::new(depot_core::store_registry::StoreRegistry::new());
    stores
        .add(
            "default",
            Arc::new(blobs) as Arc<dyn depot_core::store::blob::BlobStore>,
        )
        .await;
    let instance_id = uuid::Uuid::new_v4().to_string();
    let event_bus = Arc::new(depot_server::server::infra::event_bus::EventBus::new(64));
    let tasks = Arc::new(depot_server::server::infra::task::TaskManager::new(
        kv.clone(),
        instance_id.clone(),
    ));
    let state = AppState {
        repo: depot_server::server::RepoServices {
            kv,
            stores,
            http: reqwest::Client::new(),
            inflight: depot_core::inflight::InflightMap::new(),
            updater: depot_core::update::UpdateSender::noop(),
        },
        auth: depot_server::server::AuthServices {
            backend: auth_backend,
            jwt_secret: Arc::new(arc_swap::ArcSwap::from_pointee(
                depot_server::server::auth::JwtSecretState {
                    current: jwt_secret.clone(),
                    previous: None,
                    rotated_at: 0,
                },
            )),
            #[cfg(feature = "ldap")]
            ldap_config: Arc::new(depot_server::server::auth::ldap::LdapConfigHandle::new(
                None,
            )),
        },
        bg: depot_server::server::BackgroundServices {
            tasks,
            instance_id,
            gc_state: Arc::new(tokio::sync::Mutex::new(
                depot_server::server::worker::blob_reaper::GcState::new(),
            )),
            event_bus,
            model: Arc::new(depot_server::server::infra::event_bus::ModelHandle::new(
                depot_server::server::infra::event_bus::MaterializedModel::empty(),
            )),
            scan_trigger: Arc::new(tokio::sync::Notify::new()),
        },
        settings: Arc::new(SettingsHandle::new(Settings {
            access_log: false,
            ..Settings::default()
        })),
        rate_limiter: Arc::new(
            depot_server::server::infra::rate_limit::DynamicRateLimiter::new(None),
        ),
        default_scheme: "https",
    };

    TestAppSetup {
        dir,
        state,
        jwt_secret,
        admin_token_secret,
    }
}

fn sha256_digest(data: &[u8]) -> String {
    format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(sha2::Sha256::default(), data))
    )
}

impl TestApp {
    pub async fn new() -> Self {
        Self::new_with_default_scheme("https").await
    }

    /// Like [`TestApp::new`] but pins the listener's fallback scheme used when
    /// building absolute external URLs (npm `dist.tarball`, Docker realm, …)
    /// and no `X-Forwarded-Proto` header is present. Pass `"http"` to emulate a
    /// client talking to depot's plain-HTTP listener.
    pub async fn new_with_default_scheme(default_scheme: &'static str) -> Self {
        let TestAppSetup {
            dir,
            mut state,
            jwt_secret,
            admin_token_secret,
        } = test_app_setup().await;

        state.default_scheme = default_scheme;

        // Spawn a real update worker so dir entries are maintained.
        let (update_sender, update_rx) = depot_core::update::UpdateSender::new(4096);
        state.repo.updater = update_sender;
        tokio::spawn(depot_server::server::worker::update::run_update_worker(
            update_rx,
            state.repo.kv.clone(),
            tokio_util::sync::CancellationToken::new(),
        ));

        let router = build_router(state.clone(), None);

        Self {
            router,
            jwt_secret,
            admin_token_secret,
            state,
            _dir: dir,
        }
    }

    /// Override the configured external base URL (the Nexus-style "Base URL"
    /// hard override) in the live cluster settings shared with the running
    /// router. Pass `None` to clear it.
    pub fn set_base_url(&self, base_url: Option<&str>) {
        let cur = self.state.settings.load();
        let mut settings = (**cur).clone();
        settings.base_url = base_url.map(|s| s.to_string());
        self.state.settings.store(Arc::new(settings));
    }

    pub fn admin_token(&self) -> String {
        let gs: &[u8; 32] = self.jwt_secret.as_slice().try_into().unwrap();
        let key = auth::derive_signing_key(gs, &self.admin_token_secret);
        auth::create_token("admin", &key, 86400, vec!["admin".to_string()]).unwrap()
    }

    pub async fn token_for(&self, username: &str) -> String {
        let user = service::get_user(self.state.repo.kv.as_ref(), username)
            .await
            .unwrap()
            .unwrap();
        let gs: &[u8; 32] = self.jwt_secret.as_slice().try_into().unwrap();
        let key = auth::derive_signing_key(gs, &user.token_secret);
        auth::create_token(username, &key, 86400, user.roles.clone()).unwrap()
    }

    pub fn request(&self, method: Method, path: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    pub fn auth_request(&self, method: Method, path: &str, token: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .body(Body::empty())
            .unwrap()
    }

    pub fn json_request(
        &self,
        method: Method,
        path: &str,
        token: &str,
        body: Value,
    ) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    pub fn basic_auth_request(
        &self,
        method: Method,
        path: &str,
        username: &str,
        password: &str,
    ) -> Request<Body> {
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", username, password));
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Basic {}", encoded))
            .body(Body::empty())
            .unwrap()
    }

    pub async fn call(&self, req: Request<Body>) -> (StatusCode, Value) {
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).to_string()))
        };
        (status, body)
    }

    /// Call returning raw bytes (for artifact/blob downloads).
    pub async fn call_raw(&self, req: Request<Body>) -> (StatusCode, Vec<u8>) {
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    /// Call returning the full response (for header inspection).
    pub async fn call_resp(&self, req: Request<Body>) -> axum::http::Response<axum::body::Body> {
        self.router.clone().oneshot(req).await.unwrap()
    }

    /// Create a test user via the API, returning the token for that user.
    pub async fn create_user_with_roles(&self, username: &str, password: &str, roles: Vec<&str>) {
        let admin_token = self.admin_token();
        let req = self.json_request(
            Method::POST,
            "/api/v1/users",
            &admin_token,
            json!({
                "username": username,
                "password": password,
                "roles": roles,
            }),
        );
        let (status, _) = self.call(req).await;
        assert!(
            status == StatusCode::CREATED || status == StatusCode::CONFLICT,
            "create_user_with_roles failed: {status}"
        );
    }

    /// Create a role via the API.
    pub async fn create_role_with_caps(&self, name: &str, capabilities: Vec<CapabilityGrant>) {
        let admin_token = self.admin_token();
        let caps_json: Vec<Value> = capabilities
            .iter()
            .map(|c| {
                json!({
                    "capability": c.capability,
                    "repo": c.repo,
                })
            })
            .collect();
        let req = self.json_request(
            Method::POST,
            "/api/v1/roles",
            &admin_token,
            json!({
                "name": name,
                "description": format!("Test role {}", name),
                "capabilities": caps_json,
            }),
        );
        let (status, _) = self.call(req).await;
        assert!(
            status == StatusCode::CREATED || status == StatusCode::CONFLICT,
            "create_role_with_caps failed: {status}"
        );
    }

    /// Create a hosted repo via the API (raw format by default).
    pub async fn create_hosted_repo(&self, name: &str) {
        let admin_token = self.admin_token();
        let req = self.json_request(
            Method::POST,
            "/api/v1/repositories",
            &admin_token,
            json!({
                "name": name,
                "repo_type": "hosted",
                "format": "raw",
                "store": "default",
            }),
        );
        let (status, _) = self.call(req).await;
        assert!(
            status == StatusCode::CREATED || status == StatusCode::CONFLICT,
            "create_hosted_repo failed: {status}"
        );
    }

    /// Create a repository with full config. Defaults `store` to `"default"` if omitted.
    pub async fn create_repo(&self, mut body: Value) {
        if body.get("store").is_none() {
            body["store"] = serde_json::Value::String("default".to_string());
        }
        let admin_token = self.admin_token();
        let req = self.json_request(Method::POST, "/api/v1/repositories", &admin_token, body);
        let (status, _) = self.call(req).await;
        assert!(
            status == StatusCode::CREATED || status == StatusCode::CONFLICT,
            "create_repo failed: {status}"
        );
    }

    /// Create a hosted repo of any format.
    pub async fn create_format_repo(&self, name: &str, format: &str) {
        self.create_repo(json!({
            "name": name,
            "repo_type": "hosted",
            "format": format,
            "store": "default",
        }))
        .await;
    }

    /// Create a cache repo of any format.
    pub async fn create_format_cache_repo(
        &self,
        name: &str,
        format: &str,
        upstream_url: &str,
        ttl: u64,
    ) {
        self.create_repo(json!({
            "name": name,
            "repo_type": "cache",
            "format": format,
            "upstream_url": upstream_url,
            "cache_ttl_secs": ttl,
            "store": "default",
        }))
        .await;
    }

    /// Create a proxy repo of any format.
    pub async fn create_format_proxy_repo(&self, name: &str, format: &str, members: Vec<&str>) {
        self.create_repo(json!({
            "name": name,
            "repo_type": "proxy",
            "format": format,
            "members": members,
            "store": "default",
        }))
        .await;
    }

    pub async fn create_docker_repo(&self, name: &str) {
        self.create_format_repo(name, "docker").await;
    }
    pub async fn create_docker_cache_repo(&self, name: &str, upstream_url: &str, ttl: u64) {
        self.create_format_cache_repo(name, "docker", upstream_url, ttl)
            .await;
    }
    pub async fn create_docker_proxy_repo(&self, name: &str, members: Vec<&str>) {
        self.create_format_proxy_repo(name, "docker", members).await;
    }

    pub async fn create_pypi_repo(&self, name: &str) {
        self.create_format_repo(name, "pypi").await;
    }
    pub async fn create_pypi_cache_repo(&self, name: &str, upstream_url: &str, ttl: u64) {
        self.create_format_cache_repo(name, "pypi", upstream_url, ttl)
            .await;
    }
    pub async fn create_pypi_proxy_repo(&self, name: &str, members: Vec<&str>) {
        self.create_format_proxy_repo(name, "pypi", members).await;
    }

    /// Upload a raw artifact.
    pub async fn upload_artifact(&self, repo: &str, path: &str, data: &[u8], token: &str) {
        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!("/repository/{}/{}", repo, path))
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(data.to_vec()))
            .unwrap();
        let (status, _) = self.call(req).await;
        assert_eq!(status, StatusCode::CREATED, "upload_artifact failed");
    }

    /// Push a Docker blob (monolithic upload). Returns the sha256 digest.
    pub async fn push_docker_blob(&self, repo: &str, data: &[u8]) -> String {
        let token = self.admin_token();
        let digest = sha256_digest(data);

        // Monolithic upload: POST with digest query param.
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/v2/{}/blobs/uploads/?digest={}", repo, digest))
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, data.len().to_string())
            .body(Body::from(data.to_vec()))
            .unwrap();
        let (status, _) = self.call(req).await;
        assert!(
            status == StatusCode::CREATED || status == StatusCode::ACCEPTED,
            "push_docker_blob failed: {status}"
        );
        digest
    }

    /// Push a Docker manifest. Returns the sha256 digest.
    pub async fn push_docker_manifest(
        &self,
        repo: &str,
        reference: &str,
        manifest_json: &Value,
    ) -> String {
        let token = self.admin_token();
        let body = serde_json::to_vec(manifest_json).unwrap();
        let digest = sha256_digest(&body);

        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!("/v2/{}/manifests/{}", repo, reference))
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .header(
                header::CONTENT_TYPE,
                "application/vnd.docker.distribution.manifest.v2+json",
            )
            .body(Body::from(body))
            .unwrap();
        let (status, _) = self.call(req).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "push_docker_manifest failed: {status}"
        );
        digest
    }

    /// Build a minimal Docker V2 manifest JSON.
    pub fn make_manifest(config_digest: &str, layer_digests: &[&str]) -> Value {
        json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "config": {
                "mediaType": "application/vnd.docker.container.image.v1+json",
                "size": 0,
                "digest": config_digest,
            },
            "layers": layer_digests.iter().map(|d| json!({
                "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
                "size": 0,
                "digest": d,
            })).collect::<Vec<_>>(),
        })
    }

    /// Create a TestApp with a dedicated Docker port router.
    pub async fn new_with_docker_port(repo_name: &str) -> Self {
        let TestAppSetup {
            dir,
            state,
            jwt_secret,
            admin_token_secret,
        } = test_app_setup().await;

        // Pre-create the Docker repo in KV.
        use depot_core::store::kv::{FormatConfig, RepoConfig, RepoKind};
        let now = depot_server::server::repo::now_utc();
        let repo_config = RepoConfig {
            schema_version: CURRENT_RECORD_VERSION,
            name: repo_name.to_string(),
            kind: RepoKind::Hosted,
            format_config: FormatConfig::Docker {
                listen: None,
                cleanup_untagged_manifests: None,
            },
            store: "default".to_string(),
            created_at: now,
            cleanup_max_unaccessed_days: None,
            cleanup_max_age_days: None,
            deleting: false,
        };
        service::put_repo(state.repo.kv.as_ref(), &repo_config)
            .await
            .unwrap();

        let router = build_docker_port_router(state.clone(), repo_name.to_string());

        Self {
            router,
            jwt_secret,
            admin_token_secret,
            state,
            _dir: dir,
        }
    }

    /// Push a Docker blob via two-segment (namespaced) route. Returns the sha256 digest.
    pub async fn push_docker_blob_ns(&self, repo: &str, image: &str, data: &[u8]) -> String {
        let token = self.admin_token();
        let digest = sha256_digest(data);

        let req = Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/v2/{}/{}/blobs/uploads/?digest={}",
                repo, image, digest
            ))
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, data.len().to_string())
            .body(Body::from(data.to_vec()))
            .unwrap();
        let (status, _) = self.call(req).await;
        assert!(
            status == StatusCode::CREATED || status == StatusCode::ACCEPTED,
            "push_docker_blob_ns failed: {status}"
        );
        digest
    }

    /// Push a Docker manifest via two-segment (namespaced) route. Returns the sha256 digest.
    pub async fn push_docker_manifest_ns(
        &self,
        repo: &str,
        image: &str,
        reference: &str,
        manifest_json: &Value,
    ) -> String {
        let token = self.admin_token();
        let body = serde_json::to_vec(manifest_json).unwrap();
        let digest = sha256_digest(&body);

        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!("/v2/{}/{}/manifests/{}", repo, image, reference))
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .header(
                header::CONTENT_TYPE,
                "application/vnd.docker.distribution.manifest.v2+json",
            )
            .body(Body::from(body))
            .unwrap();
        let (status, _) = self.call(req).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "push_docker_manifest_ns failed: {status}"
        );
        digest
    }

    /// Push a Docker blob via single-segment route on a dedicated Docker port.
    pub async fn push_docker_blob_port(&self, image: &str, data: &[u8]) -> String {
        let token = self.admin_token();
        let digest = sha256_digest(data);

        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/v2/{}/blobs/uploads/?digest={}", image, digest))
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, data.len().to_string())
            .body(Body::from(data.to_vec()))
            .unwrap();
        let (status, _) = self.call(req).await;
        assert!(
            status == StatusCode::CREATED || status == StatusCode::ACCEPTED,
            "push_docker_blob_port failed: {status}"
        );
        digest
    }

    /// Push a Docker manifest via single-segment route on a dedicated Docker port.
    pub async fn push_docker_manifest_port(
        &self,
        image: &str,
        reference: &str,
        manifest_json: &Value,
    ) -> String {
        let token = self.admin_token();
        let body = serde_json::to_vec(manifest_json).unwrap();
        let digest = sha256_digest(&body);

        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!("/v2/{}/manifests/{}", image, reference))
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .header(
                header::CONTENT_TYPE,
                "application/vnd.docker.distribution.manifest.v2+json",
            )
            .body(Body::from(body))
            .unwrap();
        let (status, _) = self.call(req).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "push_docker_manifest_port failed: {status}"
        );
        digest
    }

    pub async fn create_apt_repo(&self, name: &str) {
        self.create_format_repo(name, "apt").await;
    }
    pub async fn create_apt_cache_repo(&self, name: &str, upstream_url: &str, ttl: u64) {
        self.create_format_cache_repo(name, "apt", upstream_url, ttl)
            .await;
    }
    pub async fn create_apt_proxy_repo(&self, name: &str, members: Vec<&str>) {
        self.create_format_proxy_repo(name, "apt", members).await;
    }

    pub async fn create_helm_repo(&self, name: &str) {
        self.create_format_repo(name, "helm").await;
    }
    pub async fn create_helm_cache_repo(&self, name: &str, upstream_url: &str, ttl: u64) {
        self.create_format_cache_repo(name, "helm", upstream_url, ttl)
            .await;
    }
    pub async fn create_helm_proxy_repo(&self, name: &str, members: Vec<&str>) {
        self.create_format_proxy_repo(name, "helm", members).await;
    }

    /// Poll a task until it reaches a terminal status. Returns the final TaskInfo JSON.
    pub async fn wait_for_task(&self, task_id: &str) -> Value {
        let token = self.admin_token();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let req = self.auth_request(Method::GET, &format!("/api/v1/tasks/{}", task_id), &token);
            let (status, body) = self.call(req).await;
            assert_eq!(status, StatusCode::OK, "get task failed: {status}");
            let task_status = body["status"].as_str().unwrap_or("");
            if task_status == "completed" || task_status == "failed" || task_status == "cancelled" {
                return body;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "task did not complete within 10s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    pub async fn create_golang_repo(&self, name: &str) {
        self.create_format_repo(name, "golang").await;
    }
    pub async fn create_golang_cache_repo(&self, name: &str, upstream_url: &str, ttl: u64) {
        self.create_format_cache_repo(name, "golang", upstream_url, ttl)
            .await;
    }
    pub async fn create_golang_proxy_repo(&self, name: &str, members: Vec<&str>) {
        self.create_format_proxy_repo(name, "golang", members).await;
    }

    pub async fn create_yum_repo(&self, name: &str) {
        self.create_format_repo(name, "yum").await;
    }
    pub async fn create_yum_repo_with_depth(&self, name: &str, depth: u32) {
        self.create_repo(serde_json::json!({
            "name": name,
            "repo_type": "hosted",
            "format": "yum",
            "store": "default",
            "repodata_depth": depth,
        }))
        .await;
    }
    pub async fn create_yum_cache_repo(&self, name: &str, upstream_url: &str, ttl: u64) {
        self.create_format_cache_repo(name, "yum", upstream_url, ttl)
            .await;
    }
    pub async fn create_yum_proxy_repo(&self, name: &str, members: Vec<&str>) {
        self.create_format_proxy_repo(name, "yum", members).await;
    }

    pub async fn create_npm_repo(&self, name: &str) {
        self.create_format_repo(name, "npm").await;
    }
    pub async fn create_npm_cache_repo(&self, name: &str, upstream_url: &str, ttl: u64) {
        self.create_format_cache_repo(name, "npm", upstream_url, ttl)
            .await;
    }
    pub async fn create_npm_proxy_repo(&self, name: &str, members: Vec<&str>) {
        self.create_format_proxy_repo(name, "npm", members).await;
    }
}

/// Assert that accessing a non-existent repo returns 404.
pub async fn assert_nonexistent_repo_404(app: &TestApp, path: &str) {
    let req = app.auth_request(Method::GET, path, &app.admin_token());
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Assert that a format-specific URL shape on a Raw repo falls through to the
/// generic artifact lookup and 404s. Under the unified `/repository/{repo}/...`
/// router the URL shape is no longer tied to a format, so there is no 400
/// "wrong format" response — the request just looks for an artifact that
/// doesn't exist.
pub async fn assert_wrong_format_rejected(app: &TestApp, path: &str) {
    app.create_hosted_repo("raw-repo").await;
    let req = app.auth_request(Method::GET, path, &app.admin_token());
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
