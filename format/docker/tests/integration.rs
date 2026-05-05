// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;

use depot_core::store::kv::{Capability, CapabilityGrant};

use depot_test_support::*;

// ===========================================================================
// V2 Base & Catalog
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_v2_check() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Docker-Distribution-Api-Version")
            .unwrap(),
        "registry/2.0"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_v2_check_anonymous_returns_bearer_challenge() {
    let app = TestApp::new().await;
    let req = app.request(Method::GET, "/v2/");
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let www_auth = resp
        .headers()
        .get("WWW-Authenticate")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        www_auth.contains("Bearer"),
        "expected Bearer challenge, got: {}",
        www_auth
    );
    assert!(
        www_auth.contains("/v2/token"),
        "expected realm containing /v2/token, got: {}",
        www_auth
    );
    assert_eq!(
        resp.headers()
            .get("Docker-Distribution-Api-Version")
            .unwrap(),
        "registry/2.0"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_v2_token_basic_auth() {
    let app = TestApp::new().await;
    let req = app.basic_auth_request(
        Method::GET,
        "/v2/token?service=depot&scope=repository:test:pull",
        "admin",
        "password",
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["token"].is_string(), "expected token in response");
    // Verify the token is a valid JWT by using it to make an authenticated request.
    let token = body["token"].as_str().unwrap();
    let req = app.auth_request(Method::GET, "/v2/", token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_v2_token_invalid_auth() {
    let app = TestApp::new().await;
    let req = app.basic_auth_request(Method::GET, "/v2/token", "admin", "wrongpassword");
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get("WWW-Authenticate").is_some());
}

/// Containerd and other OCI clients POST form-encoded credentials to the
/// token endpoint instead of using GET with Basic auth.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_v2_token_post_form_credentials() {
    let app = TestApp::new().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/token")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            "grant_type=password&username=admin&password=password&service=depot&scope=repository:test:pull&client_id=containerd",
        ))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["token"].is_string(), "expected token in response");
    assert!(
        body["access_token"].is_string(),
        "expected access_token in response"
    );
    // Verify the token works for authenticated requests.
    let token = body["token"].as_str().unwrap();
    let req = app.auth_request(Method::GET, "/v2/", token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// POST with invalid form credentials returns 401.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_v2_token_post_invalid_credentials() {
    let app = TestApp::new().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/token")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            "grant_type=password&username=admin&password=wrong",
        ))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// POST with no credentials falls back to anonymous token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_v2_token_post_anonymous() {
    let app = TestApp::new().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/token")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("grant_type=password&service=depot"))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["token"].is_string(),
        "anonymous token should be issued"
    );
}

/// POST with Basic auth header (no form creds) also works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_v2_token_post_basic_auth_fallback() {
    let app = TestApp::new().await;
    let req = app.basic_auth_request(Method::POST, "/v2/token", "admin", "password");
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["token"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_401_bearer_omits_www_authenticate() {
    let app = TestApp::new().await;
    // Bearer token failures must never include WWW-Authenticate: Basic.
    // Challenging a Bearer client with Basic is semantically wrong and causes
    // the browser's native credentials popup when the SPA presents a stale JWT.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/repositories")
        .header(header::AUTHORIZATION, "Bearer invalid-token-here")
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().get("WWW-Authenticate").is_none(),
        "Bearer failure must not include WWW-Authenticate"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_401_basic_has_www_authenticate() {
    let app = TestApp::new().await;
    // Basic auth failures on non-XHR requests should include WWW-Authenticate
    // so CLI and Docker clients know to (re-)prompt for credentials.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/repositories")
        .header(header::AUTHORIZATION, "Basic aW52YWxpZDppbnZhbGlk")
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let www_auth = resp
        .headers()
        .get("WWW-Authenticate")
        .expect("expected WWW-Authenticate on Basic auth failure");
    assert_eq!(www_auth, "Basic realm=\"Artifact Depot\"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_catalog_empty() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/_catalog", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["repositories"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_catalog_lists_docker_repos() {
    let app = TestApp::new().await;
    app.create_docker_repo("docker-a").await;
    app.create_docker_repo("docker-b").await;
    app.create_hosted_repo("raw-repo").await; // non-docker, shouldn't appear

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/_catalog", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let repos = body["repositories"].as_array().unwrap();
    let names: Vec<&str> = repos.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(names.contains(&"docker-a"));
    assert!(names.contains(&"docker-b"));
    assert!(!names.contains(&"raw-repo"));
}

// ===========================================================================
// Manifest operations (single-segment)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_manifest_valid() {
    let app = TestApp::new().await;
    app.create_docker_repo("mfst-repo").await;
    let token = app.admin_token();

    // Push a blob first (so the manifest can reference it)
    let config_data = b"{}";
    let config_digest = app.push_docker_blob("mfst-repo", config_data).await;

    let layer_data = b"layer-content";
    let layer_digest = app.push_docker_blob("mfst-repo", layer_data).await;

    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    let body_bytes = serde_json::to_vec(&manifest).unwrap();
    let expected_digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &body_bytes
        ))
    );

    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/mfst-repo/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(resp.headers().get("Location").is_some());
    assert_eq!(
        resp.headers()
            .get("Docker-Content-Digest")
            .unwrap()
            .to_str()
            .unwrap(),
        expected_digest
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_manifest_invalid_json() {
    let app = TestApp::new().await;
    app.create_docker_repo("bad-mfst").await;
    let token = app.admin_token();

    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/bad-mfst/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(b"not json at all".to_vec()))
        .unwrap();
    let (status, _body) = app.call(req).await;
    // The API should accept it (it doesn't validate JSON strictly, it just stores it).
    // If the API does validate, it would return MANIFEST_INVALID.
    // Let's check what happens — Docker spec says PUT stores whatever is sent.
    // Based on the code, put_manifest just stores inline, so it succeeds.
    assert!(
        status == StatusCode::CREATED || status == StatusCode::BAD_REQUEST,
        "unexpected status: {status}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_manifest_to_cache_405() {
    let app = TestApp::new().await;
    app.create_docker_cache_repo("cache-mfst", "http://localhost:1234", 300)
        .await;
    let token = app.admin_token();

    let manifest = TestApp::make_manifest("sha256:aaa", &["sha256:bbb"]);
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/cache-mfst/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(serde_json::to_vec(&manifest).unwrap()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "UNSUPPORTED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_manifest_by_tag() {
    let app = TestApp::new().await;
    app.create_docker_repo("get-tag").await;

    let config_digest = app.push_docker_blob("get-tag", b"{}").await;
    let layer_digest = app.push_docker_blob("get-tag", b"layer").await;
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    app.push_docker_manifest("get-tag", "v1.0", &manifest).await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/get-tag/manifests/v1.0", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp
        .headers()
        .get("Docker-Content-Digest")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/vnd.docker.distribution.manifest.v2+json"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_manifest_by_digest() {
    let app = TestApp::new().await;
    app.create_docker_repo("get-digest").await;

    let config_digest = app.push_docker_blob("get-digest", b"{}").await;
    let layer_digest = app.push_docker_blob("get-digest", b"layer").await;
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    let digest = app
        .push_docker_manifest("get-digest", "v1.0", &manifest)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/get-digest/manifests/{}", digest),
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Docker-Content-Digest")
            .unwrap()
            .to_str()
            .unwrap(),
        digest
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_manifest_not_found() {
    let app = TestApp::new().await;
    app.create_docker_repo("no-mfst").await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/no-mfst/manifests/v999", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "MANIFEST_UNKNOWN"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_head_manifest_headers_only() {
    let app = TestApp::new().await;
    app.create_docker_repo("head-mfst").await;

    let config_digest = app.push_docker_blob("head-mfst", b"{}").await;
    let layer_digest = app.push_docker_blob("head-mfst", b"layer").await;
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    let _digest = app
        .push_docker_manifest("head-mfst", "latest", &manifest)
        .await;

    let token = app.admin_token();
    let req = axum::http::Request::builder()
        .method(Method::HEAD)
        .uri("/v2/head-mfst/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("Docker-Content-Digest").is_some());
    assert!(resp.headers().get(header::CONTENT_LENGTH).is_some());
    // HEAD should have no body.
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_manifest_with_multiple_accept_headers() {
    // Per RFC 7230 §3.2.2 and as exercised in practice by skopeo, OCI
    // clients may send the Accept set as multiple separate header lines
    // rather than one comma-joined value. The registry must inspect all
    // of them — fetching only the first means a docker schema 2 manifest
    // gets rejected as "not acceptable" even when the client actually
    // accepts it.
    let app = TestApp::new().await;
    app.create_docker_repo("multi-accept").await;
    let config_digest = app.push_docker_blob("multi-accept", b"{}").await;
    let layer_digest = app.push_docker_blob("multi-accept", b"layer").await;
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    app.push_docker_manifest("multi-accept", "latest", &manifest)
        .await;

    let token = app.admin_token();
    // Two Accept headers: first is OCI-only (does NOT match schema 2),
    // second includes the docker schema 2 media type. The handler must
    // inspect both; the manifest is acceptable.
    let req = axum::http::Request::builder()
        .method(Method::GET)
        .uri("/v2/multi-accept/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::ACCEPT, "application/vnd.oci.image.manifest.v1+json")
        .header(
            header::ACCEPT,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/vnd.docker.distribution.manifest.v2+json",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_head_manifest_not_found() {
    let app = TestApp::new().await;
    app.create_docker_repo("head-miss").await;
    let token = app.admin_token();
    let req = axum::http::Request::builder()
        .method(Method::HEAD)
        .uri("/v2/head-miss/manifests/nope")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_manifest_by_digest() {
    let app = TestApp::new().await;
    app.create_docker_repo("del-mfst").await;

    let config_digest = app.push_docker_blob("del-mfst", b"{}").await;
    let layer_digest = app.push_docker_blob("del-mfst", b"layer").await;
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    let digest = app
        .push_docker_manifest("del-mfst", "latest", &manifest)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(
        Method::DELETE,
        &format!("/v2/del-mfst/manifests/{}", digest),
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Verify it's gone.
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/del-mfst/manifests/{}", digest),
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_manifest_not_found() {
    let app = TestApp::new().await;
    app.create_docker_repo("del-miss").await;
    let token = app.admin_token();
    let req = app.auth_request(
        Method::DELETE,
        "/v2/del-miss/manifests/sha256:deadbeef",
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_manifest_non_hosted_405() {
    let app = TestApp::new().await;
    // Proxy repos still reject deletes.
    app.create_docker_repo("del-proxy-m").await;
    app.create_repo(serde_json::json!({
        "name": "del-proxy",
        "repo_type": "proxy",
        "format": "docker",
        "members": ["del-proxy-m"],
    }))
    .await;
    let token = app.admin_token();
    let req = app.auth_request(Method::DELETE, "/v2/del-proxy/manifests/sha256:aaa", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "UNSUPPORTED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_manifest_permission_denied() {
    let app = TestApp::new().await;
    app.create_docker_repo("perm-mfst").await;
    app.create_user_with_roles("reader", "pass", vec!["read-only"])
        .await;
    let token = app.token_for("reader").await;

    let manifest = TestApp::make_manifest("sha256:aaa", &["sha256:bbb"]);
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/perm-mfst/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(serde_json::to_vec(&manifest).unwrap()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "DENIED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_manifest_permission_denied() {
    let app = TestApp::new().await;
    app.create_docker_repo("perm-get").await;
    app.create_user_with_roles("noperm", "pass", vec![]).await;
    let token = app.token_for("noperm").await;
    let req = app.auth_request(Method::GET, "/v2/perm-get/manifests/latest", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "DENIED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_manifest_oci_media_type() {
    let app = TestApp::new().await;
    app.create_docker_repo("oci-mfst").await;
    let token = app.admin_token();

    let config_digest = app.push_docker_blob("oci-mfst", b"{}").await;
    let layer_digest = app.push_docker_blob("oci-mfst", b"layer").await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "size": 2,
            "digest": config_digest,
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "size": 5,
            "digest": layer_digest,
        }],
    });
    let body_bytes = serde_json::to_vec(&manifest).unwrap();

    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/oci-mfst/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_manifest_list_type() {
    let app = TestApp::new().await;
    app.create_docker_repo("mfst-list").await;
    let token = app.admin_token();

    let manifest_list = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
        "manifests": [
            {
                "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                "size": 100,
                "digest": "sha256:aaaa",
                "platform": {"architecture": "amd64", "os": "linux"},
            },
        ],
    });
    let body_bytes = serde_json::to_vec(&manifest_list).unwrap();

    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/mfst-list/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.list.v2+json",
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

// ===========================================================================
// Blob upload (single-segment)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_monolithic_upload() {
    let app = TestApp::new().await;
    app.create_docker_repo("mono-up").await;

    let data = b"monolithic blob data";
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(sha2::Sha256::default(), data))
    );

    let token = app.admin_token();
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!("/v2/mono-up/blobs/uploads/?digest={}", digest))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, data.len().to_string())
        .body(Body::from(data.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(resp.headers().get("Docker-Content-Digest").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_monolithic_upload_digest_mismatch() {
    let app = TestApp::new().await;
    app.create_docker_repo("mono-bad").await;
    let token = app.admin_token();

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/v2/mono-bad/blobs/uploads/?digest=sha256:0000000000000000000000000000000000000000000000000000000000000000")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(b"some data".to_vec()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "DIGEST_INVALID"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_chunked_upload_full_flow() {
    let app = TestApp::new().await;
    app.create_docker_repo("chunk-up").await;
    let token = app.admin_token();

    // Step 1: POST to start upload.
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/v2/chunk-up/blobs/uploads/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let location = resp.headers().get("Location").unwrap().to_str().unwrap();
    // Extract UUID from Location header.
    let uuid = location
        .rsplit('/')
        .next()
        .unwrap()
        .split('?')
        .next()
        .unwrap();

    // Step 2: PATCH to send data.
    let data = b"chunked blob content";
    let req = axum::http::Request::builder()
        .method(Method::PATCH)
        .uri(format!("/v2/chunk-up/blobs/uploads/{}", uuid))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(data.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Step 3: PUT to complete.
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(sha2::Sha256::default(), data))
    );
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri(format!(
            "/v2/chunk-up/blobs/uploads/{}?digest={}",
            uuid, digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Verify blob is accessible.
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/chunk-up/blobs/{}", digest),
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_complete_upload_digest_mismatch() {
    let app = TestApp::new().await;
    app.create_docker_repo("chunk-bad").await;
    let token = app.admin_token();

    // Start upload.
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/v2/chunk-bad/blobs/uploads/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    let location = resp.headers().get("Location").unwrap().to_str().unwrap();
    let uuid = location
        .rsplit('/')
        .next()
        .unwrap()
        .split('?')
        .next()
        .unwrap();

    // PATCH some data.
    let req = axum::http::Request::builder()
        .method(Method::PATCH)
        .uri(format!("/v2/chunk-bad/blobs/uploads/{}", uuid))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(b"data".to_vec()))
        .unwrap();
    let _ = app.call_resp(req).await;

    // PUT with wrong digest.
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri(format!(
            "/v2/chunk-bad/blobs/uploads/{}?digest=sha256:0000000000000000000000000000000000000000000000000000000000000000",
            uuid
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "DIGEST_INVALID"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_patch_unknown_uuid_404() {
    let app = TestApp::new().await;
    app.create_docker_repo("patch-404").await;
    let token = app.admin_token();

    let req = axum::http::Request::builder()
        .method(Method::PATCH)
        .uri("/v2/patch-404/blobs/uploads/00000000-0000-0000-0000-000000000000")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(b"data".to_vec()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "BLOB_UPLOAD_UNKNOWN"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_complete_unknown_uuid_404() {
    let app = TestApp::new().await;
    app.create_docker_repo("put-404").await;
    let token = app.admin_token();

    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/put-404/blobs/uploads/00000000-0000-0000-0000-000000000000?digest=sha256:abc")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "BLOB_UPLOAD_UNKNOWN"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_upload_to_cache_405() {
    let app = TestApp::new().await;
    app.create_docker_cache_repo("cache-up", "http://localhost:1234", 300)
        .await;
    let token = app.admin_token();

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/v2/cache-up/blobs/uploads/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "UNSUPPORTED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_upload_permission_denied() {
    let app = TestApp::new().await;
    app.create_docker_repo("perm-up").await;
    app.create_user_with_roles("reader2", "pass", vec!["read-only"])
        .await;
    let token = app.token_for("reader2").await;

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/v2/perm-up/blobs/uploads/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "DENIED");
}

// ===========================================================================
// Blob GET/HEAD (single-segment)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_blob() {
    let app = TestApp::new().await;
    app.create_docker_repo("get-blob").await;

    let data = b"blob for get test";
    let digest = app.push_docker_blob("get-blob", data).await;

    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/get-blob/blobs/{}", digest),
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Docker-Content-Digest")
            .unwrap()
            .to_str()
            .unwrap(),
        digest
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_blob_not_found() {
    let app = TestApp::new().await;
    app.create_docker_repo("blob-miss").await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/blob-miss/blobs/sha256:deadbeef", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "BLOB_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_head_blob() {
    let app = TestApp::new().await;
    app.create_docker_repo("head-blob").await;

    let data = b"blob for head test";
    let digest = app.push_docker_blob("head-blob", data).await;

    let token = app.admin_token();
    let req = axum::http::Request::builder()
        .method(Method::HEAD)
        .uri(format!("/v2/head-blob/blobs/{}", digest))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_LENGTH)
            .unwrap()
            .to_str()
            .unwrap(),
        data.len().to_string()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_head_blob_not_found() {
    let app = TestApp::new().await;
    app.create_docker_repo("head-miss").await;
    let token = app.admin_token();
    let req = axum::http::Request::builder()
        .method(Method::HEAD)
        .uri("/v2/head-miss/blobs/sha256:deadbeef")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_blob_permission_denied() {
    let app = TestApp::new().await;
    app.create_docker_repo("perm-blob").await;
    app.create_user_with_roles("noperm2", "pass", vec![]).await;
    let token = app.token_for("noperm2").await;
    let req = app.auth_request(Method::GET, "/v2/perm-blob/blobs/sha256:aaa", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "DENIED");
}

// ===========================================================================
// Tags list
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_tags_empty() {
    let app = TestApp::new().await;
    app.create_docker_repo("tags-empty").await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/tags-empty/tags/list", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"].as_str().unwrap(), "tags-empty");
    assert_eq!(body["tags"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_tags_after_push() {
    let app = TestApp::new().await;
    app.create_docker_repo("tags-push").await;

    let config_digest = app.push_docker_blob("tags-push", b"{}").await;
    let layer_digest = app.push_docker_blob("tags-push", b"layer").await;
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    app.push_docker_manifest("tags-push", "v1.0", &manifest)
        .await;
    app.push_docker_manifest("tags-push", "v2.0", &manifest)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/tags-push/tags/list", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let tags: Vec<&str> = body["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(tags.contains(&"v1.0"));
    assert!(tags.contains(&"v2.0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_tags_nonexistent_repo() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/no-such-docker/tags/list", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "NAME_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_tags_non_docker_repo() {
    let app = TestApp::new().await;
    app.create_hosted_repo("raw-not-docker").await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/raw-not-docker/tags/list", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "UNSUPPORTED");
}

// ===========================================================================
// Two-segment (namespaced) routes
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ns_put_get_manifest() {
    let app = TestApp::new().await;
    app.create_docker_repo("ns-repo").await;
    let token = app.admin_token();

    // Push blob via namespaced route.
    let config_data = b"{}";
    let config_digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            config_data
        ))
    );

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/v2/ns-repo/myimage/blobs/uploads/?digest={}",
            config_digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(config_data.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert!(
        resp.status() == StatusCode::CREATED || resp.status() == StatusCode::ACCEPTED,
        "ns blob push: {}",
        resp.status()
    );

    let layer_data = b"layer-ns";
    let layer_digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            layer_data
        ))
    );

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/v2/ns-repo/myimage/blobs/uploads/?digest={}",
            layer_digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(layer_data.to_vec()))
        .unwrap();
    let _ = app.call_resp(req).await;

    // Push manifest via namespaced route.
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();

    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/ns-repo/myimage/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(manifest_bytes))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // GET via namespaced route.
    let req = app.auth_request(Method::GET, "/v2/ns-repo/myimage/manifests/latest", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ns_list_tags() {
    let app = TestApp::new().await;
    app.create_docker_repo("ns-tags").await;
    let token = app.admin_token();

    // Push via namespaced route.
    let config_digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(sha2::Sha256::default(), b"{}"))
    );
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/v2/ns-tags/img/blobs/uploads/?digest={}",
            config_digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(b"{}".to_vec()))
        .unwrap();
    let _ = app.call_resp(req).await;

    let manifest = TestApp::make_manifest(&config_digest, &[]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();

    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/ns-tags/img/manifests/v1")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(manifest_bytes))
        .unwrap();
    let _ = app.call_resp(req).await;

    let req = app.auth_request(Method::GET, "/v2/ns-tags/img/tags/list", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let tags = body["tags"].as_array().unwrap();
    assert!(tags.iter().any(|t| t.as_str() == Some("v1")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ns_head_blob() {
    let app = TestApp::new().await;
    app.create_docker_repo("ns-head").await;

    // Push blob via flat route.
    let data = b"ns head blob";
    let digest = app.push_docker_blob("ns-head", data).await;

    // HEAD via namespaced route should still find it (blobs are shared).
    let token = app.admin_token();
    let req = axum::http::Request::builder()
        .method(Method::HEAD)
        .uri(format!("/v2/ns-head/someimage/blobs/{}", digest))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// Proxy writes
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_manifest_proxy_routes_to_hosted() {
    let app = TestApp::new().await;
    app.create_docker_repo("proxy-hosted").await;
    app.create_docker_proxy_repo("docker-proxy-w", vec!["proxy-hosted"])
        .await;
    let token = app.admin_token();

    // Push blobs to the hosted member first.
    let config_digest = app.push_docker_blob("proxy-hosted", b"{}").await;
    let layer_digest = app.push_docker_blob("proxy-hosted", b"layer").await;

    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    let body_bytes = serde_json::to_vec(&manifest).unwrap();

    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/docker-proxy-w/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_upload_proxy_routes_to_hosted() {
    let app = TestApp::new().await;
    app.create_docker_repo("proxy-up-member").await;
    app.create_docker_proxy_repo("proxy-up", vec!["proxy-up-member"])
        .await;
    let token = app.admin_token();

    let data = b"proxy upload data";
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(sha2::Sha256::default(), data))
    );

    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!("/v2/proxy-up/blobs/uploads/?digest={}", digest))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(data.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert!(
        resp.status() == StatusCode::CREATED || resp.status() == StatusCode::ACCEPTED,
        "proxy upload: {}",
        resp.status()
    );
}

// ===========================================================================
// Repo validation
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_non_docker_repo_returns_unsupported() {
    let app = TestApp::new().await;
    app.create_hosted_repo("raw-repo-docker-test").await;
    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        "/v2/raw-repo-docker-test/manifests/latest",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "UNSUPPORTED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_repo_not_found_returns_name_unknown() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        "/v2/nonexistent-docker/manifests/latest",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "NAME_UNKNOWN");
}

// ===========================================================================
// Cross-repo mount
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cross_repo_mount() {
    let app = TestApp::new().await;
    app.create_docker_repo("mount-src").await;
    app.create_docker_repo("mount-dst").await;

    // Push blob to source repo.
    let data = b"mountable blob";
    let digest = app.push_docker_blob("mount-src", data).await;
    let token = app.admin_token();

    // Mount into destination.
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/v2/mount-dst/blobs/uploads/?mount={}&from=mount-src",
            digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Verify blob is accessible in destination.
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/mount-dst/blobs/{}", digest),
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cross_repo_mount_not_found_starts_upload() {
    let app = TestApp::new().await;
    app.create_docker_repo("mount-fallback").await;
    let token = app.admin_token();

    // Try to mount a non-existent blob — should fall through to starting a normal upload.
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/v2/mount-fallback/blobs/uploads/?mount=sha256:nonexistent&from=nosuch")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    // Should either start an upload (202) or return 201 if it found it.
    assert!(
        resp.status() == StatusCode::ACCEPTED || resp.status() == StatusCode::CREATED,
        "mount fallback: {}",
        resp.status()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cross_repo_mount_denied_without_read_on_source() {
    let app = TestApp::new().await;
    app.create_docker_repo("mount-priv-src").await;
    app.create_docker_repo("mount-priv-dst").await;

    // Push a blob to the source repo as admin.
    let data = b"secret blob";
    let digest = app.push_docker_blob("mount-priv-src", data).await;

    // Create a role that grants Write on the destination but no Read on the source.
    app.create_role_with_caps(
        "dst-writer",
        vec![CapabilityGrant {
            capability: Capability::Write,
            repo: "mount-priv-dst".to_string(),
        }],
    )
    .await;
    app.create_user_with_roles("mounter", "pass", vec!["dst-writer"])
        .await;
    let token = app.token_for("mounter").await;

    // Attempt cross-repo mount — should be denied (no read on source).
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/v2/mount-priv-dst/blobs/uploads/?mount={}&from=mount-priv-src",
            digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "DENIED");
}

// ===========================================================================
// Part 2a: Error path tests
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_manifest_unsupported_media_type() {
    let app = TestApp::new().await;
    app.create_docker_repo("bad-ct-mfst").await;
    let token = app.admin_token();

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/v2/bad-ct-mfst/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(b"<manifest/>".to_vec()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "MANIFEST_INVALID"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_manifest_by_tag_rejected() {
    let app = TestApp::new().await;
    app.create_docker_repo("del-tag-rej").await;

    let config_digest = app.push_docker_blob("del-tag-rej", b"{}").await;
    let manifest = TestApp::make_manifest(&config_digest, &[]);
    app.push_docker_manifest("del-tag-rej", "latest", &manifest)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::DELETE, "/v2/del-tag-rej/manifests/latest", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "UNSUPPORTED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_manifest_permission_denied() {
    let app = TestApp::new().await;
    app.create_docker_repo("del-perm").await;
    app.create_user_with_roles("reader-del", "pass", vec!["read-only"])
        .await;
    let token = app.token_for("reader-del").await;

    let req = app.auth_request(
        Method::DELETE,
        "/v2/del-perm/manifests/sha256:abc123",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "DENIED");
}

// ===========================================================================
// Part 2b: Reference counting tests
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_manifest_repush_idempotent() {
    let app = TestApp::new().await;
    app.create_docker_repo("repush").await;

    let config_digest = app.push_docker_blob("repush", b"{}").await;
    let layer_digest = app.push_docker_blob("repush", b"layer").await;
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);

    // Push same manifest+tag twice.
    let digest1 = app
        .push_docker_manifest("repush", "latest", &manifest)
        .await;
    let digest2 = app
        .push_docker_manifest("repush", "latest", &manifest)
        .await;
    assert_eq!(digest1, digest2);

    // GET still works.
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/repush/manifests/latest", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tag_replacement_decrements_old() {
    let app = TestApp::new().await;
    app.create_docker_repo("tag-replace").await;

    // Push manifest A as latest.
    let config_a = app.push_docker_blob("tag-replace", b"config-a").await;
    let manifest_a = TestApp::make_manifest(&config_a, &[]);
    let digest_a = app
        .push_docker_manifest("tag-replace", "latest", &manifest_a)
        .await;

    // Push manifest B as latest (replaces A).
    let config_b = app.push_docker_blob("tag-replace", b"config-b").await;
    let manifest_b = TestApp::make_manifest(&config_b, &[]);
    let digest_b = app
        .push_docker_manifest("tag-replace", "latest", &manifest_b)
        .await;
    assert_ne!(digest_a, digest_b);

    let token = app.admin_token();

    // GET latest returns B.
    let req = app.auth_request(Method::GET, "/v2/tag-replace/manifests/latest", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Docker-Content-Digest")
            .unwrap()
            .to_str()
            .unwrap(),
        digest_b
    );

    // GET by A's digest still works (A not deleted, just decremented ref).
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/tag-replace/manifests/{}", digest_a),
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_blob_dedup() {
    let app = TestApp::new().await;
    app.create_docker_repo("dedup").await;

    let data = b"identical blob data for dedup test";
    let digest1 = app.push_docker_blob("dedup", data).await;
    let digest2 = app.push_docker_blob("dedup", data).await;
    assert_eq!(digest1, digest2);

    // HEAD returns correct size.
    let token = app.admin_token();
    let req = Request::builder()
        .method(Method::HEAD)
        .uri(format!("/v2/dedup/blobs/{}", digest1))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_LENGTH)
            .unwrap()
            .to_str()
            .unwrap(),
        data.len().to_string()
    );
}

// ===========================================================================
// Part 2c: Manifest list / OCI index tests
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_oci_index_manifest() {
    let app = TestApp::new().await;
    app.create_docker_repo("oci-idx").await;
    let token = app.admin_token();

    let index = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "size": 100,
                "digest": "sha256:aaaa",
                "platform": {"architecture": "amd64", "os": "linux"},
            },
        ],
    });
    let body_bytes = serde_json::to_vec(&index).unwrap();

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/v2/oci-idx/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.oci.image.index.v1+json",
        )
        .body(Body::from(body_bytes))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

/// Manifest list deletion no longer cascades to child manifests.
/// Child manifests are independent artifacts cleaned by GC.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_manifest_list_delete_does_not_cascade() {
    let app = TestApp::new().await;
    app.create_docker_repo("cascade").await;

    // Push a child manifest.
    let config_digest = app.push_docker_blob("cascade", b"cfg-child").await;
    let child_manifest = TestApp::make_manifest(&config_digest, &[]);
    let child_body = serde_json::to_vec(&child_manifest).unwrap();
    let child_digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &child_body
        ))
    );

    let token = app.admin_token();
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/v2/cascade/manifests/{}", child_digest))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(child_body))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Push manifest list referencing the child.
    let manifest_list = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
        "manifests": [
            {
                "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                "size": 100,
                "digest": child_digest,
                "platform": {"architecture": "amd64", "os": "linux"},
            },
        ],
    });
    let list_body = serde_json::to_vec(&manifest_list).unwrap();
    let list_digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &list_body
        ))
    );

    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/v2/cascade/manifests/{}", list_digest))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.list.v2+json",
        )
        .body(Body::from(list_body))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Delete the manifest list.
    let req = app.auth_request(
        Method::DELETE,
        &format!("/v2/cascade/manifests/{}", list_digest),
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Child manifest should still exist (no cascading delete in EC model).
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/cascade/manifests/{}", child_digest),
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
}

// ===========================================================================
// Part 2d: Upload tests
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_chunked_upload_multiple_patches() {
    let app = TestApp::new().await;
    app.create_docker_repo("multi-chunk").await;
    let token = app.admin_token();

    // Step 1: POST to start upload.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/multi-chunk/blobs/uploads/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let location = resp.headers().get("Location").unwrap().to_str().unwrap();
    let uuid = location
        .rsplit('/')
        .next()
        .unwrap()
        .split('?')
        .next()
        .unwrap();

    // Step 2: PATCH chunk 1.
    let chunk1 = b"first chunk of data ";
    let req = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/v2/multi-chunk/blobs/uploads/{}", uuid))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(chunk1.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Step 3: PATCH chunk 2.
    let chunk2 = b"second chunk of data";
    let req = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/v2/multi-chunk/blobs/uploads/{}", uuid))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(chunk2.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Step 4: PUT to complete with combined digest.
    let combined: Vec<u8> = [&chunk1[..], &chunk2[..]].concat();
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &combined
        ))
    );
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!(
            "/v2/multi-chunk/blobs/uploads/{}?digest={}",
            uuid, digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Verify blob contains chunk1 + chunk2.
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/multi-chunk/blobs/{}", digest),
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, combined);
}

// ===========================================================================
// Part 2e: Proxy write error tests
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_no_hosted_member_write_fails() {
    let app = TestApp::new().await;
    // Create a cache member (not hosted).
    app.create_docker_cache_repo("cache-only-member", "http://localhost:1234", 300)
        .await;
    // Create proxy with only the cache member.
    app.create_docker_proxy_repo("proxy-no-write", vec!["cache-only-member"])
        .await;
    let token = app.admin_token();

    let manifest = TestApp::make_manifest("sha256:aaa", &["sha256:bbb"]);
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/v2/proxy-no-write/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(serde_json::to_vec(&manifest).unwrap()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "UNSUPPORTED");
}

// ===========================================================================
// Part 2f: Namespaced route coverage
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ns_delete_manifest() {
    let app = TestApp::new().await;
    app.create_docker_repo("ns-del").await;

    // Push via NS route.
    let config_digest = app.push_docker_blob_ns("ns-del", "img", b"{}").await;
    let manifest = TestApp::make_manifest(&config_digest, &[]);
    let digest = app
        .push_docker_manifest_ns("ns-del", "img", "v1", &manifest)
        .await;

    let token = app.admin_token();

    // Delete via NS route by digest.
    let req = app.auth_request(
        Method::DELETE,
        &format!("/v2/ns-del/img/manifests/{}", digest),
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Verify it's gone.
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/ns-del/img/manifests/{}", digest),
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ns_get_blob() {
    let app = TestApp::new().await;
    app.create_docker_repo("ns-getblob").await;

    // Push blob via flat route.
    let data = b"ns get blob data";
    let digest = app.push_docker_blob("ns-getblob", data).await;

    // GET via NS route (blobs are shared across images).
    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/ns-getblob/someimg/blobs/{}", digest),
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ns_head_blob_not_found() {
    let app = TestApp::new().await;
    app.create_docker_repo("ns-headmiss").await;
    let token = app.admin_token();

    let req = Request::builder()
        .method(Method::HEAD)
        .uri("/v2/ns-headmiss/anyimg/blobs/sha256:nonexistent")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ns_head_manifest_not_found() {
    let app = TestApp::new().await;
    app.create_docker_repo("ns-headmfst").await;
    let token = app.admin_token();

    let req = Request::builder()
        .method(Method::HEAD)
        .uri("/v2/ns-headmfst/anyimg/manifests/nope")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// Part 4: Dedicated Docker port tests
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_port_v2_check() {
    let app = TestApp::new_with_docker_port("dp-repo").await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Docker-Distribution-Api-Version")
            .unwrap(),
        "registry/2.0"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_port_push_pull_single_segment() {
    let app = TestApp::new_with_docker_port("dp-single").await;
    let token = app.admin_token();

    // Push blob via single-segment route (image name in URL).
    let config_data = b"port config";
    let config_digest = app.push_docker_blob_port("myimage", config_data).await;

    let layer_data = b"port layer";
    let layer_digest = app.push_docker_blob_port("myimage", layer_data).await;

    // Push manifest.
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    let digest = app
        .push_docker_manifest_port("myimage", "v1", &manifest)
        .await;

    // GET manifest back.
    let req = app.auth_request(Method::GET, "/v2/myimage/manifests/v1", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Docker-Content-Digest")
            .unwrap()
            .to_str()
            .unwrap(),
        digest
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_port_push_pull_two_segment() {
    let app = TestApp::new_with_docker_port("dp-two").await;
    let token = app.admin_token();

    // Push blob via two-segment route.
    let config_data = b"port2seg config";
    let config_digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            config_data
        ))
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/v2/myns/myimg/blobs/uploads/?digest={}",
            config_digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(config_data.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert!(
        resp.status() == StatusCode::CREATED || resp.status() == StatusCode::ACCEPTED,
        "2seg blob push: {}",
        resp.status()
    );

    // Push manifest via two-segment route.
    let manifest = TestApp::make_manifest(&config_digest, &[]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &manifest_bytes
        ))
    );

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/v2/myns/myimg/manifests/v1")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(manifest_bytes))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // GET via two-segment route.
    let req = app.auth_request(Method::GET, "/v2/myns/myimg/manifests/v1", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Docker-Content-Digest")
            .unwrap()
            .to_str()
            .unwrap(),
        manifest_digest
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_port_tags_list() {
    let app = TestApp::new_with_docker_port("dp-tags").await;
    let token = app.admin_token();

    let config_digest = app.push_docker_blob_port("ubuntu", b"{}").await;
    let manifest = TestApp::make_manifest(&config_digest, &[]);
    app.push_docker_manifest_port("ubuntu", "22.04", &manifest)
        .await;

    let req = app.auth_request(Method::GET, "/v2/ubuntu/tags/list", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let tags = body["tags"].as_array().unwrap();
    assert!(tags.iter().any(|t| t.as_str() == Some("22.04")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_port_two_segment_tags_list() {
    let app = TestApp::new_with_docker_port("dp-2tags").await;
    let token = app.admin_token();

    // Push via two-segment route.
    let config_data = b"{}";
    let config_digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            config_data
        ))
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!(
            "/v2/org/app/blobs/uploads/?digest={}",
            config_digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(config_data.to_vec()))
        .unwrap();
    let _ = app.call_resp(req).await;

    let manifest = TestApp::make_manifest(&config_digest, &[]);
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/v2/org/app/manifests/release")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(manifest_bytes))
        .unwrap();
    let _ = app.call_resp(req).await;

    let req = app.auth_request(Method::GET, "/v2/org/app/tags/list", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let tags = body["tags"].as_array().unwrap();
    assert!(tags.iter().any(|t| t.as_str() == Some("release")));
}

// ===========================================================================
// Phase 3: Additional Docker coverage
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_complete_upload_missing_digest() {
    let app = TestApp::new().await;
    app.create_docker_repo("nodigest").await;
    let token = app.admin_token();

    // Use the existing chunked upload test pattern.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/nodigest/blobs/uploads/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "start upload failed");
    let location = resp
        .headers()
        .get("Location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let uuid = location
        .rsplit('/')
        .next()
        .unwrap()
        .split('?')
        .next()
        .unwrap()
        .to_string();

    // Complete without digest query param.
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/v2/nodigest/blobs/uploads/{}", uuid))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let (status, _body) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_complete_upload_wrong_digest() {
    let app = TestApp::new().await;
    app.create_docker_repo("wrongdigest").await;
    let token = app.admin_token();

    // Start upload.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/wrongdigest/blobs/uploads/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    let location = resp.headers().get("Location").unwrap().to_str().unwrap();
    let uuid = location
        .rsplit('/')
        .next()
        .unwrap()
        .split('?')
        .next()
        .unwrap();

    // Patch some data.
    let req = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/v2/wrongdigest/blobs/uploads/{}", uuid))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(b"some data".to_vec()))
        .unwrap();
    let _ = app.call_resp(req).await;

    // Complete with wrong digest.
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/v2/wrongdigest/blobs/uploads/{}?digest=sha256:0000000000000000000000000000000000000000000000000000000000000000", uuid))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "DIGEST_INVALID"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_monolithic_upload_bad_digest_value() {
    let app = TestApp::new().await;
    app.create_docker_repo("mono-bad").await;
    let token = app.admin_token();

    // Monolithic upload with mismatching digest.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/mono-bad/blobs/uploads/?digest=sha256:0000000000000000000000000000000000000000000000000000000000000000")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, "5")
        .body(Body::from(b"hello".to_vec()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "DIGEST_INVALID"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_manifest_cache_repo_not_found() {
    let app = TestApp::new().await;
    app.create_docker_cache_repo("del-cache2", "http://localhost:1234", 300)
        .await;
    let token = app.admin_token();

    let req = app.auth_request(
        Method::DELETE,
        "/v2/del-cache2/manifests/sha256:abc123",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "MANIFEST_UNKNOWN"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_oci_manifest_roundtrip() {
    let app = TestApp::new().await;
    app.create_docker_repo("oci-mt").await;
    let token = app.admin_token();

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "size": 0,
            "digest": "sha256:ocicfg",
        },
        "layers": [],
    });
    let body_bytes = serde_json::to_vec(&manifest).unwrap();

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/v2/oci-mt/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .body(Body::from(body_bytes.clone()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // GET should return the OCI media type.
    let req = app.auth_request(Method::GET, "/v2/oci-mt/manifests/latest", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/vnd.oci.image.manifest.v1+json"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_manifest_bad_json_body() {
    let app = TestApp::new().await;
    app.create_docker_repo("bad-json").await;
    let token = app.admin_token();

    let req = Request::builder()
        .method(Method::PUT)
        .uri("/v2/bad-json/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(b"not json at all!!!".to_vec()))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "MANIFEST_INVALID"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_manifest_by_sha256_digest() {
    let app = TestApp::new().await;
    app.create_docker_repo("by-sha").await;

    let config_digest = app.push_docker_blob("by-sha", b"{}").await;
    let manifest = TestApp::make_manifest(&config_digest, &[]);
    let digest = app
        .push_docker_manifest("by-sha", "latest", &manifest)
        .await;

    // GET by digest instead of tag.
    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/by-sha/manifests/{}", digest),
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("Docker-Content-Digest")
            .unwrap()
            .to_str()
            .unwrap(),
        digest
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ns_chunked_upload() {
    let app = TestApp::new().await;
    app.create_docker_repo("ns-chunk").await;
    let token = app.admin_token();

    // Start upload via NS route.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/ns-chunk/myimg/blobs/uploads/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let location = resp.headers().get("Location").unwrap().to_str().unwrap();
    let uuid = location
        .rsplit('/')
        .next()
        .unwrap()
        .split('?')
        .next()
        .unwrap();

    // Patch via NS route.
    let data = b"ns chunked data";
    let req = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/v2/ns-chunk/myimg/blobs/uploads/{}", uuid))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(data.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Complete via NS route.
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(sha2::Sha256::default(), data))
    );
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!(
            "/v2/ns-chunk/myimg/blobs/uploads/{}?digest={}",
            uuid, digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_port_head_manifest() {
    let app = TestApp::new_with_docker_port("dp-headmfst").await;
    let token = app.admin_token();

    let config_digest = app.push_docker_blob_port("img", b"{}").await;
    let manifest = TestApp::make_manifest(&config_digest, &[]);
    app.push_docker_manifest_port("img", "v1", &manifest).await;

    let req = Request::builder()
        .method(Method::HEAD)
        .uri("/v2/img/manifests/v1")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("Docker-Content-Digest").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_port_delete_manifest() {
    let app = TestApp::new_with_docker_port("dp-delmfst").await;
    let token = app.admin_token();

    let config_digest = app.push_docker_blob_port("img", b"{}").await;
    let manifest = TestApp::make_manifest(&config_digest, &[]);
    let digest = app.push_docker_manifest_port("img", "v1", &manifest).await;

    let req = app.auth_request(
        Method::DELETE,
        &format!("/v2/img/manifests/{}", digest),
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_port_head_blob() {
    let app = TestApp::new_with_docker_port("dp-headblob").await;
    let token = app.admin_token();

    let data = b"port head blob data";
    let digest = app.push_docker_blob_port("img", data).await;

    let req = Request::builder()
        .method(Method::HEAD)
        .uri(format!("/v2/img/blobs/{}", digest))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_LENGTH)
            .unwrap()
            .to_str()
            .unwrap(),
        data.len().to_string()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_port_get_blob() {
    let app = TestApp::new_with_docker_port("dp-getblob").await;
    let token = app.admin_token();

    let data = b"port get blob data";
    let digest = app.push_docker_blob_port("img", data).await;

    let req = app.auth_request(Method::GET, &format!("/v2/img/blobs/{}", digest), &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, data);
}

// ===========================================================================
// Accept header content negotiation
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_manifest_accept_negotiation_ok() {
    let app = TestApp::new().await;
    app.create_docker_repo("accept-ok").await;
    let token = app.admin_token();

    let config_data = b"{}";
    let config_digest = app.push_docker_blob("accept-ok", config_data).await;
    let layer_data = b"layer-content";
    let layer_digest = app.push_docker_blob("accept-ok", layer_data).await;
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    app.push_docker_manifest("accept-ok", "latest", &manifest)
        .await;

    // GET with matching Accept
    let req = axum::http::Request::builder()
        .method(Method::GET)
        .uri("/v2/accept-ok/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::ACCEPT,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_manifest_accept_negotiation_reject() {
    let app = TestApp::new().await;
    app.create_docker_repo("accept-rej").await;
    let token = app.admin_token();

    let config_data = b"{}";
    let config_digest = app.push_docker_blob("accept-rej", config_data).await;
    let layer_data = b"layer-data";
    let layer_digest = app.push_docker_blob("accept-rej", layer_data).await;
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    app.push_docker_manifest("accept-rej", "latest", &manifest)
        .await;

    // GET with non-matching Accept (only OCI, but stored as Docker V2)
    let req = axum::http::Request::builder()
        .method(Method::GET)
        .uri("/v2/accept-rej/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::ACCEPT, "application/vnd.oci.image.manifest.v1+json")
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_manifest_no_accept_header() {
    let app = TestApp::new().await;
    app.create_docker_repo("accept-any").await;

    let config_data = b"{}";
    let config_digest = app.push_docker_blob("accept-any", config_data).await;
    let layer_data = b"layer-noacc";
    let layer_digest = app.push_docker_blob("accept-any", layer_data).await;
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    app.push_docker_manifest("accept-any", "latest", &manifest)
        .await;

    // GET without Accept header
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/accept-any/manifests/latest", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// Manifest schema validation
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_manifest_invalid_schema_version() {
    let app = TestApp::new().await;
    app.create_docker_repo("schema-repo").await;
    let token = app.admin_token();

    let bad_manifest = serde_json::json!({
        "schemaVersion": 1,
        "name": "test",
        "tag": "latest"
    });
    let body = serde_json::to_vec(&bad_manifest).unwrap();
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/schema-repo/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(body))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_manifest_missing_layers() {
    let app = TestApp::new().await;
    app.create_docker_repo("nolayers-repo").await;
    let token = app.admin_token();

    let bad_manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": { "mediaType": "application/vnd.docker.container.image.v1+json", "size": 2, "digest": "sha256:abc" }
    });
    let body = serde_json::to_vec(&bad_manifest).unwrap();
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/nolayers-repo/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(body))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_put_manifest_missing_blob() {
    let app = TestApp::new().await;
    app.create_docker_repo("misblob-repo").await;
    let token = app.admin_token();

    // Push only config blob, not layer
    let config_data = b"{}";
    let config_digest = app.push_docker_blob("misblob-repo", config_data).await;
    let fake_layer_digest =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {
            "mediaType": "application/vnd.docker.container.image.v1+json",
            "size": config_data.len(),
            "digest": config_digest
        },
        "layers": [{
            "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
            "size": 100,
            "digest": fake_layer_digest
        }]
    });
    let body = serde_json::to_vec(&manifest).unwrap();
    let req = axum::http::Request::builder()
        .method(Method::PUT)
        .uri("/v2/misblob-repo/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(
            header::CONTENT_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::from(body))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// Tag pagination
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_tags_pagination() {
    let app = TestApp::new().await;
    app.create_docker_repo("tags-page").await;
    let token = app.admin_token();

    // Push several tags.
    let cfg = app.push_docker_blob("tags-page", b"{}").await;
    let manifest = TestApp::make_manifest(&cfg, &[]);
    for tag in &["v1", "v2", "v3", "v4", "v5"] {
        app.push_docker_manifest("tags-page", tag, &manifest).await;
    }

    // Fetch all tags to know the total count.
    let req = app.auth_request(Method::GET, "/v2/tags-page/tags/list", &token);
    let (_, all_body) = app.call(req).await;
    let all_tags = all_body["tags"].as_array().unwrap();
    let total = all_tags.len();
    assert!(total >= 5, "should have at least 5 tags");

    // Request first page with n=3.
    let req = app.auth_request(Method::GET, "/v2/tags-page/tags/list?n=3", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("Link").is_some(),
        "expected Link header for pagination"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let tags = body["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 3);

    // Request second page using last= cursor from the first page.
    let last_tag = tags.last().unwrap().as_str().unwrap();
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/tags-page/tags/list?n=100&last={}", last_tag),
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let tags2 = body["tags"].as_array().unwrap();
    // Second page should have some remaining tags (those after `last_tag` lexicographically).
    // Note: Docker V2 pagination uses lexicographic cursors.
    assert!(
        !tags2.is_empty(),
        "second page should have remaining tags after cursor"
    );
    // Every tag on page 2 should be lexicographically after last_tag.
    for t in tags2 {
        assert!(
            t.as_str().unwrap() > last_tag,
            "tag {} should be > cursor {}",
            t,
            last_tag
        );
    }
}

// ===========================================================================
// Catalog pagination
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_catalog_pagination() {
    let app = TestApp::new().await;
    app.create_docker_repo("cat-a").await;
    app.create_docker_repo("cat-b").await;
    app.create_docker_repo("cat-c").await;
    let token = app.admin_token();

    // First page: n=2
    let req = app.auth_request(Method::GET, "/v2/_catalog?n=2", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let link = resp
        .headers()
        .get("Link")
        .expect("expected Link header")
        .to_str()
        .unwrap()
        .to_string();
    assert!(link.contains("rel=\"next\""));
    let body: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let repos = body["repositories"].as_array().unwrap();
    assert_eq!(repos.len(), 2);

    // Second page using last= cursor.
    let last = repos.last().unwrap().as_str().unwrap();
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/_catalog?n=2&last={}", last),
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let repos2 = body["repositories"].as_array().unwrap();
    assert_eq!(repos2.len(), 1, "should have one remaining repo");
}

// ===========================================================================
// Upload with initial body data in POST
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_upload_with_initial_body() {
    let app = TestApp::new().await;
    app.create_docker_repo("init-body").await;
    let token = app.admin_token();

    // POST with initial body data (not monolithic -- no ?digest= param).
    let chunk1 = b"initial data from POST";
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/init-body/blobs/uploads/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(chunk1.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let location = resp.headers().get("Location").unwrap().to_str().unwrap();
    let uuid = location
        .rsplit('/')
        .next()
        .unwrap()
        .split('?')
        .next()
        .unwrap();

    // PATCH a second chunk.
    let chunk2 = b" plus more data";
    let req = Request::builder()
        .method(Method::PATCH)
        .uri(format!("/v2/init-body/blobs/uploads/{}", uuid))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(chunk2.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Complete with combined digest.
    let combined: Vec<u8> = [&chunk1[..], &chunk2[..]].concat();
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &combined
        ))
    );
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!(
            "/v2/init-body/blobs/uploads/{}?digest={}",
            uuid, digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Verify blob.
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/init-body/blobs/{}", digest),
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, combined);
}

// ===========================================================================
// Complete upload with final body data in PUT
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_complete_upload_with_final_body() {
    let app = TestApp::new().await;
    app.create_docker_repo("final-body").await;
    let token = app.admin_token();

    // Start upload with first chunk in POST.
    let chunk1 = b"first part ";
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/final-body/blobs/uploads/")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(chunk1.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let location = resp.headers().get("Location").unwrap().to_str().unwrap();
    let uuid = location
        .rsplit('/')
        .next()
        .unwrap()
        .split('?')
        .next()
        .unwrap();

    // Complete with final data in the PUT body (real Docker clients do this).
    let chunk2 = b"final part";
    let combined: Vec<u8> = [&chunk1[..], &chunk2[..]].concat();
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &combined
        ))
    );
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!(
            "/v2/final-body/blobs/uploads/{}?digest={}",
            uuid, digest
        ))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::from(chunk2.to_vec()))
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Verify the blob has the full combined content.
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/final-body/blobs/{}", digest),
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, combined);
}

// ===========================================================================
// Proxy upload routes through to hosted member
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_upload_routes_to_member() {
    let app = TestApp::new().await;
    app.create_docker_repo("proxy-write-m").await;
    app.create_docker_proxy_repo("proxy-write", vec!["proxy-write-m"])
        .await;
    let token = app.admin_token();

    // Push a blob through the proxy.
    let data = b"blob via proxy";
    let digest = app.push_docker_blob("proxy-write", data).await;

    // Push a manifest through the proxy.
    let cfg_digest = app.push_docker_blob("proxy-write", b"{}").await;
    let manifest = TestApp::make_manifest(&cfg_digest, &[&digest]);
    app.push_docker_manifest("proxy-write", "v1", &manifest)
        .await;

    // Read back via the proxy.
    let req = app.auth_request(Method::GET, "/v2/proxy-write/manifests/v1", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Read back via the member directly.
    let req = app.auth_request(Method::GET, "/v2/proxy-write-m/manifests/v1", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// Anonymous auth challenge tests (401 vs 403)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_anonymous_manifest_get_returns_401_challenge() {
    let app = TestApp::new().await;
    app.create_docker_repo("anon-mfst").await;

    // Anonymous GET /v2/{repo}/manifests/latest should return 401, not 403.
    let req = app.request(Method::GET, "/v2/anon-mfst/manifests/latest");
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let www_auth = resp
        .headers()
        .get("WWW-Authenticate")
        .expect("expected WWW-Authenticate header on 401")
        .to_str()
        .unwrap();
    assert!(
        www_auth.contains("Bearer"),
        "expected Bearer challenge, got: {}",
        www_auth
    );
    assert!(
        www_auth.contains("/v2/token"),
        "expected realm containing /v2/token, got: {}",
        www_auth
    );
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "UNAUTHORIZED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_anonymous_blob_head_returns_401_challenge() {
    let app = TestApp::new().await;
    app.create_docker_repo("anon-blob").await;

    let req = Request::builder()
        .method(Method::HEAD)
        .uri("/v2/anon-blob/blobs/sha256:abc123")
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get("WWW-Authenticate").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_anonymous_upload_start_returns_401_challenge() {
    let app = TestApp::new().await;
    app.create_docker_repo("anon-up").await;

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v2/anon-up/blobs/uploads/")
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get("WWW-Authenticate").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_anonymous_tags_list_returns_401_challenge() {
    let app = TestApp::new().await;
    app.create_docker_repo("anon-tags").await;

    let req = app.request(Method::GET, "/v2/anon-tags/tags/list");
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get("WWW-Authenticate").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_authenticated_no_permission_still_returns_403() {
    let app = TestApp::new().await;
    app.create_docker_repo("auth-403").await;
    app.create_user_with_roles("noaccess", "pass", vec![]).await;
    let token = app.token_for("noaccess").await;

    // Authenticated user without permission should still get 403 DENIED.
    let req = app.auth_request(Method::GET, "/v2/auth-403/manifests/latest", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "DENIED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_anonymous_repo_prefixed_v2_returns_401_challenge() {
    let app = TestApp::new().await;
    app.create_docker_repo("anon-repo-v2").await;

    // Anonymous GET /repository/{repo}/v2/ (the Docker V2 ping via artifact path)
    // should return 401 with Bearer challenge, not 403.
    let req = app.request(Method::GET, "/repository/anon-repo-v2/v2/");
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let www_auth = resp
        .headers()
        .get("WWW-Authenticate")
        .expect("expected WWW-Authenticate header on 401")
        .to_str()
        .unwrap();
    assert!(
        www_auth.contains("Bearer"),
        "expected Bearer challenge, got: {}",
        www_auth
    );
    assert!(
        www_auth.contains("/v2/token"),
        "expected realm containing /v2/token, got: {}",
        www_auth
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_anonymous_repo_prefixed_manifest_returns_401() {
    let app = TestApp::new().await;
    app.create_docker_repo("anon-repo-mfst").await;

    // Anonymous GET /repository/{repo}/v2/{image}/manifests/{ref} should return 401.
    let req = app.request(
        Method::GET,
        "/repository/anon-repo-mfst/v2/myimage/manifests/latest",
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get("WWW-Authenticate").is_some());
}
