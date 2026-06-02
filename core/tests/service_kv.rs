// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Domain-level tests for the service layer (above the KvStore trait boundary).
//!
//! These tests verify service-layer logic using a real redb backend.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use depot_core::service;
use depot_core::store::kv::*;
use depot_kv_redb::sharded_redb::ShardedRedbKvStore;

fn test_tempdir() -> tempfile::TempDir {
    tempfile::TempDir::new().expect("failed to create temp dir")
}

async fn test_kv() -> (ShardedRedbKvStore, tempfile::TempDir) {
    let dir = test_tempdir();
    let kv = ShardedRedbKvStore::open(&dir.path().join("kv"), 2, 0)
        .await
        .unwrap();
    (kv, dir)
}

fn make_record(created_at: i64, last_accessed_at: i64) -> ArtifactRecord {
    let ca = chrono::DateTime::from_timestamp(created_at, 0).unwrap();
    let la = chrono::DateTime::from_timestamp(last_accessed_at, 0).unwrap();
    ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: String::new(),
        size: 100,
        content_type: "application/octet-stream".to_string(),
        kind: ArtifactKind::Raw,
        created_at: ca,
        updated_at: ca,
        last_accessed_at: la,
        path: String::new(),
        internal: false,
        blob_id: None,
        content_hash: None,
        etag: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_put_get_artifact() {
    let (kv, _dir) = test_kv().await;
    let rec = make_record(1000, 1000);
    service::put_artifact(&kv, "repo", "path/file.txt", &rec)
        .await
        .unwrap();
    let got = service::get_artifact(&kv, "repo", "path/file.txt")
        .await
        .unwrap()
        .unwrap();
    assert!(!got.id.is_empty());
    assert_eq!(got.path, "path/file.txt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_delete_artifact() {
    let (kv, _dir) = test_kv().await;
    service::put_artifact(&kv, "repo", "f.txt", &make_record(1000, 1000))
        .await
        .unwrap();
    assert!(service::delete_artifact(&kv, "repo", "f.txt")
        .await
        .unwrap()
        .is_some());
    assert!(service::get_artifact(&kv, "repo", "f.txt")
        .await
        .unwrap()
        .is_none());
}

/// Check whether a path has a corresponding `TreeEntry::File` in the browse tree.
async fn tree_file_exists(kv: &dyn KvStore, repo: &str, path: &str) -> bool {
    let (table, pk, sk) = depot_core::store::keys::tree_entry_key(repo, path);
    kv.get(table, pk, sk).await.unwrap().is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_delete_artifacts_paired_batch_clears_both_tables() {
    let (kv, _dir) = test_kv().await;
    let kv_dyn: &dyn KvStore = &kv;

    // Two artifacts under different parent dirs (so their tree entries land
    // in different shards), plus a survivor we won't delete.
    service::put_artifact(kv_dyn, "repo", "a/1", &make_record(1, 1))
        .await
        .unwrap();
    service::put_artifact(kv_dyn, "repo", "b/2", &make_record(1, 1))
        .await
        .unwrap();
    service::put_artifact(kv_dyn, "repo", "a/keep", &make_record(1, 1))
        .await
        .unwrap();

    assert!(tree_file_exists(kv_dyn, "repo", "a/1").await);
    assert!(tree_file_exists(kv_dyn, "repo", "b/2").await);
    assert!(tree_file_exists(kv_dyn, "repo", "a/keep").await);

    service::delete_artifacts_paired_batch(kv_dyn, "repo", &["a/1", "b/2"])
        .await
        .unwrap();

    // Both the artifact record AND its paired tree entry are gone for the
    // deleted paths — the UI would no longer list these.
    for path in ["a/1", "b/2"] {
        assert!(
            service::get_artifact(kv_dyn, "repo", path)
                .await
                .unwrap()
                .is_none(),
            "{path} record should be gone"
        );
        assert!(
            !tree_file_exists(kv_dyn, "repo", path).await,
            "{path} tree entry should be gone"
        );
    }

    // The survivor under the same parent dir is untouched in both tables.
    assert!(service::get_artifact(kv_dyn, "repo", "a/keep")
        .await
        .unwrap()
        .is_some());
    assert!(tree_file_exists(kv_dyn, "repo", "a/keep").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_delete_artifacts_paired_batch_empty_and_missing_are_noops() {
    let (kv, _dir) = test_kv().await;
    let kv_dyn: &dyn KvStore = &kv;
    service::put_artifact(kv_dyn, "repo", "keep.txt", &make_record(1, 1))
        .await
        .unwrap();

    // Empty list does nothing.
    service::delete_artifacts_paired_batch(kv_dyn, "repo", &[])
        .await
        .unwrap();
    assert!(service::get_artifact(kv_dyn, "repo", "keep.txt")
        .await
        .unwrap()
        .is_some());
    assert!(tree_file_exists(kv_dyn, "repo", "keep.txt").await);

    // Deleting paths that don't exist is harmless and leaves the survivor.
    service::delete_artifacts_paired_batch(kv_dyn, "repo", &["missing.txt", "also/missing"])
        .await
        .unwrap();
    assert!(service::get_artifact(kv_dyn, "repo", "keep.txt")
        .await
        .unwrap()
        .is_some());
    assert!(tree_file_exists(kv_dyn, "repo", "keep.txt").await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_list_search_artifacts() {
    let (kv, _dir) = test_kv().await;
    service::put_artifact(&kv, "repo", "a/1", &make_record(1, 1))
        .await
        .unwrap();
    service::put_artifact(&kv, "repo", "a/2", &make_record(2, 2))
        .await
        .unwrap();
    service::put_artifact(&kv, "repo", "b/1", &make_record(3, 3))
        .await
        .unwrap();

    let list = service::list_artifacts(&kv, "repo", "a/", &Pagination::default())
        .await
        .unwrap();
    assert_eq!(list.items.len(), 2);

    let search = service::search_artifacts(&kv, "repo", "a/", &Pagination::default())
        .await
        .unwrap();
    assert_eq!(search.items.len(), 2);
}

async fn tree_atime(
    kv: &ShardedRedbKvStore,
    repo: &str,
    path: &str,
) -> chrono::DateTime<chrono::Utc> {
    use depot_core::store::keys;
    let (_, pk, sk) = keys::tree_entry_key(repo, path);
    let bytes = kv
        .get(keys::TABLE_DIR_ENTRIES, pk, sk)
        .await
        .unwrap()
        .expect("tree entry should exist");
    let te: TreeEntry = rmp_serde::from_slice(&bytes).unwrap();
    te.last_accessed_at
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_touch_debounce() {
    let (kv, _dir) = test_kv().await;
    service::put_artifact(&kv, "repo", "f.txt", &make_record(1000, 1000))
        .await
        .unwrap();

    // Within 60s — no update.
    service::touch_artifact(
        &kv,
        "repo",
        "f.txt",
        chrono::DateTime::from_timestamp(1050, 0).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        tree_atime(&kv, "repo", "f.txt").await,
        chrono::DateTime::from_timestamp(1000, 0).unwrap()
    );

    // After 60s — updates.
    service::touch_artifact(
        &kv,
        "repo",
        "f.txt",
        chrono::DateTime::from_timestamp(1061, 0).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        tree_atime(&kv, "repo", "f.txt").await,
        chrono::DateTime::from_timestamp(1061, 0).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_blob_crud() {
    let (kv, _dir) = test_kv().await;
    let blob = BlobRecord {
        schema_version: CURRENT_RECORD_VERSION,
        blob_id: "test-blob-id".to_string(),
        hash: "abc".to_string(),
        size: 42,
        created_at: chrono::Utc::now(),
        store: "default".to_string(),
    };
    service::put_blob(&kv, "default", &blob).await.unwrap();
    let got = service::get_blob(&kv, "default", "abc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.size, 42);

    assert!(service::delete_blob(&kv, "default", "abc").await.unwrap());
    assert!(service::get_blob(&kv, "default", "abc")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_put_dedup_record_dedup() {
    let (kv, _dir) = test_kv().await;
    let blob1 = BlobRecord {
        schema_version: CURRENT_RECORD_VERSION,
        blob_id: "blob-1".to_string(),
        hash: "same-hash".to_string(),
        size: 100,
        created_at: chrono::Utc::now(),
        store: "default".to_string(),
    };
    // First call — creates record, returns None.
    let result = service::put_dedup_record(&kv, "default", &blob1)
        .await
        .unwrap();
    assert!(result.is_none());

    // Second call with different blob_id but same hash — returns existing blob_id.
    let blob2 = BlobRecord {
        blob_id: "blob-2".to_string(),
        ..blob1.clone()
    };
    let result = service::put_dedup_record(&kv, "default", &blob2)
        .await
        .unwrap();
    assert_eq!(result, Some("blob-1".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_user_role_meta() {
    let (kv, _dir) = test_kv().await;
    let user = UserRecord {
        schema_version: CURRENT_RECORD_VERSION,
        username: "alice".to_string(),
        password_hash: "hash".to_string(),
        roles: vec!["admin".to_string()],
        auth_source: "builtin".to_string(),
        token_secret: vec![],
        must_change_password: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    service::put_user(&kv, &user).await.unwrap();
    assert!(service::get_user(&kv, "alice").await.unwrap().is_some());
    assert_eq!(service::list_users(&kv).await.unwrap().len(), 1);

    let role = RoleRecord {
        schema_version: CURRENT_RECORD_VERSION,
        name: "admin".to_string(),
        description: "Full access".to_string(),
        capabilities: vec![],
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    service::put_role(&kv, &role).await.unwrap();
    assert!(service::get_role(&kv, "admin").await.unwrap().is_some());

    service::put_meta(&kv, "version", b"42").await.unwrap();
    assert_eq!(
        service::get_meta(&kv, "version").await.unwrap(),
        Some(b"42".to_vec())
    );
}

/// Regression test for DynamoDB pagination bug: scan_range must return
/// `done: false` when exactly `limit` items are collected but more exist.
/// The bug caused `done: true` at the limit boundary, silently truncating
/// scan results to 1000 per shard (256 shards × 1000 = 256,000 system-wide
/// ceiling).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_range_done_false_at_limit_boundary() {
    use std::borrow::Cow;

    let (kv, _dir) = test_kv().await;
    let table = "artifacts";
    let pk = "test-partition";
    let n = 1100; // More than the typical scan limit of 1000

    // Insert N items with sequential sort keys.
    for i in 0..n {
        let sk = format!("item/{i:06}");
        let value = rmp_serde::to_vec(&make_record(i as i64, i as i64)).unwrap();
        kv.put(table, Cow::Borrowed(pk), Cow::Borrowed(sk.as_str()), &value)
            .await
            .unwrap();
    }

    // First scan with limit=1000: must return done=false since 1100 > 1000.
    let result = kv
        .scan_range(table, Cow::Borrowed(pk), Cow::Borrowed(""), None, 1000)
        .await
        .unwrap();
    assert_eq!(result.items.len(), 1000);
    assert!(
        !result.done,
        "scan_range must return done=false when more items exist beyond the limit"
    );

    // Continue from cursor: should get remaining 100.
    let last_sk = &result.items.last().unwrap().0;
    let cursor = format!("{last_sk}\0");
    let result2 = kv
        .scan_range(
            table,
            Cow::Borrowed(pk),
            Cow::Borrowed(cursor.as_str()),
            None,
            1000,
        )
        .await
        .unwrap();
    assert_eq!(result2.items.len(), 100);
    assert!(
        result2.done,
        "scan_range must return done=true when no more items exist"
    );
}

/// Same regression test for scan_prefix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_prefix_done_false_at_limit_boundary() {
    use std::borrow::Cow;

    let (kv, _dir) = test_kv().await;
    let table = "artifacts";
    let pk = "test-partition";
    let n = 1100;

    for i in 0..n {
        let sk = format!("pfx/{i:06}");
        let value = rmp_serde::to_vec(&make_record(i as i64, i as i64)).unwrap();
        kv.put(table, Cow::Borrowed(pk), Cow::Borrowed(sk.as_str()), &value)
            .await
            .unwrap();
    }

    let result = kv
        .scan_prefix(table, Cow::Borrowed(pk), Cow::Borrowed("pfx/"), 1000)
        .await
        .unwrap();
    assert_eq!(result.items.len(), 1000);
    assert!(
        !result.done,
        "scan_prefix must return done=false when more items exist beyond the limit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_batched_delete_repo() {
    let (kv, _dir) = test_kv().await;

    let count = 50; // Use smaller count for faster tests

    let config = RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: "deleteme".to_string(),
        kind: depot_core::store::kv::RepoKind::Hosted,
        format_config: depot_core::store::kv::FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: chrono::DateTime::from_timestamp(1000, 0).unwrap(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    };
    service::put_repo(&kv, &config).await.unwrap();
    for i in 0..count {
        let path = format!("file/{i:05}.dat");
        let rec = make_record(i as i64 + 1, i as i64 + 1);
        service::put_artifact(&kv, "deleteme", &path, &rec)
            .await
            .unwrap();
    }

    let existed = service::delete_repo(&kv, "deleteme").await.unwrap();
    assert!(existed);

    assert!(service::get_repo(&kv, "deleteme").await.unwrap().is_none());
}
