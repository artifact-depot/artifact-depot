// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Tests for the Nexus-compatible staging move/delete endpoints
//! (`/service/rest/v1/staging/move/{destination}` and `.../staging/delete`).

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use serde_json::{json, Value};

use depot_test_support::*;

fn sha256_digest(data: &[u8]) -> String {
    format!(
        "sha256:{:x}",
        sha2::Digest::finalize(sha2::Digest::chain_update(sha2::Sha256::default(), data))
    )
}

const MANIFEST_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.oci.image.index.v1+json";

/// Push a blob under an arbitrary `/v2/<path>` prefix (`<repo>` or
/// `<repo>/<image>`). Returns its sha256 digest.
async fn push_blob_at(app: &TestApp, v2_path: &str, data: &[u8]) -> String {
    let token = app.admin_token();
    let digest = sha256_digest(data);
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/v2/{}/blobs/uploads/?digest={}", v2_path, digest))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, data.len().to_string())
        .body(Body::from(data.to_vec()))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::ACCEPTED,
        "push_blob_at failed: {status}"
    );
    digest
}

/// Push a manifest (any media type) under an arbitrary `/v2/<path>` prefix.
/// Returns its sha256 digest.
async fn push_manifest_at(
    app: &TestApp,
    v2_path: &str,
    reference: &str,
    content_type: &str,
    manifest: &Value,
) -> String {
    let token = app.admin_token();
    let body = serde_json::to_vec(manifest).unwrap();
    let digest = sha256_digest(&body);
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/v2/{}/manifests/{}", v2_path, reference))
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "push_manifest_at failed: {body}"
    );
    digest
}

fn make_manifest_list(child_digests: &[&str]) -> Value {
    json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
        "manifests": child_digests.iter().map(|d| json!({
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "size": 0,
            "digest": d,
            "platform": {"architecture": "amd64", "os": "linux"},
        })).collect::<Vec<_>>(),
    })
}

/// HEAD a manifest by reference (tag or digest); returns the status code.
async fn head_manifest(app: &TestApp, v2_path: &str, reference: &str) -> StatusCode {
    let token = app.admin_token();
    let req = app.auth_request(
        Method::HEAD,
        &format!("/v2/{}/manifests/{}", v2_path, reference),
        &token,
    );
    app.call(req).await.0
}

/// HEAD a blob by digest; returns the status code.
async fn head_blob(app: &TestApp, v2_path: &str, digest: &str) -> StatusCode {
    let token = app.admin_token();
    let req = app.auth_request(
        Method::HEAD,
        &format!("/v2/{}/blobs/{}", v2_path, digest),
        &token,
    );
    app.call(req).await.0
}

async fn post(app: &TestApp, uri: &str) -> (StatusCode, Value) {
    let token = app.admin_token();
    app.call(app.auth_request(Method::POST, uri, &token)).await
}

/// Register a second file-backed blob store rooted at a fresh temp dir.
/// Returns the kept `TempDir` (drop it and the store's files vanish).
async fn create_file_store(app: &TestApp, name: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let token = app.admin_token();
    let (status, body) = app
        .call(app.json_request(
            Method::POST,
            "/api/v1/stores",
            &token,
            json!({
                "name": name,
                "store_type": "file",
                "root": tmp.path().to_str().unwrap(),
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create store {name}: {body}");
    tmp
}

// ===========================================================================
// staging/move
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_move_single_image_relocates_tag() {
    let app = TestApp::new().await;
    app.create_docker_repo("src").await;
    app.create_docker_repo("dst").await;

    let cfg = app.push_docker_blob("src", b"config-bytes").await;
    let layer = app.push_docker_blob("src", b"layer-bytes").await;
    let manifest = TestApp::make_manifest(&cfg, &[&layer]);
    let mdigest = app.push_docker_manifest("src", "1.2.3", &manifest).await;

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/move/dst?repository=src&docker.imageTag=1.2.3",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["components"][0]["version"], "1.2.3");
    assert_eq!(body["components"][0]["repository"], "dst");

    // Destination serves the image by tag, by digest, and the layer blob.
    assert_eq!(head_manifest(&app, "dst", "1.2.3").await, StatusCode::OK);
    assert_eq!(head_manifest(&app, "dst", &mdigest).await, StatusCode::OK);
    assert_eq!(head_blob(&app, "dst", &layer).await, StatusCode::OK);

    // A real pull (GET with Accept negotiation) succeeds against the destination.
    let token = app.admin_token();
    let get = Request::builder()
        .method(Method::GET)
        .uri("/v2/dst/manifests/1.2.3")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::ACCEPT, MANIFEST_ACCEPT)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.call(get).await.0, StatusCode::OK);

    // Source no longer serves the tag (it was a move, not a copy).
    assert_eq!(
        head_manifest(&app, "src", "1.2.3").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_move_multi_arch_copies_full_closure() {
    let app = TestApp::new().await;
    app.create_docker_repo("src").await;
    app.create_docker_repo("dst").await;

    // Two per-arch manifests, pushed by digest (no tag of their own).
    let cfg_a = app.push_docker_blob("src", b"cfg-amd64").await;
    let lay_a = app.push_docker_blob("src", b"layer-amd64").await;
    let man_a = TestApp::make_manifest(&cfg_a, &[&lay_a]);
    let dig_a = sha256_digest(&serde_json::to_vec(&man_a).unwrap());
    push_manifest_at(
        &app,
        "src",
        &dig_a,
        "application/vnd.docker.distribution.manifest.v2+json",
        &man_a,
    )
    .await;

    let cfg_b = app.push_docker_blob("src", b"cfg-arm64").await;
    let lay_b = app.push_docker_blob("src", b"layer-arm64").await;
    let man_b = TestApp::make_manifest(&cfg_b, &[&lay_b]);
    let dig_b = sha256_digest(&serde_json::to_vec(&man_b).unwrap());
    push_manifest_at(
        &app,
        "src",
        &dig_b,
        "application/vnd.docker.distribution.manifest.v2+json",
        &man_b,
    )
    .await;

    // The manifest list, tagged.
    let list = make_manifest_list(&[&dig_a, &dig_b]);
    let list_digest = push_manifest_at(
        &app,
        "src",
        "multi",
        "application/vnd.docker.distribution.manifest.list.v2+json",
        &list,
    )
    .await;

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/move/dst?repository=src&docker.imageTag=multi",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Destination has the list (by tag + digest), both children, and all blobs.
    assert_eq!(head_manifest(&app, "dst", "multi").await, StatusCode::OK);
    assert_eq!(
        head_manifest(&app, "dst", &list_digest).await,
        StatusCode::OK
    );
    assert_eq!(head_manifest(&app, "dst", &dig_a).await, StatusCode::OK);
    assert_eq!(head_manifest(&app, "dst", &dig_b).await, StatusCode::OK);
    assert_eq!(head_blob(&app, "dst", &lay_a).await, StatusCode::OK);
    assert_eq!(head_blob(&app, "dst", &lay_b).await, StatusCode::OK);
    assert_eq!(head_blob(&app, "dst", &cfg_a).await, StatusCode::OK);

    // Source tag is gone.
    assert_eq!(
        head_manifest(&app, "src", "multi").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_move_namespaced_image_by_name_and_tag() {
    let app = TestApp::new().await;
    app.create_docker_repo("src").await;
    app.create_docker_repo("dst").await;

    let cfg = push_blob_at(&app, "src/app1", b"ns-config").await;
    let layer = push_blob_at(&app, "src/app1", b"ns-layer").await;
    let manifest = TestApp::make_manifest(&cfg, &[&layer]);
    push_manifest_at(
        &app,
        "src/app1",
        "1.0.0",
        "application/vnd.docker.distribution.manifest.v2+json",
        &manifest,
    )
    .await;

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/move/dst?repository=src&docker.imageName=app1&docker.imageTag=1.0.0",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["components"][0]["name"], "app1");

    assert_eq!(
        head_manifest(&app, "dst/app1", "1.0.0").await,
        StatusCode::OK
    );
    assert_eq!(
        head_manifest(&app, "src/app1", "1.0.0").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_move_all_tags_of_an_image() {
    let app = TestApp::new().await;
    app.create_docker_repo("src").await;
    app.create_docker_repo("dst").await;

    let cfg = push_blob_at(&app, "src/app2", b"a2-config").await;
    let layer = push_blob_at(&app, "src/app2", b"a2-layer").await;
    let manifest = TestApp::make_manifest(&cfg, &[&layer]);
    for tag in ["1.0.0", "1.1.0", "latest"] {
        push_manifest_at(
            &app,
            "src/app2",
            tag,
            "application/vnd.docker.distribution.manifest.v2+json",
            &manifest,
        )
        .await;
    }

    // No tag criterion → every tag of the named image moves.
    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/move/dst?repository=src&docker.imageName=app2",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["components"].as_array().unwrap().len(), 3);

    for tag in ["1.0.0", "1.1.0", "latest"] {
        assert_eq!(head_manifest(&app, "dst/app2", tag).await, StatusCode::OK);
        assert_eq!(
            head_manifest(&app, "src/app2", tag).await,
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_move_conflict_then_overwrite() {
    let app = TestApp::new().await;
    app.create_docker_repo("src").await;
    app.create_docker_repo("dst").await;

    // Destination already has tag `1.0` pointing at one image.
    let cfg0 = app.push_docker_blob("dst", b"existing-config").await;
    let existing = TestApp::make_manifest(&cfg0, &[]);
    app.push_docker_manifest("dst", "1.0", &existing).await;

    // Source has a different image at the same tag.
    let cfg1 = app.push_docker_blob("src", b"new-config").await;
    let layer1 = app.push_docker_blob("src", b"new-layer").await;
    let new = TestApp::make_manifest(&cfg1, &[&layer1]);
    let new_digest = app.push_docker_manifest("src", "1.0", &new).await;

    // Without overwrite → 409.
    let (status, _) = post(
        &app,
        "/service/rest/v1/staging/move/dst?repository=src&docker.imageTag=1.0",
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // With overwrite → succeeds and dst now resolves to the source image.
    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/move/dst?repository=src&docker.imageTag=1.0&overwrite=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        head_manifest(&app, "dst", &new_digest).await,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_move_cross_store_rejected() {
    let app = TestApp::new().await;
    app.create_docker_repo("src").await;

    // Register a second file store and a destination repo on it.
    let _store2 = create_file_store(&app, "store2").await;
    app.create_repo(json!({
        "name": "dst2",
        "repo_type": "hosted",
        "format": "docker",
        "store": "store2",
    }))
    .await;

    let cfg = app.push_docker_blob("src", b"x-config").await;
    let manifest = TestApp::make_manifest(&cfg, &[]);
    app.push_docker_manifest("src", "1.0", &manifest).await;

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/move/dst2?repository=src&docker.imageTag=1.0",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body.to_string();
    assert!(
        msg.contains("same blob store"),
        "expected same-blob-store error, got: {msg}"
    );
    // The source tag was not touched.
    assert_eq!(head_manifest(&app, "src", "1.0").await, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_move_requires_criteria() {
    let app = TestApp::new().await;
    app.create_docker_repo("src").await;
    app.create_docker_repo("dst").await;

    let (status, _) = post(&app, "/service/rest/v1/staging/move/dst?repository=src").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_move_non_docker_rejected() {
    let app = TestApp::new().await;
    app.create_hosted_repo("raw-src").await; // raw format
    app.create_docker_repo("dst").await;

    let (status, _) = post(
        &app,
        "/service/rest/v1/staging/move/dst?repository=raw-src&docker.imageTag=1.0",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ===========================================================================
// staging/delete
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_delete_removes_tag() {
    let app = TestApp::new().await;
    app.create_docker_repo("src").await;

    let cfg = app.push_docker_blob("src", b"del-config").await;
    let layer = app.push_docker_blob("src", b"del-layer").await;
    let manifest = TestApp::make_manifest(&cfg, &[&layer]);
    app.push_docker_manifest("src", "9.9.9", &manifest).await;

    assert_eq!(head_manifest(&app, "src", "9.9.9").await, StatusCode::OK);

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/delete?repository=src&docker.imageTag=9.9.9",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["components"][0]["version"], "9.9.9");

    assert_eq!(
        head_manifest(&app, "src", "9.9.9").await,
        StatusCode::NOT_FOUND
    );
}

// ===========================================================================
// /v2 fallback
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unmatched_v2_path_returns_docker_404_not_html() {
    let app = TestApp::new().await;
    let token = app.admin_token();

    // A multi-segment image addressed via the /v2/{repo}/{image} shortcut spans
    // more than two name segments, so it matches no registry route. It must come
    // back as a docker JSON error, not the SPA HTML.
    let req = app.auth_request(
        Method::GET,
        "/v2/docker-internal/breakpad/builder_redhat7/tags/list",
        &token,
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["errors"][0]["code"], "NAME_UNKNOWN", "body: {body}");

    // A non-/v2 unmatched path still routes to the SPA branch, not the docker
    // error envelope (the SPA returns HTML, or "UI not available" if the
    // frontend wasn't built — either way, no docker `errors` body).
    let (_, spa_body) = app
        .call(app.request(Method::GET, "/some/client-side/route"))
        .await;
    assert!(
        spa_body.get("errors").is_none(),
        "non-/v2 path should not get a docker error envelope: {spa_body}"
    );
}

// ===========================================================================
// staging/copy (cross-store)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_copy_cross_store_streams_blobs() {
    let app = TestApp::new().await;
    app.create_docker_repo("src").await; // on "default" store

    let _store2 = create_file_store(&app, "store2").await;
    app.create_repo(json!({
        "name": "dst2",
        "repo_type": "hosted",
        "format": "docker",
        "store": "store2",
    }))
    .await;

    let cfg = app.push_docker_blob("src", b"cs-config").await;
    let layer = app.push_docker_blob("src", b"cs-layer").await;
    let manifest = TestApp::make_manifest(&cfg, &[&layer]);
    let mdigest = app.push_docker_manifest("src", "1.2.3", &manifest).await;

    // Copy across stores (no deleteSource → source stays intact).
    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/copy/dst2?repository=src&docker.imageTag=1.2.3",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Destination (on store2) serves the manifest and the layer blob — the
    // bytes were physically streamed into store2.
    assert_eq!(head_manifest(&app, "dst2", "1.2.3").await, StatusCode::OK);
    assert_eq!(head_manifest(&app, "dst2", &mdigest).await, StatusCode::OK);
    assert_eq!(head_blob(&app, "dst2", &layer).await, StatusCode::OK);

    // Copy (not move) leaves the source intact.
    assert_eq!(head_manifest(&app, "src", "1.2.3").await, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_copy_cross_store_with_delete_source_is_a_move() {
    let app = TestApp::new().await;
    app.create_docker_repo("src").await;

    let _store2 = create_file_store(&app, "store2").await;
    app.create_repo(json!({
        "name": "dst2",
        "repo_type": "hosted",
        "format": "docker",
        "store": "store2",
    }))
    .await;

    let cfg = app.push_docker_blob("src", b"csm-config").await;
    let layer = app.push_docker_blob("src", b"csm-layer").await;
    let manifest = TestApp::make_manifest(&cfg, &[&layer]);
    app.push_docker_manifest("src", "2.0.0", &manifest).await;

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/copy/dst2?repository=src&docker.imageTag=2.0.0&deleteSource=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(head_manifest(&app, "dst2", "2.0.0").await, StatusCode::OK);
    assert_eq!(head_blob(&app, "dst2", &layer).await, StatusCode::OK);
    // deleteSource removed the source tag.
    assert_eq!(
        head_manifest(&app, "src", "2.0.0").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_copy_same_store_also_works() {
    let app = TestApp::new().await;
    app.create_docker_repo("src").await;
    app.create_docker_repo("dst").await;

    let cfg = app.push_docker_blob("src", b"ss-config").await;
    let layer = app.push_docker_blob("src", b"ss-layer").await;
    let manifest = TestApp::make_manifest(&cfg, &[&layer]);
    app.push_docker_manifest("src", "3.0.0", &manifest).await;

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/copy/dst?repository=src&docker.imageTag=3.0.0",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(head_manifest(&app, "dst", "3.0.0").await, StatusCode::OK);
    assert_eq!(head_blob(&app, "dst", &layer).await, StatusCode::OK);
    assert_eq!(head_manifest(&app, "src", "3.0.0").await, StatusCode::OK);
}

/// A *same-store* staging move re-points records at shared, content-addressed
/// blobs and writes no bytes to disk. Measured authoritatively via
/// `reconcile_store_stats` (recomputed from the blob records, so it's immune to
/// the async `store_changed` worker), the store's physical blob count and bytes
/// must be identical before and after the move — i.e. no duplication. (This also
/// guards against a regression where the move takes the byte-streaming path on a
/// shared store.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_move_same_store_adds_no_physical_blobs() {
    use depot_core::service::{get_store_stats, reconcile_store_stats};

    let app = TestApp::new().await;
    app.create_docker_repo("src").await;
    app.create_docker_repo("dst").await;

    let cfg = app.push_docker_blob("src", b"stats-config").await;
    let layer = app.push_docker_blob("src", &vec![7u8; 8192]).await; // a sizeable layer
    let manifest = TestApp::make_manifest(&cfg, &[&layer]);
    app.push_docker_manifest("src", "1.0.0", &manifest).await;
    let kv = app.state.repo.kv.clone();

    reconcile_store_stats(kv.as_ref()).await.unwrap();
    let before = get_store_stats(kv.as_ref(), "default")
        .await
        .unwrap()
        .unwrap();

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/move/dst?repository=src&docker.imageTag=1.0.0",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(head_manifest(&app, "dst", "1.0.0").await, StatusCode::OK);

    reconcile_store_stats(kv.as_ref()).await.unwrap();
    let after = get_store_stats(kv.as_ref(), "default")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.blob_count, before.blob_count,
        "same-store move must not add physical blobs"
    );
    assert_eq!(
        after.total_bytes, before.total_bytes,
        "same-store move must not add physical bytes (before={}, after={})",
        before.total_bytes, after.total_bytes
    );
}

/// `reconcile_store_stats` re-derives the store counter from the actual blob
/// records, correcting any drift (the whole point of fix B).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_store_stats_corrects_drift() {
    use depot_core::service::{get_store_stats, put_store_stats, reconcile_store_stats};
    use depot_core::store::kv::StoreStatsRecord;

    let app = TestApp::new().await;
    app.create_docker_repo("src").await;
    let kv = app.state.repo.kv.clone();

    // Two distinct blobs (1000 + 2000 bytes) plus a duplicate of the first,
    // which dedups → 2 physical blobs, 3000 bytes.
    app.push_docker_blob("src", &vec![1u8; 1000]).await;
    app.push_docker_blob("src", &vec![2u8; 2000]).await;
    app.push_docker_blob("src", &vec![1u8; 1000]).await; // dup → no new physical blob

    // Inject drift: a wildly wrong stored counter.
    put_store_stats(
        kv.as_ref(),
        "default",
        &StoreStatsRecord {
            blob_count: 999,
            total_bytes: 9_999_999,
            updated_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    // Reconcile re-derives the truth from the blob records.
    let returned = reconcile_store_stats(kv.as_ref()).await.unwrap();
    assert!(returned.iter().any(|(s, _)| s == "default"));

    let stats = get_store_stats(kv.as_ref(), "default")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.blob_count, 2, "two distinct blobs after dedup");
    assert_eq!(
        stats.total_bytes, 3000,
        "1000 + 2000, dup not double-counted"
    );
}

// ===========================================================================
// Helm staging move / delete
// ===========================================================================

/// PUT a synthetic chart into a hosted helm repo at its canonical root path.
async fn put_helm_chart(app: &TestApp, repo: &str, name: &str, version: &str) {
    let chart = depot_format_helm::store::build_synthetic_chart(name, version, "test chart")
        .expect("build synthetic chart");
    let token = app.admin_token();
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/repository/{repo}/{name}-{version}.tgz"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/gzip")
        .body(Body::from(chart))
        .unwrap();
    let (status, _) = app.call(req).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "upload {name}-{version} to {repo}"
    );
}

/// Fetch a repo's index.yaml as a string (triggers lazy rebuild).
async fn helm_index(app: &TestApp, repo: &str) -> String {
    let token = app.admin_token();
    let req = app.auth_request(
        Method::GET,
        &format!("/repository/{repo}/index.yaml"),
        &token,
    );
    let (status, body) = app.call_raw(req).await;
    assert_eq!(status, StatusCode::OK, "GET index.yaml for {repo}");
    String::from_utf8(body).unwrap()
}

/// A single explicit chart version moves; the destination index gains it and
/// the source index loses it (the stale-flag rebuild on both repos).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helm_staging_move_single_version() {
    let app = TestApp::new().await;
    app.create_helm_repo("hsrc").await;
    app.create_helm_repo("hdst").await;
    put_helm_chart(&app, "hsrc", "myriad", "1.4.2").await;

    // Sanity: source has it, dest doesn't.
    assert!(helm_index(&app, "hsrc").await.contains("1.4.2"));
    assert!(!helm_index(&app, "hdst").await.contains("myriad"));

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/move/hdst?repository=hsrc&name=myriad&version=1.4.2",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["components"][0]["format"], "helm");
    assert_eq!(body["components"][0]["name"], "myriad");
    assert_eq!(body["components"][0]["version"], "1.4.2");

    // Destination gained the chart; it is pullable.
    let dst = helm_index(&app, "hdst").await;
    assert!(dst.contains("myriad"), "dest index missing chart: {dst}");
    assert!(dst.contains("1.4.2"));
    let token = app.admin_token();
    let req = app.auth_request(Method::GET, "/repository/hdst/myriad-1.4.2.tgz", &token);
    assert_eq!(
        app.call_raw(req).await.0,
        StatusCode::OK,
        "chart pullable from dest"
    );

    // Source lost it (index rebuilt via stale flag).
    let src = helm_index(&app, "hsrc").await;
    assert!(
        !src.contains("1.4.2"),
        "source index still lists moved chart: {src}"
    );
}

/// A name-only criterion moves every version of that chart, leaving other
/// charts in the source untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helm_staging_move_all_versions_of_chart() {
    let app = TestApp::new().await;
    app.create_helm_repo("hsrc2").await;
    app.create_helm_repo("hdst2").await;
    put_helm_chart(&app, "hsrc2", "myriad", "1.0.0").await;
    put_helm_chart(&app, "hsrc2", "myriad", "1.1.0").await;
    put_helm_chart(&app, "hsrc2", "observability", "2.0.0").await;

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/move/hdst2?repository=hsrc2&name=myriad",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let moved = body["components"].as_array().unwrap();
    assert_eq!(moved.len(), 2, "both myriad versions moved: {body}");

    let dst = helm_index(&app, "hdst2").await;
    assert!(dst.contains("1.0.0") && dst.contains("1.1.0"));
    // Untouched other chart stays in source; myriad is gone from source.
    let src = helm_index(&app, "hsrc2").await;
    assert!(
        src.contains("observability"),
        "unrelated chart should remain: {src}"
    );
    assert!(
        !src.contains("myriad"),
        "all myriad versions should be gone: {src}"
    );
}

/// Staging delete removes a matched chart version and rebuilds the index.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helm_staging_delete() {
    let app = TestApp::new().await;
    app.create_helm_repo("hdel").await;
    put_helm_chart(&app, "hdel", "myriad", "0.9.0-deleteme").await;
    put_helm_chart(&app, "hdel", "myriad", "1.0.0").await;

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/delete?repository=hdel&name=myriad&version=0.9.0-deleteme",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["components"].as_array().unwrap().len(), 1);

    let idx = helm_index(&app, "hdel").await;
    assert!(
        !idx.contains("deleteme"),
        "junk version should be gone: {idx}"
    );
    assert!(idx.contains("1.0.0"), "kept version should remain");
}

/// Cross-store helm move is rejected by the move endpoint (points at copy).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helm_staging_move_cross_store_rejected() {
    let app = TestApp::new().await;
    let _tmp = create_file_store(&app, "other").await;
    app.create_helm_repo("hsrc3").await;
    // Destination on a different store.
    let token = app.admin_token();
    let (status, _) = app
        .call(app.json_request(
            Method::POST,
            "/api/v1/repositories",
            &token,
            json!({"name": "hdst3", "format": "helm", "repo_type": "hosted", "store": "other"}),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    put_helm_chart(&app, "hsrc3", "myriad", "1.0.0").await;

    let (status, _) = post(
        &app,
        "/service/rest/v1/staging/move/hdst3?repository=hsrc3&name=myriad&version=1.0.0",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "cross-store move must be rejected"
    );
}

/// Cross-store helm copy streams the blob into the destination store; the
/// chart is pullable there and (with deleteSource) removed from the source.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helm_staging_copy_cross_store() {
    let app = TestApp::new().await;
    let _tmp = create_file_store(&app, "other2").await;
    app.create_helm_repo("hsrc4").await;
    let token = app.admin_token();
    let (status, _) = app
        .call(app.json_request(
            Method::POST,
            "/api/v1/repositories",
            &token,
            json!({"name": "hdst4", "format": "helm", "repo_type": "hosted", "store": "other2"}),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    put_helm_chart(&app, "hsrc4", "myriad", "1.0.0").await;

    let (status, body) = post(
        &app,
        "/service/rest/v1/staging/copy/hdst4?repository=hsrc4&name=myriad&version=1.0.0&deleteSource=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Pullable from the different-store destination.
    let req = app.auth_request(Method::GET, "/repository/hdst4/myriad-1.0.0.tgz", &token);
    assert_eq!(
        app.call_raw(req).await.0,
        StatusCode::OK,
        "chart pullable from cross-store dest"
    );
    // deleteSource removed it from the source.
    assert!(!helm_index(&app, "hsrc4").await.contains("1.0.0"));
}

/// A criteria-less helm staging request is refused (no accidental repo sweep).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helm_staging_requires_criteria() {
    let app = TestApp::new().await;
    app.create_helm_repo("hsrc5").await;
    app.create_helm_repo("hdst5").await;
    let (status, _) = post(&app, "/service/rest/v1/staging/move/hdst5?repository=hsrc5").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Cross-format staging (helm source, docker dest) is rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_cross_format_rejected() {
    let app = TestApp::new().await;
    app.create_helm_repo("hsrc6").await;
    app.create_docker_repo("ddst6").await;
    put_helm_chart(&app, "hsrc6", "myriad", "1.0.0").await;
    let (status, _) = post(
        &app,
        "/service/rest/v1/staging/move/ddst6?repository=hsrc6&name=myriad&version=1.0.0",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "helm→docker move must be rejected"
    );
}
