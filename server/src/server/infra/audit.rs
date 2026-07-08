// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Request event middleware.
//!
//! The per-request structured event is emitted as a tracing event with target
//! `depot.request`, which flows through the OpenTelemetry log pipeline to all
//! configured logging endpoints.

use std::net::SocketAddr;
use std::time::Instant;

use axum::{
    extract::{ConnectInfo, Request, State},
    middleware::Next,
    response::Response,
};

use crate::server::infra::log_export;
use crate::server::AppState;
use depot_core::auth::AuthenticatedUser;

/// Per-request middleware that emits a structured request event to the OTel
/// logging pipeline when `access_log` is enabled.
pub async fn request_event_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();

    let request_id = request
        .extensions()
        .get::<crate::server::infra::middleware::RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_default();

    let ip = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("unknown").trim().to_string())
        .or_else(|| {
            request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0.ip().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let bytes_recv = request
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    // Capture the identity from the request side as a fallback; this
    // middleware is layered outside `resolve_identity`, so on the normal
    // path the username arrives via the `ResolvedIdentity` response
    // extension instead (which also carries the *attempted* username on
    // auth-rejected requests).
    let request_user = request
        .extensions()
        .get::<AuthenticatedUser>()
        .map(|u| u.0.clone());

    let start = Instant::now();
    let response = next.run(request).await;
    let elapsed_ns = start.elapsed().as_nanos() as u64;

    let username = response
        .extensions()
        .get::<crate::server::auth::ResolvedIdentity>()
        .map(|r| r.0.clone())
        .or(request_user)
        .unwrap_or_else(|| "unknown".to_string());

    let status = response.status().as_u16();
    let action = log_export::classify_action(&method, &path);

    let bytes_sent = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    if state.settings.load().access_log {
        log_export::emit_request_event(
            &request_id,
            &username,
            &ip,
            &method,
            &path,
            status,
            &action,
            elapsed_ns,
            bytes_recv,
            bytes_sent,
        );
    }

    // Feed the admin Activity view regardless of the access_log setting —
    // it is a live view, not a log. The UI serving itself (marked by the
    // SPA fallback) and self-referential endpoints are excluded as noise.
    if response
        .extensions()
        .get::<crate::server::infra::request_stream::UiServed>()
        .is_some()
        || crate::server::infra::request_stream::is_feed_noise(&path)
    {
        return response;
    }
    state.bg.request_stream.publish(
        &request_id,
        &username,
        &ip,
        &method,
        &path,
        status,
        &action,
        elapsed_ns,
        bytes_recv,
        bytes_sent,
    );

    response
}

/// Emit a structured upstream request event. Call after the response headers
/// arrive so the event is recorded promptly.
#[allow(clippy::too_many_arguments)]
pub fn log_upstream_event(
    method: &str,
    url: &str,
    status: u16,
    elapsed: std::time::Duration,
    bytes_sent: u64,
    bytes_recv: u64,
    action: &str,
) {
    log_export::emit_upstream_event(method, url, status, elapsed, bytes_sent, bytes_recv, action);
}
