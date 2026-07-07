// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use std::sync::Arc;

use crate::server::auth;
use crate::server::auth::backend::{AuthBackend, BuiltinAuthBackend, CachedAuthBackend};
use crate::server::config::settings::{Settings, SettingsHandle};
use crate::server::config::CorsConfig;
use crate::server::store::{Capability, CapabilityGrant, KvStore, RoleRecord, UserRecord};
use crate::server::{build_router, AppState};
use depot_blob_file::FileBlobStore;
use depot_core::service;
use depot_core::store::kv::{StoreKind, StoreRecord, CURRENT_RECORD_VERSION};
use depot_kv_redb::ShardedRedbKvStore;

use depot_test_support::ServiceExt;

/// Build an AppState + Router with the given CorsConfig.
async fn app_with_cors(cors: Option<CorsConfig>) -> axum::Router {
    let dir = crate::server::infra::test_support::test_tempdir();
    let db_path = dir.path().join("kv");
    let blob_root = dir.path().join("blobs");
    std::fs::create_dir_all(&blob_root).unwrap();

    let kv = ShardedRedbKvStore::open(&db_path, 2, 0).await.unwrap();
    let blobs = FileBlobStore::new(&blob_root, false, 1_048_576, false).unwrap();
    let jwt_secret = b"test-cors-secret-key-for-sign!!!".to_vec();

    let now = crate::server::repo::now_utc();

    let admin_password_hash = auth::hash_password("password".to_string()).await.unwrap();

    let blob_root_str = blob_root.to_string_lossy().to_string();
    service::put_role(
        &kv,
        &RoleRecord {
            schema_version: CURRENT_RECORD_VERSION,
            name: "admin".to_string(),
            description: "Full access".to_string(),
            capabilities: vec![CapabilityGrant {
                capability: Capability::Read,
                repo: "*".to_string(),
            }],
            created_at: now,
            updated_at: now,
        },
    )
    .await
    .unwrap();
    service::put_user(
        &kv,
        &UserRecord {
            schema_version: CURRENT_RECORD_VERSION,
            username: "anonymous".to_string(),
            password_hash: String::new(),
            roles: vec![],
            auth_source: "builtin".to_string(),
            token_secret: vec![],
            must_change_password: false,
            created_at: now,
            updated_at: now,
        },
    )
    .await
    .unwrap();
    service::put_user(
        &kv,
        &UserRecord {
            schema_version: CURRENT_RECORD_VERSION,
            username: "admin".to_string(),
            password_hash: admin_password_hash,
            roles: vec!["admin".to_string()],
            auth_source: "builtin".to_string(),
            token_secret: vec![],
            must_change_password: false,
            created_at: now,
            updated_at: now,
        },
    )
    .await
    .unwrap();
    service::put_store(
        &kv,
        &StoreRecord {
            schema_version: CURRENT_RECORD_VERSION,
            name: "default".to_string(),
            kind: StoreKind::File {
                root: blob_root_str,
                sync: false,
                io_size: 1024,
                direct_io: false,
            },
            created_at: now,
        },
    )
    .await
    .unwrap();

    let kv: Arc<dyn KvStore> = Arc::new(kv);
    let auth: Arc<dyn AuthBackend> = Arc::new(CachedAuthBackend::new(
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
    let event_bus = Arc::new(crate::server::infra::event_bus::EventBus::new(64));
    let tasks = Arc::new(crate::server::infra::task::TaskManager::new(
        kv.clone(),
        instance_id.clone(),
    ));
    let state = AppState {
        repo: crate::server::RepoServices {
            kv,
            stores,
            http: reqwest::Client::new(),
            inflight: depot_core::inflight::InflightMap::new(),
            updater: depot_core::update::UpdateSender::noop(),
        },
        auth: crate::server::AuthServices {
            backend: auth,
            jwt_secret: Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::server::auth::JwtSecretState {
                    current: jwt_secret,
                    previous: None,
                    rotated_at: 0,
                },
            )),
            #[cfg(feature = "ldap")]
            ldap_config: Arc::new(crate::server::auth::ldap::LdapConfigHandle::new(None)),
        },
        bg: crate::server::BackgroundServices {
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
            cors,
            ..Settings::default()
        })),
        rate_limiter: Arc::new(crate::server::infra::rate_limit::DynamicRateLimiter::new(
            None,
        )),
        default_scheme: "https",
    };

    // Leak the tempdir so it lives long enough for the router.
    std::mem::forget(dir);

    build_router(state, None)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_cors_config_no_cors_headers() {
    let router = app_with_cors(None).await;

    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/health")
        .header("Origin", "http://evil.com")
        .body(Body::empty())
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_bypasses_auth() {
    let cors = CorsConfig {
        allowed_origins: vec!["*".to_string()],
        allowed_methods: vec!["GET".to_string(), "POST".to_string()],
        allowed_headers: vec!["Authorization".to_string(), "Content-Type".to_string()],
        max_age_secs: 3600,
    };
    let router = app_with_cors(Some(cors)).await;

    // OPTIONS preflight — no Authorization header.
    let req = Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/v1/repositories")
        .header("Origin", "http://localhost:3000")
        .header("Access-Control-Request-Method", "GET")
        .header("Access-Control-Request-Headers", "Authorization")
        .body(Body::empty())
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    // Should NOT be 401 — CORS layer intercepts OPTIONS before auth.
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("access-control-allow-origin").is_some());
    assert!(resp.headers().get("access-control-allow-methods").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn specific_origin_allowed() {
    let cors = CorsConfig {
        allowed_origins: vec!["http://localhost:3000".to_string()],
        allowed_methods: vec!["GET".to_string()],
        allowed_headers: vec!["Authorization".to_string()],
        max_age_secs: 3600,
    };
    let router = app_with_cors(Some(cors)).await;

    // Matching origin should be reflected.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/health")
        .header("Origin", "http://localhost:3000")
        .body(Body::empty())
        .unwrap();

    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .unwrap()
            .to_str()
            .unwrap(),
        "http://localhost:3000"
    );

    // Non-matching origin should get no CORS header.
    let req2 = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/health")
        .header("Origin", "http://evil.com")
        .body(Body::empty())
        .unwrap();

    let resp2 = router.oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    assert!(resp2.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wildcard_origin_allows_any() {
    let cors = CorsConfig {
        allowed_origins: vec!["*".to_string()],
        allowed_methods: vec!["GET".to_string()],
        allowed_headers: vec!["Content-Type".to_string()],
        max_age_secs: 600,
    };
    let router = app_with_cors(Some(cors)).await;

    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/health")
        .header("Origin", "http://any-origin.example.com")
        .body(Body::empty())
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .unwrap()
            .to_str()
            .unwrap(),
        "*"
    );
}
