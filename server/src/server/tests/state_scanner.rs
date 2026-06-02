// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Tests for the state scanner: repo + store + task change detection and
//! the cleanup sweep's task reap (orphan detection, TTL, and deleting
//! drain).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use chrono::Utc;
use tokio_util::sync::CancellationToken;

use crate::server::config::settings::{Settings, SettingsHandle};
use crate::server::infra::event_bus::{Envelope, EventBus, ModelEvent, Topic};
use crate::server::worker::cleanup::run_cleanup_tick;
use crate::server::worker::state_scanner::run_state_scanner;
use depot_core::service;
use depot_core::service::task as task_svc;
use depot_core::store::kv::{
    FormatConfig, KvStore, RepoConfig, RepoKind, StoreKind, StoreRecord, StoreStatsRecord,
    TaskRecord, TreeEntry, TreeEntryKind, CURRENT_RECORD_VERSION,
};
use depot_core::store_registry::StoreRegistry;
use depot_core::task::{TaskKind, TaskStatus};
use depot_kv_redb::sharded_redb::ShardedRedbKvStore;

async fn test_kv() -> (Arc<dyn KvStore>, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let kv: Arc<dyn KvStore> = Arc::new(
        ShardedRedbKvStore::open(&dir.path().join("kv"), 2, 0)
            .await
            .unwrap(),
    );
    (kv, dir)
}

fn settings_handle() -> Arc<SettingsHandle> {
    Arc::new(SettingsHandle::new(Settings {
        state_scan_interval_ms: 50,
        task_retention_secs: 60,
        ..Settings::default()
    }))
}

fn new_repo(name: &str) -> RepoConfig {
    RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: name.to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: Utc::now(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    }
}

async fn collect_events_for_duration(
    bus: &Arc<EventBus>,
    duration: std::time::Duration,
) -> Vec<Envelope> {
    let mut rx = bus.subscribe();
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            res = rx.recv() => match res {
                Ok(env) => out.push(env),
                Err(_) => break,
            }
        }
    }
    out
}

/// Spawn the scanner, drive KV changes, collect the emitted events.
async fn run_scanner_and_collect<F, Fut>(
    kv: Arc<dyn KvStore>,
    drive: F,
    collect_ms: u64,
) -> Vec<Envelope>
where
    F: FnOnce(Arc<dyn KvStore>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let event_bus = Arc::new(EventBus::new(1024));
    let settings = settings_handle();
    let cancel = CancellationToken::new();

    let scan_trigger = Arc::new(tokio::sync::Notify::new());
    let scanner_handle = tokio::spawn(run_state_scanner(
        kv.clone(),
        event_bus.clone(),
        settings.clone(),
        scan_trigger,
        cancel.clone(),
    ));

    let collect_handle = tokio::spawn({
        let bus = event_bus.clone();
        async move {
            collect_events_for_duration(&bus, std::time::Duration::from_millis(collect_ms)).await
        }
    });

    drive(kv).await;

    let events = collect_handle.await.unwrap();
    cancel.cancel();
    let _ = scanner_handle.await;
    events
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scanner_emits_repo_created_and_stats_updated() {
    let (kv, _dir) = test_kv().await;

    // Seed a store so RepoConfig validation doesn't choke downstream.
    service::put_store(
        kv.as_ref(),
        &StoreRecord {
            schema_version: CURRENT_RECORD_VERSION,
            name: "default".to_string(),
            kind: StoreKind::File {
                root: "/tmp/bogus".to_string(),
                sync: false,
                io_size: 1024,
                direct_io: false,
            },
            created_at: Utc::now(),
        },
    )
    .await
    .unwrap();

    let events = run_scanner_and_collect(
        kv.clone(),
        |kv| async move {
            // Wait one tick so scanner populates its empty baseline.
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;

            // Insert a repo and its root dir entry, then wait for the next tick.
            service::put_repo(kv.as_ref(), &new_repo("alpha"))
                .await
                .unwrap();
            let te = TreeEntry {
                kind: TreeEntryKind::Dir {
                    artifact_count: 7,
                    total_bytes: 100,
                },
                last_modified_at: Utc::now(),
                last_accessed_at: Utc::now(),
            };
            let (table, pk, sk) = depot_core::store::keys::tree_entry_key("alpha", "");
            kv.put(table, pk, sk, &rmp_serde::to_vec_named(&te).unwrap())
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        },
        500,
    )
    .await;

    assert!(events.iter().any(|e| matches!(
        (e.topic, &e.event),
        (Topic::Repos, ModelEvent::RepoCreated { repo }) if repo.name == "alpha"
    )));
    assert!(events.iter().any(|e| matches!(
        (e.topic, &e.event),
        (
            Topic::Repos,
            ModelEvent::RepoStatsUpdated {
                name,
                artifact_count,
                total_bytes,
            }
        ) if name == "alpha" && *artifact_count == 7 && *total_bytes == 100
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scanner_emits_store_stats_updated() {
    let (kv, _dir) = test_kv().await;
    service::put_store(
        kv.as_ref(),
        &StoreRecord {
            schema_version: CURRENT_RECORD_VERSION,
            name: "default".to_string(),
            kind: StoreKind::File {
                root: "/tmp/bogus".to_string(),
                sync: false,
                io_size: 1024,
                direct_io: false,
            },
            created_at: Utc::now(),
        },
    )
    .await
    .unwrap();

    let events = run_scanner_and_collect(
        kv.clone(),
        |kv| async move {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            let stats = StoreStatsRecord {
                blob_count: 42,
                total_bytes: 4096,
                updated_at: Utc::now(),
            };
            service::put_store_stats(kv.as_ref(), "default", &stats)
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        },
        500,
    )
    .await;

    assert!(events.iter().any(|e| matches!(
        (e.topic, &e.event),
        (
            Topic::Stores,
            ModelEvent::StoreStatsUpdated {
                name,
                blob_count,
                total_bytes,
            }
        ) if name == "default" && *blob_count == 42 && *total_bytes == 4096
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scanner_emits_task_created_and_deleted() {
    let (kv, _dir) = test_kv().await;
    let task_uuid = uuid::Uuid::new_v4().to_string();
    let task_uuid_clone = task_uuid.clone();

    let events = run_scanner_and_collect(
        kv.clone(),
        |kv| async move {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            let record = TaskRecord {
                schema_version: CURRENT_RECORD_VERSION,
                id: task_uuid_clone.clone(),
                kind: TaskKind::Check,
                status: TaskStatus::Pending,
                created_at: Utc::now(),
                started_at: None,
                completed_at: None,
                updated_at: Utc::now(),
                progress: Default::default(),
                result: None,
                error: None,
                summary: String::new(),
                owner_instance_id: "other-instance".to_string(),
                deleting: false,
                event_seq: 0,
            };
            task_svc::put_task_summary(kv.as_ref(), &record)
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            task_svc::delete_task_summary(kv.as_ref(), &task_uuid_clone)
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        },
        900,
    )
    .await;

    let expected_id = task_uuid.clone();
    assert!(
        events.iter().any(|e| matches!(
            (e.topic, &e.event),
            (Topic::Tasks, ModelEvent::TaskCreated { task }) if task.id.to_string() == expected_id
        )),
        "expected TaskCreated for {expected_id}",
    );
    assert!(
        events.iter().any(|e| matches!(
            (e.topic, &e.event),
            (Topic::Tasks, ModelEvent::TaskDeleted { id }) if *id == expected_id
        )),
        "expected TaskDeleted for {expected_id}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_reaps_orphaned_task() {
    let (kv, _dir) = test_kv().await;
    let stores: Arc<StoreRegistry> = Arc::new(StoreRegistry::new());

    // Create a Running task whose owner_instance_id is not in the live set
    // (we never register an instance record, so live set is empty → orphan
    // detection is skipped for safety). Register a decoy so the live set is
    // non-empty and the orphan sweep fires.
    use crate::server::worker::cluster;
    cluster::register_instance(kv.as_ref(), "live-instance", "host-live")
        .await
        .unwrap();

    let record = TaskRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: "orphan-task".to_string(),
        kind: TaskKind::Check,
        status: TaskStatus::Running,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        completed_at: None,
        updated_at: Utc::now(),
        progress: Default::default(),
        result: None,
        error: None,
        summary: String::new(),
        owner_instance_id: "dead-instance".to_string(),
        deleting: false,
        event_seq: 0,
    };
    task_svc::put_task_summary(kv.as_ref(), &record)
        .await
        .unwrap();

    run_cleanup_tick(&kv, &stores, &settings_handle())
        .await
        .unwrap();

    let after = task_svc::get_task_summary(kv.as_ref(), "orphan-task")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, TaskStatus::Failed);
    assert!(after.completed_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_drains_deleting_tasks() {
    let (kv, _dir) = test_kv().await;
    let stores: Arc<StoreRegistry> = Arc::new(StoreRegistry::new());

    // Seed a task with deleting=true and a handful of event records.
    let record = TaskRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: "to-reap".to_string(),
        kind: TaskKind::Check,
        status: TaskStatus::Completed,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        completed_at: Some(Utc::now()),
        updated_at: Utc::now(),
        progress: Default::default(),
        result: None,
        error: None,
        summary: String::new(),
        owner_instance_id: "live-instance".to_string(),
        deleting: true,
        event_seq: 3,
    };
    task_svc::put_task_summary(kv.as_ref(), &record)
        .await
        .unwrap();
    for seq in 0..3 {
        task_svc::append_task_event(kv.as_ref(), "to-reap", seq, format!("m-{seq}"))
            .await
            .unwrap();
    }

    run_cleanup_tick(&kv, &stores, &settings_handle())
        .await
        .unwrap();

    assert!(task_svc::get_task_summary(kv.as_ref(), "to-reap")
        .await
        .unwrap()
        .is_none());
    let events = task_svc::list_task_events_since(kv.as_ref(), "to-reap", 0, 10)
        .await
        .unwrap();
    assert!(events.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_ttl_marks_old_completed_tasks_deleting() {
    let (kv, _dir) = test_kv().await;
    let stores: Arc<StoreRegistry> = Arc::new(StoreRegistry::new());

    // A completed task finished 2 hours ago, with a 60-second TTL in settings.
    let old = Utc::now() - chrono::Duration::hours(2);
    let record = TaskRecord {
        schema_version: CURRENT_RECORD_VERSION,
        id: "old-task".to_string(),
        kind: TaskKind::Check,
        status: TaskStatus::Completed,
        created_at: old,
        started_at: Some(old),
        completed_at: Some(old),
        updated_at: old,
        progress: Default::default(),
        result: None,
        error: None,
        summary: String::new(),
        owner_instance_id: "live-instance".to_string(),
        deleting: false,
        event_seq: 0,
    };
    task_svc::put_task_summary(kv.as_ref(), &record)
        .await
        .unwrap();

    run_cleanup_tick(&kv, &stores, &settings_handle())
        .await
        .unwrap();

    // First tick marks deleting=true.
    let after_first = task_svc::get_task_summary(kv.as_ref(), "old-task")
        .await
        .unwrap()
        .unwrap();
    assert!(after_first.deleting);

    // Second tick drains it.
    run_cleanup_tick(&kv, &stores, &settings_handle())
        .await
        .unwrap();
    assert!(task_svc::get_task_summary(kv.as_ref(), "old-task")
        .await
        .unwrap()
        .is_none());
}

/// Regression: when a worker calls `update_summary` immediately followed by
/// `mark_completed`, the scanner must converge to `Completed` and never
/// leave the materialized state stuck on `Running` with the new summary.
///
/// Background: this used to fail because `TaskManager` published `TaskUpdated`
/// in-band on `mark_completed`, racing with the scanner's stale-snapshot
/// publish from a `list_tasks` call taken between `update_summary` and
/// `mark_completed`. Now that the scanner is the sole publisher, the worst
/// case is a one-tick flicker that self-heals.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scanner_converges_to_completed_after_update_summary_then_mark_completed() {
    use crate::server::infra::task::TaskManager;
    use depot_core::task::{CheckResult, TaskResult};

    let (kv, _dir) = test_kv().await;
    let mgr = Arc::new(TaskManager::new(kv.clone(), "test-instance".to_string()));

    // Pre-create the task so the scanner sees it before we drive transitions.
    let (task_id, _tx, _cancel) = mgr.create(TaskKind::Check).await;
    mgr.mark_running(task_id).await;

    let mgr_clone = mgr.clone();
    let events = run_scanner_and_collect(
        kv.clone(),
        |_kv| async move {
            // Let the scanner take its baseline snapshot.
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;

            // Reproduce the racey worker tail: update_summary then a fast
            // mark_completed. The scanner's tick may snapshot the KV between
            // these two writes.
            mgr_clone
                .update_summary(task_id, "Complete: 100 passed".to_string())
                .await;
            mgr_clone
                .mark_completed(task_id, TaskResult::Check(CheckResult::default()))
                .await;

            // Give the scanner several ticks to converge.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        },
        700,
    )
    .await;

    let task_id_str = task_id.to_string();
    let last_status_for_task = events
        .iter()
        .filter_map(|e| match (&e.topic, &e.event) {
            (Topic::Tasks, ModelEvent::TaskCreated { task })
            | (Topic::Tasks, ModelEvent::TaskUpdated { task })
                if task.id.to_string() == task_id_str =>
            {
                Some(task.status)
            }
            _ => None,
        })
        .next_back();

    assert_eq!(
        last_status_for_task,
        Some(TaskStatus::Completed),
        "scanner should converge to Completed; got {:?}",
        last_status_for_task
    );

    // KV is the source of truth — verify it agrees.
    let final_record = task_svc::get_task_summary(kv.as_ref(), &task_id_str)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_record.status, TaskStatus::Completed);
    assert_eq!(final_record.summary, "Complete: 100 passed");
}
