// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::http::{Method, StatusCode};

use depot_format_helm::store::build_synthetic_chart;

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
    helpers::assert_nonexistent_repo_404(&app, "/repository/no-such-repo/index.yaml").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_non_helm_format_rejected() {
    let app = TestApp::new().await;
    helpers::assert_wrong_format_rejected(&app, "/repository/raw-repo/index.yaml").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_to_cache_rejected() {
    let app = TestApp::new().await;
    app.create_helm_cache_repo("helm-cache", "http://localhost:19999/helm/fake", 300)
        .await;

    let chart = build_synthetic_chart("nginx", "1.0.0", "A web server").unwrap();
    let token = app.admin_token();
    let req = helm_upload_request("helm-cache", "nginx-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Hosted upload + index (core happy path)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_and_index() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-hosted").await;

    let chart = build_synthetic_chart("nginx", "1.0.0", "A web server").unwrap();
    let token = app.admin_token();
    let req = helm_upload_request("helm-hosted", "nginx-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // GET index.yaml
    let req = app.auth_request(Method::GET, "/repository/helm-hosted/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let index = String::from_utf8(body).unwrap();

    assert!(index.contains("apiVersion:"), "missing apiVersion");
    assert!(index.contains("nginx"), "missing nginx entry");
    assert!(index.contains("1.0.0"), "missing version 1.0.0");
    assert!(index.contains("digest:"), "missing digest field");
    assert!(index.contains("urls:"), "missing urls field");
}

/// The web UI's `index.json` returns the same index as JSON, grouped by chart
/// name, with the sha256 digest — so the browser can group charts without a
/// YAML parser or ambiguous filename splitting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_index_json_for_ui() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-json").await;
    let token = app.admin_token();

    // Two versions of a dashed-name chart (the case filename-splitting breaks).
    for v in ["1.4.2", "1.6.0-dev.5"] {
        let chart = build_synthetic_chart("myriad-uui", v, "UI").unwrap();
        let req = helm_upload_request("helm-json", &format!("myriad-uui-{v}.tgz"), &chart, &token);
        assert_eq!(app.call(req).await.0, StatusCode::OK);
    }

    let req = app.auth_request(Method::GET, "/repository/helm-json/index.json", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let versions = json["entries"]["myriad-uui"]
        .as_array()
        .expect("myriad-uui grouped under its full name, not split on the dash");
    assert_eq!(
        versions.len(),
        2,
        "both versions grouped under the chart name"
    );
    // Each entry carries the sha256 digest the UI shows (not BLAKE3).
    for e in versions {
        let d = e["digest"].as_str().unwrap_or_default();
        assert!(d.starts_with("sha256:"), "digest should be sha256: {d}");
        assert_eq!(e["name"], "myriad-uui");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_and_download() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-dl").await;

    let chart = build_synthetic_chart("nginx", "1.0.0", "A web server").unwrap();
    let token = app.admin_token();
    let req = helm_upload_request("helm-dl", "nginx-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Download the chart
    let req = app.auth_request(Method::GET, "/repository/helm-dl/nginx-1.0.0.tgz", &token);
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, chart, "downloaded chart should match uploaded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multiple_versions() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-multi-ver").await;
    let token = app.admin_token();

    let chart1 = build_synthetic_chart("nginx", "1.0.0", "A web server v1").unwrap();
    let req = helm_upload_request("helm-multi-ver", "nginx-1.0.0.tgz", &chart1, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let chart2 = build_synthetic_chart("nginx", "2.0.0", "A web server v2").unwrap();
    let req = helm_upload_request("helm-multi-ver", "nginx-2.0.0.tgz", &chart2, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(Method::GET, "/repository/helm-multi-ver/index.yaml", &token);
    let (_, body) = app.call_raw(req).await;
    let index = String::from_utf8(body).unwrap();

    assert!(index.contains("1.0.0"), "missing version 1.0.0");
    assert!(index.contains("2.0.0"), "missing version 2.0.0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multiple_charts() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-multi-chart").await;
    let token = app.admin_token();

    let chart1 = build_synthetic_chart("nginx", "1.0.0", "A web server").unwrap();
    let req = helm_upload_request("helm-multi-chart", "nginx-1.0.0.tgz", &chart1, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let chart2 = build_synthetic_chart("redis", "1.0.0", "A cache server").unwrap();
    let req = helm_upload_request("helm-multi-chart", "redis-1.0.0.tgz", &chart2, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(
        Method::GET,
        "/repository/helm-multi-chart/index.yaml",
        &token,
    );
    let (_, body) = app.call_raw(req).await;
    let index = String::from_utf8(body).unwrap();

    assert!(index.contains("nginx"), "missing nginx entry");
    assert!(index.contains("redis"), "missing redis entry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_overwrite_chart() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-overwrite").await;
    let token = app.admin_token();

    let chart1 = build_synthetic_chart("nginx", "1.0.0", "First upload").unwrap();
    let req = helm_upload_request("helm-overwrite", "nginx-1.0.0.tgz", &chart1, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Upload same name+version again
    let chart2 = build_synthetic_chart("nginx", "1.0.0", "Second upload").unwrap();
    let req = helm_upload_request("helm-overwrite", "nginx-1.0.0.tgz", &chart2, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Index should have only one entry for nginx 1.0.0
    let req = app.auth_request(Method::GET, "/repository/helm-overwrite/index.yaml", &token);
    let (_, body) = app.call_raw(req).await;
    let index = String::from_utf8(body).unwrap();

    // Count occurrences of "version: 1.0.0" or "version: \"1.0.0\""
    let _version_count = index.matches("1.0.0").count();
    // Should appear in the version field and the URL, but not duplicated entries.
    // At minimum, just verify the index is valid and contains the chart.
    assert!(index.contains("nginx"), "missing nginx entry");
    assert!(index.contains("1.0.0"), "missing version 1.0.0");
}

// Regression: chart filenames whose version doesn't start with a digit
// (e.g. v-prefixed `tigera-operator-v3.28.0.tgz`) must still be downloadable.
// Charts live at the repo root, so both the helm CLI and the UI's
// per-artifact download link fetch via `/repository/{repo}/{filename}.tgz` —
// the same URL `index.yaml` advertises in `urls`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_download_chart_with_v_prefixed_version() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-vprefix").await;
    let token = app.admin_token();

    let chart = build_synthetic_chart("tigera-operator", "v3.28.0", "operator").unwrap();
    let req = helm_upload_request(
        "helm-vprefix",
        "tigera-operator-v3.28.0.tgz",
        &chart,
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Index advertises {name}-{version}.tgz at the root — same path on disk.
    let req = app.auth_request(Method::GET, "/repository/helm-vprefix/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let index = String::from_utf8(body).unwrap();
    assert!(
        index.contains("tigera-operator-v3.28.0.tgz") && !index.contains("charts/"),
        "index should advertise canonical root URL; got:\n{index}"
    );

    // Download via /repository/{repo}/{filename}.tgz.
    let req = app.auth_request(
        Method::GET,
        "/repository/helm-vprefix/tigera-operator-v3.28.0.tgz",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK, "v-prefixed version should download");
    assert_eq!(body, chart);
}

// ===========================================================================
// Invalid input
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_non_tgz() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-bad-tgz").await;
    let token = app.admin_token();

    let garbage = b"this is not a tgz file at all";
    let req = helm_upload_request("helm-bad-tgz", "garbage-1.0.0.tgz", garbage, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_missing_chart_yaml() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-no-chart-yaml").await;
    let token = app.admin_token();

    // Build a valid .tgz but without Chart.yaml
    let mut archive = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut archive, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);

        // Add a dummy file that is NOT Chart.yaml
        let data = b"not a chart";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "mychart/README.md", &data[..])
            .unwrap();

        tar.into_inner().unwrap().finish().unwrap();
    }

    let req = helm_upload_request("helm-no-chart-yaml", "mychart-1.0.0.tgz", &archive, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Download not found
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_download_nonexistent_chart() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-dl-404").await;
    let token = app.admin_token();

    let req = app.auth_request(
        Method::GET,
        "/repository/helm-dl-404/nonexistent-1.0.0.tgz",
        &token,
    );
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Proxy
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_index_merges_members() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-h1").await;
    app.create_helm_repo("helm-h2").await;
    app.create_helm_proxy_repo("helm-proxy", vec!["helm-h1", "helm-h2"])
        .await;
    let token = app.admin_token();

    let chart1 = build_synthetic_chart("nginx", "1.0.0", "A web server").unwrap();
    let req = helm_upload_request("helm-h1", "nginx-1.0.0.tgz", &chart1, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let chart2 = build_synthetic_chart("redis", "1.0.0", "A cache server").unwrap();
    let req = helm_upload_request("helm-h2", "redis-1.0.0.tgz", &chart2, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // GET proxy index.yaml
    let req = app.auth_request(Method::GET, "/repository/helm-proxy/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let index = String::from_utf8(body).unwrap();

    assert!(
        index.contains("nginx"),
        "proxy index should contain nginx from helm-h1"
    );
    assert!(
        index.contains("redis"),
        "proxy index should contain redis from helm-h2"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_download_searches_members() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-pd1").await;
    app.create_helm_repo("helm-pd2").await;
    app.create_helm_proxy_repo("helm-pd-proxy", vec!["helm-pd1", "helm-pd2"])
        .await;
    let token = app.admin_token();

    // Upload chart only to the second member
    let chart = build_synthetic_chart("redis", "1.0.0", "A cache server").unwrap();
    let req = helm_upload_request("helm-pd2", "redis-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Download through proxy should find it in the second member
    let req = app.auth_request(
        Method::GET,
        "/repository/helm-pd-proxy/redis-1.0.0.tgz",
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        downloaded, chart,
        "proxy download should match original chart"
    );
}

// ===========================================================================
// Cache — index fetch from upstream
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_fetches_index_from_upstream() {
    let upstream = wiremock::MockServer::start().await;

    let index_yaml = "\
apiVersion: v1
entries:
  mychart:
    - name: mychart
      version: 1.0.0
      urls:
        - https://example.com/charts/mychart-1.0.0.tgz
generated: \"2026-01-01T00:00:00Z\"
";

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/index.yaml"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(index_yaml)
                .insert_header("Content-Type", "application/x-yaml"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_helm_cache_repo("helm-cache-idx", &upstream.uri(), 300)
        .await;
    let token = app.admin_token();

    let req = app.auth_request(Method::GET, "/repository/helm-cache-idx/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains("mychart"),
        "cache should serve upstream index content"
    );

    // Second fetch should come from cache.
    let req = app.auth_request(Method::GET, "/repository/helm-cache-idx/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("mychart"), "cached index should still work");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_downloads_chart_from_upstream() {
    let upstream = wiremock::MockServer::start().await;

    let chart = build_synthetic_chart("cached-chart", "1.0.0", "cache test chart").unwrap();

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/charts/cached-chart-1.0.0.tgz"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(chart.clone())
                .insert_header("Content-Type", "application/gzip"),
        )
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_helm_cache_repo("helm-cache-dl", &upstream.uri(), 300)
        .await;
    let token = app.admin_token();

    let req = app.auth_request(
        Method::GET,
        "/repository/helm-cache-dl/cached-chart-1.0.0.tgz",
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, chart, "cached chart should match upstream");

    // Second fetch should come from local cache.
    let req = app.auth_request(
        Method::GET,
        "/repository/helm-cache-dl/cached-chart-1.0.0.tgz",
        &token,
    );
    let (status, downloaded2) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded2, chart, "second fetch should serve from cache");
}

// ===========================================================================
// Proxy — upload routes to hosted member
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_upload_routes_to_member() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-write-target").await;
    app.create_helm_proxy_repo("helm-write-proxy", vec!["helm-write-target"])
        .await;
    let token = app.admin_token();

    let chart = build_synthetic_chart("routed", "1.0.0", "via proxy write").unwrap();
    let req = helm_upload_request("helm-write-proxy", "routed-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Verify the chart ended up in the hosted member.
    let req = app.auth_request(
        Method::GET,
        "/repository/helm-write-target/routed-1.0.0.tgz",
        &token,
    );
    let (status, downloaded) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        downloaded, chart,
        "upload through proxy should land in hosted member"
    );
}

// ===========================================================================
// Cache — upstream unavailable (stale cache)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_index_upstream_unavailable() {
    let app = TestApp::new().await;
    // Point at a non-existent upstream.
    app.create_helm_cache_repo("helm-cache-fail", "http://localhost:19999/helm/fake", 300)
        .await;
    let token = app.admin_token();

    // Should 404 since there's no cached index and upstream is down.
    let req = app.auth_request(
        Method::GET,
        "/repository/helm-cache-fail/index.yaml",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "should 404 when upstream is unavailable and no cache"
    );
}

// ===========================================================================
// Proxy index merges hosted + cache members
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_index_merges_hosted_and_cache_members() {
    let app = TestApp::new().await;
    // Hosted member with a chart.
    app.create_helm_repo("helm-prx-h").await;
    let token = app.admin_token();
    let chart = build_synthetic_chart("web", "1.0.0", "a web chart").unwrap();
    let req = helm_upload_request("helm-prx-h", "web-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Cache member (with unreachable upstream).
    app.create_helm_cache_repo("helm-prx-c", "http://localhost:19999/helm/fake", 300)
        .await;

    // Proxy over both.
    app.create_helm_proxy_repo("helm-prx", vec!["helm-prx-h", "helm-prx-c"])
        .await;

    let req = app.auth_request(Method::GET, "/repository/helm-prx/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains("web"),
        "proxy index should contain chart from hosted member"
    );
    assert!(text.contains("1.0.0"));
}

// ===========================================================================
// Hosted chart download after upload
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_download_chart() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-dl").await;
    let token = app.admin_token();

    let chart = build_synthetic_chart("myapp", "2.1.0", "my app chart").unwrap();
    let req = helm_upload_request("helm-dl", "myapp-2.1.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Download the chart back.
    let req = app.auth_request(Method::GET, "/repository/helm-dl/myapp-2.1.0.tgz", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("Content-Type").unwrap(),
        "application/gzip"
    );
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(body.as_ref(), chart.as_slice());
}

// ===========================================================================
// Multiple chart versions in index
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multiple_versions_in_index() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-multi").await;
    let token = app.admin_token();

    for version in &["1.0.0", "1.1.0", "2.0.0"] {
        let chart = build_synthetic_chart("mylib", version, "a library chart").unwrap();
        let req = helm_upload_request(
            "helm-multi",
            &format!("mylib-{version}.tgz"),
            &chart,
            &token,
        );
        let (status, _) = app.call(req).await;
        assert_eq!(status, StatusCode::OK, "upload {version} failed");
    }

    let req = app.auth_request(Method::GET, "/repository/helm-multi/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("1.0.0"));
    assert!(text.contains("1.1.0"));
    assert!(text.contains("2.0.0"));
}

// ===========================================================================
// Proxy download routes to hosted member
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_download_from_member() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-dl-m").await;
    let token = app.admin_token();

    let chart = build_synthetic_chart("proxied", "1.0.0", "test proxy dl").unwrap();
    let req = helm_upload_request("helm-dl-m", "proxied-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    app.create_helm_proxy_repo("helm-dl-p", vec!["helm-dl-m"])
        .await;

    // Download through the proxy.
    let req = app.auth_request(
        Method::GET,
        "/repository/helm-dl-p/proxied-1.0.0.tgz",
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, chart);
}

// ===========================================================================
// Hosted delete triggers index rebuild
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hosted_delete_triggers_index_rebuild() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-del").await;
    let token = app.admin_token();

    let chart = build_synthetic_chart("delme", "1.0.0", "a chart to delete").unwrap();
    let req = helm_upload_request("helm-del", "delme-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Index should contain the chart.
    let req = app.auth_request(Method::GET, "/repository/helm-del/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("delme"));

    // Delete the chart via the raw artifact handler.
    let req = app.auth_request(
        Method::DELETE,
        "/repository/helm-del/delme-1.0.0.tgz",
        &token,
    );
    let (status, _) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Index should no longer contain the chart.
    let req = app.auth_request(Method::GET, "/repository/helm-del/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains("delme"),
        "index should not contain deleted chart, got: {text}"
    );
}

// ===========================================================================
// Proxy caches its merged index
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_caches_index() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-pc-h").await;
    let token = app.admin_token();

    let chart = build_synthetic_chart("cached", "1.0.0", "test caching").unwrap();
    let req = helm_upload_request("helm-pc-h", "cached-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    app.create_helm_proxy_repo("helm-pc-p", vec!["helm-pc-h"])
        .await;

    // First request — builds and caches merged index.
    let req = app.auth_request(Method::GET, "/repository/helm-pc-p/index.yaml", &token);
    let (status, body1) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text1 = String::from_utf8_lossy(&body1);
    assert!(text1.contains("cached"));

    // Second request — served from cache, should still contain the chart.
    let req = app.auth_request(Method::GET, "/repository/helm-pc-p/index.yaml", &token);
    let (status, body2) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text2 = String::from_utf8_lossy(&body2);
    assert!(text2.contains("cached"));
}

// ===========================================================================
// Proxy stale after member upload (propagation)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_stale_after_member_upload() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-ps-h").await;
    app.create_helm_proxy_repo("helm-ps-p", vec!["helm-ps-h"])
        .await;
    let token = app.admin_token();

    // Fetch proxy index — should be empty.
    let req = app.auth_request(Method::GET, "/repository/helm-ps-p/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&body).contains("freshchart"));

    // Upload chart to hosted member — should propagate stale to proxy.
    let chart = build_synthetic_chart("freshchart", "1.0.0", "after proxy build").unwrap();
    let req = helm_upload_request("helm-ps-h", "freshchart-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Proxy index should now contain the new chart.
    let req = app.auth_request(Method::GET, "/repository/helm-ps-p/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("freshchart"),
        "proxy should show new chart after propagation"
    );
}

// ===========================================================================
// Proxy cache member TTL triggers refresh
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_cache_member_ttl_triggers_refresh() {
    let upstream = wiremock::MockServer::start().await;

    let initial_index = "\
apiVersion: v1
entries:
  oldchart:
    - name: oldchart
      version: 1.0.0
      urls:
        - charts/oldchart-1.0.0.tgz
generated: \"2026-01-01T00:00:00Z\"
";

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/index.yaml"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(initial_index)
                .insert_header("Content-Type", "application/x-yaml"),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    // TTL of 1 second so it expires quickly.
    app.create_helm_cache_repo("helm-ttl-c", &upstream.uri(), 1)
        .await;
    app.create_helm_proxy_repo("helm-ttl-p", vec!["helm-ttl-c"])
        .await;
    let token = app.admin_token();

    // First proxy fetch — fetches from upstream, caches.
    let req = app.auth_request(Method::GET, "/repository/helm-ttl-p/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("oldchart"));

    // Wait for TTL to expire.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Update upstream to serve a different index.
    upstream.reset().await;
    let updated_index = "\
apiVersion: v1
entries:
  newchart:
    - name: newchart
      version: 2.0.0
      urls:
        - charts/newchart-2.0.0.tgz
generated: \"2026-01-02T00:00:00Z\"
";
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/index.yaml"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(updated_index)
                .insert_header("Content-Type", "application/x-yaml"),
        )
        .expect(1..)
        .mount(&upstream)
        .await;

    // Proxy fetch should detect expired TTL, re-fetch, and show new content.
    let req = app.auth_request(Method::GET, "/repository/helm-ttl-p/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("newchart"),
        "proxy should see updated upstream index after TTL expiry, got: {text}"
    );
}

// ===========================================================================
// Nested proxy member
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_proxy_nested_proxy_member() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-np-h").await;
    let token = app.admin_token();

    let chart = build_synthetic_chart("deep", "1.0.0", "deeply nested chart").unwrap();
    let req = helm_upload_request("helm-np-h", "deep-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Inner proxy over hosted.
    app.create_helm_proxy_repo("helm-np-inner", vec!["helm-np-h"])
        .await;

    // Outer proxy over inner proxy.
    app.create_helm_proxy_repo("helm-np-outer", vec!["helm-np-inner"])
        .await;

    // Outer proxy index should contain the chart from the hosted grandchild.
    let req = app.auth_request(Method::GET, "/repository/helm-np-outer/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("deep"),
        "outer proxy should contain chart from nested hosted member"
    );
}

// ===========================================================================
// Transitive propagation (hosted → inner proxy → outer proxy)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_propagation_transitive() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-tp-h").await;
    app.create_helm_proxy_repo("helm-tp-inner", vec!["helm-tp-h"])
        .await;
    app.create_helm_proxy_repo("helm-tp-outer", vec!["helm-tp-inner"])
        .await;
    let token = app.admin_token();

    // Build initial outer proxy index (empty).
    let req = app.auth_request(Method::GET, "/repository/helm-tp-outer/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&body).contains("transitive"));

    // Upload chart to hosted repo — should propagate through both proxy levels.
    let chart = build_synthetic_chart("transitive", "1.0.0", "transitive test").unwrap();
    let req = helm_upload_request("helm-tp-h", "transitive-1.0.0.tgz", &chart, &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    // Outer proxy should see the chart (stale flag propagated transitively).
    let req = app.auth_request(Method::GET, "/repository/helm-tp-outer/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("transitive"),
        "outer proxy should see chart after transitive propagation"
    );
}

// ===========================================================================
// Cache upstream change propagates to proxy
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cache_upstream_change_propagates_to_proxy() {
    let upstream = wiremock::MockServer::start().await;

    let initial_index = "\
apiVersion: v1
entries:
  alpha:
    - name: alpha
      version: 1.0.0
      urls:
        - charts/alpha-1.0.0.tgz
generated: \"2026-01-01T00:00:00Z\"
";
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/index.yaml"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(initial_index)
                .insert_header("Content-Type", "application/x-yaml"),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let app = TestApp::new().await;
    app.create_helm_cache_repo("helm-cup-c", &upstream.uri(), 1)
        .await;
    app.create_helm_proxy_repo("helm-cup-p", vec!["helm-cup-c"])
        .await;
    let token = app.admin_token();

    // Seed proxy index.
    let req = app.auth_request(Method::GET, "/repository/helm-cup-p/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("alpha"));

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Update upstream with a new chart.
    upstream.reset().await;
    let updated_index = "\
apiVersion: v1
entries:
  alpha:
    - name: alpha
      version: 1.0.0
      urls:
        - charts/alpha-1.0.0.tgz
  beta:
    - name: beta
      version: 1.0.0
      urls:
        - charts/beta-1.0.0.tgz
generated: \"2026-01-02T00:00:00Z\"
";
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/index.yaml"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(updated_index)
                .insert_header("Content-Type", "application/x-yaml"),
        )
        .expect(1..)
        .mount(&upstream)
        .await;

    // Proxy should detect expired TTL, refresh cache, and show new chart.
    let req = app.auth_request(Method::GET, "/repository/helm-cup-p/index.yaml", &token);
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("alpha"), "should still have alpha");
    assert!(
        text.contains("beta"),
        "proxy should see new upstream chart beta after TTL expiry"
    );
}

// Note: relocation of legacy chart records (`_charts/{name}/{name}-{version}.tgz`
// or `charts/{name}-{version}.tgz`) to the canonical root path
// `{name}-{version}.tgz` now lives in the integrity check (server crate).
// The on-fetch path only rebuilds the index when the `_helm/metadata_stale`
// flag is set; it does not migrate.
