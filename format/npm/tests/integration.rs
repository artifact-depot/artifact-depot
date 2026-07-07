// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::body::Body;
use axum::http::{header, Method, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use std::collections::HashMap;

use depot_format_npm::store::NpmStore;

mod common;

use common::*;
use depot_test_support as helpers;
use depot_test_support::*;

// ===========================================================================
// Validation / Error handling
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_nonexistent_repo_404() {
    let app = TestApp::new().await;
    helpers::assert_nonexistent_repo_404(&app, "/repository/no-such-repo/some-pkg").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_non_npm_repo_400() {
    // Under the unified `/repository/{repo}/...` router, an npm-shaped URL on
    // a raw repo falls through to the generic artifact lookup and returns 404
    // rather than 400 — URL shape is no longer tied to format.
    let app = TestApp::new().await;
    app.create_hosted_repo("raw-repo").await;
    let req = app.auth_request(
        Method::GET,
        "/repository/raw-repo/some-pkg",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_publish_invalid_json_body_400() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-badjson").await;
    let token = app.admin_token();

    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/repository/npm-badjson/some-pkg")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(b"this is not json".to_vec()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_publish_missing_versions_field_400() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-nover").await;
    let token = app.admin_token();

    let body = json!({
        "name": "my-pkg",
        "_attachments": {
            "my-pkg-1.0.0.tgz": {
                "data": "AAAA",
                "length": 3,
            }
        }
    });
    let req = app.json_request(Method::PUT, "/repository/npm-nover/my-pkg", &token, body);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_publish_missing_attachments_field_400() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-noatt").await;
    let token = app.admin_token();

    let body = json!({
        "name": "my-pkg",
        "versions": {
            "1.0.0": {
                "name": "my-pkg",
                "version": "1.0.0",
            }
        }
    });
    let req = app.json_request(Method::PUT, "/repository/npm-noatt/my-pkg", &token, body);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_publish_missing_attachment_data_field_400() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-nodata").await;
    let token = app.admin_token();

    let body = json!({
        "name": "my-pkg",
        "versions": {
            "1.0.0": {
                "name": "my-pkg",
                "version": "1.0.0",
            }
        },
        "_attachments": {
            "my-pkg-1.0.0.tgz": {
                "length": 3,
            }
        }
    });
    let req = app.json_request(Method::PUT, "/repository/npm-nodata/my-pkg", &token, body);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_publish_invalid_base64_attachment_400() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-badb64").await;
    let token = app.admin_token();

    let body = json!({
        "name": "my-pkg",
        "versions": {
            "1.0.0": {
                "name": "my-pkg",
                "version": "1.0.0",
            }
        },
        "_attachments": {
            "my-pkg-1.0.0.tgz": {
                "data": "!!!not-valid-base64!!!",
                "length": 3,
            }
        }
    });
    let req = app.json_request(Method::PUT, "/repository/npm-badb64/my-pkg", &token, body);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_publish_to_cache_repo_400() {
    let app = TestApp::new().await;
    app.create_npm_cache_repo("npm-cache-nopub", "http://127.0.0.1:1", 300)
        .await;
    let token = app.admin_token();

    let body = build_npm_publish_body("my-pkg", "1.0.0", b"tarball-data");
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/repository/npm-cache-nopub/my-pkg")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_invalid_tarball_filename_no_tgz_400() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-badfile").await;

    // Request a tarball whose filename does not end in .tgz
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-badfile/some-pkg/-/some-pkg-1.0.0.tar.gz",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_rbac_readonly_user_cannot_publish() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-rbac").await;

    app.create_user_with_roles("reader", "pass123", vec!["read-only"])
        .await;
    let reader_token = app.token_for("reader").await;

    let body = build_npm_publish_body("my-pkg", "1.0.0", b"data");
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/repository/npm-rbac/my-pkg")
        .header(header::AUTHORIZATION, format!("Bearer {}", reader_token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ===========================================================================
// Hosted repo operations
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_publish_and_get_packument() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-hosted").await;

    let tarball = b"fake-tarball-content-v1";
    let req = npm_publish_request(&app, "npm-hosted", "my-pkg", "1.0.0", tarball);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Get packument
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-hosted/my-pkg",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Verify packument structure
    assert_eq!(body["name"], "my-pkg");
    assert_eq!(body["dist-tags"]["latest"], "1.0.0");
    assert!(body["versions"]["1.0.0"].is_object());
    assert_eq!(body["versions"]["1.0.0"]["name"], "my-pkg");
    assert_eq!(body["versions"]["1.0.0"]["version"], "1.0.0");
    // Verify tarball URL is rewritten
    let tarball_url = body["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert!(
        tarball_url.contains("/repository/npm-hosted/"),
        "tarball URL should point to our server: {tarball_url}"
    );
    assert!(
        tarball_url.ends_with(".tgz"),
        "tarball URL should end with .tgz: {tarball_url}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_publish_and_download_tarball() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-dl").await;

    let tarball = b"test-tarball-data-xyz";
    let req = npm_publish_request(&app, "npm-dl", "my-pkg", "1.0.0", tarball);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Download tarball
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-dl/my-pkg/-/my-pkg-1.0.0.tgz",
        &app.admin_token(),
    );
    let (status, data) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, tarball);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_download_tarball_headers() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-headers").await;

    let tarball = b"header-check-tarball-content";
    let req = npm_publish_request(&app, "npm-headers", "hdr-pkg", "1.0.0", tarball);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/npm-headers/hdr-pkg/-/hdr-pkg-1.0.0.tgz",
        &app.admin_token(),
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Content-Type
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/octet-stream"
    );

    // Content-Length
    let cl: u64 = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(cl, tarball.len() as u64);

    // Content-Disposition
    let cd = resp
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        cd.contains("hdr-pkg-1.0.0.tgz"),
        "Content-Disposition should contain filename: {cd}"
    );

    // Verify body
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), tarball);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scoped_package_publish_get_download() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-scoped").await;
    let token = app.admin_token();

    // Publish scoped package
    let tarball = b"scoped-tarball-content";
    let body = build_npm_publish_body("@myorg/widget", "2.0.0", tarball);
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/repository/npm-scoped/@myorg/widget")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Get packument (scoped)
    let req = app.auth_request(Method::GET, "/repository/npm-scoped/@myorg/widget", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "@myorg/widget");
    assert_eq!(body["dist-tags"]["latest"], "2.0.0");

    // Verify tarball URL format for scoped package
    let tarball_url = body["versions"]["2.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert!(
        tarball_url.contains("/repository/npm-scoped/@myorg/widget/-/"),
        "scoped tarball URL should include scope: {tarball_url}"
    );
    assert!(
        tarball_url.ends_with("widget-2.0.0.tgz"),
        "scoped tarball URL should end with bare-name-version.tgz: {tarball_url}"
    );

    // Download tarball (scoped) -- filename uses bare name without scope
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-scoped/@myorg/widget/-/widget-2.0.0.tgz",
        &token,
    );
    let (status, data) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, tarball);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multiple_versions() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-multi").await;

    // Publish v1
    let req = npm_publish_request(&app, "npm-multi", "multi-pkg", "1.0.0", b"v1-content");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Publish v2
    let req = npm_publish_request(&app, "npm-multi", "multi-pkg", "2.0.0", b"v2-content");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Get packument -- should have both versions, dist-tags updated to v2
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-multi/multi-pkg",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dist-tags"]["latest"], "2.0.0");
    assert!(body["versions"]["1.0.0"].is_object());
    assert!(body["versions"]["2.0.0"].is_object());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_overwrite_existing_version() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-overwrite").await;

    // Publish v1 with content A
    let req = npm_publish_request(&app, "npm-overwrite", "ow-pkg", "1.0.0", b"content-A");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Overwrite v1 with content B
    let req = npm_publish_request(&app, "npm-overwrite", "ow-pkg", "1.0.0", b"content-B");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Download -- should get new content
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-overwrite/ow-pkg/-/ow-pkg-1.0.0.tgz",
        &app.admin_token(),
    );
    let (status, data) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, b"content-B");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_packument_includes_time_field() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-time").await;

    let req = npm_publish_request(&app, "npm-time", "time-pkg", "1.0.0", b"data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/npm-time/time-pkg",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Verify time field exists and has an entry for version 1.0.0
    assert!(
        body["time"].is_object(),
        "packument should have a 'time' field"
    );
    assert!(
        body["time"]["1.0.0"].is_string(),
        "time field should have entry for version 1.0.0"
    );
    // The timestamp should be an RFC3339 string
    let ts = body["time"]["1.0.0"].as_str().unwrap();
    assert!(ts.contains("T"), "timestamp should be RFC3339 format: {ts}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_packument_tarball_url_format_unscoped() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-url").await;

    let req = npm_publish_request(&app, "npm-url", "url-pkg", "3.0.0", b"data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/npm-url/url-pkg",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let tarball_url = body["versions"]["3.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    // Must be an absolute URL — npm 7+ treats a leading-slash tarball as a
    // local file path and every install fails with ENOENT.
    // Unscoped: {origin}/repository/{repo}/{name}/-/{name}-{version}.tgz
    // With no Host/X-Forwarded-Proto headers the origin defaults to https://localhost.
    assert_eq!(
        tarball_url,
        "https://localhost/repository/npm-url/url-pkg/-/url-pkg-3.0.0.tgz"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_packument_tarball_url_format_scoped() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-url-s").await;
    let token = app.admin_token();

    let body = build_npm_publish_body("@scope/mypkg", "1.2.3", b"data");
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/repository/npm-url-s/@scope/mypkg")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(Method::GET, "/repository/npm-url-s/@scope/mypkg", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let tarball_url = body["versions"]["1.2.3"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    // Scoped: {origin}/repository/{repo}/@scope/pkg/-/{bare_name}-{version}.tgz
    assert_eq!(
        tarball_url,
        "https://localhost/repository/npm-url-s/@scope/mypkg/-/mypkg-1.2.3.tgz"
    );
}

/// The packument's `dist.tarball` must be a fully-qualified absolute URL rooted
/// at the externally-reachable origin, derived from `Host`/`X-Forwarded-Proto`
/// when behind a reverse proxy. A relative path breaks all npm/npx installs
/// because npm 7+ interprets a leading-slash tarball as a local file path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_packument_tarball_url_honors_forwarded_headers() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-fwd").await;

    let req = npm_publish_request(&app, "npm-fwd", "fwd-pkg", "2.1.0", b"data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Simulate a request arriving through a TLS-terminating reverse proxy.
    let req = axum::http::Request::builder()
        .method(Method::GET)
        .uri("/repository/npm-fwd/fwd-pkg")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", app.admin_token()),
        )
        .header(header::HOST, "repository.example.com")
        .header("x-forwarded-proto", "https")
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let tarball_url = body["versions"]["2.1.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert_eq!(
        tarball_url,
        "https://repository.example.com/repository/npm-fwd/fwd-pkg/-/fwd-pkg-2.1.0.tgz"
    );
}

/// A client talking directly to the plain-HTTP listener (no proxy, so no
/// `X-Forwarded-Proto`) must receive `http://` tarball URLs — not the historical
/// hardcoded `https://`. This is the reported depot-dev bug: HTTP metadata that
/// hands back HTTPS tarballs, forcing clients onto a broken TLS endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_packument_tarball_url_http_listener_defaults_to_http() {
    let app = TestApp::new_with_default_scheme("http").await;
    app.create_npm_repo("npm-http").await;

    let req = npm_publish_request(&app, "npm-http", "http-pkg", "1.60.0", b"data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Plain HTTP request: a Host header but no X-Forwarded-Proto.
    let req = axum::http::Request::builder()
        .method(Method::GET)
        .uri("/repository/npm-http/http-pkg")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", app.admin_token()),
        )
        .header(header::HOST, "depot-dev.lab.example.com")
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let tarball_url = body["versions"]["1.60.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    // TC1 (scheme is http), TC2 (absolute, has "://"), TC3 (host parity).
    assert_eq!(
        tarball_url,
        "http://depot-dev.lab.example.com/repository/npm-http/http-pkg/-/http-pkg-1.60.0.tgz"
    );

    // TC4: the exact baked URL is fetchable. Strip the origin and GET the path
    // back through the same app; it must return the published tarball bytes.
    let path = tarball_url
        .strip_prefix("http://depot-dev.lab.example.com")
        .expect("baked URL must be rooted at the request host");
    let req = app.auth_request(Method::GET, path, &app.admin_token());
    let (status, data) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, b"data");
}

/// TC6: a scoped package's `dist.tarball` is rewritten the same way — absolute,
/// on the request scheme (http here) — over the plain-HTTP listener.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scoped_packument_tarball_url_http_listener_defaults_to_http() {
    let app = TestApp::new_with_default_scheme("http").await;
    app.create_npm_repo("npm-http-scoped").await;
    let token = app.admin_token();

    let body = build_npm_publish_body("@myorg/widget", "2.0.0", b"scoped-data");
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/repository/npm-http-scoped/@myorg/widget")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Plain HTTP fetch: Host header, no X-Forwarded-Proto.
    let req = axum::http::Request::builder()
        .method(Method::GET)
        .uri("/repository/npm-http-scoped/@myorg/widget")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::HOST, "depot-dev.lab.example.com")
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let tarball_url = body["versions"]["2.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert_eq!(
        tarball_url,
        "http://depot-dev.lab.example.com/repository/npm-http-scoped/@myorg/widget/-/widget-2.0.0.tgz"
    );
}

/// A configured `base_url` is a hard override (Nexus "Base URL" capability):
/// it wins even over an `X-Forwarded-Proto: https` header, so an operator can
/// pin `http://` while a TLS certificate is being fixed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_packument_tarball_url_base_url_override_wins() {
    let app = TestApp::new().await;
    app.set_base_url(Some("http://depot-dev.lab.example.com"));
    app.create_npm_repo("npm-base").await;

    let req = npm_publish_request(&app, "npm-base", "base-pkg", "3.0.0", b"data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Even with proxy headers advertising https and a different host, the
    // configured base_url overrides everything.
    let req = axum::http::Request::builder()
        .method(Method::GET)
        .uri("/repository/npm-base/base-pkg")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", app.admin_token()),
        )
        .header(header::HOST, "internal.svc:8080")
        .header("x-forwarded-proto", "https")
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let tarball_url = body["versions"]["3.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert_eq!(
        tarball_url,
        "http://depot-dev.lab.example.com/repository/npm-base/base-pkg/-/base-pkg-3.0.0.tgz"
    );
}

// ===========================================================================
// Search
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_no_text_lists_all() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-search-all").await;

    let req = npm_publish_request(&app, "npm-search-all", "alpha", "1.0.0", b"alpha-data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = npm_publish_request(&app, "npm-search-all", "beta", "1.0.0", b"beta-data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Search with no text -- should return all packages
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-search-all/-/v1/search",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let objects = body["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_with_text_filter() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-search-filt").await;

    let req = npm_publish_request(&app, "npm-search-filt", "alpha", "1.0.0", b"alpha-data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = npm_publish_request(&app, "npm-search-filt", "beta", "1.0.0", b"beta-data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Search for "alpha"
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-search-filt/-/v1/search?text=alpha",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let objects = body["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0]["package"]["name"], "alpha");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_with_size_limit() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-search-lim").await;

    let req = npm_publish_request(&app, "npm-search-lim", "aaa", "1.0.0", b"a");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = npm_publish_request(&app, "npm-search-lim", "bbb", "1.0.0", b"b");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = npm_publish_request(&app, "npm-search-lim", "ccc", "1.0.0", b"c");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Search with size=2
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-search-lim/-/v1/search?size=2",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let objects = body["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_packument_not_found() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-empty").await;

    let req = app.auth_request(
        Method::GET,
        "/repository/npm-empty/nonexistent",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tarball_not_found() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-notarball").await;

    let req = app.auth_request(
        Method::GET,
        "/repository/npm-notarball/some-pkg/-/some-pkg-1.0.0.tgz",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Proxy repo operations
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_repo_aggregates_packuments() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-a").await;
    app.create_npm_repo("npm-b").await;
    app.create_npm_proxy_repo("npm-proxy", vec!["npm-a", "npm-b"])
        .await;

    // Publish to member A
    let req = npm_publish_request(&app, "npm-a", "shared-pkg", "1.0.0", b"from-a");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Publish different version to member B
    let req = npm_publish_request(&app, "npm-b", "shared-pkg", "2.0.0", b"from-b");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Query through proxy -- should see both versions
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-proxy/shared-pkg",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["versions"]["1.0.0"].is_object());
    assert!(body["versions"]["2.0.0"].is_object());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_first_member_wins_on_version_conflict() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-c1").await;
    app.create_npm_repo("npm-c2").await;
    app.create_npm_proxy_repo("npm-proxy-conflict", vec!["npm-c1", "npm-c2"])
        .await;

    // Publish same version to both members with different content
    let req = npm_publish_request(&app, "npm-c1", "conflict-pkg", "1.0.0", b"from-c1");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = npm_publish_request(&app, "npm-c2", "conflict-pkg", "1.0.0", b"from-c2");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Query through proxy -- first member (npm-c1) wins
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-proxy-conflict/conflict-pkg",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["versions"]["1.0.0"].is_object());
    // The version metadata should come from the first member (npm-c1).
    // Both have name "conflict-pkg" so we just verify it exists.
    assert_eq!(body["versions"]["1.0.0"]["name"], "conflict-pkg");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_download_from_first_member_that_has_it() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-dl-a").await;
    app.create_npm_repo("npm-dl-b").await;
    app.create_npm_proxy_repo("npm-proxy-dl", vec!["npm-dl-a", "npm-dl-b"])
        .await;

    // Publish only to member B
    let req = npm_publish_request(&app, "npm-dl-b", "dl-pkg", "1.0.0", b"from-b-tarball");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Download through proxy -- should find it in member B
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-proxy-dl/dl-pkg/-/dl-pkg-1.0.0.tgz",
        &app.admin_token(),
    );
    let (status, data) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, b"from-b-tarball");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_publish_routes_to_first_hosted_member() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-pw-a").await;
    app.create_npm_repo("npm-pw-b").await;
    app.create_npm_proxy_repo("npm-proxy-pub", vec!["npm-pw-a", "npm-pw-b"])
        .await;

    // Publish via proxy
    let req = npm_publish_request(
        &app,
        "npm-proxy-pub",
        "proxy-pub-pkg",
        "1.0.0",
        b"proxy-pub",
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Verify it landed in the first hosted member (npm-pw-a)
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-pw-a/proxy-pub-pkg",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "proxy-pub-pkg");
    assert!(body["versions"]["1.0.0"].is_object());

    // Verify it is NOT in the second member
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-pw-b/proxy-pub-pkg",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_with_no_matching_package_404() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-empty-a").await;
    app.create_npm_repo("npm-empty-b").await;
    app.create_npm_proxy_repo("npm-proxy-empty", vec!["npm-empty-a", "npm-empty-b"])
        .await;

    let req = app.auth_request(
        Method::GET,
        "/repository/npm-proxy-empty/nonexistent",
        &app.admin_token(),
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_tarball_download_across_members() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-tar-a").await;
    app.create_npm_repo("npm-tar-b").await;
    app.create_npm_proxy_repo("npm-proxy-tar", vec!["npm-tar-a", "npm-tar-b"])
        .await;

    // Put different packages in different members
    let req = npm_publish_request(&app, "npm-tar-a", "pkg-a", "1.0.0", b"tarball-a");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = npm_publish_request(&app, "npm-tar-b", "pkg-b", "1.0.0", b"tarball-b");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Download pkg-a through proxy (found in member A)
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-proxy-tar/pkg-a/-/pkg-a-1.0.0.tgz",
        &app.admin_token(),
    );
    let (status, data) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, b"tarball-a");

    // Download pkg-b through proxy (found in member B)
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-proxy-tar/pkg-b/-/pkg-b-1.0.0.tgz",
        &app.admin_token(),
    );
    let (status, data) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, b"tarball-b");

    // Nonexistent tarball through proxy -> 404
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-proxy-tar/no-pkg/-/no-pkg-1.0.0.tgz",
        &app.admin_token(),
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Name normalization
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_name_case_insensitive() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-case").await;

    // Publish with mixed case
    let req = npm_publish_request(&app, "npm-case", "MyPkg", "1.0.0", b"mixed-case-data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Fetch with original case -- should work
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-case/MyPkg",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "packument via original case should exist"
    );
    assert!(body["versions"]["1.0.0"].is_object());

    // Fetch with lowercase -- should also find it because names are normalized
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-case/mypkg",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "packument via lowercase should also work"
    );
    assert!(body["versions"]["1.0.0"].is_object());

    // list_packages returns the normalized name
    let blobs = app.state.repo.blob_store("default").await.unwrap();
    let store = NpmStore {
        repo: "npm-case",
        kv: app.state.repo.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: "default",
        updater: &app.state.repo.updater,
    };
    let pkgs = store.list_packages().await.unwrap();
    assert_eq!(
        pkgs,
        vec!["mypkg"],
        "package names should be normalized to lowercase"
    );
}

// ===========================================================================
// Data model (repo layer) -- direct NpmStore operations
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_store_list_packages_sorted_unique() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-list").await;

    // Publish multiple packages
    let req = npm_publish_request(&app, "npm-list", "charlie", "1.0.0", b"c");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = npm_publish_request(&app, "npm-list", "alpha", "1.0.0", b"a");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = npm_publish_request(&app, "npm-list", "bravo", "1.0.0", b"b");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Publish second version of alpha (should not duplicate in list)
    let req = npm_publish_request(&app, "npm-list", "alpha", "2.0.0", b"a2");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let blobs = app.state.repo.blob_store("default").await.unwrap();
    let store = NpmStore {
        repo: "npm-list",
        kv: app.state.repo.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: "default",
        updater: &app.state.repo.updater,
    };

    let packages = store.list_packages().await.unwrap();
    assert_eq!(packages, vec!["alpha", "bravo", "charlie"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_store_list_package_versions() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-listver").await;

    let req = npm_publish_request(&app, "npm-listver", "ver-pkg", "1.0.0", b"v1");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = npm_publish_request(&app, "npm-listver", "ver-pkg", "2.0.0", b"v2");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let blobs = app.state.repo.blob_store("default").await.unwrap();
    let store = NpmStore {
        repo: "npm-listver",
        kv: app.state.repo.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: "default",
        updater: &app.state.repo.updater,
    };

    let versions = store.list_package_versions("ver-pkg").await.unwrap();
    assert_eq!(versions.len(), 2, "should have two tarball entries");

    // Check that both version paths are present
    let paths: Vec<&str> = versions.iter().map(|(p, _)| p.as_str()).collect();
    assert!(
        paths.iter().any(|p| p.contains("1.0.0")),
        "should contain version 1.0.0"
    );
    assert!(
        paths.iter().any(|p| p.contains("2.0.0")),
        "should contain version 2.0.0"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_store_dist_tags_roundtrip() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-tags").await;

    let blobs = app.state.repo.blob_store("default").await.unwrap();
    let store = NpmStore {
        repo: "npm-tags",
        kv: app.state.repo.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: "default",
        updater: &app.state.repo.updater,
    };

    // Initially no dist-tags
    let tags = store.get_dist_tags("test-pkg").await.unwrap();
    assert!(tags.is_empty());

    // Put some dist-tags
    let mut new_tags = HashMap::new();
    new_tags.insert("latest".to_string(), "1.0.0".to_string());
    new_tags.insert("beta".to_string(), "2.0.0-beta.1".to_string());
    store.put_dist_tags("test-pkg", &new_tags).await.unwrap();

    // Read them back
    let tags = store.get_dist_tags("test-pkg").await.unwrap();
    assert_eq!(tags.len(), 2);
    assert_eq!(tags["latest"], "1.0.0");
    assert_eq!(tags["beta"], "2.0.0-beta.1");

    // Overwrite
    let mut updated_tags = HashMap::new();
    updated_tags.insert("latest".to_string(), "2.0.0".to_string());
    store
        .put_dist_tags("test-pkg", &updated_tags)
        .await
        .unwrap();

    let tags = store.get_dist_tags("test-pkg").await.unwrap();
    assert_eq!(tags["latest"], "2.0.0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_store_get_version_metadata() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-meta").await;

    // Publish a package
    let req = npm_publish_request(&app, "npm-meta", "meta-pkg", "1.0.0", b"meta-data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let blobs = app.state.repo.blob_store("default").await.unwrap();
    let store = NpmStore {
        repo: "npm-meta",
        kv: app.state.repo.kv.as_ref(),
        blobs: blobs.as_ref(),
        store: "default",
        updater: &app.state.repo.updater,
    };

    // Get version metadata
    let meta = store
        .get_version_metadata("meta-pkg", "1.0.0")
        .await
        .unwrap();
    assert!(meta.is_some(), "version metadata should exist");
    let meta_str = meta.unwrap();
    let meta_json: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
    assert_eq!(meta_json["name"], "meta-pkg");
    assert_eq!(meta_json["version"], "1.0.0");

    // Nonexistent version returns None
    let meta = store
        .get_version_metadata("meta-pkg", "9.9.9")
        .await
        .unwrap();
    assert!(meta.is_none());
}

// ===========================================================================
// Bug fix tests (Group D)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_packument_has_dist_shasum_and_integrity() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-hashes").await;

    let tarball = b"tarball-with-hashes";
    let req = npm_publish_request(&app, "npm-hashes", "hash-pkg", "1.0.0", tarball);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Also publish a second version to check both
    let req = npm_publish_request(&app, "npm-hashes", "hash-pkg", "2.0.0", b"tarball-v2");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/npm-hashes/hash-pkg",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Check version 1.0.0
    let dist_1 = &body["versions"]["1.0.0"]["dist"];
    let shasum = dist_1["shasum"].as_str().expect("shasum should be present");
    assert!(!shasum.is_empty(), "shasum should be non-empty");
    let integrity = dist_1["integrity"]
        .as_str()
        .expect("integrity should be present");
    assert!(
        integrity.starts_with("sha512-"),
        "integrity should start with 'sha512-': {integrity}"
    );

    // Check version 2.0.0
    let dist_2 = &body["versions"]["2.0.0"]["dist"];
    let shasum_2 = dist_2["shasum"].as_str().expect("shasum should be present");
    assert!(!shasum_2.is_empty(), "shasum should be non-empty");
    let integrity_2 = dist_2["integrity"]
        .as_str()
        .expect("integrity should be present");
    assert!(
        integrity_2.starts_with("sha512-"),
        "integrity should start with 'sha512-': {integrity_2}"
    );

    // The two versions should have different hashes (different content)
    assert_ne!(
        shasum, shasum_2,
        "different content should have different sha1"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_publish_without_dist_tags_creates_latest() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-notags").await;
    let token = app.admin_token();

    // Build a publish body with empty dist-tags
    use base64::Engine;
    let tarball_data = b"no-tags-tarball";
    let b64 = base64::engine::general_purpose::STANDARD.encode(tarball_data);
    let body = json!({
        "name": "notags-pkg",
        "versions": {
            "1.2.3": {
                "name": "notags-pkg",
                "version": "1.2.3",
                "dist": {
                    "tarball": "placeholder/notags-pkg-1.2.3.tgz",
                }
            }
        },
        "dist-tags": {},
        "_attachments": {
            "notags-pkg-1.2.3.tgz": {
                "data": b64,
                "length": tarball_data.len(),
            }
        }
    });
    let req = app.json_request(
        Method::PUT,
        "/repository/npm-notags/notags-pkg",
        &token,
        body,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Fetch packument and verify "latest" dist-tag was auto-created
    let req = app.auth_request(Method::GET, "/repository/npm-notags/notags-pkg", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["dist-tags"]["latest"], "1.2.3",
        "latest dist-tag should be auto-created"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_whoami_endpoint() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-whoami").await;
    let token = app.admin_token();

    let req = app.auth_request(Method::GET, "/repository/npm-whoami/-/whoami", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["username"], "admin");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_login_endpoint() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-login").await;

    let login_body = json!({
        "name": "admin",
        "password": "password",
    });
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/repository/npm-login/-/user/org.couchdb.user:admin")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&login_body).unwrap()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["ok"], true);
    assert!(
        body["token"].as_str().is_some(),
        "response should contain a token"
    );
    assert!(
        !body["token"].as_str().unwrap().is_empty(),
        "token should be non-empty"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_login_bad_credentials() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-login-bad").await;

    let login_body = json!({
        "name": "admin",
        "password": "wrong-password",
    });
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/repository/npm-login-bad/-/user/org.couchdb.user:admin")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&login_body).unwrap()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_returns_real_version() {
    let app = TestApp::new().await;
    app.create_npm_repo("npm-search-ver").await;

    let req = npm_publish_request(&app, "npm-search-ver", "real-ver-pkg", "3.2.1", b"data");
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/npm-search-ver/-/v1/search?text=real-ver",
        &app.admin_token(),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let objects = body["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(
        objects[0]["package"]["version"], "3.2.1",
        "search should return the actual latest version, not 0.0.0"
    );
}

#[test]
fn test_extract_version_edge_cases() {
    use depot_format_npm::api::extract_version_from_filename;

    // Package name with digit: should use primary (known name) logic
    assert_eq!(
        extract_version_from_filename("level-2-cache-1.0.0.tgz", "level-2-cache"),
        Some("1.0.0".to_string())
    );

    // Simple case
    assert_eq!(
        extract_version_from_filename("simple-1.0.0.tgz", "simple"),
        Some("1.0.0".to_string())
    );

    // Package name contains digits
    assert_eq!(
        extract_version_from_filename("my-1pkg-2.0.0.tgz", "my-1pkg"),
        Some("2.0.0".to_string())
    );

    // Scoped package (bare name used in filename)
    assert_eq!(
        extract_version_from_filename("widget-1.0.0.tgz", "@scope/widget"),
        Some("1.0.0".to_string())
    );

    // No .tgz suffix
    assert_eq!(
        extract_version_from_filename("simple-1.0.0.tar.gz", "simple"),
        None
    );
}

// ===========================================================================
// Cache repo — packument from upstream
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_packument_from_upstream() {
    let upstream = wiremock::MockServer::start().await;

    let packument = serde_json::json!({
        "name": "cached-pkg",
        "dist-tags": {"latest": "1.0.0"},
        "versions": {
            "1.0.0": {
                "name": "cached-pkg",
                "version": "1.0.0",
                "dist": {
                    "tarball": format!("{}/cached-pkg/-/cached-pkg-1.0.0.tgz", upstream.uri()),
                    "shasum": "abc123"
                }
            }
        }
    });

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/cached-pkg"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(&packument)
                .insert_header("Content-Type", "application/json"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_npm_cache_repo("npm-cache", &upstream.uri(), 300)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/repository/npm-cache/cached-pkg", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "cached-pkg");
    // Tarball URLs should be rewritten to point to the cache repo.
    let tarball_url = body["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert!(
        tarball_url.contains("/repository/npm-cache/"),
        "tarball URL should be rewritten: {tarball_url}"
    );
}

// ===========================================================================
// Cache repo — tarball download from upstream
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_download_tarball_from_upstream() {
    let upstream = wiremock::MockServer::start().await;

    let tarball_data = b"fake npm tarball data";

    // Serve the tarball.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/dl-pkg/-/dl-pkg-1.0.0.tgz"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(tarball_data.to_vec())
                .insert_header("Content-Type", "application/octet-stream"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_npm_cache_repo("npm-cache-dl", &upstream.uri(), 300)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-cache-dl/dl-pkg/-/dl-pkg-1.0.0.tgz",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, tarball_data);

    // Second request should serve from cache (upstream not called again).
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-cache-dl/dl-pkg/-/dl-pkg-1.0.0.tgz",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, tarball_data);
}

// ===========================================================================
// Cache repo — packument not found upstream
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_packument_not_found() {
    let upstream = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/no-such-pkg"))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_npm_cache_repo("npm-cache-miss", &upstream.uri(), 300)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        "/repository/npm-cache-miss/no-such-pkg",
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Proxy with cache member
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_with_hosted_and_cache_members() {
    let upstream = wiremock::MockServer::start().await;

    let packument = serde_json::json!({
        "name": "from-cache",
        "dist-tags": {"latest": "2.0.0"},
        "versions": {
            "2.0.0": {
                "name": "from-cache",
                "version": "2.0.0",
                "dist": {
                    "tarball": format!("{}/from-cache/-/from-cache-2.0.0.tgz", upstream.uri()),
                    "shasum": "def456"
                }
            }
        }
    });

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/from-cache"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(&packument)
                .insert_header("Content-Type", "application/json"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_npm_repo("npm-prx-h").await;
    app.create_npm_cache_repo("npm-prx-c", &upstream.uri(), 300)
        .await;
    app.create_npm_proxy_repo("npm-prx", vec!["npm-prx-h", "npm-prx-c"])
        .await;

    let token = app.admin_token();

    // Prime the cache by fetching from the cache repo directly.
    let req = app.auth_request(Method::GET, "/repository/npm-prx-c/from-cache", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK, "cache prime: {body}");

    // Now the proxy should find it from the cache member's local store.
    let req = app.auth_request(Method::GET, "/repository/npm-prx/from-cache", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "from-cache");
}
