// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Depot Vue frontend, compiled by `build.rs` and embedded via
//! `rust-embed`. Exposes a single axum `Router` that serves the built
//! assets with SPA fallback to `index.html`.

use axum::{
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    Router,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "frontend/dist"]
struct Assets;

/// Serve embedded static assets, falling back to index.html for SPA routing.
///
/// Public so a server can branch its own fallback (e.g. return a docker JSON
/// 404 under `/v2`) and delegate everything else here.
pub async fn static_handler(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // Try to serve the exact file first.
    if !path.is_empty() {
        if let Some(file) = Assets::get(path) {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime.as_ref())],
                file.data,
            )
                .into_response();
        }
    }

    // Fall back to index.html for SPA client-side routing.
    match Assets::get("index.html") {
        Some(file) => Html(String::from_utf8_lossy(&file.data).into_owned()).into_response(),
        None => (StatusCode::NOT_FOUND, "UI not available").into_response(),
    }
}

/// Routes that serve the embedded Vue frontend.
///
/// Mount this as a fallback on a depot-server router so all unmatched
/// paths fall through to the SPA shell.
pub fn ui_routes() -> Router {
    Router::new().fallback(static_handler)
}
