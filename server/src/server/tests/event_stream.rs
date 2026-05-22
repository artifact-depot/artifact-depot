// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Header-level tests for the SSE event-stream endpoint.

use axum::http::{Method, StatusCode};

use depot_test_support::*;

/// Regression: Firefox interprets `Cache-Control: no-cache` as "store but
/// revalidate", which causes a second tab opening the same SSE URL to park
/// in `AwaitingCacheCallbacks` waiting for the never-ending first stream to
/// finish writing the cache entry. The endpoint must therefore send
/// `no-store` so the response is never cached at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_stream_sets_cache_control_no_store() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/events/stream", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let cache_control = resp
        .headers()
        .get("cache-control")
        .expect("cache-control header present")
        .to_str()
        .unwrap();
    assert_eq!(
        cache_control, "no-store",
        "SSE responses must use no-store, not no-cache, to avoid Firefox \
         caching the live stream and blocking subsequent tabs"
    );
}
