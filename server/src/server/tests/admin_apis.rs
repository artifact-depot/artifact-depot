// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::body::Body;
use axum::http::{Method, StatusCode};
use serde_json::json;

use depot_test_support::*;

// ---------------------------------------------------------------------------
// S3 test server helper
// ---------------------------------------------------------------------------

/// Start an in-process S3 server backed by s3s-fs.  Returns the endpoint URL
/// and the TempDir that must be held alive for the server's lifetime.
async fn start_s3s_server() -> (String, tempfile::TempDir) {
    use hyper_util::rt::TokioIo;
    use s3s::service::S3ServiceBuilder;
    use tokio::net::TcpListener;

    const BUCKET: &str = "test-bucket";

    let s3_root = tempfile::tempdir().expect("create s3 root");
    std::fs::create_dir_all(s3_root.path().join(BUCKET)).expect("create bucket dir");

    let fs = s3s_fs::FileSystem::new(s3_root.path()).expect("s3s_fs::FileSystem::new");
    let service = {
        let mut b = S3ServiceBuilder::new(fs);
        b.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        b.build()
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(_) => break,
            };
            let svc = service.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection(io, svc)
                .await;
            });
        }
    });

    (format!("http://127.0.0.1:{}", addr.port()), s3_root)
}

// ===========================================================================
// Stores CRUD
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_store_not_found() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/stores/nonexistent", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_and_delete_file_store() {
    let app = TestApp::new().await;
    let token = app.admin_token();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_str().unwrap();

    // Create a file store.
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/api/v1/stores")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", token),
        )
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "name": "test-store",
                "store_type": "file",
                "root": root,
            }))
            .unwrap(),
        ))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED, "create store: {body}");
    assert_eq!(body["name"], "test-store");
    assert_eq!(body["store_type"], "file");

    // Verify it appears in list.
    let req = app.auth_request(Method::GET, "/api/v1/stores", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let stores = body.as_array().unwrap();
    assert!(
        stores.iter().any(|s| s["name"] == "test-store"),
        "new store should appear in list"
    );

    // Get by name.
    let req = app.auth_request(Method::GET, "/api/v1/stores/test-store", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "test-store");

    // Delete it.
    let req = app.auth_request(Method::DELETE, "/api/v1/stores/test-store", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify it's gone.
    let req = app.auth_request(Method::GET, "/api/v1/stores/test-store", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_store_duplicate_rejected() {
    let app = TestApp::new().await;
    let token = app.admin_token();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_str().unwrap();

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/api/v1/stores")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", token),
        )
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "name": "dup-store",
                "store_type": "file",
                "root": root,
            }))
            .unwrap(),
        ))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Try to create again — should conflict.
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/api/v1/stores")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", token),
        )
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "name": "dup-store",
                "store_type": "file",
                "root": root,
            }))
            .unwrap(),
        ))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_store_in_use_rejected() {
    let app = TestApp::new().await;
    let token = app.admin_token();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_str().unwrap();

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/api/v1/stores")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", token),
        )
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "name": "busy-store",
                "store_type": "file",
                "root": root,
            }))
            .unwrap(),
        ))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Create a repo that uses the new store.
    app.create_repo(json!({
        "name": "repo-on-busy",
        "repo_type": "hosted",
        "format": "raw",
        "store": "busy-store",
    }))
    .await;

    // Try to delete — should fail because repo uses it.
    let req = app.auth_request(Method::DELETE, "/api/v1/stores/busy-store", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// ===========================================================================
// Settings API
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_settings() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/settings", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    // Should have some known fields.
    assert!(body["max_docker_chunk_count"].is_number());
    // gc_min_interval_secs should be present with correct default.
    assert_eq!(
        body["gc_min_interval_secs"], 3600,
        "gc_min_interval_secs default: {body}"
    );
    assert_eq!(body["gc_max_orphan_candidates"], 1_000_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_settings() {
    let app = TestApp::new().await;
    let token = app.admin_token();

    // Get current settings.
    let req = app.auth_request(Method::GET, "/api/v1/settings", &token);
    let (status, current) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Modify and PUT back.
    let mut updated = current.clone();
    updated["max_docker_chunk_count"] = json!(500);

    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/api/v1/settings")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", token),
        )
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&updated).unwrap()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK, "put settings: {body}");
    assert_eq!(body["max_docker_chunk_count"], 500);

    // Verify it persisted.
    let req = app.auth_request(Method::GET, "/api/v1/settings", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max_docker_chunk_count"], 500);
}

// ===========================================================================
// S3 store retry/timeout settings
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_s3_store_with_retry_settings() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let (endpoint, _s3_dir) = start_s3s_server().await;

    let (status, body) = app
        .call(app.json_request(
            Method::POST,
            "/api/v1/stores",
            &token,
            json!({
                "name": "s3-retry",
                "store_type": "s3",
                "bucket": "test-bucket",
                "endpoint": endpoint,
                "region": "us-east-1",
                "access_key": "test",
                "secret_key": "test",
                "max_retries": 5,
                "connect_timeout_secs": 10,
                "read_timeout_secs": 60,
                "retry_mode": "adaptive",
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create s3 store: {body}");
    assert_eq!(body["store_type"], "s3");
    assert_eq!(body["max_retries"], 5);
    assert_eq!(body["connect_timeout_secs"], 10);
    assert_eq!(body["read_timeout_secs"], 60);
    assert_eq!(body["retry_mode"], "adaptive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_s3_store_default_retry_settings() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let (endpoint, _s3_dir) = start_s3s_server().await;

    // Create without specifying retry fields — should get defaults.
    let (status, body) = app
        .call(app.json_request(
            Method::POST,
            "/api/v1/stores",
            &token,
            json!({
                "name": "s3-defaults",
                "store_type": "s3",
                "bucket": "test-bucket",
                "endpoint": endpoint,
                "region": "us-east-1",
                "access_key": "test",
                "secret_key": "test",
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create s3 store: {body}");
    assert_eq!(body["max_retries"], 3);
    assert_eq!(body["connect_timeout_secs"], 3);
    assert_eq!(body["read_timeout_secs"], 0);
    assert_eq!(body["retry_mode"], "standard");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_update_s3_store_retry_settings() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let (endpoint, _s3_dir) = start_s3s_server().await;

    // Create with defaults.
    let (status, body) = app
        .call(app.json_request(
            Method::POST,
            "/api/v1/stores",
            &token,
            json!({
                "name": "s3-update",
                "store_type": "s3",
                "bucket": "test-bucket",
                "endpoint": endpoint,
                "region": "us-east-1",
                "access_key": "test",
                "secret_key": "test",
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create: {body}");
    assert_eq!(body["retry_mode"], "standard");

    // Update retry settings.
    let (status, body) = app
        .call(app.json_request(
            Method::PUT,
            "/api/v1/stores/s3-update",
            &token,
            json!({
                "max_retries": 7,
                "retry_mode": "adaptive",
                "read_timeout_secs": 60,
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "update: {body}");
    assert_eq!(body["max_retries"], 7);
    assert_eq!(body["retry_mode"], "adaptive");
    assert_eq!(body["read_timeout_secs"], 60);
    // Unchanged fields should be preserved.
    assert_eq!(body["connect_timeout_secs"], 3);
    assert_eq!(body["bucket"], "test-bucket");
    assert_eq!(body["region"], "us-east-1");

    // GET and verify persistence.
    let (status, body) = app
        .call(app.auth_request(Method::GET, "/api/v1/stores/s3-update", &token))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max_retries"], 7);
    assert_eq!(body["retry_mode"], "adaptive");
    assert_eq!(body["read_timeout_secs"], 60);
    assert_eq!(body["connect_timeout_secs"], 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_s3_store_retry_validation() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let (endpoint, _s3_dir) = start_s3s_server().await;

    // max_retries > 20
    let (status, _) = app
        .call(app.json_request(
            Method::POST,
            "/api/v1/stores",
            &token,
            json!({
                "name": "s3-bad1",
                "store_type": "s3",
                "bucket": "test-bucket",
                "endpoint": &endpoint,
                "access_key": "test",
                "secret_key": "test",
                "max_retries": 25,
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "max_retries > 20");

    // connect_timeout_secs = 0
    let (status, _) = app
        .call(app.json_request(
            Method::POST,
            "/api/v1/stores",
            &token,
            json!({
                "name": "s3-bad2",
                "store_type": "s3",
                "bucket": "test-bucket",
                "endpoint": &endpoint,
                "access_key": "test",
                "secret_key": "test",
                "connect_timeout_secs": 0,
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "connect_timeout = 0");

    // read_timeout_secs > 7200
    let (status, _) = app
        .call(app.json_request(
            Method::POST,
            "/api/v1/stores",
            &token,
            json!({
                "name": "s3-bad3",
                "store_type": "s3",
                "bucket": "test-bucket",
                "endpoint": &endpoint,
                "access_key": "test",
                "secret_key": "test",
                "read_timeout_secs": 9999,
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "read_timeout > 7200");

    // Invalid retry_mode
    let (status, _) = app
        .call(app.json_request(
            Method::POST,
            "/api/v1/stores",
            &token,
            json!({
                "name": "s3-bad4",
                "store_type": "s3",
                "bucket": "test-bucket",
                "endpoint": &endpoint,
                "access_key": "test",
                "secret_key": "test",
                "retry_mode": "exponential",
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "bad retry_mode");

    // Validation on update path too — create a valid store first.
    let (status, _) = app
        .call(app.json_request(
            Method::POST,
            "/api/v1/stores",
            &token,
            json!({
                "name": "s3-valid",
                "store_type": "s3",
                "bucket": "test-bucket",
                "endpoint": &endpoint,
                "access_key": "test",
                "secret_key": "test",
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = app
        .call(app.json_request(
            Method::PUT,
            "/api/v1/stores/s3-valid",
            &token,
            json!({ "max_retries": 25 }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "update max_retries > 20");

    let (status, _) = app
        .call(app.json_request(
            Method::PUT,
            "/api/v1/stores/s3-valid",
            &token,
            json!({ "retry_mode": "bogus" }),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "update bad retry_mode");
}

// ===========================================================================
// Partial-update semantics (absent = preserve, null = clear)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_settings_partial_body_preserves_absent_fields() {
    let app = TestApp::new().await;
    let token = app.admin_token();

    // Establish non-default values for two unrelated fields.
    let req = app.json_request(
        Method::PUT,
        "/api/v1/settings",
        &token,
        json!({"default_docker_repo": "docker", "base_url": "https://depot.example.com"}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // A partial PUT naming only gc_interval_secs must not disturb them.
    // (A body exactly like this one nulled default_docker_repo in production.)
    let req = app.json_request(
        Method::PUT,
        "/api/v1/settings",
        &token,
        json!({"gc_interval_secs": 43200}),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["gc_interval_secs"], 43200);
    assert_eq!(body["default_docker_repo"], "docker");
    assert_eq!(body["base_url"], "https://depot.example.com");

    // Explicit null clears.
    let req = app.json_request(
        Method::PUT,
        "/api/v1/settings",
        &token,
        json!({"default_docker_repo": null}),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["default_docker_repo"], serde_json::Value::Null);
    assert_eq!(body["base_url"], "https://depot.example.com");

    // Merged documents are still validated as a whole.
    let req = app.json_request(
        Method::PUT,
        "/api/v1/settings",
        &token,
        json!({"gc_start_time": "not a time"}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Non-object bodies are rejected.
    let req = app.json_request(Method::PUT, "/api/v1/settings", &token, json!([1, 2]));
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_update_repo_partial_body_preserves_absent_fields() {
    let app = TestApp::new().await;
    let token = app.admin_token();

    app.create_repo(json!({
        "name": "part-cache",
        "format": "docker",
        "repo_type": "cache",
        "upstream_url": "https://upstream.example.com",
        "cache_ttl_secs": 300,
        "upstream_auth": {"username": "pull-user", "password": "s3cret"},
        "cleanup_max_age_days": 30,
        "cleanup_max_unaccessed_days": 14,
    }))
    .await;

    // Reproduces the production wipe: a partial update naming only an
    // unrelated field used to null upstream_auth and both cleanup policies.
    let req = app.json_request(
        Method::PUT,
        "/api/v1/repositories/part-cache",
        &token,
        json!({"cache_ttl_secs": 600}),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["cache_ttl_secs"], 600);
    assert_eq!(body["upstream_auth"]["username"], "pull-user");
    assert_eq!(body["cleanup_max_age_days"], 30);
    assert_eq!(body["cleanup_max_unaccessed_days"], 14);

    // The stored password (not just the masked response) must survive.
    let stored = depot_core::service::get_repo(app.state.repo.kv.as_ref(), "part-cache")
        .await
        .unwrap()
        .unwrap();
    let depot_core::store::kv::RepoKind::Cache { upstream_auth, .. } = &stored.kind else {
        panic!("expected cache repo");
    };
    assert_eq!(
        upstream_auth.as_ref().and_then(|a| a.password.as_deref()),
        Some("s3cret")
    );

    // The masked-password sentinel still round-trips without clobbering it.
    let req = app.json_request(
        Method::PUT,
        "/api/v1/repositories/part-cache",
        &token,
        json!({"upstream_auth": {"username": "pull-user2", "password": "********"}}),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stored = depot_core::service::get_repo(app.state.repo.kv.as_ref(), "part-cache")
        .await
        .unwrap()
        .unwrap();
    let depot_core::store::kv::RepoKind::Cache { upstream_auth, .. } = &stored.kind else {
        panic!("expected cache repo");
    };
    assert_eq!(
        upstream_auth.as_ref().map(|a| a.username.as_str()),
        Some("pull-user2")
    );
    assert_eq!(
        upstream_auth.as_ref().and_then(|a| a.password.as_deref()),
        Some("s3cret")
    );

    // Explicit nulls clear exactly the named fields.
    let req = app.json_request(
        Method::PUT,
        "/api/v1/repositories/part-cache",
        &token,
        json!({"upstream_auth": null, "cleanup_max_unaccessed_days": null}),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["upstream_auth"], serde_json::Value::Null);
    assert_eq!(body["cleanup_max_unaccessed_days"], serde_json::Value::Null);
    assert_eq!(body["cleanup_max_age_days"], 30);

    // Zero is still rejected.
    let req = app.json_request(
        Method::PUT,
        "/api/v1/repositories/part-cache",
        &token,
        json!({"cleanup_max_age_days": 0}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_task_endpoints_require_admin() {
    let app = TestApp::new().await;
    app.create_user_with_roles("task-viewer", "password", vec!["read-only"])
        .await;
    let token = app.token_for("task-viewer").await;

    for (method, path) in [
        (Method::GET, "/api/v1/tasks"),
        (Method::GET, "/api/v1/tasks/scheduler"),
        (Method::POST, "/api/v1/tasks/gc"),
        (Method::POST, "/api/v1/tasks/check"),
        (Method::POST, "/api/v1/tasks/rebuild-dir-entries"),
        (
            Method::GET,
            "/api/v1/tasks/00000000-0000-0000-0000-000000000000",
        ),
        (
            Method::DELETE,
            "/api/v1/tasks/00000000-0000-0000-0000-000000000000",
        ),
    ] {
        let req = app.auth_request(method.clone(), path, &token);
        let (status, _) = app.call(req).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {path} must require admin"
        );
    }

    // Admin still has full access.
    let admin = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/tasks", &admin);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}
