// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! End-to-end test that resources created from `[initialization]` are visible
//! through the same REST API the operator would use to manage them.

use axum::http::{Method, StatusCode};
use depot_test_support::*;

use depot_core::store::kv::{ArtifactFormat, Capability, CapabilityGrant, RepoType};
use depot_server::server::api::repositories::CreateRepoRequest;
use depot_server::server::api::roles::CreateRoleRequest;
use depot_server::server::api::users::CreateUserRequest;
use depot_server::server::config::init::{apply_initialization, InitializationConfig};
use depot_server::server::config::settings::Settings;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_init_visible_through_rest_api() {
    // TestApp pre-creates the default store, admin/anonymous users, and the
    // admin/read-only roles. We layer init on top to add a custom role,
    // repository, user, and a settings override — exactly what an edge-cache
    // or demo TOML would declare on a fresh KV.
    let app = TestApp::new().await;

    let init = InitializationConfig {
        roles: vec![CreateRoleRequest {
            name: "developer".into(),
            description: "rw on hosted-raw".into(),
            capabilities: vec![
                CapabilityGrant {
                    capability: Capability::Read,
                    repo: "*".into(),
                },
                CapabilityGrant {
                    capability: Capability::Write,
                    repo: "hosted-raw".into(),
                },
            ],
        }],
        stores: vec![],
        repositories: vec![CreateRepoRequest {
            name: "hosted-raw".into(),
            repo_type: RepoType::Hosted,
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
            max_artifact_bytes: 12345,
            ..Settings::default()
        }),
    };

    let summary = apply_initialization(&app.state, &init).await.unwrap();
    assert_eq!(summary.roles_created, 1);
    assert_eq!(summary.repos_created, 1);
    assert_eq!(summary.users_created, 1);
    assert!(summary.settings_overridden);

    let admin_token = app.admin_token();

    // Role visible via REST.
    let req = app.auth_request(Method::GET, "/api/v1/roles", &admin_token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"developer"),
        "developer role not in {names:?}"
    );

    // Repo visible via REST.
    let req = app.auth_request(Method::GET, "/api/v1/repositories/hosted-raw", &admin_token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "hosted-raw");
    assert_eq!(body["repo_type"], "hosted");
    assert_eq!(body["format"], "raw");

    // User visible via REST.
    let req = app.auth_request(Method::GET, "/api/v1/users/ci", &admin_token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["username"], "ci");
    assert_eq!(body["roles"], serde_json::json!(["developer"]));

    // Settings override visible via REST.
    let req = app.auth_request(Method::GET, "/api/v1/settings", &admin_token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["max_artifact_bytes"], 12345);

    // The init-created user can authenticate (verifies password was hashed).
    let req = app.basic_auth_request(Method::GET, "/api/v1/auth/me", "ci", "hunter2");
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK, "ci login failed: {body}");
}
