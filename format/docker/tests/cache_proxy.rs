// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::body::Body;
use axum::http::{header, Method, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;

use depot_test_support::*;

// ===========================================================================
// Cache manifest
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_get_manifest_fetches_upstream() {
    let upstream = wiremock::MockServer::start().await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "size": 0, "digest": "sha256:cfg"},
        "layers": [],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &manifest_bytes
        ))
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/v2/cache-docker/manifests/latest",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header(
                    "Content-Type",
                    "application/vnd.docker.distribution.manifest.v2+json",
                )
                .insert_header("Docker-Content-Digest", digest.as_str()),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_cache_repo("cache-docker", &upstream.uri(), 300)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/cache-docker/manifests/latest", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp
        .headers()
        .get("Docker-Content-Digest")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("sha256:"));
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), manifest_bytes.as_slice());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_get_manifest_serves_cached() {
    let upstream = wiremock::MockServer::start().await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "size": 0, "digest": "sha256:cfg2"},
        "layers": [],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &manifest_bytes
        ))
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v2/cached-mfst/manifests/v1"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header(
                    "Content-Type",
                    "application/vnd.docker.distribution.manifest.v2+json",
                )
                .insert_header("Docker-Content-Digest", digest.as_str()),
        )
        .expect(1) // Should only be called once
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_cache_repo("cached-mfst", &upstream.uri(), 3600)
        .await;

    let token = app.admin_token();

    // First request — fetches from upstream.
    let req = app.auth_request(Method::GET, "/v2/cached-mfst/manifests/v1", &token);
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);

    // Second request — should serve from cache (upstream mock only allows 1 call).
    let req = app.auth_request(Method::GET, "/v2/cached-mfst/manifests/v1", &token);
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_get_manifest_upstream_down_no_cache_errors() {
    let app = TestApp::new().await;
    // Point to a non-existent upstream.
    app.create_docker_cache_repo("dead-cache", "http://127.0.0.1:1", 0)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/dead-cache/manifests/latest", &token);
    let (status, _) = app.call(req).await;
    // Should error (no cache, no upstream).
    assert_ne!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_get_manifest_upstream_404() {
    let upstream = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v2/miss/manifests/latest"))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_cache_repo("cache-404", &upstream.uri(), 0)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/cache-404/manifests/latest", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "MANIFEST_UNKNOWN"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_head_manifest_fetches_and_caches() {
    let upstream = wiremock::MockServer::start().await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "size": 0, "digest": "sha256:cfghead"},
        "layers": [],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &manifest_bytes
        ))
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v2/head-cache/manifests/latest"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header(
                    "Content-Type",
                    "application/vnd.docker.distribution.manifest.v2+json",
                )
                .insert_header("Docker-Content-Digest", digest.as_str()),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_cache_repo("head-cache", &upstream.uri(), 3600)
        .await;

    let token = app.admin_token();
    let req = axum::http::Request::builder()
        .method(Method::HEAD)
        .uri("/v2/head-cache/manifests/latest")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("Docker-Content-Digest").is_some());
    assert!(resp.headers().get(header::CONTENT_LENGTH).is_some());
}

// ===========================================================================
// Cache blob
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_get_blob_fetches_upstream() {
    let upstream = wiremock::MockServer::start().await;
    let blob_data = b"cached blob data";
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            blob_data
        ))
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!(
            "/v2/cache-blob/blobs/{}",
            digest
        )))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(blob_data.to_vec()))
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_cache_repo("cache-blob", &upstream.uri(), 300)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/cache-blob/blobs/{}", digest),
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, blob_data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_get_blob_serves_cached() {
    let upstream = wiremock::MockServer::start().await;
    let blob_data = b"blob to cache";
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            blob_data
        ))
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!(
            "/v2/cache-blob2/blobs/{}",
            digest
        )))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(blob_data.to_vec()))
        .expect(1) // only fetch once
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_cache_repo("cache-blob2", &upstream.uri(), 3600)
        .await;

    let token = app.admin_token();

    // First fetch.
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/cache-blob2/blobs/{}", digest),
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);

    // Second fetch — from cache.
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/cache-blob2/blobs/{}", digest),
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, blob_data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_get_blob_upstream_404() {
    let upstream = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/v2/cache-blob404/blobs/sha256:missing",
        ))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_cache_repo("cache-blob404", &upstream.uri(), 0)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        "/v2/cache-blob404/blobs/sha256:missing",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "BLOB_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_head_blob_local_hit() {
    let app = TestApp::new().await;
    // Create a cache repo but push directly to it via internal store (simulate cached blob).
    app.create_docker_cache_repo("head-local", "http://127.0.0.1:1", 3600)
        .await;

    // Manually push a blob via the hosted store path since cache repos share the same KV/blob.
    let data = b"head-local-blob";
    let digest = depot_format_docker::store::sha256_digest(data);
    let noop_atime = depot_core::update::UpdateSender::noop();
    let blobs = app.state.repo.blob_store("default").await.unwrap();
    let docker_store = depot_format_docker::store::DockerStore {
        repo: "head-local",
        image: None,
        kv: app.state.repo.kv.as_ref(),
        blobs: blobs.as_ref(),
        updater: &noop_atime,
        store: "default",
        track_access: true,
    };
    docker_store.put_blob(data).await.unwrap();

    let token = app.admin_token();
    let req = axum::http::Request::builder()
        .method(Method::HEAD)
        .uri(format!("/v2/head-local/blobs/{}", digest))
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
// Proxy manifest
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_get_manifest_from_hosted() {
    let app = TestApp::new().await;
    app.create_docker_repo("proxy-h-member").await;

    let config_digest = app.push_docker_blob("proxy-h-member", b"{}").await;
    let layer_digest = app.push_docker_blob("proxy-h-member", b"layer").await;
    let manifest = TestApp::make_manifest(&config_digest, &[&layer_digest]);
    app.push_docker_manifest("proxy-h-member", "latest", &manifest)
        .await;

    app.create_docker_proxy_repo("proxy-get-mfst", vec!["proxy-h-member"])
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/proxy-get-mfst/manifests/latest", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_get_manifest_not_found_all_members() {
    let app = TestApp::new().await;
    app.create_docker_repo("empty-member").await;
    app.create_docker_proxy_repo("proxy-miss", vec!["empty-member"])
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/proxy-miss/manifests/latest", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body["errors"][0]["code"].as_str().unwrap(),
        "MANIFEST_UNKNOWN"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_head_manifest() {
    let app = TestApp::new().await;
    app.create_docker_repo("proxy-head-m").await;

    let config_digest = app.push_docker_blob("proxy-head-m", b"{}").await;
    let manifest = TestApp::make_manifest(&config_digest, &[]);
    app.push_docker_manifest("proxy-head-m", "v1", &manifest)
        .await;

    app.create_docker_proxy_repo("proxy-head", vec!["proxy-head-m"])
        .await;

    let token = app.admin_token();
    let req = axum::http::Request::builder()
        .method(Method::HEAD)
        .uri("/v2/proxy-head/manifests/v1")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("Docker-Content-Digest").is_some());
}

// ===========================================================================
// Proxy blob
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_get_blob_from_hosted() {
    let app = TestApp::new().await;
    app.create_docker_repo("proxy-blob-h").await;

    let data = b"proxy blob data";
    let digest = app.push_docker_blob("proxy-blob-h", data).await;

    app.create_docker_proxy_repo("proxy-blob", vec!["proxy-blob-h"])
        .await;

    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/proxy-blob/blobs/{}", digest),
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_get_blob_not_found() {
    let app = TestApp::new().await;
    app.create_docker_repo("proxy-blob-empty").await;
    app.create_docker_proxy_repo("proxy-blob-miss", vec!["proxy-blob-empty"])
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/proxy-blob-miss/blobs/sha256:dead", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["errors"][0]["code"].as_str().unwrap(), "BLOB_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_head_blob() {
    let app = TestApp::new().await;
    app.create_docker_repo("proxy-hblob-m").await;
    let data = b"proxy head blob";
    let digest = app.push_docker_blob("proxy-hblob-m", data).await;
    app.create_docker_proxy_repo("proxy-hblob", vec!["proxy-hblob-m"])
        .await;

    let token = app.admin_token();
    let req = axum::http::Request::builder()
        .method(Method::HEAD)
        .uri(format!("/v2/proxy-hblob/blobs/{}", digest))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// Edge cases
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_skips_non_docker_members() {
    let app = TestApp::new().await;
    app.create_hosted_repo("raw-member").await; // not Docker
    app.create_docker_repo("docker-member").await;

    let config_digest = app.push_docker_blob("docker-member", b"{}").await;
    let manifest = TestApp::make_manifest(&config_digest, &[]);
    app.push_docker_manifest("docker-member", "latest", &manifest)
        .await;

    app.create_docker_proxy_repo("proxy-mixed", vec!["raw-member", "docker-member"])
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/proxy-mixed/manifests/latest", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_list_tags() {
    let upstream = wiremock::MockServer::start().await;

    // Upstream won't be called for tags list — cache tags are local only.
    let app = TestApp::new().await;
    app.create_docker_cache_repo("cache-tags", &upstream.uri(), 3600)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/cache-tags/tags/list", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tags"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_list_tags_aggregates() {
    let app = TestApp::new().await;
    app.create_docker_repo("tags-m1").await;
    app.create_docker_repo("tags-m2").await;

    // Push different tags to different members.
    let config_digest1 = app.push_docker_blob("tags-m1", b"c1").await;
    let manifest1 = TestApp::make_manifest(&config_digest1, &[]);
    app.push_docker_manifest("tags-m1", "alpha", &manifest1)
        .await;

    let config_digest2 = app.push_docker_blob("tags-m2", b"c2").await;
    let manifest2 = TestApp::make_manifest(&config_digest2, &[]);
    app.push_docker_manifest("tags-m2", "beta", &manifest2)
        .await;

    app.create_docker_proxy_repo("proxy-tags", vec!["tags-m1", "tags-m2"])
        .await;

    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/v2/proxy-tags/tags/list", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let tags: Vec<&str> = body["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(tags.contains(&"alpha"));
    assert!(tags.contains(&"beta"));
}

// ===========================================================================
// Proxy tag pagination
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_list_tags_pagination() {
    let app = TestApp::new().await;
    app.create_docker_repo("ptags-m").await;

    // Push three tags.
    for tag in &["aaa", "bbb", "ccc"] {
        let cfg = app.push_docker_blob("ptags-m", b"{}").await;
        let manifest = TestApp::make_manifest(&cfg, &[]);
        app.push_docker_manifest("ptags-m", tag, &manifest).await;
    }

    app.create_docker_proxy_repo("ptags-proxy", vec!["ptags-m"])
        .await;

    let token = app.admin_token();

    // Page 1.
    let req = app.auth_request(Method::GET, "/v2/ptags-proxy/tags/list?n=2", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("Link").is_some());
    let body: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let tags = body["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2);

    // Page 2.
    let last = tags.last().unwrap().as_str().unwrap();
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/ptags-proxy/tags/list?n=2&last={}", last),
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tags"].as_array().unwrap().len(), 1);
}

// ===========================================================================
// Cache upstream with two-segment (namespaced) routes
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_ns_manifest_from_upstream() {
    let upstream = wiremock::MockServer::start().await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "size": 0, "digest": "sha256:nscfg"},
        "layers": [],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &manifest_bytes
        ))
    );

    // Upstream serves at /v2/{image}/manifests/{ref} — image is "library/nginx".
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/v2/library/nginx/manifests/latest",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header(
                    "Content-Type",
                    "application/vnd.docker.distribution.manifest.v2+json",
                )
                .insert_header("Docker-Content-Digest", digest.as_str()),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_cache_repo("cache-ns", &upstream.uri(), 300)
        .await;

    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        "/v2/cache-ns/library/nginx/manifests/latest",
        &token,
    );
    let resp = app.call_resp(req).await;
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "cache ns manifest: {}",
        String::from_utf8_lossy(&body)
    );
}

// ===========================================================================
// Cache blob HEAD fetches from upstream
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_head_blob_upstream() {
    let upstream = wiremock::MockServer::start().await;
    let blob_data = b"head-blob-upstream";
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            blob_data
        ))
    );

    // For HEAD, the cache needs to GET the blob first (to cache it), then serve HEAD.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!(
            "/v2/cache-hblob/blobs/{}",
            digest
        )))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(blob_data.to_vec()))
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_cache_repo("cache-hblob", &upstream.uri(), 300)
        .await;

    let token = app.admin_token();

    // GET to cache the blob first.
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/cache-hblob/blobs/{}", digest),
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);

    // HEAD on cached blob.
    let req = axum::http::Request::builder()
        .method(Method::HEAD)
        .uri(format!("/v2/cache-hblob/blobs/{}", digest))
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
        blob_data.len().to_string()
    );
}

// ===========================================================================
// Proxy with cache member — manifest
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_get_manifest_from_cache_member() {
    let upstream = wiremock::MockServer::start().await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "size": 0, "digest": "sha256:proxycachecfg"},
        "layers": [],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &manifest_bytes
        ))
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/v2/prx-cache-img/manifests/latest",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header(
                    "Content-Type",
                    "application/vnd.docker.distribution.manifest.v2+json",
                )
                .insert_header("Docker-Content-Digest", digest.as_str()),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_repo("dkr-prx-hosted").await;
    app.create_docker_cache_repo("dkr-prx-cache", &upstream.uri(), 300)
        .await;
    app.create_docker_proxy_repo("dkr-prx-combo", vec!["dkr-prx-hosted", "dkr-prx-cache"])
        .await;

    let token = app.admin_token();

    // Manifest only exists in cache member's upstream — proxy should find it.
    let req = app.auth_request(
        Method::GET,
        "/v2/dkr-prx-combo/prx-cache-img/manifests/latest",
        &token,
    );
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp
        .headers()
        .get("Docker-Content-Digest")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("sha256:"));
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), manifest_bytes.as_slice());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_get_manifest_hosted_before_cache() {
    let upstream = wiremock::MockServer::start().await;

    // Upstream serves a different manifest for the same tag.
    let upstream_manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "size": 0, "digest": "sha256:upstreamcfg"},
        "layers": [],
    });
    let upstream_bytes = serde_json::to_vec(&upstream_manifest).unwrap();
    let upstream_digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &upstream_bytes
        ))
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v2/dkr-prio/manifests/v1"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(upstream_bytes.clone())
                .insert_header(
                    "Content-Type",
                    "application/vnd.docker.distribution.manifest.v2+json",
                )
                .insert_header("Docker-Content-Digest", upstream_digest.as_str()),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_repo("dkr-prio-hosted").await;
    app.create_docker_cache_repo("dkr-prio-cache", &upstream.uri(), 300)
        .await;
    app.create_docker_proxy_repo("dkr-prio", vec!["dkr-prio-hosted", "dkr-prio-cache"])
        .await;

    // Push a manifest to the hosted member.
    let config_digest = app.push_docker_blob("dkr-prio-hosted", b"{}").await;
    let hosted_manifest = TestApp::make_manifest(&config_digest, &[]);
    app.push_docker_manifest("dkr-prio-hosted", "v1", &hosted_manifest)
        .await;

    let token = app.admin_token();

    // Proxy should return the hosted member's manifest (first match wins).
    let req = app.auth_request(Method::GET, "/v2/dkr-prio/manifests/v1", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let hosted_bytes = serde_json::to_vec(&hosted_manifest).unwrap();
    assert_eq!(body.as_ref(), hosted_bytes.as_slice());
}

// ===========================================================================
// Proxy with cache member — blob
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_get_blob_from_cache_member() {
    let upstream = wiremock::MockServer::start().await;
    let blob_data = b"proxy-cache-blob-data";
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            blob_data
        ))
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!(
            "/v2/prx-cache-blobimg/blobs/{}",
            digest
        )))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(blob_data.to_vec()))
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_docker_repo("dkr-bprx-hosted").await;
    app.create_docker_cache_repo("dkr-bprx-cache", &upstream.uri(), 300)
        .await;
    app.create_docker_proxy_repo("dkr-bprx", vec!["dkr-bprx-hosted", "dkr-bprx-cache"])
        .await;

    let token = app.admin_token();

    // Blob only exists in cache member's upstream.
    let req = app.auth_request(
        Method::GET,
        &format!("/v2/dkr-bprx/prx-cache-blobimg/blobs/{}", digest),
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, blob_data);
}

// ===========================================================================
// Proxy with cache member — tag aggregation
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_list_tags_includes_cache_member() {
    let upstream = wiremock::MockServer::start().await;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "size": 0, "digest": "sha256:tagcfg"},
        "layers": [],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let digest = format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(
            sha2::Sha256::default(),
            &manifest_bytes
        ))
    );

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/v2/dkr-tagagg-cache/manifests/cache-tag",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header(
                    "Content-Type",
                    "application/vnd.docker.distribution.manifest.v2+json",
                )
                .insert_header("Docker-Content-Digest", digest.as_str()),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;

    // Hosted member with a tag.
    app.create_docker_repo("dkr-tagagg-h").await;
    let config_digest = app.push_docker_blob("dkr-tagagg-h", b"{}").await;
    let hosted_manifest = TestApp::make_manifest(&config_digest, &[]);
    app.push_docker_manifest("dkr-tagagg-h", "hosted-tag", &hosted_manifest)
        .await;

    // Cache member — prime it so the tag is locally visible.
    app.create_docker_cache_repo("dkr-tagagg-cache", &upstream.uri(), 300)
        .await;
    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        "/v2/dkr-tagagg-cache/manifests/cache-tag",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);

    // Proxy over both.
    app.create_docker_proxy_repo("dkr-tagagg", vec!["dkr-tagagg-h", "dkr-tagagg-cache"])
        .await;

    let req = app.auth_request(Method::GET, "/v2/dkr-tagagg/tags/list", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    let tags: Vec<&str> = body["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(tags.contains(&"hosted-tag"), "should include hosted tag");
    assert!(tags.contains(&"cache-tag"), "should include cache tag");
}
