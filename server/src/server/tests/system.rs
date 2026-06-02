// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::http::{Method, StatusCode};

use depot_test_support::*;

// ===========================================================================
// System / Health
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_health_returns_ok() {
    let app = TestApp::new().await;
    let req = app.request(Method::GET, "/api/v1/health");
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"].as_str().unwrap(), "ok");
    assert!(body["version"].as_str().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_health_no_auth_required() {
    let app = TestApp::new().await;
    // No auth header at all — health should still work.
    let req = app.request(Method::GET, "/api/v1/health");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_health_version_matches_build_version() {
    let app = TestApp::new().await;
    let req = app.request(Method::GET, "/api/v1/health");
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let version = body["version"].as_str().unwrap();
    // The reported version is the full build version (crate version + build
    // counter), computed in build.rs and exposed as `crate::VERSION`.
    assert_eq!(version, crate::VERSION);
    // It always starts with the bare crate version; in a git checkout the
    // build counter is appended as `+<n>`.
    assert!(
        version == env!("CARGO_PKG_VERSION")
            || version.starts_with(concat!(env!("CARGO_PKG_VERSION"), "+")),
        "version {version:?} should be the crate version, optionally with a +counter suffix"
    );
}
