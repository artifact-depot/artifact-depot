// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use std::sync::Arc;

use bloomfilter::Bloom;

use depot_blob_file::file_blob::FileBlobStore;
use depot_core::service;
use depot_core::store::blob::BlobStore;
use depot_core::store::kv::KvStore;
use depot_core::store::kv::*;
use depot_core::store_registry::StoreRegistry;
use depot_kv_redb::sharded_redb::ShardedRedbKvStore;

use depot_core::update::UpdateSender;

use super::bloom::{bloom_empty_like, bloom_union, BloomAccumulator};
use super::docker_gc::extract_manifest_refs;
use super::gc_loop::{gc_due, run_blob_reaper};
use super::gc_pass::{gc_pass, GcState};
use super::repo_cleanup::clean_repo_artifacts;

#[test]
fn bloom_filter_basic() {
    let mut bf: Bloom<[u8]> = Bloom::new_for_fp_rate(100, 0.01);
    bf.set(b"hello" as &[u8]);
    bf.set(b"world" as &[u8]);
    assert!(bf.check(b"hello" as &[u8]));
    assert!(bf.check(b"world" as &[u8]));
    assert!(!bf.check(b"missing" as &[u8]));
}

#[test]
fn bloom_filter_union() {
    let mut bf1: Bloom<[u8]> = Bloom::new_for_fp_rate(100, 0.01);
    let mut bf2 = bloom_empty_like(&bf1);
    bf1.set(b"alpha" as &[u8]);
    bf2.set(b"beta" as &[u8]);

    bloom_union(&mut bf1, &bf2);
    assert!(bf1.check(b"alpha" as &[u8]));
    assert!(bf1.check(b"beta" as &[u8]));
}

/// Verify the zero-allocation `BloomAccumulator::or_from` path produces
/// a bitmap byte-for-byte identical to the old `bloom_union` fold. This
/// locks in the semantic equivalence used by the hot path in `gc_pass`.
#[test]
fn bloom_accumulator_matches_union() {
    let template: Bloom<[u8]> = Bloom::new_for_fp_rate(1000, 0.01);

    // Build several shard-local filters with overlapping and disjoint keys.
    let mut shards: Vec<Bloom<[u8]>> = (0..8).map(|_| bloom_empty_like(&template)).collect();
    for (i, shard) in shards.iter_mut().enumerate() {
        for j in 0..64 {
            shard.set(format!("shard-{i}-item-{j}").as_bytes());
        }
        // Shared keys that appear in multiple shards.
        shard.set(b"shared-key-a" as &[u8]);
        shard.set(b"shared-key-b" as &[u8]);
    }

    // Reference: fold via the allocating `bloom_union` path.
    let mut via_union = bloom_empty_like(&template);
    for s in &shards {
        bloom_union(&mut via_union, s);
    }

    // New path: fold via `BloomAccumulator::or_from` and finalize.
    let acc = BloomAccumulator::empty_like(&template);
    for s in &shards {
        acc.or_from(s);
    }
    let via_acc = acc.finalize();

    assert_eq!(via_union.bitmap(), via_acc.bitmap());
    assert_eq!(via_union.number_of_bits(), via_acc.number_of_bits());
    assert_eq!(
        via_union.number_of_hash_functions(),
        via_acc.number_of_hash_functions()
    );
    assert_eq!(via_union.sip_keys(), via_acc.sip_keys());

    // And verify the check() behaviour matches for a sample of inserted
    // and non-inserted keys.
    for i in 0..8 {
        for j in 0..64 {
            let key = format!("shard-{i}-item-{j}");
            assert!(via_acc.check(key.as_bytes()));
        }
    }
    assert!(via_acc.check(b"shared-key-a" as &[u8]));
    assert!(via_acc.check(b"shared-key-b" as &[u8]));
}

/// Verify that concurrent `or_from` calls across many threads produce the
/// same combined filter as the sequential fold. This is the production
/// scenario: the artifact scan has up to 128 shard tasks folding into a
/// shared `Arc<BloomAccumulator>` simultaneously.
#[test]
fn bloom_accumulator_concurrent_or_matches_sequential() {
    use std::sync::Arc;
    use std::thread;

    let template: Bloom<[u8]> = Bloom::new_for_fp_rate(10_000, 0.01);
    let n_shards = 32;
    let items_per_shard = 64;

    // Build shard-local filters with overlapping and disjoint keys.
    let shards: Vec<Bloom<[u8]>> = (0..n_shards)
        .map(|i| {
            let mut bf = bloom_empty_like(&template);
            for j in 0..items_per_shard {
                bf.set(format!("shard-{i}-item-{j}").as_bytes());
            }
            bf.set(b"shared-key-a" as &[u8]);
            bf.set(b"shared-key-b" as &[u8]);
            bf
        })
        .collect();

    // Sequential reference.
    let seq_acc = BloomAccumulator::empty_like(&template);
    for s in &shards {
        seq_acc.or_from(s);
    }
    let seq = seq_acc.finalize();

    // Concurrent fold.
    let par_acc = Arc::new(BloomAccumulator::empty_like(&template));
    let handles: Vec<_> = shards
        .into_iter()
        .map(|bf| {
            let acc = Arc::clone(&par_acc);
            thread::spawn(move || acc.or_from(&bf))
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let par = Arc::try_unwrap(par_acc)
        .unwrap_or_else(|_| panic!("leaked Arc"))
        .finalize();

    assert_eq!(seq.bitmap(), par.bitmap());
    for i in 0..n_shards {
        for j in 0..items_per_shard {
            assert!(par.check(format!("shard-{i}-item-{j}").as_bytes()));
        }
    }
    assert!(par.check(b"shared-key-a" as &[u8]));
    assert!(par.check(b"shared-key-b" as &[u8]));
}

#[test]
fn bloom_filter_false_positive_rate() {
    let n = 10_000;
    let mut bf: Bloom<[u8]> = Bloom::new_for_fp_rate(n, 0.01);
    for i in 0..n {
        bf.set(format!("item-{i}").as_bytes());
    }

    // All inserted items must be found (no false negatives).
    for i in 0..n {
        assert!(bf.check(format!("item-{i}").as_bytes()));
    }

    // Check false positive rate on non-inserted items.
    let test_count = 100_000;
    let mut false_positives = 0;
    for i in n..(n + test_count) {
        if bf.check(format!("item-{i}").as_bytes()) {
            false_positives += 1;
        }
    }
    let fp_rate = false_positives as f64 / test_count as f64;
    // Allow up to 2% (target is 1%, some variance expected).
    assert!(
        fp_rate < 0.02,
        "false positive rate {fp_rate:.4} exceeds 2%"
    );
}

/// Helper: create the standard test scaffolding (KV, blob store, registry).
async fn setup_test_env() -> (
    Arc<dyn KvStore>,
    Arc<FileBlobStore>,
    Arc<StoreRegistry>,
    tempfile::TempDir,
) {
    let dir = crate::server::infra::test_support::test_tempdir();
    let kv: Arc<dyn KvStore> = Arc::new(
        ShardedRedbKvStore::open(&dir.path().join("kv"), 2, 0)
            .await
            .unwrap(),
    );
    let blobs =
        Arc::new(FileBlobStore::new(&dir.path().join("blobs"), false, 1_048_576, false).unwrap());
    let registry = Arc::new(StoreRegistry::new());
    registry
        .add("default", blobs.clone() as Arc<dyn BlobStore>)
        .await;

    service::put_store(
        kv.as_ref(),
        &StoreRecord {
            schema_version: CURRENT_RECORD_VERSION,
            name: "default".to_string(),
            kind: StoreKind::File {
                root: dir.path().join("blobs").to_string_lossy().to_string(),
                sync: false,
                io_size: 1024,
                direct_io: false,
            },
            created_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    (kv, blobs, registry, dir)
}

/// Helper: create a repo, blob, and artifact that references the blob.
async fn create_referenced_blob(
    kv: &dyn KvStore,
    blobs: &FileBlobStore,
    blob_id: &str,
    hash: &str,
    data: &[u8],
) {
    let repo = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "test-repo".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: chrono::Utc::now(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    };
    // put_repo is idempotent for our purposes.
    let _ = service::put_repo(kv, &repo).await;

    blobs.put(blob_id, data).await.unwrap();
    service::put_blob(
        kv,
        "default",
        &BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.to_string(),
            hash: hash.to_string(),
            size: data.len() as u64,
            created_at: chrono::Utc::now(),
            store: "default".to_string(),
        },
    )
    .await
    .unwrap();

    let now = chrono::Utc::now();
    service::put_artifact(
        kv,
        "test-repo",
        &format!("file-{blob_id}.txt"),
        &ArtifactRecord {
            schema_version: CURRENT_RECORD_VERSION,
            id: String::new(),
            size: data.len() as u64,
            content_type: "text/plain".to_string(),
            kind: ArtifactKind::Raw,
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            path: String::new(),
            internal: false,
            blob_id: Some(blob_id.to_string()),
            content_hash: Some(hash.to_string()),
            etag: Some(hash.to_string().clone()),
        },
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_pass_deletes_orphaned_blobs() {
    let (kv, blobs, registry, _dir) = setup_test_env().await;

    // Create a blob record + file, but no artifact references it.
    // Use hex-prefixed blob_ids so they land inside the 256-shard hex
    // layout used by the sharded blob-store walk.
    let blob_id = "aa00orphanblob1";
    blobs.put(blob_id, b"orphaned data").await.unwrap();
    service::put_blob(
        kv.as_ref(),
        "default",
        &BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: blob_id.to_string(),
            hash: "orphan_hash_1".to_string(),
            size: 13,
            created_at: chrono::Utc::now(),
            store: "default".to_string(),
        },
    )
    .await
    .unwrap();

    assert!(blobs.exists(blob_id).await.unwrap());

    let mut state = GcState::new();

    // Pass 1: Phase A deletes the KV record immediately (blob_id not
    // referenced by any artifact).  Phase B discovers the orphan blob
    // and marks it as a candidate.
    let stats = gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.deleted_dedup_refs, 1);
    assert_eq!(stats.orphaned_blobs_deleted, 0);
    assert_eq!(state.orphan_candidates.len(), 1);
    // KV record is gone.
    assert!(service::get_blob(kv.as_ref(), "default", "orphan_hash_1")
        .await
        .unwrap()
        .is_none());
    // File still exists (grace period).
    assert!(blobs.exists(blob_id).await.unwrap());

    // Pass 2: Phase B deletes the orphan blob (second consecutive pass).
    let stats = gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.orphaned_blobs_deleted, 1);
    assert_eq!(state.orphan_candidates.len(), 0);
    assert!(!blobs.exists(blob_id).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_pass_preserves_referenced_blobs() {
    let (kv, blobs, registry, _dir) = setup_test_env().await;

    create_referenced_blob(
        kv.as_ref(),
        &blobs,
        "dd03referencedblob",
        "ref_hash",
        b"important data",
    )
    .await;

    let mut state = GcState::new();

    // Two passes — blob is always referenced, never deleted.
    gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();

    assert!(blobs.exists("dd03referencedblob").await.unwrap());
    assert!(service::get_blob(kv.as_ref(), "default", "ref_hash")
        .await
        .unwrap()
        .is_some());
    assert_eq!(state.orphan_candidates.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_orphan_blob_without_kv_record() {
    let (kv, blobs, registry, _dir) = setup_test_env().await;

    // Create a blob file on disk with NO KV BlobRecord (simulates a
    // crash between writing the file and creating the KV record).
    let blob_id = "bb01orphannokv1";
    blobs.put(blob_id, b"crash orphan").await.unwrap();
    assert!(blobs.exists(blob_id).await.unwrap());

    let mut state = GcState::new();

    // Pass 1: Phase A has nothing to delete (no KV record exists).
    // Phase B discovers the file and marks it as a candidate.
    let stats = gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.deleted_dedup_refs, 0);
    assert_eq!(stats.orphaned_blobs_deleted, 0);
    assert_eq!(state.orphan_candidates.len(), 1);
    assert!(blobs.exists(blob_id).await.unwrap());

    // Pass 2: Phase B deletes the orphan blob.
    let stats = gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.orphaned_blobs_deleted, 1);
    assert_eq!(state.orphan_candidates.len(), 0);
    assert!(!blobs.exists(blob_id).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_orphan_grace_period_cleared_by_new_reference() {
    let (kv, blobs, registry, _dir) = setup_test_env().await;

    // Create an orphan blob.
    let blob_id = "cc02graceblob1";
    blobs.put(blob_id, b"grace data").await.unwrap();

    let mut state = GcState::new();

    // Pass 1: file becomes a candidate.
    gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(state.orphan_candidates.len(), 1);

    // Between passes: an artifact is created that references this blob_id.
    create_referenced_blob(kv.as_ref(), &blobs, blob_id, "grace_hash", b"grace data").await;

    // Pass 2: bloom filter now includes blob_id, so the candidate is
    // pruned and the file is NOT deleted.
    let stats = gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.orphaned_blobs_deleted, 0);
    assert_eq!(state.orphan_candidates.len(), 0);
    assert!(blobs.exists(blob_id).await.unwrap());
}

// -----------------------------------------------------------------------
// extract_manifest_refs
// -----------------------------------------------------------------------

#[test]
fn test_extract_manifest_refs_single_manifest() {
    let json = serde_json::json!({
        "schemaVersion": 2,
        "config": {"digest": "sha256:configaaa", "size": 50},
        "layers": [
            {"digest": "sha256:layer111", "size": 100},
            {"digest": "sha256:layer222", "size": 200},
        ],
    })
    .to_string();
    let refs = extract_manifest_refs(&json);
    assert_eq!(refs.len(), 3);
    assert!(refs.contains(&"sha256:configaaa".to_string()));
    assert!(refs.contains(&"sha256:layer111".to_string()));
    assert!(refs.contains(&"sha256:layer222".to_string()));
}

#[test]
fn test_extract_manifest_refs_manifest_list() {
    let json = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [
            {"digest": "sha256:child1", "platform": {"architecture": "amd64"}},
            {"digest": "sha256:child2", "platform": {"architecture": "arm64"}},
        ],
    })
    .to_string();
    let refs = extract_manifest_refs(&json);
    assert_eq!(refs.len(), 2);
    assert!(refs.contains(&"sha256:child1".to_string()));
    assert!(refs.contains(&"sha256:child2".to_string()));
}

#[test]
fn test_extract_manifest_refs_invalid_json() {
    assert!(extract_manifest_refs("not{json").is_empty());
}

#[test]
fn test_extract_manifest_refs_empty_object() {
    assert!(extract_manifest_refs("{}").is_empty());
}

// -----------------------------------------------------------------------
// GC edge cases
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_orphan_candidate_cap() {
    let (kv, blobs, registry, _dir) = setup_test_env().await;

    // Create 5 orphan blob files (no KV records, no artifacts). Use hex
    // prefixes so they're discoverable by the sharded blob-store walk.
    for i in 0..5u8 {
        blobs
            .put(
                &format!("{i:02x}{i:02x}orphancap{i}"),
                format!("data-{i}").as_bytes(),
            )
            .await
            .unwrap();
    }

    let mut state = GcState::new();

    // Pass 1 with cap=3: only 3 should become candidates.
    let stats = gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        3,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.orphaned_blobs_deleted, 0);
    assert_eq!(state.orphan_candidates.len(), 3);

    // Pass 2: the 3 tracked candidates are eligible for deletion.
    let stats = gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        3,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.orphaned_blobs_deleted, 3);

    // The 2 untracked orphans should still exist on disk.
    let mut remaining = 0;
    for i in 0..5u8 {
        if blobs
            .exists(&format!("{i:02x}{i:02x}orphancap{i}"))
            .await
            .unwrap()
        {
            remaining += 1;
        }
    }
    assert_eq!(remaining, 2, "2 orphans should remain (were never tracked)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_multiple_stores() {
    let dir = crate::server::infra::test_support::test_tempdir();
    let kv: Arc<dyn KvStore> = Arc::new(
        ShardedRedbKvStore::open(&dir.path().join("kv"), 2, 0)
            .await
            .unwrap(),
    );

    // Set up two blob stores.
    let blobs1 =
        Arc::new(FileBlobStore::new(&dir.path().join("blobs1"), false, 1_048_576, false).unwrap());
    let blobs2 =
        Arc::new(FileBlobStore::new(&dir.path().join("blobs2"), false, 1_048_576, false).unwrap());
    let registry = Arc::new(StoreRegistry::new());
    registry
        .add("store1", blobs1.clone() as Arc<dyn BlobStore>)
        .await;
    registry
        .add("store2", blobs2.clone() as Arc<dyn BlobStore>)
        .await;

    for (name, root) in [
        ("store1", dir.path().join("blobs1")),
        ("store2", dir.path().join("blobs2")),
    ] {
        service::put_store(
            kv.as_ref(),
            &StoreRecord {
                schema_version: CURRENT_RECORD_VERSION,
                name: name.to_string(),
                kind: StoreKind::File {
                    root: root.to_string_lossy().to_string(),
                    sync: false,
                    io_size: 1024,
                    direct_io: false,
                },
                created_at: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();
    }

    // Create an orphan blob in each store.
    blobs1.put("aa00orphans1", b"store1-data").await.unwrap();
    blobs2.put("bb01orphans2", b"store2-data").await.unwrap();

    let mut state = GcState::new();

    // Pass 1: both become candidates.
    gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(state.orphan_candidates.len(), 2);

    // Pass 2: both are deleted.
    let stats = gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.orphaned_blobs_deleted, 2);
    assert!(!blobs1.exists("aa00orphans1").await.unwrap());
    assert!(!blobs2.exists("bb01orphans2").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_internal_artifacts_immune() {
    let (kv, _blobs, registry, _dir) = setup_test_env().await;

    // Create a repo with a 1-day max_age cleanup policy.
    let repo = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "internal-repo".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: chrono::Utc::now(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: Some(1),
        deleting: false,
    };
    service::put_repo(kv.as_ref(), &repo).await.unwrap();

    let old = chrono::Utc::now() - chrono::Duration::days(10);

    // Internal artifact (e.g., APT metadata) — should be immune.
    let internal = ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: 0,
        content_type: "application/octet-stream".to_string(),
        created_at: old,
        updated_at: old,
        last_accessed_at: old,
        path: String::new(),
        kind: ArtifactKind::Raw,
        internal: true,
        blob_id: None,
        content_hash: None,
        etag: None,
    };
    service::put_artifact(kv.as_ref(), "internal-repo", "internal.txt", &internal)
        .await
        .unwrap();

    // Non-internal artifact with same age — should be expired.
    let external = ArtifactRecord {
        internal: false,
        ..internal.clone()
    };
    service::put_artifact(kv.as_ref(), "internal-repo", "external.txt", &external)
        .await
        .unwrap();

    let mut state = GcState::new();
    let stats = gc_pass(
        kv.clone(),
        &registry,
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
    assert!(
        service::get_artifact(kv.as_ref(), "internal-repo", "internal.txt")
            .await
            .unwrap()
            .is_some(),
        "internal artifact must survive"
    );
    assert!(
        service::get_artifact(kv.as_ref(), "internal-repo", "external.txt")
            .await
            .unwrap()
            .is_none(),
        "external artifact must be expired"
    );
}

// -----------------------------------------------------------------------
// run_blob_reaper loop tests
// -----------------------------------------------------------------------

use crate::server::config::settings::{Settings, SettingsHandle};
use crate::server::infra::task::{TaskKind, TaskManager, TaskStatus};
use tokio_util::sync::CancellationToken;

/// Helper: create settings with a given GC interval.
fn test_settings(gc_interval: u64) -> Arc<SettingsHandle> {
    let mut s = Settings::default();
    s.gc_interval_secs = Some(gc_interval);
    s.gc_min_interval_secs = Some(gc_interval);
    Arc::new(SettingsHandle::new(s))
}

#[tokio::test(start_paused = true)]
async fn run_blob_reaper_acquires_lease_and_runs_gc() {
    let (kv, blobs, registry, _dir) = setup_test_env().await;

    // Create an orphan blob — if GC runs, it will become a candidate.
    blobs.put("ee04reaperorphan", b"orphan").await.unwrap();

    let cancel = CancellationToken::new();
    // gc_interval=0 so GC fires on the first 60s tick.
    let settings = test_settings(0);
    let task_manager = Arc::new(TaskManager::new(kv.clone(), "test-instance".into()));

    let reaper = tokio::spawn({
        let kv = kv.clone();
        let registry = registry.clone();
        let cancel = cancel.clone();
        let settings = settings.clone();
        let task_manager = task_manager.clone();
        async move {
            run_blob_reaper(
                kv,
                registry,
                "test-instance".to_string(),
                cancel,
                settings,
                task_manager,
                UpdateSender::noop(),
                Arc::new(tokio::sync::Mutex::new(GcState::new())),
            )
            .await;
        }
    });

    // Wait for the reaper to complete a GC pass (ticks every 60s).
    for _ in 0..700 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let tasks = task_manager.list().await;
        if tasks
            .iter()
            .any(|t| t.kind == TaskKind::BlobGc && t.status == TaskStatus::Completed)
        {
            break;
        }
    }

    cancel.cancel();
    reaper.await.unwrap();

    // Verify a GC task ran to completion.
    let tasks = task_manager.list().await;
    let gc_tasks: Vec<_> = tasks
        .iter()
        .filter(|t| t.kind == TaskKind::BlobGc)
        .collect();
    assert!(
        !gc_tasks.is_empty(),
        "reaper should have created at least one GC task"
    );
    assert!(
        gc_tasks.iter().any(|t| t.status == TaskStatus::Completed),
        "at least one GC task should complete"
    );
}

#[tokio::test(start_paused = true)]
async fn run_blob_reaper_shutdown_releases_lease() {
    let (kv, _blobs, registry, _dir) = setup_test_env().await;

    let cancel = CancellationToken::new();
    let settings = test_settings(0);
    let task_manager = Arc::new(TaskManager::new(kv.clone(), "test-instance".into()));

    let reaper = tokio::spawn({
        let kv = kv.clone();
        let registry = registry.clone();
        let cancel = cancel.clone();
        let settings = settings.clone();
        let task_manager = task_manager.clone();
        async move {
            run_blob_reaper(
                kv,
                registry,
                "test-instance".to_string(),
                cancel,
                settings,
                task_manager,
                UpdateSender::noop(),
                Arc::new(tokio::sync::Mutex::new(GcState::new())),
            )
            .await;
        }
    });

    // Wait for GC to run.
    for _ in 0..700 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let tasks = task_manager.list().await;
        if !tasks.is_empty() {
            break;
        }
    }

    // Cancel and wait for graceful shutdown.
    cancel.cancel();
    reaper.await.unwrap();

    // After shutdown, another instance should be able to acquire the lease.
    let acquired = crate::server::worker::cluster::try_acquire_lease(
        kv.as_ref(),
        crate::server::worker::cluster::LEASE_GC,
        "other-instance",
        3600,
    )
    .await
    .unwrap();
    assert!(acquired, "lease should be released after shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_pass_cancellation() {
    let (kv, _blobs, registry, _dir) = setup_test_env().await;

    // Create a repo so the pass has some work to do.
    let repo = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "cancel-repo".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: chrono::Utc::now(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    };
    service::put_repo(kv.as_ref(), &repo).await.unwrap();

    let cancel = CancellationToken::new();
    cancel.cancel(); // Cancel immediately.

    let mut state = GcState::new();
    let result = gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        Some(&cancel),
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await;
    assert!(result.is_err(), "cancelled pass should return error");
}

// -----------------------------------------------------------------------
// S3 orphan scanning via gc_pass
// -----------------------------------------------------------------------

use depot_blob_s3::S3BlobStore;

/// Start an in-process S3 server and return (S3BlobStore, endpoint, TempDir).
async fn make_s3_store() -> (Arc<S3BlobStore>, String, tempfile::TempDir) {
    use hyper_util::rt::TokioIo;
    use s3s::service::S3ServiceBuilder;
    use tokio::net::TcpListener;

    let s3_root = tempfile::tempdir().expect("create s3 root");
    std::fs::create_dir_all(s3_root.path().join("test-bucket")).expect("create bucket dir");

    let fs = s3s_fs::FileSystem::new(s3_root.path()).expect("s3s_fs::FileSystem::new");
    let service = {
        let mut b = S3ServiceBuilder::new(fs);
        b.set_auth(s3s::auth::SimpleAuth::from_single("test", "test"));
        b.build()
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(_) => break,
            };
            let svc = service.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection(io, svc)
                .await;
            });
        }
    });

    let endpoint = format!("http://127.0.0.1:{}", addr.port());

    let store = Arc::new(
        S3BlobStore::new(
            "test-bucket".to_string(),
            Some(endpoint.clone()),
            "us-east-1".to_string(),
            None,
            Some("test".to_string()),
            Some("test".to_string()),
            3,
            3,
            30,
            "standard",
        )
        .await
        .expect("S3BlobStore::new"),
    );

    (store, endpoint, s3_root)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_pass_s3_orphan_scanning() {
    let dir = crate::server::infra::test_support::test_tempdir();
    let kv: Arc<dyn KvStore> = Arc::new(
        ShardedRedbKvStore::open(&dir.path().join("kv"), 2, 0)
            .await
            .unwrap(),
    );

    let (s3_store, endpoint, _s3_root) = make_s3_store().await;
    let registry = Arc::new(StoreRegistry::new());
    registry
        .add("s3-store", s3_store.clone() as Arc<dyn BlobStore>)
        .await;

    service::put_store(
        kv.as_ref(),
        &StoreRecord {
            schema_version: CURRENT_RECORD_VERSION,
            name: "s3-store".to_string(),
            kind: StoreKind::S3 {
                bucket: "test-bucket".to_string(),
                endpoint: Some(endpoint),
                region: "us-east-1".to_string(),
                prefix: None,
                access_key: Some("test".to_string()),
                secret_key: Some("test".to_string()),
                max_retries: 3,
                connect_timeout_secs: 3,
                read_timeout_secs: 30,
                retry_mode: "standard".to_string(),
            },
            created_at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();

    // Create an orphan blob in S3 (no artifact references it).
    let orphan_id = "ff00orphans3blob1";
    s3_store.put(orphan_id, b"s3 orphan data").await.unwrap();
    service::put_blob(
        kv.as_ref(),
        "s3-store",
        &BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: orphan_id.to_string(),
            hash: "s3_orphan_hash".to_string(),
            size: 14,
            created_at: chrono::Utc::now(),
            store: "s3-store".to_string(),
        },
    )
    .await
    .unwrap();

    let mut state = GcState::new();

    // Pass 1: KV record is deleted (not referenced), orphan blob becomes candidate.
    let stats = gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.deleted_dedup_refs, 1);
    assert_eq!(stats.orphaned_blobs_deleted, 0);
    assert_eq!(state.orphan_candidates.len(), 1);
    // Blob still exists in S3.
    assert!(s3_store.exists(orphan_id).await.unwrap());

    // Pass 2: Orphan file is deleted after grace period.
    let stats = gc_pass(
        kv.clone(),
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.orphaned_blobs_deleted, 1);
    assert_eq!(state.orphan_candidates.len(), 0);
    assert!(!s3_store.exists(orphan_id).await.unwrap());
}

// -----------------------------------------------------------------------
// Error injection tests
// -----------------------------------------------------------------------

use depot_core::error::Retryability;
use depot_core::store::test_mock::{FailingKvStore, KvOp};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_pass_transient_error() {
    let (kv, _blobs, registry, _dir) = setup_test_env().await;
    let kv = FailingKvStore::wrap(kv);

    // Fail scan_prefix on repos table → list_repos fails transiently.
    kv.fail_on_table(KvOp::ScanPrefix, "repos", Some(1), Retryability::Transient);

    let mut state = GcState::new();
    let result = gc_pass(
        kv.clone() as Arc<dyn KvStore>,
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await;
    match result {
        Err(e) => assert!(e.is_transient(), "error should be transient"),
        Ok(_) => panic!("gc_pass should have failed"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gc_pass_permanent_error() {
    let (kv, _blobs, registry, _dir) = setup_test_env().await;
    let kv = FailingKvStore::wrap(kv);

    kv.fail_on_table(KvOp::ScanPrefix, "repos", Some(1), Retryability::Permanent);

    let mut state = GcState::new();
    let result = gc_pass(
        kv.clone() as Arc<dyn KvStore>,
        &registry,
        &mut state,
        None,
        usize::MAX,
        None,
        false,
        &UpdateSender::noop(),
    )
    .await;
    match result {
        Err(e) => assert!(!e.is_transient(), "error should be permanent"),
        Ok(_) => panic!("gc_pass should have failed"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_repo_artifacts_basic() {
    let (kv, _blobs, registry, _dir) = setup_test_env().await;

    // Create a repo with 1-day max_age policy.
    let repo = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "clean-test".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: chrono::Utc::now(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: Some(1),
        deleting: false,
    };
    service::put_repo(kv.as_ref(), &repo).await.unwrap();

    let old = chrono::Utc::now() - chrono::Duration::days(5);
    let now = chrono::Utc::now();

    // Old artifact (should be expired).
    let old_rec = ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: 0,
        content_type: "application/octet-stream".to_string(),
        created_at: old,
        updated_at: old,
        last_accessed_at: old,
        path: String::new(),
        kind: ArtifactKind::Raw,
        internal: false,
        blob_id: None,
        content_hash: None,
        etag: None,
    };
    service::put_artifact(kv.as_ref(), "clean-test", "old.txt", &old_rec)
        .await
        .unwrap();

    // New artifact (should survive).
    let new_rec = ArtifactRecord {
        created_at: now,
        updated_at: now,
        last_accessed_at: now,
        ..old_rec.clone()
    };
    service::put_artifact(kv.as_ref(), "clean-test", "new.txt", &new_rec)
        .await
        .unwrap();

    let stats = clean_repo_artifacts(
        kv.clone(),
        &registry,
        &repo,
        None,
        None,
        &UpdateSender::noop(),
    )
    .await
    .unwrap();
    assert_eq!(stats.expired_artifacts, 1);
    assert_eq!(stats.scanned_artifacts, 1);

    assert!(service::get_artifact(kv.as_ref(), "clean-test", "old.txt")
        .await
        .unwrap()
        .is_none());
    assert!(service::get_artifact(kv.as_ref(), "clean-test", "new.txt")
        .await
        .unwrap()
        .is_some());
}

/// Docker manifest and blob-ref records must survive a per-record age
/// policy — they're reachable only via tags or other manifests, and
/// expiring them while their tag/parent manifest survives surfaces as
/// `MANIFEST_UNKNOWN` / `BLOB_UNKNOWN` 404s. `docker_gc` handles their
/// cleanup via reachability instead. Tags themselves remain subject to
/// the policy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expire_repo_artifacts_protects_docker_bookkeeping() {
    let (kv, _blobs, _registry, _dir) = setup_test_env().await;

    let repo = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "dkr-clean".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Docker {
            listen: None,
            cleanup_untagged_manifests: None,
        },
        store: "default".to_string(),
        created_at: chrono::Utc::now(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: Some(1),
        deleting: false,
    };
    service::put_repo(kv.as_ref(), &repo).await.unwrap();

    let old = chrono::Utc::now() - chrono::Duration::days(5);
    let stub = ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: 0,
        content_type: "application/vnd.docker.distribution.manifest.v2+json".to_string(),
        created_at: old,
        updated_at: old,
        last_accessed_at: old,
        path: String::new(),
        kind: ArtifactKind::DockerManifest {
            docker_digest: "sha256:placeholder".to_string(),
        },
        internal: false,
        blob_id: None,
        content_hash: None,
        etag: None,
    };

    let tag_path = "_tags/v1";
    let manifest_path = "_manifests/sha256:abc";
    let blob_path = "_blobs/sha256:def";
    let ns_manifest = "myimage/_manifests/sha256:xyz";
    let ns_blob = "myimage/_blobs/sha256:qrs";

    let tag_rec = ArtifactRecord {
        kind: ArtifactKind::DockerTag {
            digest: "sha256:abc".to_string(),
            tag: "v1".to_string(),
        },
        ..stub.clone()
    };
    service::put_artifact(kv.as_ref(), "dkr-clean", tag_path, &tag_rec)
        .await
        .unwrap();
    service::put_artifact(kv.as_ref(), "dkr-clean", manifest_path, &stub)
        .await
        .unwrap();
    service::put_artifact(kv.as_ref(), "dkr-clean", blob_path, &stub)
        .await
        .unwrap();
    service::put_artifact(kv.as_ref(), "dkr-clean", ns_manifest, &stub)
        .await
        .unwrap();
    service::put_artifact(kv.as_ref(), "dkr-clean", ns_blob, &stub)
        .await
        .unwrap();

    super::repo_cleanup::expire_repo_artifacts(kv.clone(), &repo, None, &UpdateSender::noop())
        .await
        .unwrap();

    assert!(
        service::get_artifact(kv.as_ref(), "dkr-clean", tag_path)
            .await
            .unwrap()
            .is_none(),
        "tag should be expired by age policy"
    );
    for protected in [manifest_path, blob_path, ns_manifest, ns_blob] {
        assert!(
            service::get_artifact(kv.as_ref(), "dkr-clean", protected)
                .await
                .unwrap()
                .is_some(),
            "{protected} should be protected from age-based cleanup"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_repo_artifacts_scan_error() {
    let (kv, _blobs, registry, _dir) = setup_test_env().await;
    let kv = FailingKvStore::wrap(kv);

    let repo = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "err-repo".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: chrono::Utc::now(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: Some(1),
        deleting: false,
    };
    service::put_repo(kv.as_ref(), &repo).await.unwrap();

    // Fail artifact scan.
    kv.fail_on_table(KvOp::ScanRange, "artifacts", None, Retryability::Permanent);

    let result = clean_repo_artifacts(
        kv.clone() as Arc<dyn KvStore>,
        &registry,
        &repo,
        None,
        None,
        &UpdateSender::noop(),
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_repo_artifacts_with_cancel() {
    let (kv, _blobs, registry, _dir) = setup_test_env().await;

    let repo = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "cancel-clean".to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: chrono::Utc::now(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    };
    service::put_repo(kv.as_ref(), &repo).await.unwrap();

    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = clean_repo_artifacts(
        kv.clone(),
        &registry,
        &repo,
        Some(&cancel),
        None,
        &UpdateSender::noop(),
    )
    .await;
    assert!(result.is_err(), "cancelled clean should return error");
}

/// Full-flow regression for the depot-dev observation that a staging-MOVED old
/// tag survived the destination's max-age cleanup: push an aged image into a
/// source repo (namespaced, like real first-party images), staging-move it to a
/// destination with a 1-day age policy, then run the expiry sweep. The moved
/// tag record must keep its original created_at and therefore expire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_moved_old_tag_expires_by_destination_age_policy() {
    let (kv, blobs, _registry, _dir) = setup_test_env().await;

    let mk_repo = |name: &str, max_age: Option<u64>| RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: name.to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Docker {
            listen: None,
            cleanup_untagged_manifests: None,
        },
        store: "default".to_string(),
        created_at: chrono::Utc::now(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: max_age,
        deleting: false,
    };
    let src = mk_repo("dkr-move-src", None);
    let dest = mk_repo("dkr-move-dest", Some(1));
    service::put_repo(kv.as_ref(), &src).await.unwrap();
    service::put_repo(kv.as_ref(), &dest).await.unwrap();

    // A minimal, parseable v2 manifest stored as a blob (copy_tag walks it).
    let manifest_json = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {"digest": "sha256:cfg", "size": 2, "mediaType": "application/vnd.docker.container.image.v1+json"},
        "layers": [{"digest": "sha256:lay", "size": 2, "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip"}]
    })
    .to_string();
    blobs.put("mblob", manifest_json.as_bytes()).await.unwrap();

    let old = chrono::Utc::now() - chrono::Duration::days(47);
    let image = "myriad/api_alert_manager";
    let stub = ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: manifest_json.len() as u64,
        content_type: "application/vnd.docker.distribution.manifest.v2+json".to_string(),
        created_at: old,
        updated_at: old,
        last_accessed_at: old,
        path: String::new(),
        kind: ArtifactKind::DockerManifest {
            docker_digest: "sha256:root".to_string(),
        },
        internal: false,
        blob_id: Some("mblob".to_string()),
        content_hash: None,
        etag: None,
    };
    let tag_rec = ArtifactRecord {
        kind: ArtifactKind::DockerTag {
            digest: "sha256:root".to_string(),
            tag: "1.0.0-sb-test".to_string(),
        },
        blob_id: None,
        ..stub.clone()
    };
    let tag_path = format!("{image}/_tags/1.0.0-sb-test");
    let manifest_path = format!("{image}/_manifests/sha256:root");
    service::put_artifact(kv.as_ref(), "dkr-move-src", &tag_path, &tag_rec)
        .await
        .unwrap();
    service::put_artifact(kv.as_ref(), "dkr-move-src", &manifest_path, &stub)
        .await
        .unwrap();
    for blob_ref in ["_blobs/sha256:cfg", "_blobs/sha256:lay"] {
        service::put_artifact(kv.as_ref(), "dkr-move-src", blob_ref, &stub)
            .await
            .unwrap();
    }

    // Staging-move the tag (same store, delete source) — the real reorg path.
    let updater = UpdateSender::noop();
    let target = depot_format_docker::CopyTarget {
        kv: kv.as_ref(),
        updater: &updater,
        source_repo: "dkr-move-src",
        source_store: "default",
        source_blobs: blobs.as_ref(),
        dest_repo: "dkr-move-dest",
        dest_store: "default",
        dest_blobs: blobs.as_ref(),
    };
    depot_format_docker::promote::copy_tag(&target, Some(image), "1.0.0-sb-test", true, true)
        .await
        .unwrap();

    // The moved record must keep its original created_at...
    let moved = service::get_artifact(kv.as_ref(), "dkr-move-dest", &tag_path)
        .await
        .unwrap()
        .expect("moved tag exists in dest");
    assert_eq!(moved.created_at, old, "move must preserve created_at");

    // ...and therefore expire under the destination's 1-day age policy.
    let (_, _, expired) =
        super::repo_cleanup::expire_repo_artifacts(kv.clone(), &dest, None, &UpdateSender::noop())
            .await
            .unwrap();
    assert!(expired >= 1, "aged moved tag should expire, got {expired}");
    assert!(
        service::get_artifact(kv.as_ref(), "dkr-move-dest", &tag_path)
            .await
            .unwrap()
            .is_none(),
        "moved tag should be expired by the destination age policy"
    );

    // The paired delete must also remove the browse-tree file entry — a
    // surviving row is a ghost (dangling tag in the UI, no record behind it).
    let (_, te_pk, te_sk) = depot_core::store::keys::tree_entry_key("dkr-move-dest", &tag_path);
    assert!(
        kv.get(depot_core::store::keys::TABLE_DIR_ENTRIES, te_pk, te_sk)
            .await
            .unwrap()
            .is_none(),
        "expired tag must not leave a ghost browse-tree entry"
    );
}

// -----------------------------------------------------------------------
// gc_due scheduling tests
// -----------------------------------------------------------------------

#[test]
fn gc_due_interval_mode() {
    use chrono::{Duration, Utc};
    let now = Utc::now();
    // Not due before the interval elapses, due after.
    assert!(!gc_due(now - Duration::seconds(100), now, 3600, 60, None));
    assert!(gc_due(now - Duration::seconds(3600), now, 3600, 60, None));
    // Min interval guards even when the interval has elapsed.
    assert!(!gc_due(
        now - Duration::seconds(3600),
        now,
        3600,
        7200,
        None
    ));
}

#[test]
fn gc_due_fixed_time_mode() {
    use chrono::{DateTime, Duration, NaiveTime, Utc};
    let t = NaiveTime::from_hms_opt(7, 0, 0).unwrap();
    let noon: DateTime<Utc> = "2026-07-06T12:00:00Z".parse().unwrap();

    // Last pass yesterday, today's 07:00 occurrence has passed -> due, even
    // though a huge gc_interval would say otherwise (fixed time wins).
    assert!(gc_due(
        noon - Duration::days(1),
        noon,
        999_999_999,
        60,
        Some(t)
    ));

    // Last pass started at today's occurrence -> not due again today.
    let today_at_7: DateTime<Utc> = "2026-07-06T07:00:10Z".parse().unwrap();
    assert!(!gc_due(today_at_7, noon, 86400, 60, Some(t)));

    // A manual pass after the occurrence does not re-anchor: still not due
    // until tomorrow's occurrence.
    let manual: DateTime<Utc> = "2026-07-06T10:00:00Z".parse().unwrap();
    assert!(!gc_due(manual, noon, 86400, 60, Some(t)));
    let tomorrow_8: DateTime<Utc> = "2026-07-07T08:00:00Z".parse().unwrap();
    assert!(gc_due(manual, tomorrow_8, 86400, 60, Some(t)));

    // Before today's occurrence the schedule looks at yesterday's: a pass
    // that ran after it is not due yet.
    let six_am: DateTime<Utc> = "2026-07-06T06:00:00Z".parse().unwrap();
    assert!(!gc_due(
        six_am - Duration::hours(20),
        six_am,
        86400,
        60,
        Some(t)
    ));
    // ...but one that predates yesterday's occurrence is.
    assert!(gc_due(
        six_am - Duration::days(2),
        six_am,
        86400,
        60,
        Some(t)
    ));

    // Min interval still guards a pass started moments ago.
    let just_after_7: DateTime<Utc> = "2026-07-06T07:00:30Z".parse().unwrap();
    assert!(!gc_due(
        just_after_7 - Duration::seconds(20),
        just_after_7,
        86400,
        3600,
        Some(t)
    ));
}

#[tokio::test(start_paused = true)]
async fn run_blob_reaper_fires_at_fixed_start_time() {
    use chrono::{Duration, Utc};

    let (kv, _blobs, registry, _dir) = setup_test_env().await;

    // Persist a last-started timestamp of ~25h ago so a scheduled
    // occurrence of the configured start time has passed since then.
    service::set_gc_last_started_at(kv.as_ref(), Utc::now() - Duration::hours(25))
        .await
        .unwrap();

    // Start time = one minute ago (UTC time-of-day); interval is huge so
    // only the fixed-time path can fire this pass.
    let start_time = (Utc::now() - Duration::minutes(1))
        .format("%H:%M")
        .to_string();
    let mut s = Settings::default();
    s.gc_interval_secs = Some(999_999_999);
    s.gc_min_interval_secs = Some(60);
    s.gc_start_time = Some(start_time);
    let settings = Arc::new(SettingsHandle::new(s));

    let cancel = CancellationToken::new();
    let task_manager = Arc::new(TaskManager::new(kv.clone(), "test-instance".into()));

    let reaper = tokio::spawn({
        let kv = kv.clone();
        let registry = registry.clone();
        let cancel = cancel.clone();
        let settings = settings.clone();
        let task_manager = task_manager.clone();
        async move {
            run_blob_reaper(
                kv,
                registry,
                "test-instance".to_string(),
                cancel,
                settings,
                task_manager,
                UpdateSender::noop(),
                Arc::new(tokio::sync::Mutex::new(GcState::new())),
            )
            .await;
        }
    });

    // Wait for the reaper to complete a GC pass (ticks every 60s).
    for _ in 0..700 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let tasks = task_manager.list().await;
        if tasks
            .iter()
            .any(|t| t.kind == TaskKind::BlobGc && t.status == TaskStatus::Completed)
        {
            break;
        }
    }

    cancel.cancel();
    reaper.await.unwrap();

    let tasks = task_manager.list().await;
    assert!(
        tasks
            .iter()
            .any(|t| t.kind == TaskKind::BlobGc && t.status == TaskStatus::Completed),
        "fixed-start-time scheduling should have fired a GC pass despite a huge gc_interval"
    );
}

// -----------------------------------------------------------------------
// record_expired reason selection (feeds the depot.cleanup audit events)
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_expired_reports_which_policy_fired() {
    use super::repo_cleanup::{record_expired, ExpiryReason};
    use chrono::{Duration, Utc};

    let (kv, _blobs, _registry, _dir) = setup_test_env().await;
    let now = Utc::now();
    let rec = |created_days: i64, accessed_days: i64, internal: bool| ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: 1,
        content_type: "text/plain".to_string(),
        kind: ArtifactKind::Raw,
        created_at: now - Duration::days(created_days),
        updated_at: now,
        last_accessed_at: now - Duration::days(accessed_days),
        path: String::new(),
        internal,
        blob_id: None,
        content_hash: None,
        etag: None,
    };
    let age_cutoff = Some(now - Duration::days(30));
    let unaccessed_cutoff = Some(now - Duration::days(30));

    // Old but recently accessed: only the age policy can fire.
    let old = rec(100, 0, false);
    assert_eq!(
        record_expired(kv.as_ref(), "r", "a.txt", &old, false, age_cutoff, None).await,
        Some(ExpiryReason::MaxAge)
    );
    assert_eq!(
        record_expired(
            kv.as_ref(),
            "r",
            "a.txt",
            &old,
            false,
            None,
            unaccessed_cutoff
        )
        .await,
        None,
        "recently accessed artifact must survive an unaccessed policy"
    );

    // Recently created but long unaccessed (no tree entry: record fallback).
    let stale = rec(0, 100, false);
    assert_eq!(
        record_expired(
            kv.as_ref(),
            "r",
            "b.txt",
            &stale,
            false,
            None,
            unaccessed_cutoff
        )
        .await,
        Some(ExpiryReason::Unaccessed)
    );

    // Both policies would fire: age wins the label.
    let ancient = rec(100, 100, false);
    assert_eq!(
        record_expired(
            kv.as_ref(),
            "r",
            "c.txt",
            &ancient,
            false,
            age_cutoff,
            unaccessed_cutoff
        )
        .await,
        Some(ExpiryReason::MaxAge)
    );

    // Internal artifacts and docker bookkeeping paths never expire.
    let internal = rec(100, 100, true);
    assert_eq!(
        record_expired(
            kv.as_ref(),
            "r",
            "d.txt",
            &internal,
            false,
            age_cutoff,
            unaccessed_cutoff
        )
        .await,
        None
    );
    let bookkeeping = rec(100, 100, false);
    assert_eq!(
        record_expired(
            kv.as_ref(),
            "r",
            "img/_manifests/sha256:abc",
            &bookkeeping,
            true,
            age_cutoff,
            unaccessed_cutoff
        )
        .await,
        None
    );
}

/// The seed-match invariant behind the whole sharded-GC design: building N
/// seed-identical shard filters and merging them must yield EXACTLY the
/// filter a single sequential build over the same keys produces — same
/// bitmap bytes, same membership. Guards against any drift in
/// `bloom_empty_like`/merge (e.g. a shard silently getting its own seed),
/// which would make live blobs read as orphans. Written against the public
/// helpers so it validates any future `bloomfilter` crate migration
/// unchanged.
#[test]
fn sharded_merge_equals_single_filter_build() {
    let n_shards = 16;
    let keys_per_shard = 128;
    let template: Bloom<[u8]> = Bloom::new_for_fp_rate(n_shards * keys_per_shard, 0.01);

    // Reference: one filter, all keys, sequential.
    let mut single = bloom_empty_like(&template);
    for i in 0..n_shards {
        for j in 0..keys_per_shard {
            single.set(format!("blob-{i}-{j}").as_bytes());
        }
    }

    // Sharded: each shard sets its own keys into its own filter; merge both
    // ways (bloom_union fold and BloomAccumulator).
    let shards: Vec<Bloom<[u8]>> = (0..n_shards)
        .map(|i| {
            let mut bf = bloom_empty_like(&template);
            for j in 0..keys_per_shard {
                bf.set(format!("blob-{i}-{j}").as_bytes());
            }
            bf
        })
        .collect();

    let mut via_union = bloom_empty_like(&template);
    for s in &shards {
        bloom_union(&mut via_union, s);
    }
    let acc = BloomAccumulator::empty_like(&template);
    for s in &shards {
        acc.or_from(s);
    }
    let via_acc = acc.finalize();

    // Exact bitmap equality against the single build — not just membership.
    assert_eq!(
        single.bitmap(),
        via_union.bitmap(),
        "union-merged shards must be byte-identical to a single build"
    );
    assert_eq!(
        single.bitmap(),
        via_acc.bitmap(),
        "accumulator-merged shards must be byte-identical to a single build"
    );

    // And the membership contract that GC actually relies on: no false
    // negatives — every key set in any shard reads as present post-merge.
    for i in 0..n_shards {
        for j in 0..keys_per_shard {
            let key = format!("blob-{i}-{j}");
            assert!(single.check(key.as_bytes()));
            assert!(via_union.check(key.as_bytes()));
            assert!(via_acc.check(key.as_bytes()));
        }
    }
}
