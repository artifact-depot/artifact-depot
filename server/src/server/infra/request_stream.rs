// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Live request feed for the admin Activity view.
//!
//! The request middleware publishes one [`RequestEvent`] per completed HTTP
//! request into a broadcast channel and a small in-memory ring. The SSE
//! endpoint replays the ring as a `snapshot` event on connect, then streams
//! live `request` events. Nothing is persisted: this is a live view, not an
//! audit trail (the OTel/Loki pipeline remains the durable record).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::header::{HeaderValue, CACHE_CONTROL};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::server::AppState;
use depot_core::auth::AuthenticatedUser;

/// How many recent events the connect-time snapshot replays.
const RECENT_CAPACITY: usize = 100;
/// Broadcast ring size per subscriber before a slow client lags out.
const BROADCAST_CAPACITY: usize = 256;

/// One completed HTTP request, as shown in the Activity view.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct RequestEvent {
    /// Monotonic sequence number; clients dedup snapshot/live overlap by it.
    pub seq: u64,
    #[schema(value_type = String, format = "date-time")]
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub request_id: String,
    pub username: String,
    pub ip: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub action: String,
    pub elapsed_ns: u64,
    pub bytes_recv: u64,
    pub bytes_sent: u64,
}

/// Fan-out point between the request middleware and SSE subscribers.
pub struct RequestStream {
    tx: broadcast::Sender<Arc<RequestEvent>>,
    recent: Mutex<VecDeque<Arc<RequestEvent>>>,
    seq: AtomicU64,
}

impl Default for RequestStream {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestStream {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            tx,
            recent: Mutex::new(VecDeque::with_capacity(RECENT_CAPACITY)),
            seq: AtomicU64::new(0),
        }
    }

    /// Record a completed request. Called by the request middleware on every
    /// response; cheap when nobody is watching (a ring push and a failed
    /// broadcast send).
    #[allow(clippy::too_many_arguments)]
    pub fn publish(
        &self,
        request_id: &str,
        username: &str,
        ip: &str,
        method: &str,
        path: &str,
        status: u16,
        action: &str,
        elapsed_ns: u64,
        bytes_recv: u64,
        bytes_sent: u64,
    ) {
        let event = Arc::new(RequestEvent {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            timestamp: chrono::Utc::now(),
            request_id: request_id.to_string(),
            username: username.to_string(),
            ip: ip.to_string(),
            method: method.to_string(),
            path: path.to_string(),
            status,
            action: action.to_string(),
            elapsed_ns,
            bytes_recv,
            bytes_sent,
        });
        if let Ok(mut recent) = self.recent.lock() {
            if recent.len() == RECENT_CAPACITY {
                recent.pop_front();
            }
            recent.push_back(event.clone());
        }
        // Err just means no subscribers right now.
        let _ = self.tx.send(event);
    }

    /// Subscribe first, then snapshot, so no event between the two is lost;
    /// the overlap this allows is deduplicated client-side by `seq`.
    pub fn subscribe(
        &self,
    ) -> (
        Vec<Arc<RequestEvent>>,
        broadcast::Receiver<Arc<RequestEvent>>,
    ) {
        let rx = self.tx.subscribe();
        let snapshot = match self.recent.lock() {
            Ok(recent) => recent.iter().cloned().collect(),
            Err(_) => Vec::new(),
        };
        (snapshot, rx)
    }
}

/// Live SSE stream of HTTP requests (admin only).
#[utoipa::path(
    get,
    path = "/api/v1/requests/stream",
    responses(
        (status = 200, description = "SSE stream: a `snapshot` event with recent requests, then live `request` events", content_type = "text/event-stream"),
        (status = 403, description = "Admin access required")
    ),
    tag = "streaming"
)]
pub async fn request_stream(
    State(state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
) -> Response {
    if let Err(e) = state.auth.backend.require_admin(&user.0).await {
        return e.into_response();
    }

    let stream = async_stream::stream! {
        let (snapshot, mut rx) = state.bg.request_stream.subscribe();
        if let Ok(json) = serde_json::to_string(&snapshot) {
            yield Ok::<Event, Infallible>(Event::default().event("snapshot").data(json));
        }

        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Ok(json) = serde_json::to_string(&*event) {
                        yield Ok(Event::default()
                            .event("request")
                            .id(event.seq.to_string())
                            .data(json));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(lagged = n, user = %user.0, "request stream client lagged, resyncing");
                    let (snapshot, new_rx) = state.bg.request_stream.subscribe();
                    rx = new_rx;
                    if let Ok(json) = serde_json::to_string(&snapshot) {
                        yield Ok(Event::default().event("snapshot").data(json));
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    // `no-store`, not `no-cache` — see event_stream.rs for the Firefox story.
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
