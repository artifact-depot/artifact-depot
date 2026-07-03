// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for shared Hosted/Cache/Proxy repo infrastructure.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use chrono::DateTime;
use futures::StreamExt;
use tokio::io::AsyncReadExt;

use depot_blob_file::file_blob::FileBlobStore;
use depot_core::inflight::InflightMap;
use depot_core::repo::cache::CacheRepo;
use depot_core::repo::hosted::HostedRepo;
use depot_core::repo::proxy::ProxyRepo;
use depot_core::repo::{ArtifactData, IngestionCtx, RawRepo};
use depot_core::service;
use depot_core::store::blob::BlobStore;
use depot_core::store::kv::{
    FormatConfig, KvStore, Pagination, RepoConfig, RepoKind, CURRENT_RECORD_VERSION,
};
use depot_core::update::UpdateSender;
use depot_kv_redb::sharded_redb::ShardedRedbKvStore;

/// Create a `TempDir` for test storage.
fn test_tempdir() -> tempfile::TempDir {
    tempfile::TempDir::new().expect("failed to create temp dir")
}

/// Read all bytes from an ArtifactData into a Vec for test assertions.
async fn read_all(data: ArtifactData) -> Vec<u8> {
    match data {
        ArtifactData::Reader(mut reader) => {
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).await.unwrap();
            buf
        }
        ArtifactData::Stream(mut stream) => {
            let mut buf = Vec::new();
            while let Some(result) = stream.next().await {
                let chunk = result.unwrap();
                buf.extend_from_slice(&chunk);
            }
            buf
        }
    }
}

async fn test_kv() -> (Arc<dyn KvStore>, Arc<dyn BlobStore>, tempfile::TempDir) {
    let dir = test_tempdir();
    let kv: Arc<dyn KvStore> = Arc::new(
        ShardedRedbKvStore::open(&dir.path().join("kv"), 2, 0)
            .await
            .unwrap(),
    );
    let blobs: Arc<dyn BlobStore> =
        Arc::new(FileBlobStore::new(&dir.path().join("blobs"), false, 1_048_576, false).unwrap());
    (kv, blobs, dir)
}

fn make_hosted_repo(name: &str) -> RepoConfig {
    RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: name.to_string(),
        kind: RepoKind::Hosted,
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: DateTime::from_timestamp(1000, 0).unwrap(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    }
}

fn make_cache_repo(name: &str, url: &str, ttl: u64) -> RepoConfig {
    RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: name.to_string(),
        kind: RepoKind::Cache {
            upstream_url: url.to_string(),
            cache_ttl_secs: ttl,
            upstream_auth: None,
        },
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: DateTime::from_timestamp(1000, 0).unwrap(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    }
}

fn make_proxy_repo(name: &str, members: Vec<String>) -> RepoConfig {
    RepoConfig {
        schema_version: CURRENT_RECORD_VERSION,
        name: name.to_string(),
        kind: RepoKind::Proxy {
            members,
            write_member: None,
        },
        format_config: FormatConfig::Raw {
            content_disposition: None,
        },
        store: "default".to_string(),
        created_at: DateTime::from_timestamp(1000, 0).unwrap(),
        cleanup_max_unaccessed_days: None,
        cleanup_max_age_days: None,
        deleting: false,
    }
}

// ============================
// HostedRepo tests
// ============================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_put_and_get() {
    let (kv, blobs, _dir) = test_kv().await;
    let repo = HostedRepo {
        name: "hosted1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    repo.put("file.txt", "text/plain", b"hello world")
        .await
        .unwrap();
    let content = repo.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(content.data).await, b"hello world");
    assert_eq!(content.record.content_type, "text/plain");
    assert_eq!(content.record.size, 11);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_get_nonexistent() {
    let (kv, blobs, _dir) = test_kv().await;
    let repo = HostedRepo {
        name: "hosted1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    assert!(repo.get("nope.txt").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_put_replaces_existing() {
    let (kv, blobs, _dir) = test_kv().await;
    let repo = HostedRepo {
        name: "hosted1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    repo.put("file.txt", "text/plain", b"v1").await.unwrap();
    repo.put("file.txt", "text/plain", b"v2-updated")
        .await
        .unwrap();
    let content = repo.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(content.data).await, b"v2-updated");
    assert_eq!(content.record.size, 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_delete_existing() {
    let (kv, blobs, _dir) = test_kv().await;
    let repo = HostedRepo {
        name: "hosted1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    repo.put("file.txt", "text/plain", b"hello").await.unwrap();
    assert!(repo.delete("file.txt").await.unwrap());
    assert!(repo.get("file.txt").await.unwrap().is_none());
}

// Regression: deleting an artifact must also prune its browse-tree entry
// (`TABLE_DIR_ENTRIES`). The UI listing reads that table, so a delete that
// drops only the `ArtifactRecord` leaves a "ghost" row whose backing record is
// gone (404 on open). Covers both the single delete (`delete_artifact`) and the
// bulk paired delete (`delete_artifacts_paired_batch`, used by retention/GC).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_prunes_browse_tree_entry_no_ghost() {
    let (kv, blobs, _dir) = test_kv().await;
    let repo = HostedRepo {
        name: "hosted1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    repo.put("d/a.txt", "text/plain", b"a").await.unwrap();
    repo.put("d/b.txt", "text/plain", b"b").await.unwrap();

    // Both files appear in the browse listing.
    let (_, files) = service::list_children(kv.as_ref(), "hosted1", "d/")
        .await
        .unwrap();
    let names: Vec<String> = files.into_iter().map(|(n, _)| n).collect();
    assert!(
        names.iter().any(|n| n == "a.txt") && names.iter().any(|n| n == "b.txt"),
        "both files should be listed, got {names:?}"
    );

    // Single delete prunes the tree entry — no ghost.
    assert!(repo.delete("d/a.txt").await.unwrap());
    let (_, files) = service::list_children(kv.as_ref(), "hosted1", "d/")
        .await
        .unwrap();
    let names: Vec<String> = files.into_iter().map(|(n, _)| n).collect();
    assert!(
        !names.iter().any(|n| n == "a.txt"),
        "single-deleted file must not ghost in the listing, got {names:?}"
    );
    assert!(service::get_artifact(kv.as_ref(), "hosted1", "d/a.txt")
        .await
        .unwrap()
        .is_none());

    // Bulk paired delete (the retention / GC path) prunes the tree entry too.
    service::delete_artifacts_paired_batch(kv.as_ref(), "hosted1", &["d/b.txt"])
        .await
        .unwrap();
    let (_, files) = service::list_children(kv.as_ref(), "hosted1", "d/")
        .await
        .unwrap();
    let names: Vec<String> = files.into_iter().map(|(n, _)| n).collect();
    assert!(
        !names.iter().any(|n| n == "b.txt"),
        "bulk-deleted file must not ghost in the listing, got {names:?}"
    );
    assert!(service::get_artifact(kv.as_ref(), "hosted1", "d/b.txt")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_delete_nonexistent() {
    let (kv, blobs, _dir) = test_kv().await;
    let repo = HostedRepo {
        name: "hosted1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    assert!(!repo.delete("nope.txt").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_list_prefix() {
    let (kv, blobs, _dir) = test_kv().await;
    let repo = HostedRepo {
        name: "hosted1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    repo.put("docs/a.txt", "text/plain", b"a").await.unwrap();
    repo.put("docs/b.txt", "text/plain", b"b").await.unwrap();
    repo.put("src/c.rs", "text/plain", b"c").await.unwrap();

    let list = repo.list("docs/", &Pagination::default()).await.unwrap();
    assert_eq!(list.items.len(), 2);
    let paths: Vec<&str> = list.items.iter().map(|(p, _)| p.as_str()).collect();
    assert!(paths.contains(&"docs/a.txt"));
    assert!(paths.contains(&"docs/b.txt"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_list_empty() {
    let (kv, blobs, _dir) = test_kv().await;
    let repo = HostedRepo {
        name: "hosted1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    let list = repo.list("", &Pagination::default()).await.unwrap();
    assert!(list.items.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_search() {
    let (kv, blobs, _dir) = test_kv().await;
    let repo = HostedRepo {
        name: "hosted1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    repo.put("docs/README.md", "text/plain", b"readme")
        .await
        .unwrap();
    repo.put("src/main.rs", "text/plain", b"main")
        .await
        .unwrap();
    repo.put("docs/readme.txt", "text/plain", b"text")
        .await
        .unwrap();

    let results = repo.search("readme", &Pagination::default()).await.unwrap();
    assert_eq!(results.items.len(), 2);
}

// ============================
// ProxyRepo tests
// ============================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_get_from_single_hosted() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    // Set up hosted member with data
    let repo = make_hosted_repo("h1");
    service::put_repo(kv.as_ref(), &repo).await.unwrap();
    let hosted = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    hosted
        .put("file.txt", "text/plain", b"content")
        .await
        .unwrap();

    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: vec!["h1".to_string()],
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    let result = proxy.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_get_returns_first_hit() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    service::put_repo(kv.as_ref(), &make_hosted_repo("h1"))
        .await
        .unwrap();
    service::put_repo(kv.as_ref(), &make_hosted_repo("h2"))
        .await
        .unwrap();

    let h1 = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h1.put("file.txt", "text/plain", b"from-h1").await.unwrap();

    let h2 = HostedRepo {
        name: "h2".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h2.put("file.txt", "text/plain", b"from-h2").await.unwrap();

    let members = vec!["h1".to_string(), "h2".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    let result = proxy.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"from-h1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_get_falls_through() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    service::put_repo(kv.as_ref(), &make_hosted_repo("h1"))
        .await
        .unwrap();
    service::put_repo(kv.as_ref(), &make_hosted_repo("h2"))
        .await
        .unwrap();

    // Only h2 has the file
    let h2 = HostedRepo {
        name: "h2".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h2.put("file.txt", "text/plain", b"from-h2").await.unwrap();

    let members = vec!["h1".to_string(), "h2".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    let result = proxy.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"from-h2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_get_skips_missing_member() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    // "missing-repo" is not in the KV, "h1" is
    let repo = make_hosted_repo("h1");
    service::put_repo(kv.as_ref(), &repo).await.unwrap();
    let h1 = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h1.put("file.txt", "text/plain", b"found").await.unwrap();

    let members = vec!["missing-repo".to_string(), "h1".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    let result = proxy.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_get_empty_members() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    let members: Vec<String> = vec![];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    assert!(proxy.get("file.txt").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_get_all_miss() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    service::put_repo(kv.as_ref(), &make_hosted_repo("h1"))
        .await
        .unwrap();
    service::put_repo(kv.as_ref(), &make_hosted_repo("h2"))
        .await
        .unwrap();
    // No files in either

    let members = vec!["h1".to_string(), "h2".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    assert!(proxy.get("file.txt").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_get_nested_proxy() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    let repo = make_hosted_repo("h1");
    service::put_repo(kv.as_ref(), &repo).await.unwrap();
    let h1 = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h1.put("deep.txt", "text/plain", b"nested").await.unwrap();

    // inner-proxy -> h1
    let inner = make_proxy_repo("inner-proxy", vec!["h1".to_string()]);
    service::put_repo(kv.as_ref(), &inner).await.unwrap();

    // outer-proxy -> inner-proxy
    let members = vec!["inner-proxy".to_string()];
    let proxy = ProxyRepo {
        name: "outer-proxy".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    let result = proxy.get("deep.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"nested");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_get_from_cache_local_hit() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    // Create a cache repo config (no wiremock needed — data is already cached locally)
    let repo = make_cache_repo("cache1", "http://unused.example.com", 0);
    service::put_repo(kv.as_ref(), &repo).await.unwrap();

    // Directly ingest into the cache repo's local store
    {
        let ctx = IngestionCtx {
            kv: kv.as_ref(),
            blobs: blobs.as_ref(),
            store: "default",
            repo: "cache1",
            format: "raw",
            repo_type: "cache",
        };
        depot_core::repo::ingest_artifact(&ctx, "file.txt", "text/plain", b"cached")
            .await
            .unwrap();
    }

    let members = vec!["cache1".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    let result = proxy.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"cached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_list_union() {
    let (kv, blobs, _dir) = test_kv().await;

    service::put_repo(kv.as_ref(), &make_hosted_repo("h1"))
        .await
        .unwrap();
    service::put_repo(kv.as_ref(), &make_hosted_repo("h2"))
        .await
        .unwrap();

    let h1 = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h1.put("a.txt", "text/plain", b"a").await.unwrap();

    let h2 = HostedRepo {
        name: "h2".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h2.put("b.txt", "text/plain", b"b").await.unwrap();

    let http = reqwest::Client::new();
    let members = vec!["h1".to_string(), "h2".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    let list = proxy.list("", &Pagination::default()).await.unwrap();
    assert_eq!(list.items.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_list_dedup() {
    let (kv, blobs, _dir) = test_kv().await;

    service::put_repo(kv.as_ref(), &make_hosted_repo("h1"))
        .await
        .unwrap();
    service::put_repo(kv.as_ref(), &make_hosted_repo("h2"))
        .await
        .unwrap();

    let h1 = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h1.put("same.txt", "text/plain", b"v1").await.unwrap();

    let h2 = HostedRepo {
        name: "h2".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h2.put("same.txt", "text/plain", b"v2").await.unwrap();

    let http = reqwest::Client::new();
    let members = vec!["h1".to_string(), "h2".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    let list = proxy.list("", &Pagination::default()).await.unwrap();
    assert_eq!(list.items.len(), 1, "duplicate paths should be deduped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_list_empty() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let members: Vec<String> = vec![];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    assert!(proxy
        .list("", &Pagination::default())
        .await
        .unwrap()
        .items
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_search_union_dedup() {
    let (kv, blobs, _dir) = test_kv().await;

    service::put_repo(kv.as_ref(), &make_hosted_repo("h1"))
        .await
        .unwrap();
    service::put_repo(kv.as_ref(), &make_hosted_repo("h2"))
        .await
        .unwrap();

    let h1 = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h1.put("readme.md", "text/plain", b"a").await.unwrap();
    h1.put("unique-h1.txt", "text/plain", b"b").await.unwrap();

    let h2 = HostedRepo {
        name: "h2".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h2.put("readme.md", "text/plain", b"c").await.unwrap();
    h2.put("notes.txt", "text/plain", b"d").await.unwrap();

    let http = reqwest::Client::new();
    let members = vec!["h1".to_string(), "h2".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    // Search for "readme" — exists in both but should be deduped
    let results = proxy
        .search("readme", &Pagination::default())
        .await
        .unwrap();
    assert_eq!(results.items.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_search_no_results() {
    let (kv, blobs, _dir) = test_kv().await;

    let repo = make_hosted_repo("h1");
    service::put_repo(kv.as_ref(), &repo).await.unwrap();
    let h1 = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    h1.put("file.txt", "text/plain", b"a").await.unwrap();

    let http = reqwest::Client::new();
    let members = vec!["h1".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    let results = proxy.search("zzzzz", &Pagination::default()).await.unwrap();
    assert!(results.items.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_list_pagination_basic() {
    let (kv, blobs, _dir) = test_kv().await;

    service::put_repo(kv.as_ref(), &make_hosted_repo("h1"))
        .await
        .unwrap();
    service::put_repo(kv.as_ref(), &make_hosted_repo("h2"))
        .await
        .unwrap();

    let h1 = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    for i in 0..5 {
        h1.put(
            &format!("h1-{i}.txt"),
            "text/plain",
            format!("h1-{i}").as_bytes(),
        )
        .await
        .unwrap();
    }

    let h2 = HostedRepo {
        name: "h2".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    for i in 0..5 {
        h2.put(
            &format!("h2-{i}.txt"),
            "text/plain",
            format!("h2-{i}").as_bytes(),
        )
        .await
        .unwrap();
    }

    let http = reqwest::Client::new();
    let members = vec!["h1".to_string(), "h2".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };

    // Paginate with limit=3
    let mut all_paths = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let pag = Pagination {
            cursor: cursor.clone(),
            offset: 0,
            limit: 3,
        };
        let result = proxy.list("", &pag).await.unwrap();
        if result.items.is_empty() {
            break;
        }
        assert!(result.items.len() <= 3);
        for (path, _) in &result.items {
            all_paths.push(path.clone());
        }
        pages += 1;
        cursor = result.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(all_paths.len(), 10, "should see all 10 unique items");
    // Verify no duplicates
    let unique: std::collections::HashSet<_> = all_paths.iter().collect();
    assert_eq!(unique.len(), 10);
    // Should take at least 3 pages to get 10 items with limit=3
    assert!(pages >= 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_search_pagination_basic() {
    let (kv, blobs, _dir) = test_kv().await;

    service::put_repo(kv.as_ref(), &make_hosted_repo("h1"))
        .await
        .unwrap();
    service::put_repo(kv.as_ref(), &make_hosted_repo("h2"))
        .await
        .unwrap();

    let h1 = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    for i in 0..4 {
        h1.put(&format!("doc-{i}.txt"), "text/plain", b"x")
            .await
            .unwrap();
    }

    let h2 = HostedRepo {
        name: "h2".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    for i in 4..8 {
        h2.put(&format!("doc-{i}.txt"), "text/plain", b"x")
            .await
            .unwrap();
    }

    let http = reqwest::Client::new();
    let members = vec!["h1".to_string(), "h2".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };

    let mut all_paths = Vec::new();
    let mut cursor = None;
    loop {
        let pag = Pagination {
            cursor: cursor.clone(),
            offset: 0,
            limit: 3,
        };
        let result = proxy.search("doc", &pag).await.unwrap();
        if result.items.is_empty() {
            break;
        }
        for (path, _) in &result.items {
            all_paths.push(path.clone());
        }
        cursor = result.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(all_paths.len(), 8);
    let unique: std::collections::HashSet<_> = all_paths.iter().collect();
    assert_eq!(unique.len(), 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_list_pagination_dedup() {
    let (kv, blobs, _dir) = test_kv().await;

    service::put_repo(kv.as_ref(), &make_hosted_repo("h1"))
        .await
        .unwrap();
    service::put_repo(kv.as_ref(), &make_hosted_repo("h2"))
        .await
        .unwrap();

    // h1 has: shared-0, shared-1, shared-2, unique-h1-0, unique-h1-1
    // h2 has: shared-0, shared-1, shared-2, unique-h2-0, unique-h2-1
    // Total unique = 7
    let h1 = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    let h2 = HostedRepo {
        name: "h2".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    for i in 0..3 {
        h1.put(&format!("shared-{i}.txt"), "text/plain", b"h1")
            .await
            .unwrap();
        h2.put(&format!("shared-{i}.txt"), "text/plain", b"h2")
            .await
            .unwrap();
    }
    for i in 0..2 {
        h1.put(&format!("unique-h1-{i}.txt"), "text/plain", b"h1")
            .await
            .unwrap();
        h2.put(&format!("unique-h2-{i}.txt"), "text/plain", b"h2")
            .await
            .unwrap();
    }

    let http = reqwest::Client::new();
    let members = vec!["h1".to_string(), "h2".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };

    let mut all_paths = Vec::new();
    let mut cursor = None;
    loop {
        let pag = Pagination {
            cursor: cursor.clone(),
            offset: 0,
            limit: 3,
        };
        let result = proxy.list("", &pag).await.unwrap();
        if result.items.is_empty() {
            break;
        }
        for (path, _) in &result.items {
            all_paths.push(path.clone());
        }
        cursor = result.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    let unique: std::collections::HashSet<_> = all_paths.iter().collect();
    assert_eq!(
        unique.len(),
        7,
        "should see exactly 7 unique paths across all pages"
    );
    assert_eq!(all_paths.len(), 7, "no duplicates across pages");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_list_pagination_exhausted_member() {
    let (kv, blobs, _dir) = test_kv().await;

    service::put_repo(kv.as_ref(), &make_hosted_repo("h1"))
        .await
        .unwrap();
    service::put_repo(kv.as_ref(), &make_hosted_repo("h2"))
        .await
        .unwrap();

    // h1 has 2 items, h2 has 8 items
    let h1 = HostedRepo {
        name: "h1".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    for i in 0..2 {
        h1.put(&format!("h1-{i}.txt"), "text/plain", b"a")
            .await
            .unwrap();
    }

    let h2 = HostedRepo {
        name: "h2".into(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        updater: UpdateSender::noop(),
        store: "default".into(),
        format: "raw".into(),
    };
    for i in 0..8 {
        h2.put(&format!("h2-{i}.txt"), "text/plain", b"b")
            .await
            .unwrap();
    }

    let http = reqwest::Client::new();
    let members = vec!["h1".to_string(), "h2".to_string()];
    let proxy = ProxyRepo {
        name: "proxy1".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };

    let mut all_paths = Vec::new();
    let mut cursor = None;
    loop {
        let pag = Pagination {
            cursor: cursor.clone(),
            offset: 0,
            limit: 3,
        };
        let result = proxy.list("", &pag).await.unwrap();
        if result.items.is_empty() {
            break;
        }
        for (path, _) in &result.items {
            all_paths.push(path.clone());
        }
        cursor = result.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(all_paths.len(), 10, "all 10 items should be returned");
    let unique: std::collections::HashSet<_> = all_paths.iter().collect();
    assert_eq!(unique.len(), 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_circular_membership_returns_none() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    // A -> B -> A (circular)
    service::put_repo(kv.as_ref(), &make_proxy_repo("A", vec!["B".to_string()]))
        .await
        .unwrap();
    service::put_repo(kv.as_ref(), &make_proxy_repo("B", vec!["A".to_string()]))
        .await
        .unwrap();

    let members = vec!["B".to_string()];
    let proxy = ProxyRepo {
        name: "A".to_string(),
        members: members.clone(),
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        depth: 0,
    };
    // Should return None instead of stack overflow
    let result = proxy.get("anything.txt").await.unwrap();
    assert!(result.is_none());
}

// ============================
// CacheRepo tests (with wiremock)
// ============================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_local_hit_infinite_ttl() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    // Pre-populate cache locally
    {
        let ctx = IngestionCtx {
            kv: kv.as_ref(),
            blobs: blobs.as_ref(),
            store: "default",
            repo: "cache1",
            format: "raw",
            repo_type: "cache",
        };
        depot_core::repo::ingest_artifact(&ctx, "file.txt", "text/plain", b"cached")
            .await
            .unwrap();
    }

    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: "http://unused.example.com".to_string(),
        cache_ttl_secs: 0, // infinite TTL
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    let result = cache.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"cached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_local_hit_not_expired() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    {
        let ctx = IngestionCtx {
            kv: kv.as_ref(),
            blobs: blobs.as_ref(),
            store: "default",
            repo: "cache1",
            format: "raw",
            repo_type: "cache",
        };
        depot_core::repo::ingest_artifact(&ctx, "file.txt", "text/plain", b"fresh")
            .await
            .unwrap();
    }

    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: "http://unused.example.com".to_string(),
        cache_ttl_secs: 3600, // 1 hour TTL — just created, so not expired
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    let result = cache.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"fresh");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_expired_refetches() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/file.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"updated-from-upstream".to_vec())
                .insert_header("content-type", "text/plain"),
        )
        .mount(&mock_server)
        .await;

    // Pre-populate with old data and backdate updated_at
    {
        let ctx = IngestionCtx {
            kv: kv.as_ref(),
            blobs: blobs.as_ref(),
            store: "default",
            repo: "cache1",
            format: "raw",
            repo_type: "cache",
        };
        depot_core::repo::ingest_artifact(&ctx, "file.txt", "text/plain", b"stale")
            .await
            .unwrap();
    }
    // Backdate the artifact so it appears expired
    let mut rec = service::get_artifact(kv.as_ref(), "cache1", "file.txt")
        .await
        .unwrap()
        .unwrap();
    rec.updated_at = DateTime::from_timestamp(1000, 0).unwrap(); // very old
    service::put_artifact(kv.as_ref(), "cache1", "file.txt", &rec)
        .await
        .unwrap();

    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 60,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    let result = cache.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"updated-from-upstream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_miss_fetches_upstream() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/new-file.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"from-upstream".to_vec())
                .insert_header("content-type", "application/octet-stream"),
        )
        .mount(&mock_server)
        .await;

    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 3600,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    let result = cache.get("new-file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"from-upstream");

    // With the mpsc-based tee-stream, read_all blocks until the background task
    // finishes and drops the sender, so KV records are committed by this point.

    // Verify it was cached
    let cached = service::get_artifact(kv.as_ref(), "cache1", "new-file.txt")
        .await
        .unwrap();
    assert!(cached.is_some());
}

/// Count regular files under a directory tree (blob files on disk).
fn count_files_recursive(dir: &std::path::Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                count += count_files_recursive(&path);
            } else if path.is_file() {
                count += 1;
            }
        }
    }
    count
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_refetch_identical_content_reuses_blob() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, dir) = test_kv().await;
    let blobs_dir = dir.path().join("blobs");
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    // Upstream serves the same bytes on every fetch (e.g. an unchanged
    // helm index.yaml re-fetched after each TTL expiry).
    Mock::given(method("GET"))
        .and(path("/index.yaml"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"unchanged-upstream-content".to_vec())
                .insert_header("content-type", "application/x-yaml"),
        )
        .mount(&mock_server)
        .await;

    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 60,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),
        format: "raw".into(),
    };

    // First fetch: cache miss, fills the cache.
    let result = cache.get("index.yaml").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"unchanged-upstream-content");
    let first = service::get_artifact(kv.as_ref(), "cache1", "index.yaml")
        .await
        .unwrap()
        .unwrap();
    let first_blob_id = first.blob_id.clone().unwrap();
    assert_eq!(count_files_recursive(&blobs_dir), 1);

    // Expire the cached record so the next get re-fetches upstream.
    let mut rec = first.clone();
    rec.updated_at = DateTime::from_timestamp(1000, 0).unwrap(); // very old
    service::put_artifact(kv.as_ref(), "cache1", "index.yaml", &rec)
        .await
        .unwrap();

    // Second fetch: upstream content is byte-identical, so the commit must
    // dedup against the existing blob — reuse its blob_id and delete the
    // freshly written duplicate.
    let result = cache.get("index.yaml").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"unchanged-upstream-content");

    let second = service::get_artifact(kv.as_ref(), "cache1", "index.yaml")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        second.blob_id.as_deref(),
        Some(first_blob_id.as_str()),
        "re-fetch of identical content must reuse the existing blob_id"
    );
    assert_eq!(
        count_files_recursive(&blobs_dir),
        1,
        "re-fetch of identical content must not leave a duplicate blob file on disk"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_passes_upstream_content_length() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    let body = b"hello world, this is 42 bytes of content!!";
    assert_eq!(body.len(), 42);

    Mock::given(method("GET"))
        .and(path("/sized.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.to_vec())
                .insert_header("content-type", "application/octet-stream"),
        )
        .mount(&mock_server)
        .await;

    let cache = CacheRepo {
        name: "cache-cl".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 3600,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),
        format: "raw".into(),
    };
    let result = cache.get("sized.bin").await.unwrap().unwrap();

    // The preliminary record should carry the upstream Content-Length.
    assert_eq!(result.record.size, 42);

    // Body must still be correct.
    assert_eq!(read_all(result.data).await, body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_upstream_404() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/missing.txt"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock_server)
        .await;

    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 3600,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    assert!(cache.get("missing.txt").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_upstream_error() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/error.txt"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock_server)
        .await;

    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 3600,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    assert!(cache.get("error.txt").await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_upstream_error_serves_stale() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/file.txt"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock_server)
        .await;

    // Pre-populate cache and backdate to force expiry
    {
        let ctx = IngestionCtx {
            kv: kv.as_ref(),
            blobs: blobs.as_ref(),
            store: "default",
            repo: "cache1",
            format: "raw",
            repo_type: "cache",
        };
        depot_core::repo::ingest_artifact(&ctx, "file.txt", "text/plain", b"stale-data")
            .await
            .unwrap();
    }
    let mut rec = service::get_artifact(kv.as_ref(), "cache1", "file.txt")
        .await
        .unwrap()
        .unwrap();
    rec.updated_at = DateTime::from_timestamp(1000, 0).unwrap();
    service::put_artifact(kv.as_ref(), "cache1", "file.txt", &rec)
        .await
        .unwrap();

    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 60,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    let result = cache.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"stale-data");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_upstream_network_error_serves_stale() {
    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();

    // Pre-populate cache and backdate to force expiry
    {
        let ctx = IngestionCtx {
            kv: kv.as_ref(),
            blobs: blobs.as_ref(),
            store: "default",
            repo: "cache1",
            format: "raw",
            repo_type: "cache",
        };
        depot_core::repo::ingest_artifact(&ctx, "file.txt", "text/plain", b"stale-net")
            .await
            .unwrap();
    }
    let mut rec = service::get_artifact(kv.as_ref(), "cache1", "file.txt")
        .await
        .unwrap()
        .unwrap();
    rec.updated_at = DateTime::from_timestamp(1000, 0).unwrap();
    service::put_artifact(kv.as_ref(), "cache1", "file.txt", &rec)
        .await
        .unwrap();

    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: "http://127.0.0.1:1".to_string(), // unreachable
        cache_ttl_secs: 60,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    let result = cache.get("file.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"stale-net");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_upstream_content_type() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/image.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"PNG-DATA".to_vec())
                .insert_header("content-type", "image/png"),
        )
        .mount(&mock_server)
        .await;

    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 3600,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    let result = cache.get("image.png").await.unwrap().unwrap();
    assert_eq!(result.record.content_type, "image/png");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_list_local_only() {
    let (kv, blobs, _dir) = test_kv().await;

    {
        let ctx = IngestionCtx {
            kv: kv.as_ref(),
            blobs: blobs.as_ref(),
            store: "default",
            repo: "cache1",
            format: "raw",
            repo_type: "cache",
        };
        depot_core::repo::ingest_artifact(&ctx, "a.txt", "text/plain", b"a")
            .await
            .unwrap();
    }
    {
        let ctx = IngestionCtx {
            kv: kv.as_ref(),
            blobs: blobs.as_ref(),
            store: "default",
            repo: "cache1",
            format: "raw",
            repo_type: "cache",
        };
        depot_core::repo::ingest_artifact(&ctx, "b.txt", "text/plain", b"b")
            .await
            .unwrap();
    }

    let http = reqwest::Client::new();
    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: "http://unused.example.com".to_string(),
        cache_ttl_secs: 0,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    let list = cache.list("", &Pagination::default()).await.unwrap();
    assert_eq!(list.items.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_search_local_only() {
    let (kv, blobs, _dir) = test_kv().await;

    {
        let ctx = IngestionCtx {
            kv: kv.as_ref(),
            blobs: blobs.as_ref(),
            store: "default",
            repo: "cache1",
            format: "raw",
            repo_type: "cache",
        };
        depot_core::repo::ingest_artifact(&ctx, "readme.md", "text/plain", b"r")
            .await
            .unwrap();
    }
    {
        let ctx = IngestionCtx {
            kv: kv.as_ref(),
            blobs: blobs.as_ref(),
            store: "default",
            repo: "cache1",
            format: "raw",
            repo_type: "cache",
        };
        depot_core::repo::ingest_artifact(&ctx, "other.txt", "text/plain", b"o")
            .await
            .unwrap();
    }

    let http = reqwest::Client::new();
    let cache = CacheRepo {
        name: "cache1".to_string(),
        upstream_url: "http://unused.example.com".to_string(),
        cache_ttl_secs: 0,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    let results = cache
        .search("readme", &Pagination::default())
        .await
        .unwrap();
    assert_eq!(results.items.len(), 1);
}

// ============================
// CacheRepo upstream auth tests
// ============================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_with_upstream_auth_sends_basic_header() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    // Expect a request with Basic auth header
    Mock::given(method("GET"))
        .and(path("/authed.txt"))
        .and(header("Authorization", "Basic dXNlcjpwYXNz")) // base64("user:pass")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"authenticated-content".to_vec())
                .insert_header("content-type", "text/plain"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let auth = depot_core::store::kv::UpstreamAuth {
        username: "user".to_string(),
        password: Some("pass".to_string()),
    };

    let cache = CacheRepo {
        name: "cache-auth".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 3600,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: Some(auth.clone()),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    let result = cache.get("authed.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"authenticated-content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_with_upstream_auth_no_password() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    // Expect a request with Basic auth header (username only, no password)
    Mock::given(method("GET"))
        .and(path("/token-auth.txt"))
        .and(header("Authorization", "Basic dG9rZW46")) // base64("token:")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(b"token-content".to_vec())
                .insert_header("content-type", "text/plain"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let auth = depot_core::store::kv::UpstreamAuth {
        username: "token".to_string(),
        password: None,
    };

    let cache = CacheRepo {
        name: "cache-token".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 3600,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: Some(auth.clone()),
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),

        format: "raw".into(),
    };
    let result = cache.get("token-auth.txt").await.unwrap().unwrap();
    assert_eq!(read_all(result.data).await, b"token-content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_propagates_upstream_blake3_header() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    let body: &[u8] = b"upstream payload with checksum";
    let expected_hash = blake3::hash(body).to_hex().to_string();

    Mock::given(method("GET"))
        .and(path("/checksummed.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.to_vec())
                .insert_header("content-type", "application/octet-stream")
                .insert_header("x-checksum-blake3", expected_hash.as_str()),
        )
        .mount(&mock_server)
        .await;

    let cache = CacheRepo {
        name: "cache-b3".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 3600,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),
        format: "raw".into(),
    };

    let result = cache.get("checksummed.bin").await.unwrap().unwrap();
    // Preliminary record carries the upstream-claimed BLAKE3 before the body finishes,
    // so the streaming response can emit `X-Checksum-Blake3` immediately.
    assert_eq!(
        result.record.content_hash.as_deref(),
        Some(expected_hash.as_str())
    );
    assert_eq!(read_all(result.data).await, body);

    // Committed record reflects the verified hash.
    let cached = service::get_artifact(kv.as_ref(), "cache-b3", "checksummed.bin")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cached.content_hash.as_deref(), Some(expected_hash.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_head_propagates_upstream_blake3_header() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    // Note: the hash advertised on HEAD passthrough is not verified against the
    // body (we never fetch one), so any well-formed value flows through.
    let advertised = "0".repeat(64);

    Mock::given(method("HEAD"))
        .and(path("/probe.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .insert_header("content-length", "1024")
                .insert_header("x-checksum-blake3", advertised.as_str()),
        )
        .mount(&mock_server)
        .await;

    let cache = CacheRepo {
        name: "cache-b3-head".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 3600,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),
        format: "raw".into(),
    };

    let record = cache.head("probe.bin").await.unwrap().unwrap();
    assert_eq!(record.content_hash.as_deref(), Some(advertised.as_str()));
    assert_eq!(record.size, 1024);
    // HEAD must not commit anything to the local cache.
    assert!(
        service::get_artifact(kv.as_ref(), "cache-b3-head", "probe.bin")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_rejects_upstream_blake3_mismatch() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    let body: &[u8] = b"upstream tells us it hashes to all zeros, but it does not";
    let bogus_hash = "0".repeat(64);

    Mock::given(method("GET"))
        .and(path("/lying.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.to_vec())
                .insert_header("content-type", "application/octet-stream")
                .insert_header("x-checksum-blake3", bogus_hash.as_str()),
        )
        .mount(&mock_server)
        .await;

    let cache = CacheRepo {
        name: "cache-b3-bad".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 3600,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),
        format: "raw".into(),
    };

    let result = cache.get("lying.bin").await.unwrap().unwrap();
    let mut saw_err = false;
    if let ArtifactData::Stream(mut stream) = result.data {
        while let Some(chunk) = stream.next().await {
            if chunk.is_err() {
                saw_err = true;
            }
        }
    } else {
        panic!("expected streaming body");
    }
    assert!(
        saw_err,
        "stream must surface an error when upstream BLAKE3 doesn't match the body"
    );
    // The mismatched blob must not be committed to the cache.
    assert!(
        service::get_artifact(kv.as_ref(), "cache-b3-bad", "lying.bin")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_get_ignores_malformed_upstream_blake3() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (kv, blobs, _dir) = test_kv().await;
    let http = reqwest::Client::new();
    let mock_server = MockServer::start().await;

    let body: &[u8] = b"upstream sent garbage in the checksum header";

    Mock::given(method("GET"))
        .and(path("/junky-hdr.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.to_vec())
                .insert_header("content-type", "application/octet-stream")
                .insert_header("x-checksum-blake3", "not-a-blake3-hash"),
        )
        .mount(&mock_server)
        .await;

    let cache = CacheRepo {
        name: "cache-b3-junk".to_string(),
        upstream_url: mock_server.uri(),
        cache_ttl_secs: 3600,
        kv: Arc::clone(&kv),
        blobs: Arc::clone(&blobs),
        store: "default".to_string(),
        http: http.clone(),
        upstream_auth: None,
        inflight: InflightMap::new(),
        updater: UpdateSender::noop(),
        format: "raw".into(),
    };

    let result = cache.get("junky-hdr.bin").await.unwrap().unwrap();
    // Malformed upstream value is ignored — preliminary record has no hash yet.
    assert!(result.record.content_hash.is_none());
    assert_eq!(read_all(result.data).await, body);

    // Committed record carries the actually-computed BLAKE3.
    let cached = service::get_artifact(kv.as_ref(), "cache-b3-junk", "junky-hdr.bin")
        .await
        .unwrap()
        .unwrap();
    let computed = blake3::hash(body).to_hex().to_string();
    assert_eq!(cached.content_hash.as_deref(), Some(computed.as_str()));
}
