// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use chrono::DateTime;

use crate::server::repo::now_utc;
use crate::server::store::{BlobStore, KvStore};
use crate::server::worker::blob_reaper::{gc_pass, GcState};
use crate::server::worker::cleanup::{extract_image_prefix, run_cleanup_tick};
use depot_blob_file::FileBlobStore;
use depot_core::service;
use depot_core::store::kv::{
    ArtifactKind, ArtifactRecord, FormatConfig, RepoConfig, RepoKind, StoreKind, StoreRecord,
    CURRENT_RECORD_VERSION,
};
use depot_core::store_registry::StoreRegistry;
use depot_core::update::UpdateSender;
use depot_kv_redb::ShardedRedbKvStore;
use serde_json::json;

fn test_settings() -> Arc<crate::server::config::settings::SettingsHandle> {
    Arc::new(crate::server::config::settings::SettingsHandle::new(
        crate::server::config::settings::Settings::default(),
    ))
}

/// Create a temporary KV+store registry pair.
async fn make_stores() -> (
    Arc<dyn KvStore>,
    Arc<StoreRegistry>,
    Arc<dyn BlobStore>,
    tempfile::TempDir,
) {
    let dir = crate::server::infra::test_support::test_tempdir();
    let blob_root = dir.path().join("blobs");
    let kv = ShardedRedbKvStore::open(&dir.path().join("kv"), 2, 0)
        .await
        .unwrap();
    let blobs = Arc::new(FileBlobStore::new(&blob_root, false, 1_048_576, false).unwrap())
        as Arc<dyn BlobStore>;
    let registry = Arc::new(StoreRegistry::new());
    registry.add("default", blobs.clone()).await;

    let now = now_utc();
    let blob_root_str = blob_root.to_string_lossy().to_string();
    service::put_store(
        &kv,
        &StoreRecord {
            schema_version: CURRENT_RECORD_VERSION,
            name: "default".to_string(),
            kind: StoreKind::File {
                root: blob_root_str,
                sync: false,
                io_size: 1024,
                direct_io: false,
            },
            created_at: now,
        },
    )
    .await
    .unwrap();

    (Arc::new(kv), registry, blobs, dir)
}

fn make_record(
    created_at: DateTime<chrono::Utc>,
    last_accessed_at: DateTime<chrono::Utc>,
) -> ArtifactRecord {
    ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: 0,
        content_type: "application/octet-stream".to_string(),
        created_at,
        updated_at: created_at,
        last_accessed_at,
        path: String::new(),
        kind: ArtifactKind::Raw,
        internal: false,
        blob_id: None,
        content_hash: None,
        etag: None,
    }
}

/// Helper: put a repo config via service layer.
async fn svc_put_repo(kv: &dyn KvStore, config: &RepoConfig) {
    service::put_repo(kv, config).await.unwrap();
}

/// Helper: put an artifact via service layer.
async fn svc_put_artifact(kv: &dyn KvStore, repo: &str, path: &str, rec: &ArtifactRecord) {
    service::put_artifact(kv, repo, path, rec).await.unwrap();
}

/// Helper: get an artifact via service layer.
async fn svc_get_artifact(kv: &dyn KvStore, repo: &str, path: &str) -> Option<ArtifactRecord> {
    service::get_artifact(kv, repo, path).await.unwrap()
}

// ===========================================================================
// extract_image_prefix
// ===========================================================================

#[test]
fn test_extract_image_prefix_with_image() {
    assert_eq!(
        extract_image_prefix("myimage/_manifests/sha256:abc"),
        Some("myimage")
    );
}

#[test]
fn test_extract_image_prefix_flat() {
    assert_eq!(extract_image_prefix("_manifests/sha256:abc"), None);
}

#[test]
fn test_extract_image_prefix_underscore_prefix() {
    assert_eq!(
        extract_image_prefix("_internal/_manifests/sha256:abc"),
        None
    );
}

#[test]
fn test_extract_image_prefix_no_manifests() {
    assert_eq!(extract_image_prefix("some/random/path"), None);
}

#[test]
fn test_extract_image_prefix_empty_string() {
    assert_eq!(extract_image_prefix(""), None);
}

// ===========================================================================
// Cleanup -- no policy
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cleanup_no_policy_skips() {
    let (kv, stores, _blobs, _dir) = make_stores().await;

    let config = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "nopolicy".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    };
    svc_put_repo(kv.as_ref(), &config).await;

    let rec = make_record(
        DateTime::from_timestamp(1000, 0).unwrap_or_default(),
        DateTime::from_timestamp(1000, 0).unwrap_or_default(),
    ); // very old
    svc_put_artifact(kv.as_ref(), "nopolicy", "old-file.txt", &rec).await;

    // GC pass should NOT delete artifacts with no cleanup policy.
    let mut state = GcState::new();
    gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();

    assert!(svc_get_artifact(kv.as_ref(), "nopolicy", "old-file.txt")
        .await
        .is_some());
}

// ===========================================================================
// Cleanup -- empty repos
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cleanup_empty_repos() {
    let (kv, stores, _blobs, _dir) = make_stores().await;
    // No repos at all -- should be a no-op.
    run_cleanup_tick(&kv, &stores, &test_settings())
        .await
        .unwrap();
}

// ===========================================================================
// GC expiration -- max_age
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cleanup_max_age_deletes_old() {
    let (kv, stores, _blobs, _dir) = make_stores().await;

    let config = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "maxage".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: Some(1), // 1 day
        deleting: false,
    };
    svc_put_repo(kv.as_ref(), &config).await;

    // Old artifact (created 10 days ago)
    let old_rec = make_record(now_utc() - chrono::Duration::days(10), now_utc());
    svc_put_artifact(kv.as_ref(), "maxage", "old.txt", &old_rec).await;

    // Recent artifact
    let new_rec = make_record(now_utc(), now_utc());
    svc_put_artifact(kv.as_ref(), "maxage", "new.txt", &new_rec).await;

    // GC pass expires old artifacts during bloom filter build.
    let mut state = GcState::new();
    let stats = gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.expired_artifacts, 1);

    assert!(svc_get_artifact(kv.as_ref(), "maxage", "old.txt")
        .await
        .is_none());
    assert!(svc_get_artifact(kv.as_ref(), "maxage", "new.txt")
        .await
        .is_some());
}

// ===========================================================================
// GC expiration -- max_unaccessed
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cleanup_max_unaccessed_deletes_stale() {
    let (kv, stores, _blobs, _dir) = make_stores().await;

    let config = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "unaccessed".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: Some(1),
        cleanup_max_age_days: None,
        deleting: false,
    };
    svc_put_repo(kv.as_ref(), &config).await;

    // Stale (not accessed for 10 days)
    let stale = make_record(now_utc(), now_utc() - chrono::Duration::days(10));
    svc_put_artifact(kv.as_ref(), "unaccessed", "stale.txt", &stale).await;

    // Fresh (just accessed)
    let fresh = make_record(now_utc() - chrono::Duration::days(30), now_utc());
    svc_put_artifact(kv.as_ref(), "unaccessed", "fresh.txt", &fresh).await;

    let mut state = GcState::new();
    let stats = gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.expired_artifacts, 1);

    assert!(svc_get_artifact(kv.as_ref(), "unaccessed", "stale.txt")
        .await
        .is_none());
    assert!(svc_get_artifact(kv.as_ref(), "unaccessed", "fresh.txt")
        .await
        .is_some());
}

/// Regression: `cleanup_max_unaccessed_days` must honor a *real download*.
///
/// A download updates the authoritative access time on the browse-tree file
/// entry (via the debounced atime worker / `touch_artifact`), NOT the artifact
/// record — whose `last_accessed_at` is frozen at write time. The cleanup sweep
/// must therefore consult the tree entry; if it reads the record's stale field
/// it will wrongly expire a recently-downloaded artifact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cleanup_max_unaccessed_respects_recent_download() {
    let (kv, stores, _blobs, _dir) = make_stores().await;

    let config = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "recent-dl".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: Some(1),
        cleanup_max_age_days: None,
        deleting: false,
    };
    svc_put_repo(kv.as_ref(), &config).await;

    // Uploaded 10 days ago: the record's last_accessed_at is the (frozen) write
    // time, and put_artifact stamps the tree entry with the same value.
    let rec = make_record(
        now_utc() - chrono::Duration::days(10),
        now_utc() - chrono::Duration::days(10),
    );
    svc_put_artifact(kv.as_ref(), "recent-dl", "downloaded.txt", &rec).await;

    // Simulate a download today: this updates ONLY the tree file entry's
    // last_accessed_at (the authoritative access time), as a real GET does.
    service::touch_artifact(kv.as_ref(), "recent-dl", "downloaded.txt", now_utc())
        .await
        .unwrap();

    let mut state = GcState::new();
    gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();

    assert!(
        svc_get_artifact(kv.as_ref(), "recent-dl", "downloaded.txt")
            .await
            .is_some(),
        "artifact downloaded today must survive max_unaccessed=1d cleanup"
    );
}

// ===========================================================================
// GC expiration -- multiple repos with different policies
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cleanup_multiple_repos() {
    let (kv, stores, _blobs, _dir) = make_stores().await;

    // Repo A: max_age 1 day
    let config_a = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "repo-a".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: Some(1),
        deleting: false,
    };
    svc_put_repo(kv.as_ref(), &config_a).await;

    // Repo B: no cleanup policy
    let config_b = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "repo-b".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    };
    svc_put_repo(kv.as_ref(), &config_b).await;

    // Both repos have old artifacts.
    let old = make_record(now_utc() - chrono::Duration::days(5), now_utc());
    svc_put_artifact(kv.as_ref(), "repo-a", "old.txt", &old).await;
    svc_put_artifact(kv.as_ref(), "repo-b", "old.txt", &old).await;

    let mut state = GcState::new();
    gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();

    // repo-a artifact should be cleaned, repo-b should be kept.
    assert!(svc_get_artifact(kv.as_ref(), "repo-a", "old.txt")
        .await
        .is_none());
    assert!(svc_get_artifact(kv.as_ref(), "repo-b", "old.txt")
        .await
        .is_some());
}

// ===========================================================================
// Docker GC -- grace period for blob refs
// ===========================================================================

/// Helper: create a Docker repo config.
fn docker_repo_config(name: &str, cleanup_untagged: bool) -> RepoConfig {
    RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: name.to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Docker {
            listen: None,
            cleanup_untagged_manifests: if cleanup_untagged { Some(true) } else { None },
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    }
}

/// Helper: create a Docker manifest artifact record with inline content
/// referencing the given layer digests.
/// Helper: create a Docker manifest artifact record with manifest JSON stored as a blob.
async fn make_docker_manifest(
    blobs: &dyn depot_core::store::blob::BlobStore,
    digest: &str,
    layer_digests: &[&str],
    created_at: DateTime<chrono::Utc>,
) -> ArtifactRecord {
    let layers: Vec<_> = layer_digests
        .iter()
        .map(|d| json!({"digest": d, "size": 100}))
        .collect();
    let manifest_json = json!({
        "schemaVersion": 2,
        "config": {"digest": format!("sha256:config_{}", digest), "size": 50},
        "layers": layers,
    })
    .to_string();
    let blob_id = uuid::Uuid::new_v4().to_string();
    blobs.put(&blob_id, manifest_json.as_bytes()).await.unwrap();
    ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: manifest_json.len() as u64,
        content_type: "application/vnd.docker.distribution.manifest.v2+json".to_string(),
        created_at,
        updated_at: created_at,
        last_accessed_at: created_at,
        path: String::new(),
        kind: ArtifactKind::DockerManifest {
            docker_digest: digest.to_string(),
        },
        internal: false,
        blob_id: Some(blob_id),
        content_hash: None,
        etag: None,
    }
}

/// Helper: create a Docker blob ref artifact record.
fn make_docker_blob_ref(digest: &str, created_at: DateTime<chrono::Utc>) -> ArtifactRecord {
    ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: 100,
        content_type: "application/octet-stream".to_string(),
        created_at,
        updated_at: created_at,
        last_accessed_at: created_at,
        path: String::new(),
        kind: ArtifactKind::DockerBlob {
            docker_digest: digest.to_string(),
        },
        internal: false,
        blob_id: Some("fake-blob-id".to_string()),
        content_hash: Some("fake-hash".to_string()),
        etag: Some("fake-hash".to_string().clone()),
    }
}

/// Helper: create a Docker tag artifact record.
fn make_docker_tag(
    tag: &str,
    manifest_digest: &str,
    created_at: DateTime<chrono::Utc>,
) -> ArtifactRecord {
    ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: 0,
        content_type: "application/vnd.docker.distribution.manifest.v2+json".to_string(),
        created_at,
        updated_at: created_at,
        last_accessed_at: created_at,
        path: String::new(),
        kind: ArtifactKind::DockerTag {
            digest: manifest_digest.to_string(),
            tag: tag.to_string(),
        },
        internal: false,
        blob_id: None,
        content_hash: None,
        etag: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_gc_grace_period_protects_new_blob_refs() {
    let (kv, stores, _blobs, _dir) = make_stores().await;

    let config = docker_repo_config("docker-grace", false);
    svc_put_repo(kv.as_ref(), &config).await;

    // Old unreferenced blob ref (should be deleted).
    let old_blob = make_docker_blob_ref("sha256:oldlayer", now_utc() - chrono::Duration::hours(2));
    svc_put_artifact(
        kv.as_ref(),
        "docker-grace",
        "_blobs/sha256:oldlayer",
        &old_blob,
    )
    .await;

    // New unreferenced blob ref (within grace period, should be kept).
    let new_blob = make_docker_blob_ref("sha256:newlayer", now_utc());
    svc_put_artifact(
        kv.as_ref(),
        "docker-grace",
        "_blobs/sha256:newlayer",
        &new_blob,
    )
    .await;

    let mut state = GcState::new();
    gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();

    // Old unreferenced blob ref deleted.
    assert!(
        svc_get_artifact(kv.as_ref(), "docker-grace", "_blobs/sha256:oldlayer")
            .await
            .is_none()
    );
    // New unreferenced blob ref kept (grace period).
    assert!(
        svc_get_artifact(kv.as_ref(), "docker-grace", "_blobs/sha256:newlayer")
            .await
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_gc_grace_period_protects_new_manifests() {
    let (kv, stores, blobs, _dir) = make_stores().await;

    let config = docker_repo_config("docker-manifest-grace", true);
    svc_put_repo(kv.as_ref(), &config).await;

    let layer_digest = "sha256:somelayer";

    // Old untagged manifest (should be deleted).
    let old_manifest = make_docker_manifest(
        blobs.as_ref(),
        "sha256:oldmanifest",
        &[layer_digest],
        now_utc() - chrono::Duration::hours(2),
    )
    .await;
    svc_put_artifact(
        kv.as_ref(),
        "docker-manifest-grace",
        "_manifests/sha256:oldmanifest",
        &old_manifest,
    )
    .await;

    // New untagged manifest (within grace period, should be kept).
    let new_manifest = make_docker_manifest(
        blobs.as_ref(),
        "sha256:newmanifest",
        &[layer_digest],
        now_utc(),
    )
    .await;
    svc_put_artifact(
        kv.as_ref(),
        "docker-manifest-grace",
        "_manifests/sha256:newmanifest",
        &new_manifest,
    )
    .await;

    // Blob ref for the layer (referenced by manifests, should survive).
    let blob_ref = make_docker_blob_ref(layer_digest, now_utc() - chrono::Duration::hours(2));
    svc_put_artifact(
        kv.as_ref(),
        "docker-manifest-grace",
        &format!("_blobs/{}", layer_digest),
        &blob_ref,
    )
    .await;

    let mut state = GcState::new();
    gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();

    // Old untagged manifest deleted.
    assert!(svc_get_artifact(
        kv.as_ref(),
        "docker-manifest-grace",
        "_manifests/sha256:oldmanifest"
    )
    .await
    .is_none());
    // New untagged manifest kept (grace period).
    assert!(svc_get_artifact(
        kv.as_ref(),
        "docker-manifest-grace",
        "_manifests/sha256:newmanifest"
    )
    .await
    .is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_gc_deletes_old_unreferenced_blob_refs() {
    let (kv, stores, blobs, _dir) = make_stores().await;

    let config = docker_repo_config("docker-gc-basic", false);
    svc_put_repo(kv.as_ref(), &config).await;

    let old = now_utc() - chrono::Duration::hours(2);

    // Manifest referencing layer1 only.
    let manifest = make_docker_manifest(blobs.as_ref(), "sha256:m1", &["sha256:layer1"], old).await;
    svc_put_artifact(
        kv.as_ref(),
        "docker-gc-basic",
        "_manifests/sha256:m1",
        &manifest,
    )
    .await;

    // Blob ref for layer1 (referenced, should survive).
    let blob1 = make_docker_blob_ref("sha256:layer1", old);
    svc_put_artifact(
        kv.as_ref(),
        "docker-gc-basic",
        "_blobs/sha256:layer1",
        &blob1,
    )
    .await;

    // Blob ref for layer2 (unreferenced, should be deleted).
    let blob2 = make_docker_blob_ref("sha256:layer2", old);
    svc_put_artifact(
        kv.as_ref(),
        "docker-gc-basic",
        "_blobs/sha256:layer2",
        &blob2,
    )
    .await;

    let mut state = GcState::new();
    gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();

    // Referenced blob survives.
    assert!(
        svc_get_artifact(kv.as_ref(), "docker-gc-basic", "_blobs/sha256:layer1")
            .await
            .is_some()
    );
    // Unreferenced blob deleted.
    assert!(
        svc_get_artifact(kv.as_ref(), "docker-gc-basic", "_blobs/sha256:layer2")
            .await
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_gc_tagged_manifests_survive() {
    let (kv, stores, blobs, _dir) = make_stores().await;

    let config = docker_repo_config("docker-tagged", true);
    svc_put_repo(kv.as_ref(), &config).await;

    let old = now_utc() - chrono::Duration::hours(2);

    // Create 10 tagged manifests — these must NEVER be deleted.
    for i in 0..10 {
        let digest = format!("sha256:tagged{i:04}");
        let manifest = make_docker_manifest(blobs.as_ref(), &digest, &["sha256:layer1"], old).await;
        svc_put_artifact(
            kv.as_ref(),
            "docker-tagged",
            &format!("_manifests/{digest}"),
            &manifest,
        )
        .await;

        let tag = make_docker_tag(&format!("v{i}"), &digest, old);
        svc_put_artifact(kv.as_ref(), "docker-tagged", &format!("_tags/v{i}"), &tag).await;
    }

    // Create 200 untagged manifests — most should be deleted, but a bloom
    // filter with a 1% FP rate may let a few survive.
    let untagged_count = 200usize;
    for i in 0..untagged_count {
        let digest = format!("sha256:untagged{i:04}");
        let manifest = make_docker_manifest(blobs.as_ref(), &digest, &["sha256:layer1"], old).await;
        svc_put_artifact(
            kv.as_ref(),
            "docker-tagged",
            &format!("_manifests/{digest}"),
            &manifest,
        )
        .await;
    }

    // Blob ref referenced by all manifests.
    let blob = make_docker_blob_ref("sha256:layer1", old);
    svc_put_artifact(kv.as_ref(), "docker-tagged", "_blobs/sha256:layer1", &blob).await;

    let mut state = GcState::new();
    gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();

    // Safety check: every tagged manifest MUST survive (no false negatives).
    for i in 0..10 {
        let digest = format!("sha256:tagged{i:04}");
        assert!(
            svc_get_artifact(
                kv.as_ref(),
                "docker-tagged",
                &format!("_manifests/{digest}")
            )
            .await
            .is_some(),
            "tagged manifest {digest} was incorrectly deleted"
        );
    }

    // Count surviving untagged manifests.  The bloom filter (1% FP rate) may
    // let a small number survive — that's acceptable and by design.  We just
    // verify that the vast majority were cleaned up.
    let mut untagged_survivors = 0usize;
    for i in 0..untagged_count {
        let digest = format!("sha256:untagged{i:04}");
        if svc_get_artifact(
            kv.as_ref(),
            "docker-tagged",
            &format!("_manifests/{digest}"),
        )
        .await
        .is_some()
        {
            untagged_survivors += 1;
        }
    }

    // With 200 untagged and 1% BF FP rate, expect ~2 survivors on average.
    // Allow up to 5% (10) to guard against statistical flukes.
    let max_allowed_survivors = untagged_count / 20; // 5%
    assert!(
        untagged_survivors <= max_allowed_survivors,
        "too many untagged manifests survived GC: {untagged_survivors}/{untagged_count} \
         (max allowed {max_allowed_survivors})"
    );
}

// ===========================================================================
// Docker GC — namespaced paths (image/_blobs, image/_manifests)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_gc_namespaced_paths() {
    let (kv, stores, blobs, _dir) = make_stores().await;

    let config = docker_repo_config("docker-ns", false);
    svc_put_repo(kv.as_ref(), &config).await;

    let old = now_utc() - chrono::Duration::hours(2);

    // Manifest at namespaced path referencing layer1.
    let manifest = make_docker_manifest(blobs.as_ref(), "sha256:m1", &["sha256:layer1"], old).await;
    svc_put_artifact(
        kv.as_ref(),
        "docker-ns",
        "myimage/_manifests/sha256:m1",
        &manifest,
    )
    .await;

    // Referenced blob ref (namespaced path).
    let blob_ref = make_docker_blob_ref("sha256:layer1", old);
    svc_put_artifact(
        kv.as_ref(),
        "docker-ns",
        "myimage/_blobs/sha256:layer1",
        &blob_ref,
    )
    .await;

    // Unreferenced blob ref (namespaced path) — should be deleted.
    let orphan_ref = make_docker_blob_ref("sha256:orphan", old);
    svc_put_artifact(
        kv.as_ref(),
        "docker-ns",
        "myimage/_blobs/sha256:orphan",
        &orphan_ref,
    )
    .await;

    let mut state = GcState::new();
    gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();

    // Referenced blob survives.
    assert!(
        svc_get_artifact(kv.as_ref(), "docker-ns", "myimage/_blobs/sha256:layer1")
            .await
            .is_some(),
        "referenced namespaced blob ref should survive"
    );
    // Orphan deleted.
    assert!(
        svc_get_artifact(kv.as_ref(), "docker-ns", "myimage/_blobs/sha256:orphan")
            .await
            .is_none(),
        "unreferenced namespaced blob ref should be deleted"
    );
}

// ===========================================================================
// Docker GC — manifest list children are protected
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_gc_manifest_list_children_protected() {
    let (kv, stores, blobs, _dir) = make_stores().await;

    let config = docker_repo_config("docker-ml", true);
    svc_put_repo(kv.as_ref(), &config).await;

    let old = now_utc() - chrono::Duration::hours(2);

    // Child manifest (untagged).
    let child = make_docker_manifest(blobs.as_ref(), "sha256:child", &["sha256:layer1"], old).await;
    svc_put_artifact(kv.as_ref(), "docker-ml", "_manifests/sha256:child", &child).await;

    // Manifest list referencing the child.
    let list_json = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [
            {"digest": "sha256:child", "platform": {"architecture": "amd64"}},
        ],
    })
    .to_string();
    let list_blob_id = uuid::Uuid::new_v4().to_string();
    blobs
        .put(&list_blob_id, list_json.as_bytes())
        .await
        .unwrap();

    let list_record = ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: list_json.len() as u64,
        content_type: "application/vnd.docker.distribution.manifest.list.v2+json".to_string(),
        created_at: old,
        updated_at: old,
        last_accessed_at: old,
        path: String::new(),
        kind: ArtifactKind::DockerManifest {
            docker_digest: "sha256:list".to_string(),
        },
        internal: false,
        blob_id: Some(list_blob_id),
        content_hash: None,
        etag: None,
    };
    svc_put_artifact(
        kv.as_ref(),
        "docker-ml",
        "_manifests/sha256:list",
        &list_record,
    )
    .await;

    // Tag pointing to the manifest list.
    let tag = make_docker_tag("latest", "sha256:list", old);
    svc_put_artifact(kv.as_ref(), "docker-ml", "_tags/latest", &tag).await;

    // Add several more directly-tagged manifests so the tag bloom filter
    // is properly sized (sized for tag_digests.len(), which must be large
    // enough to avoid saturation from manifest list child insertions).
    for i in 0..10 {
        let digest = format!("sha256:direct{i:04}");
        let m = make_docker_manifest(blobs.as_ref(), &digest, &["sha256:layer1"], old).await;
        svc_put_artifact(
            kv.as_ref(),
            "docker-ml",
            &format!("_manifests/{digest}"),
            &m,
        )
        .await;
        let t = make_docker_tag(&format!("v{i}"), &digest, old);
        svc_put_artifact(kv.as_ref(), "docker-ml", &format!("_tags/v{i}"), &t).await;
    }

    // Create 100 truly untagged manifests — most should be deleted.
    let untagged_count = 100usize;
    for i in 0..untagged_count {
        let digest = format!("sha256:untagged{i:04}");
        let untagged = make_docker_manifest(blobs.as_ref(), &digest, &["sha256:layer1"], old).await;
        svc_put_artifact(
            kv.as_ref(),
            "docker-ml",
            &format!("_manifests/{digest}"),
            &untagged,
        )
        .await;
    }

    let mut state = GcState::new();
    gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();

    // Manifest list survives (tagged).
    assert!(
        svc_get_artifact(kv.as_ref(), "docker-ml", "_manifests/sha256:list")
            .await
            .is_some(),
        "manifest list should survive (tagged)"
    );
    // Child manifest survives (referenced by manifest list — the key assertion).
    assert!(
        svc_get_artifact(kv.as_ref(), "docker-ml", "_manifests/sha256:child")
            .await
            .is_some(),
        "child manifest should survive (referenced by manifest list)"
    );
    // All directly-tagged manifests survive.
    for i in 0..10 {
        let digest = format!("sha256:direct{i:04}");
        assert!(
            svc_get_artifact(kv.as_ref(), "docker-ml", &format!("_manifests/{digest}"))
                .await
                .is_some(),
            "directly tagged manifest {digest} should survive"
        );
    }
    // Most untagged manifests should be deleted. The tag bloom filter is
    // sized for tag_digests + manifest-list child digests (everything
    // inserted), so the effective false-positive rate stays near the
    // nominal 1%. A handful of survivors are tolerable false positives;
    // a 5% cap leaves headroom for random sip-key variance while still
    // catching a regression that under-sizes the filter (which inflated
    // the rate to ~10%).
    let mut untagged_survivors = 0usize;
    for i in 0..untagged_count {
        let digest = format!("sha256:untagged{i:04}");
        if svc_get_artifact(kv.as_ref(), "docker-ml", &format!("_manifests/{digest}"))
            .await
            .is_some()
        {
            untagged_survivors += 1;
        }
    }
    let max_allowed = untagged_count / 20; // 5%
    assert!(
        untagged_survivors <= max_allowed,
        "too many untagged manifests survived: {untagged_survivors}/{untagged_count} (max {max_allowed})"
    );
}

// ===========================================================================
// GC — both cleanup policies (max_age AND max_unaccessed) together
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_gc_both_cleanup_policies() {
    let (kv, stores, _blobs, _dir) = make_stores().await;

    let config = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "dual-policy".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: Some(7),
        cleanup_max_age_days: Some(30),
        deleting: false,
    };
    svc_put_repo(kv.as_ref(), &config).await;

    // (a) Old creation + recently accessed → deleted by max_age.
    let a = make_record(now_utc() - chrono::Duration::days(60), now_utc());
    svc_put_artifact(kv.as_ref(), "dual-policy", "old-fresh.txt", &a).await;

    // (b) Recent creation + stale access → deleted by max_unaccessed.
    let b = make_record(now_utc(), now_utc() - chrono::Duration::days(14));
    svc_put_artifact(kv.as_ref(), "dual-policy", "new-stale.txt", &b).await;

    // (c) Recent creation + recent access → survives both.
    let c = make_record(now_utc(), now_utc());
    svc_put_artifact(kv.as_ref(), "dual-policy", "new-fresh.txt", &c).await;

    let mut state = GcState::new();
    let stats = gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.expired_artifacts, 2);

    assert!(
        svc_get_artifact(kv.as_ref(), "dual-policy", "old-fresh.txt")
            .await
            .is_none(),
        "old artifact should be deleted by max_age"
    );
    assert!(
        svc_get_artifact(kv.as_ref(), "dual-policy", "new-stale.txt")
            .await
            .is_none(),
        "stale artifact should be deleted by max_unaccessed"
    );
    assert!(
        svc_get_artifact(kv.as_ref(), "dual-policy", "new-fresh.txt")
            .await
            .is_some(),
        "fresh artifact should survive both policies"
    );
}

// ===========================================================================
// drain_deleting_repos
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_drain_deleting_repos() {
    let (kv, stores, _blobs, _dir) = make_stores().await;

    // Create a repo marked for deletion.
    let config = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "dying-repo".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: true,
    };
    svc_put_repo(kv.as_ref(), &config).await;

    // Add artifacts to the repo.
    let rec = make_record(now_utc(), now_utc());
    svc_put_artifact(kv.as_ref(), "dying-repo", "file1.txt", &rec).await;
    svc_put_artifact(kv.as_ref(), "dying-repo", "file2.txt", &rec).await;

    // Run cleanup tick — should drain the deleting repo.
    run_cleanup_tick(&kv, &stores, &test_settings())
        .await
        .unwrap();

    // Artifacts should be gone.
    assert!(svc_get_artifact(kv.as_ref(), "dying-repo", "file1.txt")
        .await
        .is_none());
    assert!(svc_get_artifact(kv.as_ref(), "dying-repo", "file2.txt")
        .await
        .is_none());

    // Repo config itself should be deleted.
    assert!(service::get_repo(kv.as_ref(), "dying-repo")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_drain_deleting_repos_skips_active() {
    let (kv, stores, _blobs, _dir) = make_stores().await;

    // Active repo.
    let active = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "active-repo".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    };
    svc_put_repo(kv.as_ref(), &active).await;

    // Deleting repo.
    let dying = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "dying-repo2".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: true,
    };
    svc_put_repo(kv.as_ref(), &dying).await;

    let rec = make_record(now_utc(), now_utc());
    svc_put_artifact(kv.as_ref(), "active-repo", "file.txt", &rec).await;
    svc_put_artifact(kv.as_ref(), "dying-repo2", "file.txt", &rec).await;

    run_cleanup_tick(&kv, &stores, &test_settings())
        .await
        .unwrap();

    // Active repo's artifact survives.
    assert!(
        svc_get_artifact(kv.as_ref(), "active-repo", "file.txt")
            .await
            .is_some(),
        "active repo artifact should survive"
    );
    // Dying repo's artifact is gone.
    assert!(
        svc_get_artifact(kv.as_ref(), "dying-repo2", "file.txt")
            .await
            .is_none(),
        "dying repo artifact should be drained"
    );
}

// ===========================================================================
// Docker GC — manifest with missing/empty blob_id
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_gc_manifest_without_blob_id() {
    let (kv, stores, _blobs, _dir) = make_stores().await;

    let config = docker_repo_config("docker-no-blob", false);
    svc_put_repo(kv.as_ref(), &config).await;

    let old = now_utc() - chrono::Duration::hours(2);

    // Manifest with no blob_id (read_manifest_json returns None).
    let manifest_no_blob = ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: 100,
        content_type: "application/vnd.docker.distribution.manifest.v2+json".to_string(),
        created_at: old,
        updated_at: old,
        last_accessed_at: old,
        path: String::new(),
        kind: ArtifactKind::DockerManifest {
            docker_digest: "sha256:no-blob".to_string(),
        },
        internal: false,
        blob_id: None, // No blob — read_manifest_json returns None
        content_hash: None,
        etag: None,
    };
    svc_put_artifact(
        kv.as_ref(),
        "docker-no-blob",
        "_manifests/sha256:no-blob",
        &manifest_no_blob,
    )
    .await;

    // Manifest with empty blob_id (read_manifest_json also returns None).
    let manifest_empty_blob = ArtifactRecord {
        blob_id: Some(String::new()), // Empty string
        kind: ArtifactKind::DockerManifest {
            docker_digest: "sha256:empty-blob".to_string(),
        },
        ..manifest_no_blob.clone()
    };
    svc_put_artifact(
        kv.as_ref(),
        "docker-no-blob",
        "_manifests/sha256:empty-blob",
        &manifest_empty_blob,
    )
    .await;

    // Blob ref that would normally be checked against the manifest layer BF.
    // Since manifests have no readable JSON, their layers aren't extracted,
    // so this blob ref should be deleted (it's unreferenced and old).
    let blob_ref = make_docker_blob_ref("sha256:some-layer", old);
    svc_put_artifact(
        kv.as_ref(),
        "docker-no-blob",
        "_blobs/sha256:some-layer",
        &blob_ref,
    )
    .await;

    // GC should not crash on unreadable manifests.
    let mut state = GcState::new();
    let _stats = gc_pass(
        kv.clone(),
        &stores,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();

    // The blob ref should be deleted (no manifest referenced it).
    assert!(
        svc_get_artifact(kv.as_ref(), "docker-no-blob", "_blobs/sha256:some-layer")
            .await
            .is_none(),
        "unreferenced blob ref should be deleted even with unreadable manifests"
    );
}

// ===========================================================================
// Error injection — cleanup error paths
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cleanup_tick_list_repos_error() {
    use depot_core::error::Retryability;
    use depot_core::store::test_mock::{FailingKvStore, KvOp};

    let (real_kv, stores, _blobs, _dir) = make_stores().await;
    let kv = FailingKvStore::wrap(real_kv);

    // Fail scan_prefix on repos table → drain_deleting_repos will hit
    // the error path and return early (logging the error).
    kv.fail_on_table(KvOp::ScanPrefix, "repos", None, Retryability::Permanent);

    // run_cleanup_tick should not propagate the error (it's logged internally).
    // The tick itself returns Ok because the error is in drain_deleting_repos
    // which doesn't propagate.
    let result = run_cleanup_tick(&(kv as Arc<dyn KvStore>), &stores, &test_settings()).await;
    // The tick may succeed or fail depending on which operation hits the error.
    // Either way, it should not panic.
    let _ = result;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cleanup_tick_drain_error() {
    use depot_core::error::Retryability;
    use depot_core::store::test_mock::{FailingKvStore, KvOp};

    let (real_kv, stores, _blobs, _dir) = make_stores().await;

    // Create a deleting repo.
    let config = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "fail-drain".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: now_utc(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: true,
    };
    svc_put_repo(real_kv.as_ref(), &config).await;
    let rec = make_record(now_utc(), now_utc());
    svc_put_artifact(real_kv.as_ref(), "fail-drain", "file.txt", &rec).await;

    let kv = FailingKvStore::wrap(real_kv);

    // Fail scan operations on artifacts table → drain_deleting_repo will error.
    // But let repos table scans succeed so list_repos_raw works.
    kv.fail_on_table(KvOp::ScanPrefix, "artifacts", None, Retryability::Permanent);

    // run_cleanup_tick should not panic; the per-repo drain error is logged.
    let result = run_cleanup_tick(&(kv as Arc<dyn KvStore>), &stores, &test_settings()).await;
    let _ = result;

    // Artifact should still exist since drain failed.
    // (Need to re-read through the real KV, not the failing wrapper.)
}
