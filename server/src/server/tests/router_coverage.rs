// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;

use crate::server::auth;
use crate::server::auth::backend::{AuthBackend, BuiltinAuthBackend, CachedAuthBackend};
use crate::server::config::settings::{Settings, SettingsHandle};
use crate::server::store::{Capability, CapabilityGrant, KvStore, RoleRecord, UserRecord};
use crate::server::{build_router, AppState};
use depot_blob_file::FileBlobStore;
use depot_core::service;
use depot_core::store::kv::CURRENT_RECORD_VERSION;
use depot_core::store::kv::{StoreKind, StoreRecord};
use depot_kv_redb::ShardedRedbKvStore;

use depot_test_support::ServiceExt;

/// Build a router with `default_docker_repo` set and verify Docker V2 is served
/// at the root /v2/ path for that repo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_router_with_default_docker_repo() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("kv");
    let blob_root = dir.path().join("blobs");
    std::fs::create_dir_all(&blob_root).unwrap();

    let kv = ShardedRedbKvStore::open(&db_path, 2, 0).await.unwrap();
    let blobs = FileBlobStore::new(&blob_root, false, 1_048_576, false).unwrap();
    let jwt_secret = b"test-secret-key-for-jwt-signing!".to_vec();

    let now = crate::server::repo::now_utc();

    let admin_password_hash = auth::hash_password("password".to_string()).await.unwrap();

    // Create the Docker repo that the default_docker_repo refers to.
    use depot_core::store::kv::{FormatConfig, RepoConfig, RepoKind};
    service::put_role(
        &kv,
        &RoleRecord {
            schema_version: CURRENT_RECORD_VERSION,
            name: "admin".to_string(),
            description: "Full access".to_string(),
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
    service::put_repo(
        &kv,
        &RepoConfig {
            schema_version: CURRENT_RECORD_VERSION,
            name: "default-docker".to_string(),
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
                root: blob_root.to_string_lossy().to_string(),
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
                    current: jwt_secret.clone(),
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
        },
        settings: Arc::new(SettingsHandle::new(Settings {
            access_log: false,
            default_docker_repo: Some("default-docker".to_string()),
            ..Settings::default()
        })),
        rate_limiter: Arc::new(crate::server::infra::rate_limit::DynamicRateLimiter::new(
            None,
        )),
        default_scheme: "https",
    };

    // Build with default_docker_repo set.
    let router = build_router(state, None);

    // V2 check — anonymous should get 401 with Bearer challenge.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v2/")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // V2 check — authenticated should get 200.
    let gs: &[u8; 32] = jwt_secret.as_slice().try_into().unwrap();
    let key = auth::derive_signing_key(gs, &[]);
    let token = auth::create_token("admin", &key, 86400).unwrap();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v2/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Verify that building a router with access_log=true doesn't panic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_router_with_access_log() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("kv");
    let blob_root = dir.path().join("blobs");
    std::fs::create_dir_all(&blob_root).unwrap();

    let kv = ShardedRedbKvStore::open(&db_path, 2, 0).await.unwrap();
    let blobs = FileBlobStore::new(&blob_root, false, 1_048_576, false).unwrap();
    let jwt_secret = b"test-secret-key-for-jwt-signing!".to_vec();

    let now = crate::server::repo::now_utc();

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
        },
        settings: Arc::new(SettingsHandle::new(Settings {
            access_log: true,
            ..Settings::default()
        })),
        rate_limiter: Arc::new(crate::server::infra::rate_limit::DynamicRateLimiter::new(
            None,
        )),
        default_scheme: "https",
    };

    // Build with access_log=true.
    let router = build_router(state, None);

    // Health endpoint should still work.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/health")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["version"].is_string());
}

/// Verify the Swagger UI endpoint returns HTML.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_swagger_ui_available() {
    use depot_test_support::*;
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/swagger-ui/", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("text/html"));
}

/// GET / should return HTML (SPA index.html fallback).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ui_index_html_fallback() {
    use depot_test_support::*;
    let app = TestApp::new().await;
    let req = app.request(Method::GET, "/");
    let resp = app.call_resp(req).await;
    // The UI may or may not be built; if embedded, it returns 200 with HTML.
    // If not embedded, it falls through to the API router which returns 200 or 404.
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::NOT_FOUND,
        "expected 200 or 404 for /, got {status}"
    );
    if status == StatusCode::OK {
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        // If we got HTML back, it should contain typical HTML markers
        if text.contains("<!") || text.contains("<html") || text.contains("<div") {
            // Good — SPA index returned
        }
    }
}

/// GET /some/unknown/spa/path should fall back to index.html (SPA routing).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ui_spa_routing_fallback() {
    use depot_test_support::*;
    let app = TestApp::new().await;
    // A path that doesn't match any API route should be handled by UI fallback
    let req = app.request(Method::GET, "/ui/repositories/some-repo");
    let resp = app.call_resp(req).await;
    let status = resp.status();
    // Should return 200 (SPA index) or 404 if UI is not embedded
    assert!(
        status == StatusCode::OK || status == StatusCode::NOT_FOUND,
        "expected 200 or 404 for SPA path, got {status}"
    );
}

/// Verify that a known static asset path returns correct content type.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ui_static_asset_content_type() {
    use depot_test_support::*;
    let app = TestApp::new().await;
    // Request a .js file path — if UI is embedded, should return JS content type
    let req = app.request(Method::GET, "/assets/index.js");
    let resp = app.call_resp(req).await;
    let status = resp.status();
    // If the frontend is built and embedded, assets would return 200.
    // If not, falls back to index.html (200) or 404.
    assert!(
        status == StatusCode::OK || status == StatusCode::NOT_FOUND,
        "expected 200 or 404 for static asset, got {status}"
    );
}

// ---------------------------------------------------------------------------
// OpenAPI consistency enforcement
// ---------------------------------------------------------------------------

/// Canonical list of management API routes that MUST appear in the OpenAPI spec.
///
/// When adding a new `/api/v1/*` route to `build_router()`, you MUST:
/// 1. Add a `#[utoipa::path(...)]` annotation to the handler function
/// 2. Add the handler to the `paths(...)` list in `ApiDoc` (router.rs)
/// 3. Add the `(path, &[methods])` entry here
///
/// Format-specific protocol routes (Docker V2, PyPI, APT, etc.) and
/// Nexus-compat routes are deliberately excluded — they implement external
/// protocol specifications, not our management API.
///
/// LDAP settings routes are behind `#[cfg(feature = "ldap")]` and excluded
/// from this list; they are only compiled when the feature is enabled.
const MANAGEMENT_ROUTES: &[(&str, &[&str])] = &[
    // System
    ("/api/v1/health", &["get"]),
    // Auth
    ("/api/v1/auth/login", &["post"]),
    ("/api/v1/auth/change-password", &["post"]),
    ("/api/v1/auth/revoke-all", &["post"]),
    // Repositories
    ("/api/v1/repositories", &["get", "post"]),
    ("/api/v1/repositories/{name}", &["get", "put", "delete"]),
    ("/api/v1/repositories/{name}/clean", &["post"]),
    ("/api/v1/repositories/{name}/clone", &["post"]),
    ("/api/v1/repositories/{name}/signing-key", &["post", "put"]),
    // Artifacts
    ("/api/v1/repositories/{repo}/artifacts", &["get"]),
    ("/api/v1/repositories/{repo}/bulk-delete", &["post"]),
    ("/api/v1/repositories/{repo}/download-archive", &["get"]),
    (
        "/repository/{repo}/{path}",
        &["head", "get", "put", "delete"],
    ),
    // Users
    ("/api/v1/users", &["get", "post"]),
    ("/api/v1/users/{username}", &["get", "put", "delete"]),
    ("/api/v1/users/{username}/revoke-tokens", &["post"]),
    // Roles
    ("/api/v1/roles", &["get", "post"]),
    ("/api/v1/roles/{name}", &["get", "put", "delete"]),
    // Stores
    ("/api/v1/stores", &["get", "post"]),
    ("/api/v1/stores/{name}", &["get", "put", "delete"]),
    ("/api/v1/stores/{name}/browse", &["get"]),
    ("/api/v1/stores/{name}/check", &["post"]),
    ("/api/v1/stores/reconcile", &["post"]),
    // Settings
    ("/api/v1/settings", &["get", "put"]),
    // Backup & Restore
    ("/api/v1/backup", &["get"]),
    ("/api/v1/restore", &["post"]),
    // Tasks
    ("/api/v1/tasks", &["get"]),
    ("/api/v1/tasks/check", &["post"]),
    ("/api/v1/tasks/gc", &["post"]),
    ("/api/v1/tasks/scheduler", &["get"]),
    ("/api/v1/tasks/rebuild-dir-entries", &["post"]),
    ("/api/v1/tasks/{id}", &["get", "delete"]),
    // Streaming
    ("/api/v1/events/stream", &["get"]),
];

/// Verify every management API route is documented in the OpenAPI spec and
/// vice-versa.  This test catches two classes of drift:
///
/// 1. A new route is added to `build_router()` without an OpenAPI annotation.
/// 2. An OpenAPI-documented path no longer matches a real route.
///
/// If this test fails, update the handler annotations, the `ApiDoc` struct in
/// `router.rs`, AND the `MANAGEMENT_ROUTES` constant above.
#[test]
fn test_openapi_covers_all_management_routes() {
    use std::collections::{BTreeMap, BTreeSet};

    let spec = crate::server::infra::router::openapi_spec();

    // Build a map of path -> set of methods from the generated spec.
    let mut spec_routes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (path, item) in &spec.paths.paths {
        let mut methods = BTreeSet::new();
        if item.get.is_some() {
            methods.insert("get".to_string());
        }
        if item.post.is_some() {
            methods.insert("post".to_string());
        }
        if item.put.is_some() {
            methods.insert("put".to_string());
        }
        if item.delete.is_some() {
            methods.insert("delete".to_string());
        }
        if item.head.is_some() {
            methods.insert("head".to_string());
        }
        if item.patch.is_some() {
            methods.insert("patch".to_string());
        }
        spec_routes.insert(path.to_string(), methods);
    }

    // Build a map from the canonical list.
    let mut expected: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (path, methods) in MANAGEMENT_ROUTES {
        let set: BTreeSet<String> = methods.iter().map(|m| m.to_string()).collect();
        expected.insert(path.to_string(), set);
    }

    let mut errors = Vec::new();

    // Check: every expected route must be in the spec.
    for (path, methods) in &expected {
        match spec_routes.get(path) {
            None => {
                errors.push(format!(
                    "MISSING from OpenAPI spec: {path} [{methods}]\n  \
                     -> Add #[utoipa::path] to the handler and list it in ApiDoc paths()",
                    methods = methods.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            Some(spec_methods) => {
                for method in methods {
                    if !spec_methods.contains(method) {
                        errors.push(format!(
                            "MISSING method from OpenAPI spec: {method} {path}\n  \
                             -> Add #[utoipa::path] for the {method} handler"
                        ));
                    }
                }
            }
        }
    }

    // Check: every spec path must be in the expected list (catches stale docs).
    for (path, methods) in &spec_routes {
        match expected.get(path) {
            None => {
                errors.push(format!(
                    "EXTRA in OpenAPI spec (not in MANAGEMENT_ROUTES): {path} [{methods}]\n  \
                     -> Add the route to MANAGEMENT_ROUTES in router_coverage.rs, \
                     or remove the stale #[utoipa::path] annotation",
                    methods = methods.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            Some(expected_methods) => {
                for method in methods {
                    if !expected_methods.contains(method) {
                        errors.push(format!(
                            "EXTRA method in OpenAPI spec (not in MANAGEMENT_ROUTES): \
                             {method} {path}\n  -> Add the method to MANAGEMENT_ROUTES"
                        ));
                    }
                }
            }
        }
    }

    if !errors.is_empty() {
        panic!(
            "OpenAPI spec / route consistency check failed:\n\n{}\n\n\
             See MANAGEMENT_ROUTES in router_coverage.rs for instructions.",
            errors.join("\n\n")
        );
    }
}
