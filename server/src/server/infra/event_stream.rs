// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! SSE endpoint for the real-time data model.
//!
//! On connect the client receives a pre-materialized `ModelSnapshot` (O(1),
//! no KV queries) followed by a stream of `Envelope` domain events. If the
//! client falls behind the broadcast ring buffer, a `Resync` event is sent
//! and the connection is closed so the client can reconnect.

use std::convert::Infallible;

use axum::extract::{Query, State};
use axum::http::header::{HeaderValue, CACHE_CONTROL};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::server::infra::event_bus::{parse_topics, Envelope, ModelEvent, Topic};
use crate::server::AppState;
use depot_core::auth::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    /// Comma-separated topic names. Empty or absent means all topics.
    #[serde(default)]
    pub topics: Option<String>,
}

/// Real-time SSE stream of data model events for the UI.
#[utoipa::path(
    get,
    path = "/api/v1/events/stream",
    params(
        ("topics" = Option<String>, Query, description = "Comma-separated topic filter")
    ),
    responses(
        (status = 200, description = "SSE event stream", content_type = "text/event-stream")
    ),
    tag = "streaming"
)]
pub async fn event_stream(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<StreamQuery>,
) -> Response {
    let is_admin = state.auth.backend.require_admin(&user.0).await.is_ok();
    let topics = parse_topics(params.topics.as_deref(), is_admin);

    let stream = async_stream::stream! {
        // Phase 1: Load the pre-materialized snapshot — O(1), no KV queries.
        let snapshot = state.bg.model.load().to_snapshot(is_admin);
        if let Ok(json) = serde_json::to_string(&snapshot) {
            yield Ok::<Event, Infallible>(Event::default().event("snapshot").data(json));
        }

        // Phase 2: Subscribe and stream deltas.
        let mut rx = state.bg.event_bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(envelope) => {
                    if !topics.contains(&envelope.topic) {
                        continue;
                    }
                    if let Ok(json) = serde_json::to_string(&envelope) {
                        yield Ok(Event::default()
                            .event(envelope.event_type())
                            .id(envelope.seq.to_string())
                            .data(json));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        lagged = n,
                        user = %user.0,
                        "SSE client lagged, sending resync"
                    );
                    let resync = Envelope {
                        seq: state.bg.event_bus.current_seq(),
                        timestamp: chrono::Utc::now(),
                        topic: Topic::Health,
                        event: ModelEvent::Resync,
                    };
                    if let Ok(json) = serde_json::to_string(&resync) {
                        yield Ok(Event::default().event("resync").data(json));
                    }
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    };

    // Override the default `cache-control: no-cache` that axum's Sse sets.
    // Firefox treats `no-cache` as "store but revalidate", so a second tab
    // opening the same SSE URL finds the in-progress cache entry, must
    // revalidate, and parks in `AwaitingCacheCallbacks` waiting for the
    // first stream to finish writing — which never happens for a live SSE.
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
