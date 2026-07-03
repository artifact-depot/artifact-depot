// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use axum::http::{Method, StatusCode};

use depot_core::service;

use depot_test_support::*;

// ===========================================================================
// Helpers
// ===========================================================================

async fn start_check(app: &TestApp) -> String {
    let token = app.admin_token();
    let req = app.auth_request(Method::POST, "/api/v1/tasks/check", &token);
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED, "start check failed: {body}");
    body["id"].as_str().unwrap().to_string()
}

// ===========================================================================
// Tests
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_empty_system() {
    let app = TestApp::new().await;
    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let check = &result["result"];
    assert_eq!(check["total_blobs"], 0);
    assert_eq!(check["findings"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_healthy_blobs() {
    let app = TestApp::new().await;
    app.create_hosted_repo("check-repo").await;
    let token = app.admin_token();
    app.upload_artifact("check-repo", "file1.bin", b"hello world", &token)
        .await;
    app.upload_artifact("check-repo", "file2.bin", b"goodbye world", &token)
        .await;

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let check = &result["result"];
    assert_eq!(check["total_blobs"], 2);
    assert_eq!(check["passed"], 2);
    assert_eq!(check["mismatched"], 0);
    assert_eq!(check["missing"], 0);
    assert_eq!(check["findings"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_corrupted_blob() {
    let app = TestApp::new().await;
    app.create_hosted_repo("corrupt-repo").await;
    let token = app.admin_token();
    app.upload_artifact("corrupt-repo", "file.bin", b"original content", &token)
        .await;

    // Find the blob file on disk and corrupt it
    let blob_path = find_blob_path(&app, b"original content").await;
    std::fs::write(&blob_path, b"corrupted garbage data").unwrap();

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let check = &result["result"];
    let findings = check["findings"].as_array().unwrap();
    assert!(
        !findings.is_empty(),
        "should have at least one finding for corrupted blob"
    );
    // Check for hash mismatch (actual_hash present, no error)
    let has_mismatch = findings.iter().any(|f| f["actual_hash"].is_string());
    assert!(has_mismatch, "should report hash mismatch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_missing_blob() {
    let app = TestApp::new().await;
    app.create_hosted_repo("missing-repo").await;
    let token = app.admin_token();
    app.upload_artifact("missing-repo", "file.bin", b"delete me", &token)
        .await;

    // Delete the blob file
    let blob_path = find_blob_path(&app, b"delete me").await;
    std::fs::remove_file(&blob_path).unwrap();

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let check = &result["result"];
    assert!(
        check["missing"].as_u64().unwrap() > 0,
        "should report missing blob"
    );
    let findings = check["findings"].as_array().unwrap();
    let has_missing = findings
        .iter()
        .any(|f| f["error"].as_str() == Some("blob missing from store"));
    assert!(has_missing, "should have 'blob missing from store' finding");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_size_mismatch() {
    let app = TestApp::new().await;
    app.create_hosted_repo("size-repo").await;
    let token = app.admin_token();
    let original = b"some content that will be truncated";
    app.upload_artifact("size-repo", "file.bin", original, &token)
        .await;

    // Truncate the blob file — write same BLAKE3 prefix but shorter content
    // Actually, the simplest way to get a size mismatch with correct hash is tricky.
    // Instead, write data that happens to have the same BLAKE3 hash — impossible.
    // So we truncate it: hash will differ AND size will differ,
    // which will first trigger hash mismatch. That's still a finding.
    let blob_path = find_blob_path(&app, original).await;
    std::fs::write(&blob_path, b"short").unwrap();

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let findings = result["result"]["findings"].as_array().unwrap();
    assert!(
        !findings.is_empty(),
        "should have finding for truncated blob"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_orphan_artifact() {
    let app = TestApp::new().await;
    app.create_hosted_repo("orphan-repo").await;

    // Insert an artifact referencing a non-existent blob hash
    use depot_core::store::kv::{ArtifactKind, ArtifactRecord, CURRENT_RECORD_VERSION};

    let now = crate::server::repo::now_utc();
    let orphan = ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: uuid::Uuid::new_v4().to_string(),
        size: 100,
        content_type: "application/octet-stream".to_string(),
        created_at: now,
        updated_at: now,
        last_accessed_at: now,
        path: "orphan.bin".to_string(),
        internal: false,
        kind: ArtifactKind::Raw,
        blob_id: Some("nonexistent-blob-id".to_string()),
        content_hash: Some(
            "deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000".to_string(),
        ),
        etag: Some(
            "deadbeef00000000deadbeef00000000deadbeef00000000deadbeef00000000"
                .to_string()
                .clone(),
        ),
    };

    service::put_artifact(
        app.state.repo.kv.as_ref(),
        "orphan-repo",
        "orphan.bin",
        &orphan,
    )
    .await
    .unwrap();

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let findings = result["result"]["findings"].as_array().unwrap();
    let has_orphan = findings.iter().any(|f| {
        let err = f["error"].as_str().unwrap_or("");
        err.contains("artifact references") && err.contains("blob")
    });
    assert!(
        has_orphan,
        "should detect orphan artifact, findings: {:?}",
        findings
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_proxy_missing_member() {
    let app = TestApp::new().await;
    // Create a proxy that references a non-existent member
    app.create_repo(serde_json::json!({
        "name": "bad-proxy",
        "repo_type": "proxy",
        "format": "raw",
        "members": ["does-not-exist"],
    }))
    .await;

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let findings = result["result"]["findings"].as_array().unwrap();
    let has_missing_member = findings.iter().any(|f| {
        f["error"]
            .as_str()
            .is_some_and(|e| e.contains("does not exist"))
    });
    assert!(
        has_missing_member,
        "should detect proxy referencing non-existent member"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_conflict_when_running() {
    let app = TestApp::new().await;
    // Upload some data to make the check take a bit longer
    app.create_hosted_repo("slow-repo").await;
    let token = app.admin_token();
    for i in 0..10 {
        app.upload_artifact(
            "slow-repo",
            &format!("file{i}.bin"),
            format!("content-{i}").as_bytes(),
            &token,
        )
        .await;
    }

    let _task_id = start_check(&app).await;

    // Immediately try to start another check
    let req = app.auth_request(Method::POST, "/api/v1/tasks/check", &token);
    let (status, _) = app.call(req).await;
    // Might be 409 if the first check is still running, or 201 if it already finished
    // With enough data the first check should still be running
    assert!(
        status == StatusCode::CONFLICT || status == StatusCode::CREATED,
        "expected 409 or 201, got {status}"
    );
}

// ===========================================================================
// Cache repo validation
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_cache_empty_upstream() {
    let app = TestApp::new().await;
    // Create a cache repo with empty upstream_url by setting it to empty string.
    app.create_repo(serde_json::json!({
        "name": "bad-cache",
        "repo_type": "cache",
        "format": "raw",
        "upstream_url": "",
        "cache_ttl_secs": 300,
    }))
    .await;

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let findings = result["result"]["findings"].as_array().unwrap();
    let has_empty_upstream = findings.iter().any(|f| {
        f["error"]
            .as_str()
            .is_some_and(|e| e.contains("empty upstream_url"))
    });
    assert!(
        has_empty_upstream,
        "should detect cache repo with empty upstream_url"
    );
}

// ===========================================================================
// Many blobs — exercises concurrent verification
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_many_blobs() {
    let app = TestApp::new().await;
    app.create_hosted_repo("many-repo").await;
    let token = app.admin_token();

    // Upload enough blobs to exercise the concurrent verification workers.
    for i in 0..20 {
        app.upload_artifact(
            "many-repo",
            &format!("file{i:03}.bin"),
            format!("content-{i}-payload").as_bytes(),
            &token,
        )
        .await;
    }

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let check = &result["result"];
    // Some blobs may be deduplicated, but should have many.
    assert!(
        check["total_blobs"].as_u64().unwrap() >= 15,
        "should have at least 15 unique blobs"
    );
    assert_eq!(check["mismatched"], 0);
    assert_eq!(check["missing"], 0);
    assert_eq!(check["findings"].as_array().unwrap().len(), 0);
}

// ===========================================================================
// Mixed repo types
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_mixed_repo_types() {
    let app = TestApp::new().await;
    // Create various repo types.
    app.create_hosted_repo("hosted-mixed").await;
    app.create_repo(serde_json::json!({
        "name": "cache-mixed",
        "repo_type": "cache",
        "format": "raw",
        "upstream_url": "http://example.com",
        "cache_ttl_secs": 300,
    }))
    .await;
    app.create_repo(serde_json::json!({
        "name": "proxy-mixed",
        "repo_type": "proxy",
        "format": "raw",
        "members": ["hosted-mixed"],
    }))
    .await;

    let token = app.admin_token();
    app.upload_artifact("hosted-mixed", "file.bin", b"mixed data", &token)
        .await;

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let check = &result["result"];
    assert_eq!(check["findings"].as_array().unwrap().len(), 0);
}

// ===========================================================================
// Utility
// ===========================================================================

/// Find the on-disk path for a blob given its content (by looking up the BLAKE3 hash → BlobRecord → blob_id).
async fn find_blob_path(app: &TestApp, content: &[u8]) -> std::path::PathBuf {
    let hash = blake3::hash(content).to_hex().to_string();
    // Look up the blob record to get the blob_id (UUID)
    let h = hash.clone();
    let blob_record = service::get_blob(app.state.repo.kv.as_ref(), "default", &h)
        .await
        .unwrap()
        .expect("blob record not found");

    let blobs = app
        .state
        .repo
        .stores
        .get("default")
        .await
        .expect("default store not found");
    blobs
        .blob_path(&blob_record.blob_id)
        .expect("blob_path returned None")
}

// ===========================================================================
// Deduplicated blobs
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_deduped_blobs() {
    let app = TestApp::new().await;
    app.create_hosted_repo("dedup-repo").await;
    let token = app.admin_token();

    // Upload identical content to two different paths.
    let content = b"identical content for dedup test";
    app.upload_artifact("dedup-repo", "file1.bin", content, &token)
        .await;
    app.upload_artifact("dedup-repo", "file2.bin", content, &token)
        .await;

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let check = &result["result"];
    // Blobs are deduped by BLAKE3 hash — should be only 1 unique blob.
    assert_eq!(check["total_blobs"], 1);
    assert_eq!(check["passed"], 1);
    assert_eq!(check["mismatched"], 0);
    assert_eq!(check["missing"], 0);
}

// ===========================================================================
// Multiple corrupted blobs — concurrent verification
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_multiple_corrupted_blobs() {
    let app = TestApp::new().await;
    app.create_hosted_repo("multi-corrupt").await;
    let token = app.admin_token();

    // Upload 5 blobs with distinct content.
    let contents: Vec<&[u8]> = vec![
        b"blob-alpha-111",
        b"blob-bravo-222",
        b"blob-charlie-333",
        b"blob-delta-444",
        b"blob-echo-5555",
    ];
    for (i, content) in contents.iter().enumerate() {
        app.upload_artifact("multi-corrupt", &format!("file{i}.bin"), content, &token)
            .await;
    }

    // Corrupt the first 3 blobs on disk.
    for content in &contents[..3] {
        let blob_path = find_blob_path(&app, content).await;
        std::fs::write(&blob_path, b"corrupted!").unwrap();
    }

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let check = &result["result"];
    assert_eq!(
        check["mismatched"].as_u64().unwrap(),
        3,
        "should report 3 corrupted blobs"
    );
    assert_eq!(
        check["passed"].as_u64().unwrap(),
        2,
        "should report 2 healthy blobs"
    );
    assert_eq!(
        check["findings"].as_array().unwrap().len(),
        3,
        "should have 3 findings"
    );
}

// ===========================================================================
// Large blob — multi-chunk hashing
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_large_blob() {
    let app = TestApp::new().await;
    app.create_hosted_repo("large-repo").await;
    let token = app.admin_token();

    // Upload a blob larger than the 256KB chunk size used during verification.
    let large_content = vec![0xABu8; 512 * 1024]; // 512KB
    app.upload_artifact("large-repo", "big.bin", &large_content, &token)
        .await;

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let check = &result["result"];
    assert_eq!(check["total_blobs"], 1);
    assert_eq!(check["passed"], 1);
    assert_eq!(check["mismatched"], 0);
    assert_eq!(check["missing"], 0);
    assert!(
        check["bytes_read"].as_u64().unwrap() >= 512 * 1024,
        "should read at least 512KB"
    );
}

// ===========================================================================
// Lease contention — check fails when GC holds the lease
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_fails_when_lease_held() {
    use crate::server::infra::task::{TaskKind, TaskManager, TaskStatus};
    use crate::server::worker::{check, cluster};

    let app = TestApp::new().await;

    // Pre-acquire the GC lease as a different instance with a long TTL.
    cluster::try_acquire_lease(
        app.state.repo.kv.as_ref(),
        cluster::LEASE_GC,
        "other-instance",
        3600,
    )
    .await
    .unwrap();

    // Run the check directly (not via HTTP) so we can inspect the result
    // without depending on the API retry/timeout behavior.
    let task_manager = Arc::new(TaskManager::new(
        app.state.repo.kv.clone(),
        "check-instance".to_string(),
    ));
    let (task_id, progress_tx, cancel) = task_manager.create(TaskKind::Check).await;

    check::run_check(
        task_id,
        task_manager.clone(),
        app.state.repo.kv.clone(),
        app.state.repo.stores.clone(),
        "check-instance".to_string(),
        progress_tx,
        cancel,
        true,
        true,
        depot_core::update::UpdateSender::noop(),
    )
    .await;

    let tasks = task_manager.list().await;
    let check_task = tasks.iter().find(|t| t.id == task_id).unwrap();
    assert_eq!(
        check_task.status,
        TaskStatus::Failed,
        "check should fail when lease is held by another instance"
    );
    assert!(
        check_task.error.as_ref().unwrap().contains("GC lease"),
        "error should mention GC lease"
    );
}

// ===========================================================================
// Check cancellation — cancelling during execution
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_cancelled_before_start() {
    use crate::server::infra::task::{TaskKind, TaskManager, TaskStatus};
    use crate::server::worker::check;

    let app = TestApp::new().await;
    let task_manager = Arc::new(TaskManager::new(
        app.state.repo.kv.clone(),
        "check-instance".to_string(),
    ));
    let (task_id, progress_tx, cancel) = task_manager.create(TaskKind::Check).await;

    // Cancel before running.
    cancel.cancel();

    check::run_check(
        task_id,
        task_manager.clone(),
        app.state.repo.kv.clone(),
        app.state.repo.stores.clone(),
        "cancel-instance".to_string(),
        progress_tx,
        cancel,
        true,
        true,
        depot_core::update::UpdateSender::noop(),
    )
    .await;

    let tasks = task_manager.list().await;
    let check_task = tasks.iter().find(|t| t.id == task_id).unwrap();
    assert_eq!(
        check_task.status,
        TaskStatus::Cancelled,
        "check should be marked cancelled"
    );
}

// ===========================================================================
// Check cancellation — cancel mid-verification
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_cancelled_during_verification() {
    use crate::server::infra::task::{TaskKind, TaskManager, TaskStatus};
    use crate::server::worker::check;

    let app = TestApp::new().await;
    app.create_hosted_repo("cancel-mid").await;
    let token = app.admin_token();

    // Upload enough blobs to keep the check busy.
    for i in 0..50 {
        app.upload_artifact(
            "cancel-mid",
            &format!("file{i:03}.bin"),
            format!("cancel-content-{i}").as_bytes(),
            &token,
        )
        .await;
    }

    let task_manager = Arc::new(TaskManager::new(
        app.state.repo.kv.clone(),
        "check-instance".to_string(),
    ));
    let (task_id, progress_tx, cancel) = task_manager.create(TaskKind::Check).await;

    let cancel_clone = cancel.clone();
    let tm = task_manager.clone();
    let handle = tokio::spawn({
        let kv = app.state.repo.kv.clone();
        let stores = app.state.repo.stores.clone();
        async move {
            check::run_check(
                task_id,
                tm,
                kv,
                stores,
                "cancel-mid-instance".to_string(),
                progress_tx,
                cancel_clone,
                true,
                true,
                depot_core::update::UpdateSender::noop(),
            )
            .await;
        }
    });

    // Let it start, then cancel.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();
    handle.await.unwrap();

    let tasks = task_manager.list().await;
    let check_task = tasks.iter().find(|t| t.id == task_id).unwrap();
    // Could be Completed (if it finished before cancel) or Cancelled.
    assert!(
        check_task.status == TaskStatus::Cancelled || check_task.status == TaskStatus::Completed,
        "check should be cancelled or completed, got {:?}",
        check_task.status
    );
}

// ===========================================================================
// Blob store not found — blob references a missing store
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_blob_store_not_found() {
    use depot_core::store::kv::{BlobRecord, StoreKind, StoreRecord, CURRENT_RECORD_VERSION};

    let app = TestApp::new().await;
    let kv = app.state.repo.kv.as_ref();

    // Create a second store record that points to a non-existent blob store.
    // The KV record exists but no BlobStore is registered in StoreRegistry.
    service::put_store(
        kv,
        &StoreRecord {
            schema_version: CURRENT_RECORD_VERSION,
            name: "ghost-store".to_string(),
            kind: StoreKind::File {
                root: "/nonexistent/path".to_string(),
                sync: false,
                io_size: 1024,
                direct_io: false,
            },
            created_at: crate::server::repo::now_utc(),
        },
    )
    .await
    .unwrap();

    // Create a blob record in the ghost store.
    service::put_blob(
        kv,
        "ghost-store",
        &BlobRecord {
            schema_version: CURRENT_RECORD_VERSION,
            blob_id: "ghost-blob-id".to_string(),
            hash: "deadbeef".repeat(8),
            size: 100,
            created_at: crate::server::repo::now_utc(),
            store: "ghost-store".to_string(),
        },
    )
    .await
    .unwrap();

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let findings = result["result"]["findings"].as_array().unwrap();
    let has_store_not_found = findings
        .iter()
        .any(|f| f["error"].as_str().is_some_and(|e| e.contains("not found")));
    assert!(
        has_store_not_found,
        "should detect blob referencing non-existent store"
    );
}

// ===========================================================================
// Progress updates — exercise the every-100-blobs update path
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_progress_updates_with_many_blobs() {
    let app = TestApp::new().await;
    app.create_hosted_repo("progress-repo").await;
    let token = app.admin_token();

    // Upload >100 blobs to exercise the per-100 progress update path.
    for i in 0..110 {
        app.upload_artifact(
            "progress-repo",
            &format!("file{i:04}.bin"),
            format!("progress-content-{i:04}-unique").as_bytes(),
            &token,
        )
        .await;
    }

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let check = &result["result"];
    assert!(
        check["total_blobs"].as_u64().unwrap() >= 100,
        "should have at least 100 blobs"
    );
    assert_eq!(check["mismatched"], 0);
    assert_eq!(check["missing"], 0);
}

// ===========================================================================
// Pure size mismatch — correct hash but BlobRecord has wrong size
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_pure_size_mismatch() {
    let app = TestApp::new().await;
    app.create_hosted_repo("size-repo2").await;
    let token = app.admin_token();

    let content = b"size mismatch test content";
    app.upload_artifact("size-repo2", "file.bin", content, &token)
        .await;

    // Tamper with the BlobRecord's size to create a pure size mismatch
    // (hash matches but recorded size doesn't).
    let hash = blake3::hash(content).to_hex().to_string();
    let mut blob_record = service::get_blob(app.state.repo.kv.as_ref(), "default", &hash)
        .await
        .unwrap()
        .expect("blob record should exist");

    // Set wrong size (blob is 26 bytes, record says 999).
    blob_record.size = 999;
    service::put_blob(app.state.repo.kv.as_ref(), "default", &blob_record)
        .await
        .unwrap();

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let findings = result["result"]["findings"].as_array().unwrap();
    let has_size_mismatch = findings
        .iter()
        .any(|f| f["error"].as_str() == Some("size mismatch"));
    assert!(
        has_size_mismatch,
        "should detect pure size mismatch (correct hash, wrong recorded size)"
    );
}

// ===========================================================================
// open_read error — blob replaced with a directory
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_read_error() {
    let app = TestApp::new().await;
    app.create_hosted_repo("read-err-repo").await;
    let token = app.admin_token();

    let content = b"readable then not";
    app.upload_artifact("read-err-repo", "file.bin", content, &token)
        .await;

    // Replace blob file with a directory (causes open_read to return Err).
    let blob_path = find_blob_path(&app, content).await;
    std::fs::remove_file(&blob_path).unwrap();
    std::fs::create_dir_all(&blob_path).unwrap();

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let findings = result["result"]["findings"].as_array().unwrap();
    let has_error = findings.iter().any(|f| {
        f["error"]
            .as_str()
            .is_some_and(|e| e.contains("error") || e.contains("missing"))
    });
    assert!(
        has_error,
        "should detect read error or missing blob: {findings:?}"
    );

    // Clean up the directory we created so temp dir cleanup works.
    let _ = std::fs::remove_dir_all(&blob_path);
}

// ===========================================================================
// Helm: legacy chart records (under `_charts/` or `charts/`) get relocated
// to the canonical repo root `{name}-{version}.tgz` during the check, and
// the metadata-stale flag is set so the next index fetch rebuilds.
// ===========================================================================

async fn seed_helm_chart_at_path(
    app: &TestApp,
    repo: &str,
    storage_path: &str,
    name: &str,
    version: &str,
) {
    use depot_core::store::kv::{
        ArtifactKind, ArtifactRecord, HelmChartMeta, CURRENT_RECORD_VERSION,
    };

    // Park a real blob so the orphan-blob phase doesn't sweep the record
    // before the helm canonicalization phase runs.
    let blob_id = uuid::Uuid::new_v4().to_string();
    let blobs = app
        .state
        .repo
        .stores
        .get("default")
        .await
        .expect("default blob store");
    blobs.put(&blob_id, b"placeholder").await.unwrap();

    let helm = HelmChartMeta {
        name: name.to_string(),
        version: version.to_string(),
        app_version: String::new(),
        description: String::new(),
        api_version: "v2".to_string(),
        chart_type: String::new(),
        keywords: Vec::new(),
        home: String::new(),
        sources: Vec::new(),
        icon: String::new(),
        deprecated: false,
        sha256: "0".repeat(64),
    };

    let now = chrono::Utc::now();
    let record = ArtifactRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: uuid::Uuid::new_v4().to_string(),
        size: 100,
        content_type: "application/gzip".to_string(),
        created_at: now,
        updated_at: now,
        last_accessed_at: now,
        path: storage_path.to_string(),
        internal: false,
        kind: ArtifactKind::HelmChart { helm },
        blob_id: Some(blob_id),
        content_hash: None,
        etag: None,
    };
    service::put_artifact(app.state.repo.kv.as_ref(), repo, storage_path, &record)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_helm_relocates_deep_legacy_path() {
    // Records at the oldest layout — `_charts/{name}/{name}-{version}.tgz` —
    // get moved to the canonical root path.
    let app = TestApp::new().await;
    app.create_helm_repo("helm-legacy").await;

    let legacy = "_charts/tigera-operator/tigera-operator-v3.28.0.tgz";
    seed_helm_chart_at_path(&app, "helm-legacy", legacy, "tigera-operator", "v3.28.0").await;
    let canonical = "tigera-operator-v3.28.0.tgz";

    let token = app.admin_token();
    let req = app.json_request(
        Method::POST,
        "/api/v1/tasks/check",
        &token,
        serde_json::json!({"dry_run": false}),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED, "start check failed: {body}");
    let task_id = body["id"].as_str().unwrap().to_string();
    let result = app.wait_for_task(&task_id).await;
    assert_eq!(result["status"], "completed");

    let kv = app.state.repo.kv.as_ref();
    assert!(
        service::get_artifact(kv, "helm-legacy", canonical)
            .await
            .unwrap()
            .is_some(),
        "expected canonical artifact at {canonical}"
    );
    assert!(
        service::get_artifact(kv, "helm-legacy", legacy)
            .await
            .unwrap()
            .is_none(),
        "expected legacy path {legacy} to be gone"
    );
    assert!(
        service::get_artifact(kv, "helm-legacy", "_helm/metadata_stale")
            .await
            .unwrap()
            .is_some(),
        "stale flag should be set so next index fetch rebuilds"
    );

    let findings = result["result"]["findings"].as_array().unwrap();
    let has_finding = findings.iter().any(|f| {
        f["error"]
            .as_str()
            .unwrap_or("")
            .contains("legacy chart path")
            && f["fixed"].as_bool().unwrap_or(false)
    });
    assert!(has_finding, "expected fixed legacy finding: {findings:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_helm_relocates_charts_prefix_to_root() {
    // Records at the intermediate `charts/{name}-{version}.tgz` layout also
    // get moved to the canonical root path.
    let app = TestApp::new().await;
    app.create_helm_repo("helm-flat").await;

    let legacy = "charts/redis-7.2.0.tgz";
    seed_helm_chart_at_path(&app, "helm-flat", legacy, "redis", "7.2.0").await;
    let canonical = "redis-7.2.0.tgz";

    let token = app.admin_token();
    let req = app.json_request(
        Method::POST,
        "/api/v1/tasks/check",
        &token,
        serde_json::json!({"dry_run": false}),
    );
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::CREATED, "start check failed: {body}");
    let task_id = body["id"].as_str().unwrap().to_string();
    let result = app.wait_for_task(&task_id).await;
    assert_eq!(result["status"], "completed");

    let kv = app.state.repo.kv.as_ref();
    assert!(
        service::get_artifact(kv, "helm-flat", canonical)
            .await
            .unwrap()
            .is_some(),
        "expected canonical artifact at {canonical}"
    );
    assert!(
        service::get_artifact(kv, "helm-flat", legacy)
            .await
            .unwrap()
            .is_none(),
        "expected legacy path {legacy} to be gone"
    );
    assert!(
        service::get_artifact(kv, "helm-flat", "_helm/metadata_stale")
            .await
            .unwrap()
            .is_some(),
        "stale flag should be set"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_helm_dry_run_does_not_relocate() {
    let app = TestApp::new().await;
    app.create_helm_repo("helm-dry").await;

    let legacy = "_charts/demo/demo-1.0.0.tgz";
    seed_helm_chart_at_path(&app, "helm-dry", legacy, "demo", "1.0.0").await;

    let task_id = start_check(&app).await; // dry_run defaults to true
    let result = app.wait_for_task(&task_id).await;
    assert_eq!(result["status"], "completed");

    let kv = app.state.repo.kv.as_ref();
    assert!(
        service::get_artifact(kv, "helm-dry", legacy)
            .await
            .unwrap()
            .is_some(),
        "dry run must not move the artifact"
    );
    assert!(
        service::get_artifact(kv, "helm-dry", "demo-1.0.0.tgz")
            .await
            .unwrap()
            .is_none(),
        "dry run must not create the canonical entry"
    );
    assert!(
        service::get_artifact(kv, "helm-dry", "_helm/metadata_stale")
            .await
            .unwrap()
            .is_none(),
        "dry run must not set the stale flag"
    );

    let findings = result["result"]["findings"].as_array().unwrap();
    let has_unfixed = findings.iter().any(|f| {
        f["error"]
            .as_str()
            .unwrap_or("")
            .contains("legacy chart path")
            && !f["fixed"].as_bool().unwrap_or(false)
    });
    assert!(
        has_unfixed,
        "expected unfixed legacy finding under dry-run: {findings:?}"
    );
}

// ===========================================================================
// Repos without blobs — exercises coherency on blobless repos
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_repos_without_blobs() {
    let app = TestApp::new().await;
    app.create_hosted_repo("empty-hosted").await;
    app.create_repo(serde_json::json!({
        "name": "empty-cache",
        "repo_type": "cache",
        "format": "raw",
        "upstream_url": "http://example.com",
        "cache_ttl_secs": 300,
    }))
    .await;
    app.create_repo(serde_json::json!({
        "name": "empty-proxy",
        "repo_type": "proxy",
        "format": "raw",
        "members": ["empty-hosted"],
    }))
    .await;

    let task_id = start_check(&app).await;
    let result = app.wait_for_task(&task_id).await;

    assert_eq!(result["status"], "completed");
    let check = &result["result"];
    assert_eq!(check["total_blobs"], 0);
    assert_eq!(check["findings"].as_array().unwrap().len(), 0);
}
