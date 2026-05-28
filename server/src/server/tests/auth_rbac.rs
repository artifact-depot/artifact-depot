// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use serde_json::json;

use crate::server::store::{Capability, CapabilityGrant};

use depot_test_support::*;

// ===========================================================================
// 1. Login & Identity Resolution
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_login_valid() {
    let app = TestApp::new().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"username":"admin","password":"password"})).unwrap(),
        ))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["token"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_login_invalid_password() {
    let app = TestApp::new().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"username":"admin","password":"wrong"})).unwrap(),
        ))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_login_nonexistent_user() {
    let app = TestApp::new().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"username":"nobody","password":"x"})).unwrap(),
        ))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bearer_auth_valid() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/users", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bearer_auth_invalid_token() {
    let app = TestApp::new().await;
    let req = app.auth_request(Method::GET, "/api/v1/users", "garbage.token.here");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_basic_auth_valid() {
    let app = TestApp::new().await;
    let req = app.basic_auth_request(Method::GET, "/api/v1/users", "admin", "password");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_basic_auth_invalid() {
    let app = TestApp::new().await;
    let req = app.basic_auth_request(Method::GET, "/api/v1/users", "admin", "wrong");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_unknown_auth_scheme_rejected() {
    let app = TestApp::new().await;
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/users")
        .header(header::AUTHORIZATION, "Digest realm=test")
        .body(Body::empty())
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// 2. Anonymous Access
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_anonymous_denied_by_default() {
    let app = TestApp::new().await;
    // Anonymous has no roles, so list_repos returns 200 with empty list.
    let req = app.request(Method::GET, "/api/v1/repositories");
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_anonymous_admin_endpoints_denied() {
    let app = TestApp::new().await;
    let req = app.request(Method::GET, "/api/v1/users");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_anonymous_artifact_gets_basic_challenge() {
    let app = TestApp::new().await;
    app.create_hosted_repo("test-repo").await;
    let req = app.request(Method::GET, "/repository/test-repo/file.txt");
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok()),
        Some("Basic realm=\"Artifact Depot\""),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_authenticated_artifact_denied_stays_forbidden() {
    let app = TestApp::new().await;
    app.create_hosted_repo("test-repo").await;
    // Authenticated user with no roles -> denied on Read capability.
    app.create_user_with_roles("nobody", "pass123", vec![])
        .await;
    let req = app.basic_auth_request(
        Method::GET,
        "/repository/test-repo/file.txt",
        "nobody",
        "pass123",
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(resp.headers().get(header::WWW_AUTHENTICATE).is_none());
}

// ===========================================================================
// 3. User Management CRUD
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_users_as_admin() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/users", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let users = body.as_array().unwrap();
    let usernames: Vec<&str> = users
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect();
    assert!(usernames.contains(&"admin"));
    assert!(usernames.contains(&"anonymous"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_users_as_nonadmin() {
    let app = TestApp::new().await;
    app.create_user_with_roles("regular", "pass123", vec!["read-only"])
        .await;
    let token = app.token_for("regular").await;
    let req = app.auth_request(Method::GET, "/api/v1/users", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_user() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::POST,
        "/api/v1/users",
        &token,
        json!({"username":"newuser","password":"secret","roles":["read-only"]}),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["username"].as_str().unwrap(), "newuser");
    assert_eq!(body["roles"][0].as_str().unwrap(), "read-only");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_duplicate_user() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::POST,
        "/api/v1/users",
        &token,
        json!({"username":"admin","password":"x","roles":[]}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_user() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/users/admin", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["username"].as_str().unwrap(), "admin");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_update_user_roles() {
    let app = TestApp::new().await;
    app.create_user_with_roles("testuser", "pass", vec![]).await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::PUT,
        "/api/v1/users/testuser",
        &token,
        json!({"roles":["read-only"]}),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["roles"][0].as_str().unwrap(), "read-only");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_user() {
    let app = TestApp::new().await;
    app.create_user_with_roles("todelete", "pass", vec![]).await;
    let token = app.admin_token();
    let req = app.auth_request(Method::DELETE, "/api/v1/users/todelete", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_last_admin_prevented() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::DELETE, "/api/v1/users/admin", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// 4. Role Management CRUD
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_roles_as_admin() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/roles", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"admin"));
    assert!(names.contains(&"read-only"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_roles_as_nonadmin() {
    let app = TestApp::new().await;
    app.create_user_with_roles("regular2", "pass", vec!["read-only"])
        .await;
    let token = app.token_for("regular2").await;
    let req = app.auth_request(Method::GET, "/api/v1/roles", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_role() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::POST,
        "/api/v1/roles",
        &token,
        json!({
            "name": "custom-role",
            "description": "A custom role",
            "capabilities": [{"capability": "read", "repo": "my-repo"}]
        }),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"].as_str().unwrap(), "custom-role");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_duplicate_role() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::POST,
        "/api/v1/roles",
        &token,
        json!({"name":"admin","description":"dup"}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_update_role() {
    let app = TestApp::new().await;
    app.create_role_with_caps(
        "updatable",
        vec![CapabilityGrant {
            capability: Capability::Read,
            repo: "*".to_string(),
        }],
    )
    .await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::PUT,
        "/api/v1/roles/updatable",
        &token,
        json!({"capabilities": [{"capability":"write","repo":"*"}]}),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["capabilities"][0]["capability"], "write");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_role() {
    let app = TestApp::new().await;
    app.create_role_with_caps("deletable", vec![]).await;
    let token = app.admin_token();
    let req = app.auth_request(Method::DELETE, "/api/v1/roles/deletable", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_admin_role_prevented() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::DELETE, "/api/v1/roles/admin", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// 5. Repository Permission Enforcement
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_admin_sees_all_repos() {
    let app = TestApp::new().await;
    app.create_hosted_repo("repo-x").await;
    app.create_hosted_repo("repo-y").await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/repositories", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"repo-x"));
    assert!(names.contains(&"repo-y"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_readonly_user_sees_all_repos() {
    let app = TestApp::new().await;
    app.create_hosted_repo("repo-r1").await;
    app.create_hosted_repo("repo-r2").await;
    app.create_user_with_roles("reader", "pass", vec!["read-only"])
        .await;
    let token = app.token_for("reader").await;
    let req = app.auth_request(Method::GET, "/api/v1/repositories", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"repo-r1"));
    assert!(names.contains(&"repo-r2"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scoped_user_sees_only_permitted_repos() {
    let app = TestApp::new().await;
    app.create_hosted_repo("repo-a").await;
    app.create_hosted_repo("repo-b").await;
    app.create_role_with_caps(
        "repo-a-reader",
        vec![CapabilityGrant {
            capability: Capability::Read,
            repo: "repo-a".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("scoped", "pass", vec!["repo-a-reader"])
        .await;
    let token = app.token_for("scoped").await;
    let req = app.auth_request(Method::GET, "/api/v1/repositories", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"repo-a"));
    assert!(!names.contains(&"repo-b"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_repo_permitted() {
    let app = TestApp::new().await;
    app.create_hosted_repo("repo-a").await;
    app.create_role_with_caps(
        "ra-reader",
        vec![CapabilityGrant {
            capability: Capability::Read,
            repo: "repo-a".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("ra-user", "pass", vec!["ra-reader"])
        .await;
    let token = app.token_for("ra-user").await;
    let req = app.auth_request(Method::GET, "/api/v1/repositories/repo-a", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_repo_denied() {
    let app = TestApp::new().await;
    app.create_hosted_repo("repo-a").await;
    app.create_hosted_repo("repo-b").await;
    app.create_role_with_caps(
        "ra-only",
        vec![CapabilityGrant {
            capability: Capability::Read,
            repo: "repo-a".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("ra-only-user", "pass", vec!["ra-only"])
        .await;
    let token = app.token_for("ra-only-user").await;
    let req = app.auth_request(Method::GET, "/api/v1/repositories/repo-b", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_repo_admin_only() {
    let app = TestApp::new().await;
    app.create_user_with_roles("nonadmin", "pass", vec!["read-only"])
        .await;
    let token = app.token_for("nonadmin").await;
    let req = app.json_request(
        Method::POST,
        "/api/v1/repositories",
        &token,
        json!({"name":"forbidden-repo","repo_type":"hosted","format":"raw","store":"default"}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_repo_as_admin() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::POST,
        "/api/v1/repositories",
        &token,
        json!({"name":"new-repo","repo_type":"hosted","format":"raw","store":"default"}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_repo_admin_only() {
    let app = TestApp::new().await;
    app.create_hosted_repo("del-repo").await;
    app.create_user_with_roles("nonadmin2", "pass", vec!["read-only"])
        .await;
    let token = app.token_for("nonadmin2").await;
    let req = app.auth_request(Method::DELETE, "/api/v1/repositories/del-repo", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ===========================================================================
// 6. Artifact Permission Enforcement
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_artifact_with_write() {
    let app = TestApp::new().await;
    app.create_hosted_repo("write-repo").await;
    app.create_role_with_caps(
        "writer-role",
        vec![CapabilityGrant {
            capability: Capability::Write,
            repo: "write-repo".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("writer", "pass", vec!["writer-role"])
        .await;
    let token = app.token_for("writer").await;
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/repository/write-repo/file.txt")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(b"hello world".to_vec()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_artifact_without_write() {
    let app = TestApp::new().await;
    app.create_hosted_repo("nowrite-repo").await;
    app.create_role_with_caps(
        "readonly-nowrite",
        vec![CapabilityGrant {
            capability: Capability::Read,
            repo: "nowrite-repo".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("reader-nowrite", "pass", vec!["readonly-nowrite"])
        .await;
    let token = app.token_for("reader-nowrite").await;
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/repository/nowrite-repo/file.txt")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(b"hello".to_vec()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_artifact_with_read() {
    let app = TestApp::new().await;
    app.create_hosted_repo("read-repo").await;
    let admin_token = app.admin_token();
    app.upload_artifact("read-repo", "file.txt", b"test content", &admin_token)
        .await;

    app.create_role_with_caps(
        "read-repo-reader",
        vec![CapabilityGrant {
            capability: Capability::Read,
            repo: "read-repo".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("artifact-reader", "pass", vec!["read-repo-reader"])
        .await;
    let token = app.token_for("artifact-reader").await;
    let req = app.auth_request(Method::GET, "/repository/read-repo/file.txt", &token);
    let (status, data) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, b"test content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_artifact_without_read() {
    let app = TestApp::new().await;
    app.create_hosted_repo("noaccess-repo").await;
    let admin_token = app.admin_token();
    app.upload_artifact("noaccess-repo", "file.txt", b"secret", &admin_token)
        .await;

    app.create_user_with_roles("noperms", "pass", vec![]).await;
    let token = app.token_for("noperms").await;
    let req = app.auth_request(Method::GET, "/repository/noaccess-repo/file.txt", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_artifact_with_delete() {
    let app = TestApp::new().await;
    app.create_hosted_repo("del-art-repo").await;
    let admin_token = app.admin_token();
    app.upload_artifact("del-art-repo", "file.txt", b"data", &admin_token)
        .await;

    app.create_role_with_caps(
        "deleter-role",
        vec![CapabilityGrant {
            capability: Capability::Delete,
            repo: "del-art-repo".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("deleter", "pass", vec!["deleter-role"])
        .await;
    let token = app.token_for("deleter").await;
    let req = app.auth_request(Method::DELETE, "/repository/del-art-repo/file.txt", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_artifact_without_delete() {
    let app = TestApp::new().await;
    app.create_hosted_repo("nodelete-repo").await;
    let admin_token = app.admin_token();
    app.upload_artifact("nodelete-repo", "file.txt", b"data", &admin_token)
        .await;

    // Role has read+write but NOT delete.
    app.create_role_with_caps(
        "rw-no-delete",
        vec![
            CapabilityGrant {
                capability: Capability::Read,
                repo: "nodelete-repo".to_string(),
            },
            CapabilityGrant {
                capability: Capability::Write,
                repo: "nodelete-repo".to_string(),
            },
        ],
    )
    .await;
    app.create_user_with_roles("rw-user", "pass", vec!["rw-no-delete"])
        .await;
    let token = app.token_for("rw-user").await;
    let req = app.auth_request(Method::DELETE, "/repository/nodelete-repo/file.txt", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_artifacts_with_read() {
    let app = TestApp::new().await;
    app.create_hosted_repo("list-repo").await;
    let admin_token = app.admin_token();
    app.upload_artifact("list-repo", "a.txt", b"aaa", &admin_token)
        .await;

    app.create_role_with_caps(
        "list-reader",
        vec![CapabilityGrant {
            capability: Capability::Read,
            repo: "list-repo".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("list-user", "pass", vec!["list-reader"])
        .await;
    let token = app.token_for("list-user").await;
    let req = app.auth_request(
        Method::GET,
        "/api/v1/repositories/list-repo/artifacts",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let artifacts = body.get("artifacts").unwrap().as_array().unwrap();
    assert!(!artifacts.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_artifacts_without_read() {
    let app = TestApp::new().await;
    app.create_hosted_repo("nolist-repo").await;
    app.create_user_with_roles("nolist-user", "pass", vec![])
        .await;
    let token = app.token_for("nolist-user").await;
    let req = app.auth_request(
        Method::GET,
        "/api/v1/repositories/nolist-repo/artifacts",
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ===========================================================================
// 7. Wildcard vs Scoped Permissions
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_wildcard_read_grants_all_repos() {
    let app = TestApp::new().await;
    app.create_hosted_repo("wc-repo1").await;
    app.create_hosted_repo("wc-repo2").await;
    // read-only role has Read on "*"
    app.create_user_with_roles("wc-reader", "pass", vec!["read-only"])
        .await;
    let token = app.token_for("wc-reader").await;

    let req = app.auth_request(Method::GET, "/api/v1/repositories/wc-repo1", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(Method::GET, "/api/v1/repositories/wc-repo2", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scoped_read_only_grants_named_repo() {
    let app = TestApp::new().await;
    app.create_hosted_repo("scope-a").await;
    app.create_hosted_repo("scope-b").await;
    app.create_role_with_caps(
        "scope-a-read",
        vec![CapabilityGrant {
            capability: Capability::Read,
            repo: "scope-a".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("scope-user", "pass", vec!["scope-a-read"])
        .await;
    let token = app.token_for("scope-user").await;

    let req = app.auth_request(Method::GET, "/api/v1/repositories/scope-a", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(Method::GET, "/api/v1/repositories/scope-b", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multiple_roles_combine() {
    let app = TestApp::new().await;
    app.create_hosted_repo("multi-a").await;
    app.create_hosted_repo("multi-b").await;
    app.create_role_with_caps(
        "role-multi-a",
        vec![CapabilityGrant {
            capability: Capability::Read,
            repo: "multi-a".to_string(),
        }],
    )
    .await;
    app.create_role_with_caps(
        "role-multi-b",
        vec![CapabilityGrant {
            capability: Capability::Read,
            repo: "multi-b".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("multi-user", "pass", vec!["role-multi-a", "role-multi-b"])
        .await;
    let token = app.token_for("multi-user").await;

    let req = app.auth_request(Method::GET, "/api/v1/repositories/multi-a", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(Method::GET, "/api/v1/repositories/multi-b", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_write_on_specific_repo() {
    let app = TestApp::new().await;
    app.create_hosted_repo("wr-a").await;
    app.create_hosted_repo("wr-b").await;
    app.create_role_with_caps(
        "write-a-only",
        vec![CapabilityGrant {
            capability: Capability::Write,
            repo: "wr-a".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("wr-user", "pass", vec!["write-a-only"])
        .await;
    let token = app.token_for("wr-user").await;

    // Write to wr-a should succeed.
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/repository/wr-a/file.txt")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(b"data".to_vec()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Write to wr-b should fail.
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/repository/wr-b/file.txt")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(b"data".to_vec()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ===========================================================================
// 8. Change Password
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_change_own_password() {
    let app = TestApp::new().await;
    app.create_user_with_roles("pwuser", "oldpass", vec!["read-only"])
        .await;
    let token = app.token_for("pwuser").await;
    let req = app.json_request(
        Method::POST,
        "/api/v1/auth/change-password",
        &token,
        json!({
            "username": "pwuser",
            "current_password": "oldpass",
            "new_password": "newpass"
        }),
    );
    let (status, body) = app.call(req).await;
    // Self-change returns 200 with a fresh token so the caller stays logged in.
    assert_eq!(status, StatusCode::OK);
    assert!(body
        .get("token")
        .and_then(|t| t.as_str())
        .is_some_and(|t| !t.is_empty()));

    // Verify new password works for login.
    let login_req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"username":"pwuser","password":"newpass"})).unwrap(),
        ))
        .unwrap();
    let (status, _) = app.call(login_req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_change_password_wrong_current() {
    let app = TestApp::new().await;
    app.create_user_with_roles("pwuser2", "correct", vec!["read-only"])
        .await;
    let token = app.token_for("pwuser2").await;
    let req = app.json_request(
        Method::POST,
        "/api/v1/auth/change-password",
        &token,
        json!({
            "username": "pwuser2",
            "current_password": "wrong",
            "new_password": "newpass"
        }),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_admin_change_other_password() {
    let app = TestApp::new().await;
    app.create_user_with_roles("target", "targetpass", vec!["read-only"])
        .await;
    let admin_token = app.admin_token();
    // Admins can change another user's password without the current password.
    let req = app.json_request(
        Method::POST,
        "/api/v1/auth/change-password",
        &admin_token,
        json!({
            "username": "target",
            "new_password": "changed"
        }),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_nonadmin_change_other_password() {
    let app = TestApp::new().await;
    app.create_user_with_roles("user-a", "passa", vec!["read-only"])
        .await;
    app.create_user_with_roles("user-b", "passb", vec!["read-only"])
        .await;
    let token = app.token_for("user-a").await;
    let req = app.json_request(
        Method::POST,
        "/api/v1/auth/change-password",
        &token,
        json!({
            "username": "user-b",
            "current_password": "passb",
            "new_password": "hacked"
        }),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ===========================================================================
// 9. Upload Functionality
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_roundtrip() {
    let app = TestApp::new().await;
    app.create_hosted_repo("up-repo").await;
    let token = app.admin_token();
    let data = b"hello artifact world";

    app.upload_artifact("up-repo", "greeting.txt", data, &token)
        .await;

    let req = app.auth_request(Method::GET, "/repository/up-repo/greeting.txt", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_response_metadata() {
    let app = TestApp::new().await;
    app.create_hosted_repo("meta-repo").await;
    let token = app.admin_token();
    let data = b"some file content";

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/repository/meta-repo/docs/readme.txt")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(data.to_vec()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["path"].as_str().unwrap(), "docs/readme.txt");
    assert_eq!(body["size"].as_u64().unwrap(), data.len() as u64);
    assert_eq!(body["content_type"].as_str().unwrap(), "text/plain");
    assert!(body["blob_id"].as_str().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_nested_path() {
    let app = TestApp::new().await;
    app.create_hosted_repo("nested-repo").await;
    let token = app.admin_token();

    app.upload_artifact("nested-repo", "a/b/c/deep.bin", b"\x00\x01\x02", &token)
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/nested-repo/a/b/c/deep.bin",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"\x00\x01\x02");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_replaces_existing() {
    let app = TestApp::new().await;
    app.create_hosted_repo("replace-repo").await;
    let token = app.admin_token();

    app.upload_artifact("replace-repo", "file.txt", b"version 1", &token)
        .await;
    app.upload_artifact("replace-repo", "file.txt", b"version 2", &token)
        .await;

    let req = app.auth_request(Method::GET, "/repository/replace-repo/file.txt", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"version 2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_to_nonexistent_repo() {
    let app = TestApp::new().await;
    let token = app.admin_token();

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/repository/no-such-repo/file.txt")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(b"data".to_vec()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_ne!(status, StatusCode::CREATED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_visible_in_listing() {
    let app = TestApp::new().await;
    app.create_hosted_repo("list-up-repo").await;
    let token = app.admin_token();

    app.upload_artifact("list-up-repo", "dir/file1.txt", b"aaa", &token)
        .await;
    app.upload_artifact("list-up-repo", "dir/file2.txt", b"bbb", &token)
        .await;

    // Poll until the update worker has processed the dir entry events.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let req = app.auth_request(
            Method::GET,
            "/api/v1/repositories/list-up-repo/artifacts",
            &token,
        );
        let (status, body) = app.call(req).await;
        assert_eq!(status, StatusCode::OK);
        let dirs = body["dirs"].as_array().unwrap();
        let dir_names: Vec<&str> = dirs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        if dir_names.contains(&"dir") {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("dir entry 'dir' did not appear within 5s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // List with prefix — should see both files
    let req = app.auth_request(
        Method::GET,
        "/api/v1/repositories/list-up-repo/artifacts?prefix=dir/",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let artifacts = body["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_content_type_preserved() {
    let app = TestApp::new().await;
    app.create_hosted_repo("ct-repo").await;
    let token = app.admin_token();

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/repository/ct-repo/image.png")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "image/png")
        .body(Body::from(b"fake png data".to_vec()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["content_type"].as_str().unwrap(), "image/png");

    // Verify content-type header on download
    let req = app.auth_request(Method::GET, "/repository/ct-repo/image.png", &token);
    let resp = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "image/png"
    );
}

// ===========================================================================
// 10. Role CRUD — get, update, delete edge cases
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_role_found() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/roles/admin", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"].as_str().unwrap(), "admin");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_role_not_found() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/roles/nonexistent", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_update_role_description() {
    let app = TestApp::new().await;
    app.create_role_with_caps("desc-role", vec![]).await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::PUT,
        "/api/v1/roles/desc-role",
        &token,
        json!({"description": "Updated description"}),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["description"].as_str().unwrap(), "Updated description");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_update_role_not_found() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::PUT,
        "/api/v1/roles/ghost-role",
        &token,
        json!({"description": "nope"}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_role_not_found() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::DELETE, "/api/v1/roles/ghost-role", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_role_empty_name() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::POST,
        "/api/v1/roles",
        &token,
        json!({"name": "", "description": "empty name"}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// 11. User CRUD — edge cases
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_user_not_found() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/users/nonexistent", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_update_user_password() {
    let app = TestApp::new().await;
    app.create_user_with_roles("pwchange", "oldpw", vec!["read-only"])
        .await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::PUT,
        "/api/v1/users/pwchange",
        &token,
        json!({"password": "newpw"}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Verify login with new password works.
    let login = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"username":"pwchange","password":"newpw"})).unwrap(),
        ))
        .unwrap();
    let (status, _) = app.call(login).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_update_user_not_found() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::PUT,
        "/api/v1/users/ghost",
        &token,
        json!({"roles": ["read-only"]}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_user_empty_username() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.json_request(
        Method::POST,
        "/api/v1/users",
        &token,
        json!({"username": "", "password": "x"}),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_user_not_found() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::DELETE, "/api/v1/users/nonexistent", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_admin_allowed_when_multiple_admins() {
    let app = TestApp::new().await;
    // Create a second admin.
    app.create_user_with_roles("admin2", "pass", vec!["admin"])
        .await;
    let token = app.admin_token();
    // Now deleting the second admin should succeed.
    let req = app.auth_request(Method::DELETE, "/api/v1/users/admin2", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ===========================================================================
// 12. OpenAPI / Swagger endpoint
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_openapi_spec_available() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api-docs/openapi.json", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["openapi"].is_string());
    assert!(body["info"]["title"]
        .as_str()
        .unwrap()
        .contains("Artifact Depot"));
}
