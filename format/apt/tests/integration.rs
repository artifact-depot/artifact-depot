// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};

use depot_format_apt::store::build_synthetic_deb;

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
    helpers::assert_nonexistent_repo_404(&app, "/repository/no-such-repo/dists/stable/Release")
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_non_apt_format_rejected() {
    let app = TestApp::new().await;
    helpers::assert_wrong_format_rejected(&app, "/repository/raw-repo/dists/stable/Release").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_to_cache_rejected() {
    let app = TestApp::new().await;
    app.create_apt_cache_repo("apt-cache", "http://localhost:19999/apt/fake", 300)
        .await;

    let deb = build_synthetic_deb("hello", "1.0", "amd64", "test").unwrap();
    let token = app.admin_token();
    let req = apt_upload_request("apt-cache", "hello_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Hosted upload + metadata (core happy path)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_and_packages() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-hosted").await;

    let deb = build_synthetic_deb("hello", "1.0", "amd64", "A test package").unwrap();
    let token = app.admin_token();
    let req = apt_upload_request("apt-hosted", "hello_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Fetch Packages
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-hosted/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let packages = String::from_utf8(body.to_vec()).unwrap();

    assert!(packages.contains("Package: hello"), "missing Package field");
    assert!(packages.contains("Version: 1.0"), "missing Version field");
    assert!(
        packages.contains("Architecture: amd64"),
        "missing Architecture field"
    );
    assert!(packages.contains("Size:"), "missing Size field");
    assert!(packages.contains("SHA256:"), "missing SHA256 field");
    assert!(packages.contains("Filename:"), "missing Filename field");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_and_release() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-rel").await;

    let deb = build_synthetic_deb("hello", "1.0", "amd64", "test").unwrap();
    let token = app.admin_token();
    let req = apt_upload_request("apt-rel", "hello_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-rel/dists/stable/Release",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let release = String::from_utf8(body.to_vec()).unwrap();

    assert!(release.contains("Suite:"), "missing Suite");
    assert!(release.contains("Codename:"), "missing Codename");
    assert!(release.contains("Components:"), "missing Components");
    assert!(release.contains("Architectures:"), "missing Architectures");
    assert!(release.contains("SHA256:"), "missing SHA256 checksums");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_and_inrelease() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-inrel").await;

    let deb = build_synthetic_deb("hello", "1.0", "amd64", "test").unwrap();
    let token = app.admin_token();
    let req = apt_upload_request("apt-inrel", "hello_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-inrel/dists/stable/InRelease",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let inrelease = String::from_utf8(body.to_vec()).unwrap();

    // InRelease should contain GPG-signed content (BEGIN PGP) or plain Release content
    assert!(
        inrelease.contains("Suite:") || inrelease.contains("BEGIN PGP"),
        "InRelease should contain Suite or PGP signature"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_packages_gz() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-gz").await;

    let deb = build_synthetic_deb("hello", "1.0", "amd64", "test").unwrap();
    let token = app.admin_token();
    let req = apt_upload_request("apt-gz", "hello_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Get plain Packages
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-gz/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (_, plain_body) = app.call_raw(req).await;

    // Get Packages.gz
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-gz/dists/stable/main/binary-amd64/Packages.gz",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let gz_bytes = resp.into_body().collect().await.unwrap().to_bytes();

    // Decompress and compare
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut decoder = GzDecoder::new(&gz_bytes[..]);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).unwrap();

    assert_eq!(
        decompressed, plain_body,
        "Packages.gz should decompress to match Packages"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pool_download() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-pool").await;

    let deb = build_synthetic_deb("hello", "1.0", "amd64", "test").unwrap();
    let token = app.admin_token();
    let req = apt_upload_request("apt-pool", "hello_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Read Packages to find the pool path
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-pool/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (_, packages_bytes) = app.call_raw(req).await;
    let packages = String::from_utf8(packages_bytes).unwrap();

    // Extract pool path from "Filename: pool/..." line
    let filename_line = packages
        .lines()
        .find(|l| l.starts_with("Filename:"))
        .expect("Filename field not found in Packages");
    let pool_path = filename_line.trim_start_matches("Filename:").trim();

    // Download from pool
    let req = app.auth_request(
        Method::GET,
        &format!("/repository/apt-pool/{}", pool_path),
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, deb, "downloaded .deb should match uploaded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multiple_packages() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-multi").await;
    let token = app.admin_token();

    let deb1 = build_synthetic_deb("alpha", "1.0", "amd64", "first").unwrap();
    let req = apt_upload_request("apt-multi", "alpha_1.0_amd64.deb", &deb1, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let deb2 = build_synthetic_deb("beta", "2.0", "amd64", "second").unwrap();
    let req = apt_upload_request("apt-multi", "beta_2.0_amd64.deb", &deb2, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-multi/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (_, body) = app.call_raw(req).await;
    let packages = String::from_utf8(body).unwrap();

    assert!(packages.contains("Package: alpha"), "missing alpha");
    assert!(packages.contains("Package: beta"), "missing beta");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_package_replacement() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-replace").await;
    let token = app.admin_token();

    let deb1 = build_synthetic_deb("hello", "1.0", "amd64", "first version").unwrap();
    let req = apt_upload_request("apt-replace", "hello_1.0_amd64.deb", &deb1, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let deb2 = build_synthetic_deb("hello", "2.0", "amd64", "second version").unwrap();
    let req = apt_upload_request("apt-replace", "hello_2.0_amd64.deb", &deb2, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-replace/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (_, body) = app.call_raw(req).await;
    let packages = String::from_utf8(body).unwrap();

    assert!(
        packages.contains("Version: 2.0"),
        "should contain the second version"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_public_key() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-key").await;
    let token = app.admin_token();

    // Upload a .deb to trigger key generation
    let deb = build_synthetic_deb("hello", "1.0", "amd64", "test").unwrap();
    let req = apt_upload_request("apt-key", "hello_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(Method::GET, "/repository/apt-key/public.key", &token);
    let resp = app.call_resp(req).await;
    // Key may or may not be available depending on whether signing was set up
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::NOT_FOUND,
        "expected 200 or 404, got {status}"
    );
    if status == StatusCode::OK {
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let key_text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            key_text.contains("PGP PUBLIC KEY"),
            "should contain PGP public key block"
        );
    }
}

// ===========================================================================
// Architecture handling
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multi_arch() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-arch").await;
    let token = app.admin_token();

    let deb_amd64 = build_synthetic_deb("hello", "1.0", "amd64", "amd64 pkg").unwrap();
    let req = apt_upload_request("apt-arch", "hello_1.0_amd64.deb", &deb_amd64, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let deb_arm64 = build_synthetic_deb("hello", "1.0", "arm64", "arm64 pkg").unwrap();
    let req = apt_upload_request("apt-arch", "hello_1.0_arm64.deb", &deb_arm64, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Check Release lists both architectures
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-arch/dists/stable/Release",
        &token,
    );
    let (_, body) = app.call_raw(req).await;
    let release = String::from_utf8(body).unwrap();
    assert!(release.contains("amd64"), "Release should list amd64");
    assert!(release.contains("arm64"), "Release should list arm64");

    // Check each arch has its own Packages
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-arch/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-arch/dists/stable/main/binary-arm64/Packages",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_arch_all_merged() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-all").await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("common-data", "1.0", "all", "arch-independent").unwrap();
    let req = apt_upload_request("apt-all", "common-data_1.0_all.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // arch:all should appear in binary-amd64 Packages
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-all/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (_, body) = app.call_raw(req).await;
    let packages = String::from_utf8(body).unwrap();
    assert!(
        packages.contains("Package: common-data"),
        "arch:all package should appear in binary-amd64 Packages"
    );
}

// ===========================================================================
// Components
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_custom_component() {
    let app = TestApp::new().await;
    // Create repo with "contrib" in its component list
    app.create_repo(serde_json::json!({
        "name": "apt-comp",
        "repo_type": "hosted",
        "format": "apt",
    }))
    .await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("extra-pkg", "1.0", "amd64", "contrib package").unwrap();
    let req = apt_upload_request(
        "apt-comp",
        "extra-pkg_1.0_amd64.deb",
        &deb,
        Some("contrib"),
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // The pool path should be under contrib
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-comp/dists/stable/contrib/binary-amd64/Packages",
        &token,
    );
    let (_, body) = app.call_raw(req).await;
    let packages = String::from_utf8(body).unwrap();
    assert!(
        packages.contains("Package: extra-pkg"),
        "package should be in contrib component"
    );
    assert!(
        packages.contains("pool/contrib/"),
        "pool path should reference contrib"
    );
}

// ===========================================================================
// Proxy repo
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_merged_packages() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-h1").await;
    app.create_apt_repo("apt-h2").await;
    app.create_apt_proxy_repo("apt-proxy", vec!["apt-h1", "apt-h2"])
        .await;
    let token = app.admin_token();

    let deb1 = build_synthetic_deb("pkg-one", "1.0", "amd64", "from h1").unwrap();
    let req = apt_upload_request("apt-h1", "pkg-one_1.0_amd64.deb", &deb1, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let deb2 = build_synthetic_deb("pkg-two", "1.0", "amd64", "from h2").unwrap();
    let req = apt_upload_request("apt-h2", "pkg-two_1.0_amd64.deb", &deb2, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-proxy/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (_, body) = app.call_raw(req).await;
    let packages = String::from_utf8(body).unwrap();

    assert!(packages.contains("Package: pkg-one"), "missing pkg-one");
    assert!(packages.contains("Package: pkg-two"), "missing pkg-two");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_pool_download() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-ph").await;
    app.create_apt_proxy_repo("apt-pp", vec!["apt-ph"]).await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("proxied", "1.0", "amd64", "via proxy").unwrap();
    let req = apt_upload_request("apt-ph", "proxied_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Read pool path from member's Packages
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-ph/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (_, body) = app.call_raw(req).await;
    let packages = String::from_utf8(body).unwrap();
    let filename_line = packages
        .lines()
        .find(|l| l.starts_with("Filename:"))
        .expect("Filename field not found");
    let pool_path = filename_line.trim_start_matches("Filename:").trim();

    // Download through proxy
    let req = app.auth_request(
        Method::GET,
        &format!("/repository/apt-pp/{}", pool_path),
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, deb, "proxy download should match original");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_upload_routes_to_member() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-write-target").await;
    app.create_apt_proxy_repo("apt-write-proxy", vec!["apt-write-target"])
        .await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("routed", "1.0", "amd64", "via proxy write").unwrap();
    let req = apt_upload_request(
        "apt-write-proxy",
        "routed_1.0_amd64.deb",
        &deb,
        None,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Verify the package ended up in the hosted member
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-write-target/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (_, body) = app.call_raw(req).await;
    let packages = String::from_utf8(body).unwrap();
    assert!(
        packages.contains("Package: routed"),
        "upload through proxy should land in hosted member"
    );
}

// ===========================================================================
// Error paths
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_missing_file_field() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-nofile").await;
    let token = app.admin_token();

    // Build multipart with only a component field, no file field
    let boundary = "----TestBoundary12345";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"component\"\r\n\r\nmain\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let req = Request::builder()
        .method(Method::POST)
        .uri("/repository/apt-nofile/upload")
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
async fn test_upload_invalid_deb() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-invalid").await;
    let token = app.admin_token();

    let garbage = b"this is not a deb file at all";
    let req = apt_upload_request(
        "apt-invalid",
        "garbage_1.0_amd64.deb",
        garbage,
        None,
        &token,
    );
    let (status, _) = app.call(req).await;
    // Invalid deb data triggers a parse error (400 or 500 depending on where parsing fails)
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::INTERNAL_SERVER_ERROR,
        "expected 400 or 500 for invalid deb, got {status}"
    );
}

// ===========================================================================
// Snapshot tests
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_snapshot_creation() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-snap").await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("snap-pkg", "1.0", "amd64", "snapshot test").unwrap();
    let req = apt_upload_request("apt-snap", "snap-pkg_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Trigger metadata rebuild by fetching Packages
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-snap/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);

    // Create snapshot
    let req = app.json_request(
        Method::POST,
        "/repository/apt-snap/snapshots",
        &token,
        serde_json::json!({ "source": "stable", "name": "stable-20260320" }),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Snapshot should be accessible via dists path
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-snap/dists/stable-20260320/Release",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let release = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        release.contains("Suite: stable-20260320"),
        "snapshot Release should have adjusted Suite"
    );
    assert!(
        release.contains("Codename: stable-20260320"),
        "snapshot Release should have adjusted Codename"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_snapshot_immutability() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-immut").await;
    let token = app.admin_token();

    let deb1 = build_synthetic_deb("before-snap", "1.0", "amd64", "before").unwrap();
    let req = apt_upload_request(
        "apt-immut",
        "before-snap_1.0_amd64.deb",
        &deb1,
        None,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Trigger metadata rebuild
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-immut/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);

    // Create snapshot
    let req = app.json_request(
        Method::POST,
        "/repository/apt-immut/snapshots",
        &token,
        serde_json::json!({ "source": "stable", "name": "frozen" }),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Upload another package after snapshot
    let deb2 = build_synthetic_deb("after-snap", "2.0", "amd64", "after").unwrap();
    let req = apt_upload_request("apt-immut", "after-snap_2.0_amd64.deb", &deb2, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Snapshot Packages should NOT contain the new package
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-immut/dists/frozen/main/binary-amd64/Packages",
        &token,
    );
    let (_, body) = app.call_raw(req).await;
    let snap_packages = String::from_utf8(body).unwrap();
    assert!(
        snap_packages.contains("Package: before-snap"),
        "snapshot should contain original package"
    );
    assert!(
        !snap_packages.contains("Package: after-snap"),
        "snapshot should NOT contain package uploaded after snapshot"
    );

    // Live distribution should contain both
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-immut/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (_, body) = app.call_raw(req).await;
    let live_packages = String::from_utf8(body).unwrap();
    assert!(
        live_packages.contains("Package: before-snap"),
        "live should contain original"
    );
    assert!(
        live_packages.contains("Package: after-snap"),
        "live should contain new package"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_snapshot_nonexistent_distribution() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-nosnap").await;
    let token = app.admin_token();

    // Try to snapshot a distribution that doesn't exist
    let req = app.json_request(
        Method::POST,
        "/repository/apt-nosnap/snapshots",
        &token,
        serde_json::json!({ "source": "nonexistent", "name": "snap" }),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Cleanup + APT internal artifacts
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cleanup_skips_apt_metadata() {
    use depot_server::server::worker::cleanup::run_cleanup_tick;

    let app = TestApp::new().await;

    // Create APT repo with aggressive cleanup policy
    app.create_repo(serde_json::json!({
        "name": "apt-cleanup",
        "repo_type": "hosted",
        "format": "apt",
        "cleanup_max_age_days": 1,
    }))
    .await;
    let token = app.admin_token();

    // Upload a package (creates pool + metadata artifacts)
    let deb = build_synthetic_deb("cleanup-pkg", "1.0", "amd64", "cleanup test").unwrap();
    let req = apt_upload_request(
        "apt-cleanup",
        "cleanup-pkg_1.0_amd64.deb",
        &deb,
        None,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Trigger metadata rebuild
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-cleanup/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);

    // Run cleanup — metadata should survive because it's internal (no time index)
    run_cleanup_tick(
        &app.state.repo.kv,
        &app.state.repo.stores,
        &app.state.settings,
    )
    .await
    .unwrap();

    // Metadata should still be accessible
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-cleanup/dists/stable/Release",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
}

// ===========================================================================
// Cache multi-distribution
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_multi_distro_independent() {
    // Verify that cache repo metadata is scoped per-distribution by checking
    // that two different distributions produce separate metadata entries.
    let app = TestApp::new().await;

    // Set up a hosted repo as upstream
    app.create_apt_repo("apt-upstream").await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("upstream-pkg", "1.0", "amd64", "upstream").unwrap();
    let req = apt_upload_request(
        "apt-upstream",
        "upstream-pkg_1.0_amd64.deb",
        &deb,
        None,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Verify the upstream serves its metadata at /dists/stable/...
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-upstream/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);

    // Cache repo pointed at upstream — requesting a different distribution
    // should attempt to fetch from upstream with that distribution name
    // (will 404 since upstream only has "stable", not "noble")
    app.create_apt_cache_repo(
        "apt-multi-cache",
        "http://localhost:19999/apt/apt-upstream",
        300,
    )
    .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-multi-cache/dists/noble/Release",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    // Should 404 because upstream doesn't have "noble"
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cache should 404 for distribution not in upstream"
    );
}

// ===========================================================================
// Cache repo — fetches metadata from upstream
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_fetches_release_from_upstream() {
    let upstream = wiremock::MockServer::start().await;

    let release_text = "\
Suite: stable
Codename: stable
Components: main
Architectures: amd64
Date: Sat, 21 Mar 2026 00:00:00 UTC
SHA256:
 abc123               42 main/binary-amd64/Packages
";

    // Cache now fetches InRelease first (for unsigned, InRelease == Release text).
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/dists/stable/InRelease"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(release_text)
                .insert_header("Content-Type", "text/plain"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_apt_cache_repo("apt-cache-rel", &upstream.uri(), 300)
        .await;
    let token = app.admin_token();

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-cache-rel/dists/stable/Release",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains("Suite: stable"),
        "should contain upstream Release content"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_fetches_packages_from_upstream() {
    let upstream = wiremock::MockServer::start().await;

    let packages_text = "\
Package: cached-pkg
Version: 1.0
Architecture: amd64
Filename: pool/main/c/cached-pkg/cached-pkg_1.0_amd64.deb
Size: 1234
SHA256: abc123
";

    let packages_sha_hex: String = Sha256::digest(packages_text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let release_text = format!(
        "Suite: stable\nCodename: stable\nComponents: main\nArchitectures: amd64\nSHA256:\n {} {:>16} main/binary-amd64/Packages\n",
        packages_sha_hex,
        packages_text.len(),
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/dists/stable/InRelease"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(&release_text)
                .insert_header("Content-Type", "text/plain"),
        )
        .mount(&upstream)
        .await;

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/dists/stable/main/binary-amd64/Packages",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(packages_text)
                .insert_header("Content-Type", "text/plain"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_apt_cache_repo("apt-cache-pkg", &upstream.uri(), 300)
        .await;
    let token = app.admin_token();

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-cache-pkg/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains("Package: cached-pkg"),
        "should contain upstream Packages content"
    );

    // Second fetch should serve from cache without hitting upstream
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-cache-pkg/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains("Package: cached-pkg"),
        "cached response should still contain upstream data"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_fetches_inrelease_from_upstream() {
    let upstream = wiremock::MockServer::start().await;

    let inrelease_text =
        "Suite: stable\nCodename: stable\nComponents: main\nArchitectures: amd64\nSHA256:\n";

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/dists/stable/InRelease"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(inrelease_text)
                .insert_header("Content-Type", "text/plain"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_apt_cache_repo("apt-cache-inrel", &upstream.uri(), 300)
        .await;
    let token = app.admin_token();

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-cache-inrel/dists/stable/InRelease",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("Suite: stable"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_fetches_packages_gz_from_upstream() {
    let upstream = wiremock::MockServer::start().await;

    // Build a real gzipped Packages file
    let packages_text = "Package: gztest\nVersion: 1.0\nArchitecture: amd64\n\n";
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, packages_text.as_bytes()).unwrap();
    let gz_data = encoder.finish().unwrap();

    let gz_sha: String = Sha256::digest(&gz_data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let release_text = format!(
        "Suite: stable\nCodename: stable\nComponents: main\nArchitectures: amd64\nSHA256:\n {} {:>16} main/binary-amd64/Packages.gz\n",
        gz_sha,
        gz_data.len(),
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/dists/stable/InRelease"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(&release_text)
                .insert_header("Content-Type", "text/plain"),
        )
        .mount(&upstream)
        .await;

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/dists/stable/main/binary-amd64/Packages.gz",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(gz_data.clone())
                .insert_header("Content-Type", "application/gzip"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_apt_cache_repo("apt-cache-gz", &upstream.uri(), 300)
        .await;
    let token = app.admin_token();

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-cache-gz/dists/stable/main/binary-amd64/Packages.gz",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/gzip"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_downloads_deb_from_upstream() {
    let upstream = wiremock::MockServer::start().await;

    let deb = build_synthetic_deb("cache-dl", "1.0", "amd64", "cache download test").unwrap();

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/pool/main/c/cache-dl/cache-dl_1.0_amd64.deb",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(deb.clone())
                .insert_header("Content-Type", "application/vnd.debian.binary-package"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_apt_cache_repo("apt-cache-dl", &upstream.uri(), 300)
        .await;
    let token = app.admin_token();

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-cache-dl/pool/main/c/cache-dl/cache-dl_1.0_amd64.deb",
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, deb, "cached .deb should match upstream");

    // Second fetch should serve from local cache
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-cache-dl/pool/main/c/cache-dl/cache-dl_1.0_amd64.deb",
        &token,
    );
    let (status, downloaded2) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded2, deb, "second fetch should serve from cache");
}

// ===========================================================================
// Proxy — Release and InRelease generation
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_release_generation() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-pr1").await;
    app.create_apt_proxy_repo("apt-proxy-rel", vec!["apt-pr1"])
        .await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("proxy-rel-pkg", "1.0", "amd64", "proxy release test").unwrap();
    let req = apt_upload_request("apt-pr1", "proxy-rel-pkg_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Fetch Release through the proxy
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-proxy-rel/dists/stable/Release",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let release = String::from_utf8(body).unwrap();
    assert!(
        release.contains("Suite: stable"),
        "proxy Release should have Suite"
    );
    assert!(
        release.contains("Components:"),
        "proxy Release should have Components"
    );
    assert!(
        release.contains("Architectures:"),
        "proxy Release should have Architectures"
    );
    assert!(
        release.contains("SHA256:"),
        "proxy Release should have SHA256 checksums"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_inrelease_generation() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-pi1").await;
    app.create_apt_proxy_repo("apt-proxy-inrel", vec!["apt-pi1"])
        .await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("proxy-inrel-pkg", "1.0", "amd64", "test").unwrap();
    let req = apt_upload_request(
        "apt-pi1",
        "proxy-inrel-pkg_1.0_amd64.deb",
        &deb,
        None,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-proxy-inrel/dists/stable/InRelease",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    // Without signing key, InRelease should just be the Release content
    assert!(
        text.contains("Suite: stable"),
        "proxy InRelease should contain Suite"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_release_gpg() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-pg1").await;
    app.create_apt_proxy_repo("apt-proxy-gpg", vec!["apt-pg1"])
        .await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("proxy-gpg-pkg", "1.0", "amd64", "test").unwrap();
    let req = apt_upload_request("apt-pg1", "proxy-gpg-pkg_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-proxy-gpg/dists/stable/Release.gpg",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_packages_gz() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-pgz1").await;
    app.create_apt_proxy_repo("apt-proxy-gz", vec!["apt-pgz1"])
        .await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("gz-pkg", "1.0", "amd64", "gz test").unwrap();
    let req = apt_upload_request("apt-pgz1", "gz-pkg_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Fetch Packages.gz through proxy
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-proxy-gz/dists/stable/main/binary-amd64/Packages.gz",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let gz_bytes = resp.into_body().collect().await.unwrap().to_bytes();

    // Decompress and verify content
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut decoder = GzDecoder::new(&gz_bytes[..]);
    let mut decompressed = String::new();
    decoder.read_to_string(&mut decompressed).unwrap();
    assert!(
        decompressed.contains("Package: gz-pkg"),
        "proxy Packages.gz should contain the package"
    );
}

// ===========================================================================
// Proxy — multi-member multi-arch
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_multi_member_release_architectures() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-ma1").await;
    app.create_apt_repo("apt-ma2").await;
    app.create_apt_proxy_repo("apt-proxy-ma", vec!["apt-ma1", "apt-ma2"])
        .await;
    let token = app.admin_token();

    // Upload amd64 to first member
    let deb1 = build_synthetic_deb("amd-pkg", "1.0", "amd64", "test").unwrap();
    let req = apt_upload_request("apt-ma1", "amd-pkg_1.0_amd64.deb", &deb1, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Upload arm64 to second member
    let deb2 = build_synthetic_deb("arm-pkg", "1.0", "arm64", "test").unwrap();
    let req = apt_upload_request("apt-ma2", "arm-pkg_1.0_arm64.deb", &deb2, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Proxy Release should list both architectures
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-proxy-ma/dists/stable/Release",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let release = String::from_utf8(body).unwrap();
    assert!(release.contains("amd64"), "proxy Release should list amd64");
    assert!(release.contains("arm64"), "proxy Release should list arm64");
}

// ===========================================================================
// Hosted — Release.gpg endpoint
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_release_gpg() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-hgpg").await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("gpg-pkg", "1.0", "amd64", "test").unwrap();
    let req = apt_upload_request("apt-hgpg", "gpg-pkg_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Fetch Release.gpg
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-hgpg/dists/stable/Release.gpg",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
}

// ===========================================================================
// Snapshot — Packages.gz from snapshot
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_snapshot_packages_gz() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-snap-gz").await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("snap-gz-pkg", "1.0", "amd64", "snap gz test").unwrap();
    let req = apt_upload_request(
        "apt-snap-gz",
        "snap-gz-pkg_1.0_amd64.deb",
        &deb,
        None,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Trigger metadata rebuild
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-snap-gz/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);

    // Create snapshot
    let req = app.json_request(
        Method::POST,
        "/repository/apt-snap-gz/snapshots",
        &token,
        serde_json::json!({ "source": "stable", "name": "v1" }),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Fetch Packages and Packages.gz from snapshot
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-snap-gz/dists/v1/main/binary-amd64/Packages",
        &token,
    );
    let (_, plain_body) = app.call_raw(req).await;
    let plain = String::from_utf8(plain_body.clone()).unwrap();
    assert!(plain.contains("Package: snap-gz-pkg"));

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-snap-gz/dists/v1/main/binary-amd64/Packages.gz",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let gz_bytes = resp.into_body().collect().await.unwrap().to_bytes();

    // Decompress and verify
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut decoder = GzDecoder::new(&gz_bytes[..]);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).unwrap();
    assert_eq!(
        decompressed, plain_body,
        "snapshot Packages.gz should decompress to Packages"
    );
}

// ===========================================================================
// Snapshot — InRelease and Release.gpg from snapshot
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_snapshot_inrelease_and_gpg() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-snap-sig").await;
    let token = app.admin_token();

    let deb = build_synthetic_deb("snap-sig-pkg", "1.0", "amd64", "test").unwrap();
    let req = apt_upload_request(
        "apt-snap-sig",
        "snap-sig-pkg_1.0_amd64.deb",
        &deb,
        None,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Trigger metadata rebuild
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-snap-sig/dists/stable/main/binary-amd64/Packages",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);

    // Create snapshot
    let req = app.json_request(
        Method::POST,
        "/repository/apt-snap-sig/snapshots",
        &token,
        serde_json::json!({ "source": "stable", "name": "sig-snap" }),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Fetch InRelease from snapshot
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-snap-sig/dists/sig-snap/InRelease",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains("Suite: sig-snap"),
        "snapshot InRelease should have adjusted Suite"
    );

    // Fetch Release.gpg from snapshot
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-snap-sig/dists/sig-snap/Release.gpg",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
}

// ===========================================================================
// Multi-distribution hosted repos
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multi_distribution_upload() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-multi-dist").await;
    let token = app.admin_token();

    // Upload to "focal"
    let deb = build_synthetic_deb("hello", "1.0", "amd64", "test").unwrap();
    let req = apt_upload_request_dist(
        "apt-multi-dist",
        "hello_1.0_amd64.deb",
        &deb,
        Some("focal"),
        None,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Upload to "jammy"
    let deb2 = build_synthetic_deb("world", "2.0", "amd64", "test2").unwrap();
    let req = apt_upload_request_dist(
        "apt-multi-dist",
        "world_2.0_amd64.deb",
        &deb2,
        Some("jammy"),
        None,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Fetch Packages for "focal" — should contain hello but not world
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-multi-dist/dists/focal/main/binary-amd64/Packages",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("Package: hello"), "focal should have hello");
    assert!(
        !text.contains("Package: world"),
        "focal should NOT have world"
    );

    // Fetch Packages for "jammy" — should contain world but not hello
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-multi-dist/dists/jammy/main/binary-amd64/Packages",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("Package: world"), "jammy should have world");
    assert!(
        !text.contains("Package: hello"),
        "jammy should NOT have hello"
    );

    // Both share the pool — download hello via the shared pool path
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-multi-dist/pool/main/h/hello/hello_1.0_amd64.deb",
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_distributions() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-list-dist").await;
    let token = app.admin_token();

    // Upload to two distributions
    let deb = build_synthetic_deb("hello", "1.0", "amd64", "test").unwrap();
    let req = apt_upload_request_dist(
        "apt-list-dist",
        "hello_1.0_amd64.deb",
        &deb,
        Some("focal"),
        None,
        &token,
    );
    app.call(req).await;

    // Trigger metadata build by fetching
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-list-dist/dists/focal/main/binary-amd64/Packages",
        &token,
    );
    app.call(req).await;

    let deb2 = build_synthetic_deb("world", "2.0", "amd64", "test2").unwrap();
    let req = apt_upload_request_dist(
        "apt-list-dist",
        "world_2.0_amd64.deb",
        &deb2,
        Some("jammy"),
        None,
        &token,
    );
    app.call(req).await;

    // Trigger metadata build for jammy
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-list-dist/dists/jammy/main/binary-amd64/Packages",
        &token,
    );
    app.call(req).await;

    // List distributions
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-list-dist/distributions",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let dists: Vec<String> = serde_json::from_slice(&body).unwrap();
    assert!(dists.contains(&"focal".to_string()));
    assert!(dists.contains(&"jammy".to_string()));
}

// ===========================================================================
// Proxy with cache member — download .deb through group from cache upstream
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_downloads_deb_from_cache_member() {
    let upstream = wiremock::MockServer::start().await;

    let deb_data = build_synthetic_deb("grp-cached", "1.0", "amd64", "group cache test").unwrap();

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/pool/main/g/grp-cached/grp-cached_1.0_amd64.deb",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(deb_data.clone())
                .insert_header("Content-Type", "application/vnd.debian.binary-package"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_apt_repo("apt-grp-h").await;
    app.create_apt_cache_repo("apt-grp-c", &upstream.uri(), 300)
        .await;
    app.create_apt_proxy_repo("apt-grp", vec!["apt-grp-h", "apt-grp-c"])
        .await;

    let token = app.admin_token();

    // Download through proxy — should find it in the cache member's upstream.
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-grp/pool/main/g/grp-cached/grp-cached_1.0_amd64.deb",
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, deb_data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_metadata_with_cache_member() {
    let upstream = wiremock::MockServer::start().await;

    let release_text = "\
Suite: stable
Codename: stable
Components: main
Architectures: amd64
Date: Sat, 21 Mar 2026 00:00:00 UTC
SHA256:
";

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/dists/stable/InRelease"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(release_text)
                .insert_header("Content-Type", "text/plain"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;

    // Hosted member with a package.
    app.create_apt_repo("apt-meta-h").await;
    let token = app.admin_token();
    let deb = build_synthetic_deb("meta-h-pkg", "1.0", "amd64", "hosted pkg").unwrap();
    let req = apt_upload_request("apt-meta-h", "meta-h-pkg_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Cache member.
    app.create_apt_cache_repo("apt-meta-c", &upstream.uri(), 300)
        .await;

    // Proxy over both.
    app.create_apt_proxy_repo("apt-meta-grp", vec!["apt-meta-h", "apt-meta-c"])
        .await;

    // Proxy should generate its own Release with the hosted member's packages.
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-meta-grp/dists/stable/Release",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains("Suite: stable"),
        "proxy should generate valid Release"
    );
}

// ===========================================================================
// Nexus component upload
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_nexus_apt_component_upload() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-nexus").await;

    let deb = build_synthetic_deb("hello", "1.0", "amd64", "test package").unwrap();
    let token = app.admin_token();
    let req = nexus_apt_upload_request(
        "apt-nexus",
        "hello_1.0_amd64.deb",
        "stable",
        None,
        &deb,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Download via pool path
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-nexus/pool/main/h/hello/hello_1.0_amd64.deb",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let downloaded = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(downloaded.to_vec(), deb);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_nexus_apt_component_upload_with_component() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-nexus2").await;

    let deb = build_synthetic_deb("hello", "1.0", "amd64", "test package").unwrap();
    let token = app.admin_token();
    let req = nexus_apt_upload_request(
        "apt-nexus2",
        "hello_1.0_amd64.deb",
        "stable",
        Some("contrib"),
        &deb,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Download via pool path under contrib component
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-nexus2/pool/contrib/h/hello/hello_1.0_amd64.deb",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// PUT blocked for APT repos
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_blocked_for_apt_repo() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-noput").await;

    let token = app.admin_token();
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/repository/apt-noput/some/path/file.deb")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(b"fake deb data".to_vec()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

// ===========================================================================
// Metadata browse visibility
// ===========================================================================

/// After uploading a .deb and triggering a metadata rebuild, the `dists`
/// directory tree must appear in the browse listing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_metadata_visible_in_browse() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-browse").await;

    let deb = build_synthetic_deb("hello", "1.0", "amd64", "A test package").unwrap();
    let token = app.admin_token();
    let req = apt_upload_request("apt-browse", "hello_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Fetch InRelease to trigger metadata rebuild.
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-browse/dists/stable/InRelease",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Poll the browse API until the `dists` directory appears.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let req = app.auth_request(
            Method::GET,
            "/api/v1/repositories/apt-browse/artifacts",
            &token,
        );
        let (status, body) = app.call(req).await;
        assert_eq!(status, StatusCode::OK);

        let dirs = body["dirs"].as_array().unwrap();
        if dirs.iter().any(|d| d["name"].as_str() == Some("dists")) {
            // Verify non-zero counts.
            let dists = dirs
                .iter()
                .find(|d| d["name"].as_str() == Some("dists"))
                .unwrap();
            assert!(
                dists["artifact_count"].as_u64().unwrap() > 0,
                "dists should have non-zero artifact_count"
            );
            break;
        }

        if tokio::time::Instant::now() > deadline {
            panic!("dists directory did not appear in browse within 5s");
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

// ===========================================================================
// Signing key provisioning + management
// ===========================================================================

/// A hosted apt repo auto-generates a signing key on creation, so its metadata
/// is signed (clearsigned InRelease + detached Release.gpg) and the public key
/// is served immediately.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_repo_signed_by_default() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-autosign").await;
    let token = app.admin_token();

    // public.key is available immediately after creation.
    let req = app.auth_request(Method::GET, "/repository/apt-autosign/public.key", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "public.key should exist after create"
    );
    assert!(
        body.as_str()
            .unwrap_or_default()
            .contains("BEGIN PGP PUBLIC KEY"),
        "public.key should be an armored public key"
    );

    let deb = build_synthetic_deb("hello", "1.0", "amd64", "test").unwrap();
    let req = apt_upload_request("apt-autosign", "hello_1.0_amd64.deb", &deb, None, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // InRelease must be clearsigned and still carry the Release body.
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-autosign/dists/stable/InRelease",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let inrelease = body.as_str().unwrap_or_default();
    assert!(
        inrelease.contains("BEGIN PGP SIGNED MESSAGE"),
        "InRelease should be clearsigned"
    );
    assert!(
        inrelease.contains("BEGIN PGP SIGNATURE"),
        "InRelease should carry a signature"
    );
    assert!(
        inrelease.contains("Suite:"),
        "InRelease should still contain the Release body"
    );

    // Release.gpg must be a real detached signature, not the empty fallback.
    let req = app.auth_request(
        Method::GET,
        "/repository/apt-autosign/dists/stable/Release.gpg",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.as_str()
            .unwrap_or_default()
            .contains("BEGIN PGP SIGNATURE"),
        "Release.gpg should be a detached signature"
    );
}

/// POST /signing-key rotates the key: the served public key changes and reflects
/// the value returned by the endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_signing_key_rotate_api() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-rotate").await;
    let token = app.admin_token();

    let req = app.auth_request(Method::GET, "/repository/apt-rotate/public.key", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let before = body.as_str().unwrap_or_default().to_string();

    let req = app.auth_request(
        Method::POST,
        "/api/v1/repositories/apt-rotate/signing-key",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK, "rotate should succeed");
    let rotated = body["public_key"].as_str().unwrap_or_default().to_string();
    assert!(rotated.contains("BEGIN PGP PUBLIC KEY"));

    let req = app.auth_request(Method::GET, "/repository/apt-rotate/public.key", &token);
    let (_, body) = app.call(req).await;
    let after = body.as_str().unwrap_or_default().to_string();
    assert_eq!(after, rotated, "public.key should reflect the rotated key");
    assert_ne!(before, after, "rotation should change the key");
}

/// PUT /signing-key imports an operator-supplied key; the served public key
/// matches the imported key's public half.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_signing_key_import_api() {
    let app = TestApp::new().await;
    app.create_apt_repo("apt-import").await;
    let token = app.admin_token();

    let (priv_armor, pub_armor) =
        depot_format_apt::store::generate_gpg_keypair("imported").unwrap();

    let req = app.json_request(
        Method::PUT,
        "/api/v1/repositories/apt-import/signing-key",
        &token,
        serde_json::json!({ "signing_key": priv_armor }),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK, "import should succeed");
    let returned = body["public_key"].as_str().unwrap_or_default().to_string();

    let req = app.auth_request(Method::GET, "/repository/apt-import/public.key", &token);
    let (_, body) = app.call(req).await;
    let served = body.as_str().unwrap_or_default().to_string();
    assert_eq!(
        served, returned,
        "served key should match the import response"
    );
    assert_eq!(
        served.trim(),
        pub_armor.trim(),
        "served key should be the imported key's public half"
    );
}

/// A repo can be created with an imported signing key in one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_with_imported_signing_key() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let (priv_armor, pub_armor) =
        depot_format_apt::store::generate_gpg_keypair("create-import").unwrap();

    app.create_repo(serde_json::json!({
        "name": "apt-create-import",
        "repo_type": "hosted",
        "format": "apt",
        "store": "default",
        "signing_key": priv_armor,
    }))
    .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/apt-create-import/public.key",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_str().unwrap_or_default().trim(), pub_armor.trim());
}

/// Signing-key management is apt-only: non-apt repos reject it at both the
/// endpoint and at create time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_signing_key_rejected_for_non_apt() {
    let app = TestApp::new().await;
    app.create_format_repo("raw-nosign", "raw").await;
    let token = app.admin_token();

    let req = app.auth_request(
        Method::POST,
        "/api/v1/repositories/raw-nosign/signing-key",
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "signing-key endpoint should be apt-only"
    );

    // signing_key at create time is also rejected for non-apt formats.
    let req = app.json_request(
        Method::POST,
        "/api/v1/repositories",
        &token,
        serde_json::json!({
            "name": "raw-with-key",
            "repo_type": "hosted",
            "format": "raw",
            "store": "default",
            "signing_key": "not-a-key",
        }),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
