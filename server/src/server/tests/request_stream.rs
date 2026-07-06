// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Tests for the admin live-request feed (`/api/v1/requests/stream`).

use axum::http::{Method, StatusCode};
use http_body_util::BodyExt;

use depot_test_support::*;

/// Read SSE frames until an event of `wanted` type arrives (or a 5s timeout).
/// Returns the event's data payload.
async fn read_sse_event(body: &mut axum::body::Body, wanted: &str) -> serde_json::Value {
    let mut buffer = String::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
            .await
            .expect("timed out waiting for SSE frame")
            .expect("stream ended before the expected event")
            .expect("stream errored");
        if let Some(data) = frame.data_ref() {
            buffer.push_str(&String::from_utf8_lossy(data));
        }
        while let Some(idx) = buffer.find("\n\n") {
            let part: String = buffer.drain(..idx + 2).collect();
            let mut event_type = "message";
            let mut data = String::new();
            for line in part.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event_type = rest.trim();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data.push_str(rest.trim());
                }
            }
            if event_type == wanted && !data.is_empty() {
                return serde_json::from_str(&data).expect("SSE data is JSON");
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_stream_requires_admin() {
    let app = TestApp::new().await;
    app.create_user_with_roles("viewer", "password", vec!["read-only"])
        .await;
    let token = app.token_for("viewer").await;
    let req = app.auth_request(Method::GET, "/api/v1/requests/stream", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_stream_sets_cache_control_no_store() {
    let app = TestApp::new().await;
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/api/v1/requests/stream", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap(),
        "no-store",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_stream_snapshot_replays_recent_requests() {
    let app = TestApp::new().await;
    let token = app.admin_token();

    // Generate a request that the middleware should record.
    let req = app.auth_request(Method::GET, "/api/v1/health", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let req = app.auth_request(Method::GET, "/api/v1/requests/stream", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();

    let snapshot = read_sse_event(&mut body, "snapshot").await;
    let events = snapshot.as_array().expect("snapshot is an array");
    let health = events
        .iter()
        .find(|e| e["path"] == "/api/v1/health")
        .expect("snapshot should contain the recorded /api/v1/health request");
    assert_eq!(health["method"], "GET");
    assert_eq!(health["status"], 200);
    assert_eq!(health["username"], "admin");
    assert!(health["seq"].is_u64());
    assert!(health["elapsed_ns"].is_u64());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_stream_delivers_live_events() {
    let app = TestApp::new().await;
    let token = app.admin_token();

    let req = app.auth_request(Method::GET, "/api/v1/requests/stream", &token);
    let resp = app.call_resp(req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    // Drain the snapshot first so the next read is a live event.
    let _ = read_sse_event(&mut body, "snapshot").await;

    // A request made while connected must arrive as a live `request` event.
    let req = app.auth_request(Method::GET, "/api/v1/repositories", &token);
    let (status, _) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);

    let event = read_sse_event(&mut body, "request").await;
    assert_eq!(event["path"], "/api/v1/repositories");
    assert_eq!(event["username"], "admin");
    assert_eq!(event["status"], 200);
}

#[test]
fn request_stream_ring_caps_and_sequences() {
    let stream = crate::server::infra::request_stream::RequestStream::new();
    for i in 0..150u16 {
        stream.publish(
            "rid",
            "u",
            "127.0.0.1",
            "GET",
            &format!("/p/{i}"),
            200,
            "read",
            1,
            0,
            0,
        );
    }
    let (snapshot, _rx) = stream.subscribe();
    assert_eq!(snapshot.len(), 100, "ring keeps the most recent 100");
    assert_eq!(snapshot.first().unwrap().path, "/p/50");
    assert_eq!(snapshot.last().unwrap().path, "/p/149");
    // Sequence numbers are strictly increasing.
    assert!(snapshot.windows(2).all(|w| w[1].seq == w[0].seq + 1));
}
