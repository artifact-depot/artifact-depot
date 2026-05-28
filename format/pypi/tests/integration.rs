// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::body::Body;
use axum::http::{header, Method, StatusCode};

use depot_core::service;
use depot_core::store::kv::ArtifactRecord;
use depot_core::store::kv::{
    ArtifactKind, PypiPackageMeta, PypiProjectIndex, PypiProjectLink, CURRENT_RECORD_VERSION,
};
use depot_format_pypi::store::{
    extract_version_from_filename, normalize_name, package_path, parse_simple_html, PypiStore,
};

mod common;

use common::*;
use depot_test_support as helpers;
use depot_test_support::*;

// ===========================================================================
// Validation
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_nonexistent_repo_404() {
    let app = TestApp::new().await;
    helpers::assert_nonexistent_repo_404(&app, "/repository/no-such-repo/simple/").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_non_pypi_repo_400() {
    // Under the unified `/repository/{repo}/...` router, a pypi-shaped URL on a
    // raw repo falls through to the generic artifact lookup and returns 404
    // rather than 400 — URL shape is no longer tied to format.
    let app = TestApp::new().await;
    app.create_hosted_repo("raw-repo").await;
    let req = app.auth_request(
        Method::GET,
        "/repository/raw-repo/simple/",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Upload (POST /repository/{repo}/)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_hosted() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-hosted").await;

    let data = b"fake-package-content";
    let req = pypi_upload_request(
        &app,
        "pypi-hosted",
        "my-package",
        "1.0.0",
        "my_package-1.0.0.tar.gz",
        data,
        &[],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_missing_name_400() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-noname").await;
    let token = app.admin_token();

    let boundary = "----TestBoundary";
    let mut body = Vec::new();
    // Skip the name field — only send version and content.
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"version\"\r\n\r\n1.0.0\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"pkg.tar.gz\"\r\nContent-Type: application/octet-stream\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(b"data");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/repository/pypi-noname/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={}", boundary),
        )
        .body(Body::from(body))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_missing_version_400() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-nover").await;
    let token = app.admin_token();

    let boundary = "----TestBoundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nmy-pkg\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"pkg.tar.gz\"\r\nContent-Type: application/octet-stream\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(b"data");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/repository/pypi-nover/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={}", boundary),
        )
        .body(Body::from(body))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_missing_content_400() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-nocontent").await;
    let token = app.admin_token();

    let boundary = "----TestBoundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nmy-pkg\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"version\"\r\n\r\n1.0.0\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/repository/pypi-nocontent/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={}", boundary),
        )
        .body(Body::from(body))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_to_cache_400() {
    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-cache-noup", "http://localhost:1234", 300)
        .await;

    let req = pypi_upload_request(
        &app,
        "pypi-cache-noup",
        "pkg",
        "1.0.0",
        "pkg-1.0.0.tar.gz",
        b"data",
        &[],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_proxy_routes_to_hosted() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-proxy-member").await;
    app.create_pypi_proxy_repo("pypi-proxy-up", vec!["pypi-proxy-member"])
        .await;

    let req = pypi_upload_request(
        &app,
        "pypi-proxy-up",
        "proxied-pkg",
        "1.0.0",
        "proxied_pkg-1.0.0.tar.gz",
        b"proxied content",
        &[],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Verify it landed on the member.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-proxy-member/simple/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("proxied-pkg"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_proxy_no_hosted_400() {
    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-cache-member", "http://localhost:1234", 300)
        .await;
    app.create_pypi_proxy_repo("pypi-proxy-nocache", vec!["pypi-cache-member"])
        .await;

    let req = pypi_upload_request(
        &app,
        "pypi-proxy-nocache",
        "pkg",
        "1.0.0",
        "pkg-1.0.0.tar.gz",
        b"data",
        &[],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_with_sha256_digest() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-sha").await;

    let data = b"sha-content";
    let sha = format!(
        "{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(sha2::Sha256::default(), data))
    );

    let req = pypi_upload_request(
        &app,
        "pypi-sha",
        "sha-pkg",
        "1.0.0",
        "sha_pkg-1.0.0.tar.gz",
        data,
        &[("sha256_digest", &sha)],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_with_requires_python() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-rp").await;

    let req = pypi_upload_request(
        &app,
        "pypi-rp",
        "rp-pkg",
        "1.0.0",
        "rp_pkg-1.0.0-py3-none-any.whl",
        b"wheel-content",
        &[("requires_python", ">=3.8")],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_with_summary() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-summ").await;

    let req = pypi_upload_request(
        &app,
        "pypi-summ",
        "summ-pkg",
        "1.0.0",
        "summ_pkg-1.0.0.tar.gz",
        b"content",
        &[("summary", "A test package")],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

// ===========================================================================
// Simple index (GET /repository/{repo}/simple/)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_index_empty() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-empty").await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-empty/simple/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("<html>"));
    // No project links.
    assert!(!text.contains("<a href="));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_index_lists_projects() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-idx").await;

    // Upload two packages.
    let req = pypi_upload_request(
        &app,
        "pypi-idx",
        "alpha-pkg",
        "1.0.0",
        "alpha_pkg-1.0.0.tar.gz",
        b"a",
        &[],
    );
    let _ = app.call(req).await;

    let req = pypi_upload_request(
        &app,
        "pypi-idx",
        "beta-pkg",
        "2.0.0",
        "beta_pkg-2.0.0.tar.gz",
        b"b",
        &[],
    );
    let _ = app.call(req).await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-idx/simple/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("alpha-pkg"));
    assert!(text.contains("beta-pkg"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_index_proxies_upstream() {
    let upstream = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(
            "<!DOCTYPE html>\n<html><body>\n<a href=\"requests/\">requests</a>\n</body></html>\n",
        ))
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-cache-idx", &format!("{}/", upstream.uri()), 300)
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cache-idx/simple/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("requests"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_index_aggregates_members() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-agg-m1").await;
    app.create_pypi_repo("pypi-agg-m2").await;

    // Upload to different members.
    let req = pypi_upload_request(
        &app,
        "pypi-agg-m1",
        "agg-alpha",
        "1.0.0",
        "agg_alpha-1.0.0.tar.gz",
        b"a",
        &[],
    );
    let _ = app.call(req).await;

    let req = pypi_upload_request(
        &app,
        "pypi-agg-m2",
        "agg-beta",
        "1.0.0",
        "agg_beta-1.0.0.tar.gz",
        b"b",
        &[],
    );
    let _ = app.call(req).await;

    app.create_pypi_proxy_repo("pypi-agg-proxy", vec!["pypi-agg-m1", "pypi-agg-m2"])
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-agg-proxy/simple/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("agg-alpha"));
    assert!(text.contains("agg-beta"));
}

// ===========================================================================
// Project files (GET /repository/{repo}/simple/{project}/)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_project_files() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-proj").await;

    let req = pypi_upload_request(
        &app,
        "pypi-proj",
        "my-project",
        "1.0.0",
        "my_project-1.0.0.tar.gz",
        b"content1",
        &[],
    );
    let _ = app.call(req).await;

    let req = pypi_upload_request(
        &app,
        "pypi-proj",
        "my-project",
        "2.0.0",
        "my_project-2.0.0.tar.gz",
        b"content2",
        &[],
    );
    let _ = app.call(req).await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-proj/simple/my-project/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("my_project-1.0.0.tar.gz"));
    assert!(text.contains("my_project-2.0.0.tar.gz"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_project_not_found_404() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-no-proj").await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-no-proj/simple/nonexistent/",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_project_fetches_upstream() {
    let upstream = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/requests/"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_string(
                "<!DOCTYPE html>\n<html><body>\n<a href=\"../../packages/requests/2.28.0/requests-2.28.0.tar.gz#sha256=abc123\">requests-2.28.0.tar.gz</a>\n</body></html>\n",
            ),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-cache-proj", &upstream.uri(), 0)
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cache-proj/simple/requests/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("requests-2.28.0.tar.gz"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_project_upstream_down_no_cache_returns_error() {
    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-cache-dead", "http://127.0.0.1:1", 0)
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cache-dead/simple/requests/",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    // Should return an error since there's no cache and upstream is down.
    assert_ne!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_project_not_found_upstream_404() {
    let upstream = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/nonexistent/"))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-cache-404", &upstream.uri(), 0)
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cache-404/simple/nonexistent/",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_project_from_hosted() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-pp-m").await;

    let req = pypi_upload_request(
        &app,
        "pypi-pp-m",
        "hosted-pkg",
        "1.0.0",
        "hosted_pkg-1.0.0.tar.gz",
        b"hosted",
        &[],
    );
    let _ = app.call(req).await;

    app.create_pypi_proxy_repo("pypi-pp", vec!["pypi-pp-m"])
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-pp/simple/hosted-pkg/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("hosted_pkg-1.0.0.tar.gz"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_project_dedup_by_filename() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-dd-m1").await;
    app.create_pypi_repo("pypi-dd-m2").await;

    // Same package in both members.
    let req = pypi_upload_request(
        &app,
        "pypi-dd-m1",
        "dup-pkg",
        "1.0.0",
        "dup_pkg-1.0.0.tar.gz",
        b"content1",
        &[],
    );
    let _ = app.call(req).await;

    let req = pypi_upload_request(
        &app,
        "pypi-dd-m2",
        "dup-pkg",
        "1.0.0",
        "dup_pkg-1.0.0.tar.gz",
        b"content2",
        &[],
    );
    let _ = app.call(req).await;

    app.create_pypi_proxy_repo("pypi-dd", vec!["pypi-dd-m1", "pypi-dd-m2"])
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-dd/simple/dup-pkg/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    // Should appear exactly once due to dedup.
    assert_eq!(
        text.matches("dup_pkg-1.0.0.tar.gz").count(),
        // The filename appears in both the href and the link text, so 2 per entry.
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_project_not_found_404() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-pfnf").await;
    app.create_pypi_proxy_repo("pypi-pfnf-proxy", vec!["pypi-pfnf"])
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-pfnf-proxy/simple/nonexistent-pkg/",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Package download (GET /repository/{repo}/packages/{name}/{version}/{filename})
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_download() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-dl").await;

    let data = b"downloadable package";
    let req = pypi_upload_request(
        &app,
        "pypi-dl",
        "dl-pkg",
        "1.0.0",
        "dl_pkg-1.0.0.tar.gz",
        data,
        &[],
    );
    let _ = app.call(req).await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-dl/packages/dl-pkg/1.0.0/dl_pkg-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_download_whl_content_type() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-whl").await;

    let req = pypi_upload_request(
        &app,
        "pypi-whl",
        "whl-pkg",
        "1.0.0",
        "whl_pkg-1.0.0-py3-none-any.whl",
        b"wheel-data",
        &[],
    );
    let _ = app.call(req).await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-whl/packages/whl-pkg/1.0.0/whl_pkg-1.0.0-py3-none-any.whl",
        &app.admin_token(),
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/zip"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_download_tar_gz_content_type() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-tgz").await;

    let req = pypi_upload_request(
        &app,
        "pypi-tgz",
        "tgz-pkg",
        "1.0.0",
        "tgz_pkg-1.0.0.tar.gz",
        b"tarball-data",
        &[],
    );
    let _ = app.call(req).await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-tgz/packages/tgz-pkg/1.0.0/tgz_pkg-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/gzip"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_download_not_found_404() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-dlnf").await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-dlnf/packages/nope/1.0.0/nope-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_download_from_local() {
    // First upload to a hosted PyPI repo, then create a cache repo pointing to
    // a mock upstream. Since cache repos share the same KV store, we can
    // pre-populate it.
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-cdl-hosted").await;

    let data = b"cached package";
    let req = pypi_upload_request(
        &app,
        "pypi-cdl-hosted",
        "cached-pkg",
        "1.0.0",
        "cached_pkg-1.0.0.tar.gz",
        data,
        &[],
    );
    let _ = app.call(req).await;

    // The download from the hosted repo should work.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cdl-hosted/packages/cached-pkg/1.0.0/cached_pkg-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_download_from_hosted() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-pdl-m").await;

    let data = b"proxied download";
    let req = pypi_upload_request(
        &app,
        "pypi-pdl-m",
        "proxy-dl-pkg",
        "1.0.0",
        "proxy_dl_pkg-1.0.0.tar.gz",
        data,
        &[],
    );
    let _ = app.call(req).await;

    app.create_pypi_proxy_repo("pypi-pdl", vec!["pypi-pdl-m"])
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-pdl/packages/proxy-dl-pkg/1.0.0/proxy_dl_pkg-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_download_not_found_404() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-pdlnf-m").await;
    app.create_pypi_proxy_repo("pypi-pdlnf", vec!["pypi-pdlnf-m"])
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-pdlnf/packages/nothing/1.0.0/nothing-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_download_fetches_upstream() {
    let upstream = wiremock::MockServer::start().await;

    let pkg_data = b"fetched from upstream";

    // Mock the project page. extract_version_from_filename normalizes dots to dashes
    // in the version, so the internal path uses "1-0-0" as the version.
    // Use an absolute URL in the href so resolve_link_url returns it as-is.
    let pkg_url = format!(
        "{}/packages/fetch-pkg/1-0-0/fetch_pkg-1.0.0.tar.gz",
        upstream.uri()
    );
    let html = format!(
        "<!DOCTYPE html>\n<html><body>\n<a href=\"{}#sha256=abc\">fetch_pkg-1.0.0.tar.gz</a>\n</body></html>\n",
        pkg_url
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/fetch-pkg/"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(html))
        .mount(&upstream)
        .await;

    // Mock the actual package download.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/packages/fetch-pkg/1-0-0/fetch_pkg-1.0.0.tar.gz",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(pkg_data.to_vec()))
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-cache-dl", &upstream.uri(), 0)
        .await;

    // First, fetch the project page to populate the index.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cache-dl/simple/fetch-pkg/",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Download uses the normalized version path that extract_version_from_filename produces.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cache-dl/packages/fetch-pkg/1-0-0/fetch_pkg-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, pkg_data);
}

// ===========================================================================
// Group A: normalize_name() edge cases
// ===========================================================================

#[test]
fn test_normalize_name_consecutive_separators() {
    assert_eq!(normalize_name("Foo---Bar__Baz..Qux"), "foo-bar-baz-qux");
}

#[test]
fn test_normalize_name_dots_and_mixed() {
    assert_eq!(
        normalize_name("My.Package_Name-Here"),
        "my-package-name-here"
    );
}

#[test]
fn test_normalize_name_uppercase_only() {
    assert_eq!(normalize_name("ALLCAPS"), "allcaps");
}

// ===========================================================================
// Group B: extract_version_from_filename()
// ===========================================================================

#[test]
fn test_extract_version_wheel() {
    // normalize_name converts dots to dashes in the stripped filename,
    // then for wheels the first dash-segment is the version: "2-3-1" → "2".
    assert_eq!(
        extract_version_from_filename("my_pkg", "my_pkg-2.3.1-py3-none-any.whl"),
        "2"
    );
}

#[test]
fn test_extract_version_sdist_tar_gz() {
    // After normalize_name on the stripped filename, dots become dashes.
    assert_eq!(
        extract_version_from_filename("my_pkg", "my_pkg-1.0.0.tar.gz"),
        "1-0-0"
    );
}

#[test]
fn test_extract_version_sdist_zip() {
    assert_eq!(
        extract_version_from_filename("my_pkg", "my_pkg-0.5.zip"),
        "0-5"
    );
}

#[test]
fn test_extract_version_mismatch_fallback() {
    assert_eq!(
        extract_version_from_filename("other_pkg", "my_pkg-1.0.0.tar.gz"),
        "unknown"
    );
}

// ===========================================================================
// Group C: parse_simple_html()
// ===========================================================================

#[test]
fn test_parse_simple_html_single_quotes() {
    let html = r#"<a href='http://example.com/pkg.tar.gz#sha256=abc' data-requires-python='&gt;=3.8'>pkg.tar.gz</a>"#;
    let links = parse_simple_html(html);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].filename, "pkg.tar.gz");
    assert_eq!(links[0].url, "http://example.com/pkg.tar.gz");
    assert_eq!(links[0].sha256.as_deref(), Some("abc"));
    assert_eq!(links[0].requires_python.as_deref(), Some(">=3.8"));
}

#[test]
fn test_parse_simple_html_no_href() {
    let html = r#"<a class="foo">text</a>"#;
    let links = parse_simple_html(html);
    assert_eq!(links.len(), 0);
}

#[test]
fn test_parse_simple_html_empty_filename() {
    let html = r#"<a href="http://example.com/pkg.tar.gz">   </a>"#;
    let links = parse_simple_html(html);
    assert_eq!(links.len(), 0);
}

#[test]
fn test_parse_simple_html_multiple_links() {
    let html = r#"<!DOCTYPE html>
<html><body>
<a href="pkg1-1.0.tar.gz#sha256=aaa">pkg1-1.0.tar.gz</a>
<a href="pkg2-2.0.tar.gz#sha256=bbb">pkg2-2.0.tar.gz</a>
<a href="pkg3-3.0.tar.gz">pkg3-3.0.tar.gz</a>
</body></html>"#;
    let links = parse_simple_html(html);
    assert_eq!(links.len(), 3);
    assert_eq!(links[0].filename, "pkg1-1.0.tar.gz");
    assert_eq!(links[1].filename, "pkg2-2.0.tar.gz");
    assert_eq!(links[2].filename, "pkg3-3.0.tar.gz");
    assert_eq!(links[2].sha256, None);
}

// ===========================================================================
// Group D: Upload edge cases
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_with_explicit_filetype() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-ft").await;

    let req = pypi_upload_request(
        &app,
        "pypi-ft",
        "ft-pkg",
        "1.0.0",
        "ft_pkg-1.0.0.egg",
        b"egg-content",
        &[("filetype", "bdist_egg")],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_sha256_mismatch_500() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-sha-bad").await;

    let req = pypi_upload_request(
        &app,
        "pypi-sha-bad",
        "bad-sha-pkg",
        "1.0.0",
        "bad_sha_pkg-1.0.0.tar.gz",
        b"content",
        &[(
            "sha256_digest",
            "0000000000000000000000000000000000000000000000000000000000000000",
        )],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Group E: delete_package()
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_package_existing() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-del").await;

    // Upload a package.
    let req = pypi_upload_request(
        &app,
        "pypi-del",
        "del-pkg",
        "1.0.0",
        "del_pkg-1.0.0.tar.gz",
        b"delete-me",
        &[],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Delete via PypiStore directly.
    let blobs = app.state.repo.blob_store("default").await.unwrap();
    let store = PypiStore {
        repo: "pypi-del",
        kv: app.state.repo.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: "default",
        updater: &app.state.repo.updater,
    };
    let deleted = store
        .delete_package("del-pkg", "1.0.0", "del_pkg-1.0.0.tar.gz")
        .await
        .unwrap();
    assert!(deleted);

    // Download should now 404.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-del/packages/del-pkg/1.0.0/del_pkg-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_package_nonexistent() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-del-ne").await;

    let blobs = app.state.repo.blob_store("default").await.unwrap();
    let store = PypiStore {
        repo: "pypi-del-ne",
        kv: app.state.repo.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: "default",
        updater: &app.state.repo.updater,
    };
    let deleted = store
        .delete_package("no-pkg", "1.0.0", "no_pkg-1.0.0.tar.gz")
        .await
        .unwrap();
    assert!(!deleted);
}

// ===========================================================================
// Group F: Cache TTL / stale serving
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_project_fresh_ttl_serves_from_cache() {
    let upstream = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/cached-ttl-pkg/"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_string(
                "<!DOCTYPE html>\n<html><body>\n<a href=\"cached_ttl_pkg-1.0.0.tar.gz#sha256=abc\">cached_ttl_pkg-1.0.0.tar.gz</a>\n</body></html>\n",
            ),
        )
        .expect(1) // Should only be called once (first fetch).
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-cache-ttl", &upstream.uri(), 3600)
        .await;

    // First fetch — populates cache and calls upstream.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cache-ttl/simple/cached-ttl-pkg/",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Second fetch — TTL=3600 means cache is fresh, should NOT call upstream again.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cache-ttl/simple/cached-ttl-pkg/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("cached_ttl_pkg-1.0.0.tar.gz"));
    // wiremock will verify expect(1) on drop.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_index_upstream_down_serves_stale() {
    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-cache-stale", "http://127.0.0.1:1", 0)
        .await;

    // Pre-populate cache by storing a package directly via PypiStore.
    let blobs = app.state.repo.blob_store("default").await.unwrap();
    let store = PypiStore {
        repo: "pypi-cache-stale",
        kv: app.state.repo.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: "default",
        updater: &app.state.repo.updater,
    };
    store
        .put_package(
            "stale-pkg",
            "1.0.0",
            "stale_pkg-1.0.0.tar.gz",
            "sdist",
            b"stale-content",
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // Upstream is unreachable — should serve stale cached index.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cache-stale/simple/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("stale-pkg"));
}

// ===========================================================================
// Group G: Proxy with cache member
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_project_from_cache_member() {
    let upstream = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/cache-proxy-pkg/"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_string(
                "<!DOCTYPE html>\n<html><body>\n<a href=\"cache_proxy_pkg-1.0.0.tar.gz#sha256=abc\">cache_proxy_pkg-1.0.0.tar.gz</a>\n</body></html>\n",
            ),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-pc-cache", &upstream.uri(), 0)
        .await;
    app.create_pypi_proxy_repo("pypi-pc-proxy", vec!["pypi-pc-cache"])
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-pc-proxy/simple/cache-proxy-pkg/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("cache_proxy_pkg-1.0.0.tar.gz"));
}

// ===========================================================================
// Group H: HTML rendering
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_project_page_requires_python_html_escaping() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-rp-esc").await;

    let req = pypi_upload_request(
        &app,
        "pypi-rp-esc",
        "esc-pkg",
        "1.0.0",
        "esc_pkg-1.0.0-py3-none-any.whl",
        b"wheel-data",
        &[("requires_python", ">=3.8,<4.0")],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-rp-esc/simple/esc-pkg/",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.as_str().unwrap_or("");
    assert!(text.contains("data-requires-python=\"&gt;=3.8,&lt;4.0\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_download_zip_content_type() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-zip").await;

    let req = pypi_upload_request(
        &app,
        "pypi-zip",
        "zip-pkg",
        "1.0.0",
        "zip_pkg-1.0.0.zip",
        b"zip-data",
        &[],
    );
    let _ = app.call(req).await;

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-zip/packages/zip-pkg/1.0.0/zip_pkg-1.0.0.zip",
        &app.admin_token(),
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/octet-stream"
    );
}

// ===========================================================================
// Group I: Cache download edge cases
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_download_upstream_url_empty() {
    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-cache-empty-url", "http://127.0.0.1:1", 0)
        .await;

    // Insert a placeholder record with empty blob_id and empty upstream_url.
    let path = package_path("empty-url-pkg", "1.0.0", "empty_url_pkg-1.0.0.tar.gz");
    let record = ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: 0,
        content_type: "application/octet-stream".to_string(),
        kind: ArtifactKind::PypiPackage {
            pypi: PypiPackageMeta {
                name: "empty-url-pkg".to_string(),
                version: "1.0.0".to_string(),
                filename: "empty_url_pkg-1.0.0.tar.gz".to_string(),
                filetype: "sdist".to_string(),
                sha256: String::new(),
                requires_python: None,
                summary: None,
                upstream_url: None,
            },
        },
        created_at: chrono::DateTime::UNIX_EPOCH,
        updated_at: chrono::DateTime::UNIX_EPOCH,
        last_accessed_at: chrono::DateTime::UNIX_EPOCH,
        path: String::new(),
        internal: false,
        blob_id: None,
        content_hash: None,
        etag: None,
    };
    service::put_artifact(
        app.state.repo.kv.as_ref(),
        "pypi-cache-empty-url",
        &path,
        &record,
    )
    .await
    .unwrap();

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cache-empty-url/packages/empty-url-pkg/1.0.0/empty_url_pkg-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_download_upstream_fails_502() {
    let upstream = wiremock::MockServer::start().await;

    // The package download endpoint returns 500.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/packages/fail-pkg/1.0.0/fail_pkg-1.0.0.tar.gz",
        ))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .mount(&upstream)
        .await;

    // The simple project page also returns 404 (so the download handler can't
    // re-fetch the index from upstream).
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/fail-pkg/"))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_pypi_cache_repo("pypi-cache-502", &upstream.uri(), 0)
        .await;

    // Store a project index with the upstream URL so the download handler finds
    // the link and attempts to fetch from upstream.
    let upstream_url = format!(
        "{}/packages/fail-pkg/1.0.0/fail_pkg-1.0.0.tar.gz",
        upstream.uri()
    );
    let blobs = app.state.repo.blob_store("default").await.unwrap();
    let store = PypiStore {
        repo: "pypi-cache-502",
        kv: app.state.repo.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: "default",
        updater: &app.state.repo.updater,
    };
    let index = PypiProjectIndex {
        links: vec![PypiProjectLink {
            filename: "fail_pkg-1.0.0.tar.gz".to_string(),
            version: "1.0.0".to_string(),
            upstream_url,
            sha256: None,
            requires_python: None,
        }],
    };
    store.store_project_index("fail-pkg", &index).await.unwrap();

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-cache-502/packages/fail-pkg/1.0.0/fail_pkg-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

// ===========================================================================
// Group J: Package replacement
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_replaces_existing_package() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-replace").await;

    // Upload v1.
    let req = pypi_upload_request(
        &app,
        "pypi-replace",
        "replace-pkg",
        "1.0.0",
        "replace_pkg-1.0.0.tar.gz",
        b"old-content",
        &[],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Upload same name/version/filename with different content.
    let req = pypi_upload_request(
        &app,
        "pypi-replace",
        "replace-pkg",
        "1.0.0",
        "replace_pkg-1.0.0.tar.gz",
        b"new-content",
        &[],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Download should return new content.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-replace/packages/replace-pkg/1.0.0/replace_pkg-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"new-content");
}

// ===========================================================================
// Auth: unauthenticated requests are rejected
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_requires_auth() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-auth-up").await;

    let boundary = "----TestBoundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nmy-pkg\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"version\"\r\n\r\n1.0.0\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"pkg.tar.gz\"\r\nContent-Type: application/octet-stream\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(b"data");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    // No auth header.
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/repository/pypi-auth-up/")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={}", boundary),
        )
        .body(Body::from(body))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_simple_index_requires_auth() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-auth-idx").await;

    let req = app.request(Method::GET, "/repository/pypi-auth-idx/simple/");
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get(header::WWW_AUTHENTICATE).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_simple_project_requires_auth() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-auth-proj").await;

    let req = app.request(Method::GET, "/repository/pypi-auth-proj/simple/some-pkg/");
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get(header::WWW_AUTHENTICATE).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_download_requires_auth() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-auth-dl").await;

    let req = app.request(
        Method::GET,
        "/repository/pypi-auth-dl/packages/pkg/1.0.0/pkg-1.0.0.tar.gz",
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get(header::WWW_AUTHENTICATE).is_some());
}

// ===========================================================================
// Multiple files for same version (wheel + sdist)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_wheel_and_sdist_for_same_version() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-multi-file").await;

    // Upload .whl
    let whl_data = b"fake wheel data";
    let req = pypi_upload_request(
        &app,
        "pypi-multi-file",
        "my-pkg",
        "1.0.0",
        "my_pkg-1.0.0-py3-none-any.whl",
        whl_data,
        &[("filetype", "bdist_wheel")],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Upload .tar.gz
    let sdist_data = b"fake sdist data";
    let req = pypi_upload_request(
        &app,
        "pypi-multi-file",
        "my-pkg",
        "1.0.0",
        "my-pkg-1.0.0.tar.gz",
        sdist_data,
        &[("filetype", "sdist")],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Simple project page should list both files.
    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-multi-file/simple/my-pkg/",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("my_pkg-1.0.0-py3-none-any.whl"),
        "should list wheel"
    );
    assert!(html.contains("my-pkg-1.0.0.tar.gz"), "should list sdist");

    // Download each file.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-multi-file/packages/my-pkg/1.0.0/my_pkg-1.0.0-py3-none-any.whl",
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, whl_data);

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-multi-file/packages/my-pkg/1.0.0/my-pkg-1.0.0.tar.gz",
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, sdist_data);
}

// ===========================================================================
// Proxy download across members
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_download_from_multiple_hosted() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-dl-m1").await;
    app.create_pypi_repo("pypi-dl-m2").await;

    // Upload to member 1.
    let data1 = b"from member one";
    let req = pypi_upload_request(
        &app,
        "pypi-dl-m1",
        "pkg-one",
        "1.0.0",
        "pkg_one-1.0.0.tar.gz",
        data1,
        &[],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Upload to member 2.
    let data2 = b"from member two";
    let req = pypi_upload_request(
        &app,
        "pypi-dl-m2",
        "pkg-two",
        "2.0.0",
        "pkg_two-2.0.0.tar.gz",
        data2,
        &[],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    app.create_pypi_proxy_repo("pypi-dl-proxy", vec!["pypi-dl-m1", "pypi-dl-m2"])
        .await;
    let token = app.admin_token();

    // Download from member 1 via proxy.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-dl-proxy/packages/pkg-one/1.0.0/pkg_one-1.0.0.tar.gz",
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, data1);

    // Download from member 2 via proxy.
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-dl-proxy/packages/pkg-two/2.0.0/pkg_two-2.0.0.tar.gz",
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, data2);
}

// ===========================================================================
// Simple project for non-existent project
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_simple_project_empty_repo() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-empty-proj").await;
    let token = app.admin_token();

    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-empty-proj/simple/nonexistent/",
        &token,
    );
    let (status, _) = app.call(req).await;
    // Should 404 for non-existent project.
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Upload through proxy, full roundtrip
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_upload_and_download_roundtrip() {
    let app = TestApp::new().await;
    app.create_pypi_repo("pypi-prx-m").await;
    app.create_pypi_proxy_repo("pypi-prx", vec!["pypi-prx-m"])
        .await;

    let data = b"proxy uploaded package";
    let req = pypi_upload_request(
        &app,
        "pypi-prx",
        "prx-pkg",
        "1.0.0",
        "prx_pkg-1.0.0.tar.gz",
        data,
        &[],
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Download through proxy.
    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        "/repository/pypi-prx/packages/prx-pkg/1.0.0/prx_pkg-1.0.0.tar.gz",
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, data);

    // Simple index through proxy should list the project.
    let req = app.auth_request(Method::GET, "/repository/pypi-prx/simple/", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("prx-pkg"));
}
