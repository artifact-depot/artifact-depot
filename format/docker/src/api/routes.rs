// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use axum::{
    extract::DefaultBodyLimit,
    routing::{get, head, patch, post},
    Extension, Router,
};

use depot_core::format_state::FormatState;

use super::handlers::*;
use super::tags::catalog;
use super::types::FixedDockerRepo;

/// Build routes for a dedicated Docker port where all images belong to a
/// fixed repo. Handles both single-segment (`ubuntu`) and two-segment
/// (`qkp/builder_go`) image names. The [`FixedDockerRepo`] extension tells
/// the [`DockerPath`](super::types::DockerPath) extractor to use the fixed repo name.
///
/// The returned router has `DefaultBodyLimit::disable()` applied to the upload
/// routes; the caller is responsible for layering any dynamic body-limit
/// middleware on top.
pub fn docker_port_routes(repo_name: String) -> Router<FormatState> {
    /// Maximum body size for Docker manifest uploads (32 MiB).
    const MANIFEST_BODY_LIMIT: usize = 32 * 1024 * 1024;

    // Read-only routes — axum default limit is fine for no-body requests.
    let read_routes = Router::new()
        .route("/v2/", get(v2_check))
        .route("/v2/_catalog", get(catalog))
        .route("/v2/{name}/tags/list", get(list_tags))
        .route("/v2/{ns}/{img}/tags/list", get(list_tags))
        .route("/v2/{name}/blobs/{digest}", head(head_blob).get(get_blob))
        .route(
            "/v2/{ns}/{img}/blobs/{digest}",
            head(head_blob).get(get_blob),
        )
        .route("/v2/{name}/referrers/{digest}", get(get_referrers))
        .route("/v2/{ns}/{img}/referrers/{digest}", get(get_referrers));

    // Manifest routes — 32 MiB (PUT sends JSON; HEAD/GET/DELETE have no body).
    let manifest_routes = Router::new()
        .route(
            "/v2/{name}/manifests/{reference}",
            head(head_manifest)
                .get(get_manifest)
                .put(put_manifest)
                .delete(delete_manifest),
        )
        .route(
            "/v2/{ns}/{img}/manifests/{reference}",
            head(head_manifest)
                .get(get_manifest)
                .put(put_manifest)
                .delete(delete_manifest),
        )
        .layer(DefaultBodyLimit::max(MANIFEST_BODY_LIMIT));

    // Blob upload routes — no body limit; caller applies dynamic middleware.
    let upload_routes = Router::new()
        .route("/v2/{name}/blobs/uploads/", post(start_upload))
        .route("/v2/{ns}/{img}/blobs/uploads/", post(start_upload))
        .route(
            "/v2/{name}/blobs/uploads/{uuid}",
            patch(patch_upload).put(complete_upload),
        )
        .route(
            "/v2/{ns}/{img}/blobs/uploads/{uuid}",
            patch(patch_upload).put(complete_upload),
        )
        .layer(DefaultBodyLimit::disable());

    Router::new()
        .merge(read_routes)
        .merge(manifest_routes)
        .merge(upload_routes)
        .layer(Extension(FixedDockerRepo(repo_name)))
}
