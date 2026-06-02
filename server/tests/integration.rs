// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Integration tests: start a real depot server, run bench/demo against it over HTTP.

use depot_bench::client::{CreateRepoRequest, DepotClient};
use depot_bench::demo;
use depot_bench::runner;
use depot_server::server::infra::test_support::TestServer;
use serde_json::Value;

/// Build a KvStoreConfig from environment variables, if set.
fn kv_config_from_env() -> Option<depot_server::server::config::KvStoreConfig> {
    match std::env::var("DEPOT_TEST_KV").ok().as_deref() {
        #[cfg(feature = "dynamodb")]
        Some("dynamodb") => {
            let endpoint = std::env::var("DEPOT_TEST_DYNAMODB_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:8000".to_string());
            let prefix = format!("test-{}", uuid::Uuid::new_v4());
            Some(depot_server::server::config::KvStoreConfig::DynamoDB {
                table_prefix: prefix,
                region: "us-east-1".to_string(),
                endpoint_url: Some(endpoint),
                max_retries: 3,
                connect_timeout_secs: 3,
                read_timeout_secs: 10,
                retry_mode: "standard".to_string(),
            })
        }
        #[cfg(not(feature = "dynamodb"))]
        Some("dynamodb") => {
            panic!("DEPOT_TEST_KV=dynamodb requires --features dynamodb");
        }
        _ => None,
    }
}

/// Helper to start a server and create a connected client.
async fn setup() -> (TestServer, DepotClient) {
    let server = TestServer::start_with_kv(kv_config_from_env()).await;
    let client =
        DepotClient::new(&server.url, "admin", "admin", false).expect("failed to create client");
    (server, client)
}

// ============================================================
// Client tests
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_login_success() {
    let (server, client) = setup().await;
    client.login().await.expect("login should succeed");
    server.handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_login_wrong_password() {
    let server = TestServer::start().await;
    let client = DepotClient::new(&server.url, "admin", "wrong-password", false).unwrap();
    let result = client.login().await;
    assert!(result.is_err(), "login with wrong password should fail");
    server.handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_create_repo() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    let repo = client
        .create_repo(&CreateRepoRequest {
            name: "test-raw".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .expect("create repo should succeed");

    assert_eq!(repo.name, "test-raw");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_repos() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Initially empty
    let repos = client.list_repos().await.unwrap();
    assert!(repos.is_empty(), "expected no repos initially");

    // Create one
    client
        .create_repo(&CreateRepoRequest {
            name: "list-test".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    let repos = client.list_repos().await.unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].name, "list-test");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_repo() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    client
        .create_repo(&CreateRepoRequest {
            name: "delete-me".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    client.delete_repo("delete-me").await.unwrap();

    let repos = client.list_repos().await.unwrap();
    assert!(repos.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_repo_not_found_is_ok() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    // Deleting a non-existent repo should succeed (idempotent)
    client
        .delete_repo("nonexistent")
        .await
        .expect("delete of missing repo should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_clone_repo() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Create source repo and upload some artifacts.
    client
        .create_repo(&CreateRepoRequest {
            name: "clone-src".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();
    client
        .upload_raw("clone-src", "a.txt", b"aaa".to_vec(), "text/plain")
        .await
        .unwrap();
    client
        .upload_raw("clone-src", "dir/b.txt", b"bbb".to_vec(), "text/plain")
        .await
        .unwrap();

    // Clone it (returns a task, waits for completion).
    let task = client.clone_repo("clone-src", "clone-dst").await.unwrap();
    assert_eq!(task["kind"]["type"], "repo_clone");
    assert_eq!(task["kind"]["source"], "clone-src");
    assert_eq!(task["kind"]["target"], "clone-dst");

    // Verify the new repo exists with correct settings.
    let repos = client.list_repos().await.unwrap();
    let cloned = repos.iter().find(|r| r.name == "clone-dst").unwrap();
    assert_eq!(cloned.format, "raw");
    assert_eq!(cloned.repo_type, "hosted");

    // Verify artifacts are browsable in the clone.
    let data_a = client.download_raw("clone-dst", "a.txt").await.unwrap();
    assert_eq!(data_a, b"aaa");
    let data_b = client.download_raw("clone-dst", "dir/b.txt").await.unwrap();
    assert_eq!(data_b, b"bbb");

    // Source repo is unchanged.
    let src_a = client.download_raw("clone-src", "a.txt").await.unwrap();
    assert_eq!(src_a, b"aaa");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_clone_repo_conflict() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    client
        .create_repo(&CreateRepoRequest {
            name: "conflict-a".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();
    client
        .create_repo(&CreateRepoRequest {
            name: "conflict-b".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    // Cloning to an existing name should fail.
    let err = client
        .clone_repo("conflict-a", "conflict-b")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("409"),
        "expected 409 Conflict, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_clone_repo_not_found() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    let err = client
        .clone_repo("nonexistent", "new-name")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("404"),
        "expected 404 Not Found, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_download_raw() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    client
        .create_repo(&CreateRepoRequest {
            name: "raw-test".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    let data = b"hello, depot!".to_vec();
    client
        .upload_raw("raw-test", "docs/readme.txt", data.clone(), "text/plain")
        .await
        .expect("upload should succeed");

    let downloaded = client
        .download_raw("raw-test", "docs/readme.txt")
        .await
        .expect("download should succeed");

    assert_eq!(downloaded, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_raw() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    client
        .create_repo(&CreateRepoRequest {
            name: "del-raw".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    client
        .upload_raw(
            "del-raw",
            "file.bin",
            vec![1, 2, 3],
            "application/octet-stream",
        )
        .await
        .unwrap();

    client
        .delete_raw("del-raw", "file.bin")
        .await
        .expect("delete should succeed");

    // Download should fail (404)
    let result = client.download_raw("del-raw", "file.bin").await;
    assert!(result.is_err(), "download after delete should fail");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_artifacts() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    client
        .create_repo(&CreateRepoRequest {
            name: "list-art".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    for i in 0..3 {
        client
            .upload_raw(
                "list-art",
                &format!("file-{}.txt", i),
                format!("content {}", i).into_bytes(),
                "text/plain",
            )
            .await
            .unwrap();
    }

    let artifacts = client.list_artifacts("list-art").await.unwrap();
    assert_eq!(artifacts.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bearer_token_caching() {
    let (_server, client) = setup().await;

    // First call triggers login internally
    client.login().await.unwrap();

    // Multiple calls should reuse the cached token
    let repos1 = client.list_repos().await.unwrap();
    let repos2 = client.list_repos().await.unwrap();
    assert_eq!(repos1.len(), repos2.len());
}

// ============================================================
// Docker tests
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_push_blob() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    client
        .create_repo(&CreateRepoRequest {
            name: "docker-test".to_string(),
            repo_type: "hosted".to_string(),
            format: "docker".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let digest = format!("sha256:{}", sha256_hex(&data));

    client
        .docker_push_blob("docker-test", "myimage", data, &digest)
        .await
        .expect("docker push blob should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_push_pull_manifest() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    client
        .create_repo(&CreateRepoRequest {
            name: "docker-manifest".to_string(),
            repo_type: "hosted".to_string(),
            format: "docker".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    // Push a config blob
    let config_json = serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "rootfs": { "type": "layers", "diff_ids": [] }
    });
    let config_bytes = serde_json::to_vec(&config_json).unwrap();
    let config_digest = format!("sha256:{}", sha256_hex(&config_bytes));

    client
        .docker_push_blob(
            "docker-manifest",
            "app",
            config_bytes.clone(),
            &config_digest,
        )
        .await
        .unwrap();

    // Push a manifest
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_bytes.len()
        },
        "layers": []
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();

    let digest = client
        .docker_push_manifest(
            "docker-manifest",
            "app",
            "latest",
            &manifest_bytes,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await
        .expect("push manifest should succeed");

    assert!(digest.starts_with("sha256:"), "digest should be sha256");

    // Pull it back
    let (pulled_data, content_type) = client
        .docker_pull_manifest("docker-manifest", "app", "latest")
        .await
        .expect("pull manifest should succeed");

    assert_eq!(pulled_data, manifest_bytes);
    assert!(
        content_type.contains("manifest"),
        "content type should be a manifest type, got: {}",
        content_type
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_pull_blob() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    client
        .create_repo(&CreateRepoRequest {
            name: "docker-blob-pull".to_string(),
            repo_type: "hosted".to_string(),
            format: "docker".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    let data = vec![1u8; 1024];
    let digest = format!("sha256:{}", sha256_hex(&data));

    client
        .docker_push_blob("docker-blob-pull", "myimg", data.clone(), &digest)
        .await
        .unwrap();

    let pulled = client
        .docker_pull_blob("docker-blob-pull", "myimg", &digest)
        .await
        .expect("pull blob should succeed");

    assert_eq!(pulled, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_push_image_roundtrip() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    client
        .create_repo(&CreateRepoRequest {
            name: "docker-roundtrip".to_string(),
            repo_type: "hosted".to_string(),
            format: "docker".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    // Use the demo docker module to push a full image
    let mut rng = rand_seeded(42);
    demo::docker::push_image(&client, "docker-roundtrip", "test-app", "v1.0.0", &mut rng)
        .await
        .expect("push_image should succeed");

    // Pull the manifest back
    let (manifest_bytes, _ct) = client
        .docker_pull_manifest("docker-roundtrip", "test-app", "v1.0.0")
        .await
        .expect("pull manifest should succeed");

    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("manifest should be valid JSON");
    assert_eq!(manifest["schemaVersion"], 2);
    let layers = manifest["layers"].as_array().unwrap();
    assert!(!layers.is_empty(), "should have at least one layer");
}

// ============================================================
// Docker V2 manifest tests
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_v2_manifest_push_pull() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "dv2-test").await;

    let (manifest_bytes, digest, _, _) =
        push_docker_v2_image(&client, "dv2-test", "myapp", "v1").await;

    // Pull by tag
    let (pulled, ct, dcd) = client
        .docker_pull_manifest_accept(
            "dv2-test",
            "myapp",
            "v1",
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .await
        .unwrap();
    assert_eq!(pulled, manifest_bytes);
    assert_eq!(ct, "application/vnd.docker.distribution.manifest.v2+json");
    assert_eq!(dcd, digest);

    // Pull by digest
    let (pulled2, ct2, _) = client
        .docker_pull_manifest_accept(
            "dv2-test",
            "myapp",
            &digest,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .await
        .unwrap();
    assert_eq!(pulled2, manifest_bytes);
    assert_eq!(ct2, "application/vnd.docker.distribution.manifest.v2+json");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_head_manifest() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "head-mfst").await;

    let (manifest_bytes, digest, _, _) =
        push_oci_image(&client, "head-mfst", "img", "latest").await;

    // HEAD by tag
    let (status, ct, dcd) = client
        .docker_head_manifest("head-mfst", "img", "latest")
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert!(ct.contains("manifest"));
    assert_eq!(dcd, digest);

    // HEAD by digest
    let (status2, _, dcd2) = client
        .docker_head_manifest("head-mfst", "img", &digest)
        .await
        .unwrap();
    assert_eq!(status2, 200);
    assert_eq!(dcd2, digest);

    // HEAD nonexistent
    let (status3, _, _) = client
        .docker_head_manifest("head-mfst", "img", "nonexistent")
        .await
        .unwrap();
    assert_eq!(status3, 404);

    drop(manifest_bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_head_blob() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "head-blob").await;

    let data = vec![42u8; 256];
    let digest = sha256_digest(&data);
    client
        .docker_push_blob("head-blob", "img", data.clone(), &digest)
        .await
        .unwrap();

    // HEAD existing blob
    let (status, size) = client
        .docker_head_blob("head-blob", "img", &digest)
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(size, 256);

    // HEAD missing blob
    let (status2, _) = client
        .docker_head_blob(
            "head-blob",
            "img",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )
        .await
        .unwrap();
    assert_eq!(status2, 404);
}

// ============================================================
// Manifest list & OCI index tests
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_manifest_list_push_pull() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "mlist-test").await;

    // Push two child images
    let (m1_bytes, m1_digest, _, _) = push_oci_image(&client, "mlist-test", "app", "amd64").await;
    let (m2_bytes, m2_digest, _, _) = push_oci_image(&client, "mlist-test", "app", "arm64").await;

    // Build and push manifest list
    let manifest_list = build_manifest_list(&[
        (&m1_digest, m1_bytes.len(), "amd64"),
        (&m2_digest, m2_bytes.len(), "arm64"),
    ]);
    let list_digest = client
        .docker_push_manifest(
            "mlist-test",
            "app",
            "multi",
            &manifest_list,
            "application/vnd.docker.distribution.manifest.list.v2+json",
        )
        .await
        .unwrap();

    // Pull by tag
    let (pulled, ct, dcd) = client
        .docker_pull_manifest_accept(
            "mlist-test",
            "app",
            "multi",
            "application/vnd.docker.distribution.manifest.list.v2+json",
        )
        .await
        .unwrap();
    assert_eq!(
        ct,
        "application/vnd.docker.distribution.manifest.list.v2+json"
    );
    assert_eq!(dcd, list_digest);
    let val: serde_json::Value = serde_json::from_slice(&pulled).unwrap();
    let manifests = val["manifests"].as_array().unwrap();
    assert_eq!(manifests.len(), 2);

    // Pull by digest
    let (pulled2, ct2, _) = client
        .docker_pull_manifest_accept(
            "mlist-test",
            "app",
            &list_digest,
            "application/vnd.docker.distribution.manifest.list.v2+json",
        )
        .await
        .unwrap();
    assert_eq!(pulled2, pulled);
    assert_eq!(
        ct2,
        "application/vnd.docker.distribution.manifest.list.v2+json"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_oci_index_push_pull() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "oci-idx").await;

    let (m1_bytes, m1_digest, _, _) = push_oci_image(&client, "oci-idx", "app", "amd64").await;
    let (m2_bytes, m2_digest, _, _) = push_oci_image(&client, "oci-idx", "app", "arm64").await;

    let oci_index = build_oci_index(&[
        (&m1_digest, m1_bytes.len(), "amd64"),
        (&m2_digest, m2_bytes.len(), "arm64"),
    ]);
    let idx_digest = client
        .docker_push_manifest(
            "oci-idx",
            "app",
            "multi",
            &oci_index,
            "application/vnd.oci.image.index.v1+json",
        )
        .await
        .unwrap();

    let (pulled, ct, dcd) = client
        .docker_pull_manifest_accept(
            "oci-idx",
            "app",
            "multi",
            "application/vnd.oci.image.index.v1+json",
        )
        .await
        .unwrap();
    assert_eq!(ct, "application/vnd.oci.image.index.v1+json");
    assert_eq!(dcd, idx_digest);
    let val: serde_json::Value = serde_json::from_slice(&pulled).unwrap();
    assert_eq!(val["manifests"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_manifest_list_child_refs() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "ml-refs").await;

    // Push child manifests by digest only (no tag)
    let (m1_bytes, m1_digest, _, _) = push_oci_image(&client, "ml-refs", "app", "amd64").await;
    let (m2_bytes, m2_digest, _, _) = push_oci_image(&client, "ml-refs", "app", "arm64").await;

    // Build manifest list referencing them
    let manifest_list = build_manifest_list(&[
        (&m1_digest, m1_bytes.len(), "amd64"),
        (&m2_digest, m2_bytes.len(), "arm64"),
    ]);
    client
        .docker_push_manifest(
            "ml-refs",
            "app",
            "multi",
            &manifest_list,
            "application/vnd.docker.distribution.manifest.list.v2+json",
        )
        .await
        .unwrap();

    // Child manifests should be pullable by digest
    let (_, ct1, _) = client
        .docker_pull_manifest_accept(
            "ml-refs",
            "app",
            &m1_digest,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await
        .unwrap();
    assert!(ct1.contains("manifest"));

    let (_, ct2, _) = client
        .docker_pull_manifest_accept(
            "ml-refs",
            "app",
            &m2_digest,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await
        .unwrap();
    assert!(ct2.contains("manifest"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_manifest_list_with_docker_v2_children() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "ml-v2kids").await;

    let (m1_bytes, m1_digest, _, _) =
        push_docker_v2_image(&client, "ml-v2kids", "app", "amd64").await;
    let (m2_bytes, m2_digest, _, _) =
        push_docker_v2_image(&client, "ml-v2kids", "app", "arm64").await;

    let manifest_list = build_manifest_list(&[
        (&m1_digest, m1_bytes.len(), "amd64"),
        (&m2_digest, m2_bytes.len(), "arm64"),
    ]);
    let list_digest = client
        .docker_push_manifest(
            "ml-v2kids",
            "app",
            "multi",
            &manifest_list,
            "application/vnd.docker.distribution.manifest.list.v2+json",
        )
        .await
        .unwrap();
    assert!(list_digest.starts_with("sha256:"));

    // Pull back the list
    let (pulled, ct, _) = client
        .docker_pull_manifest_accept(
            "ml-v2kids",
            "app",
            "multi",
            "application/vnd.docker.distribution.manifest.list.v2+json",
        )
        .await
        .unwrap();
    assert_eq!(
        ct,
        "application/vnd.docker.distribution.manifest.list.v2+json"
    );
    let val: serde_json::Value = serde_json::from_slice(&pulled).unwrap();
    assert_eq!(val["manifests"].as_array().unwrap().len(), 2);
}

// ============================================================
// Tag operation tests
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_list_tags() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "tags-list").await;

    // Empty repo should return empty tags
    let tags = client.docker_list_tags("tags-list", "app").await.unwrap();
    assert!(tags.is_empty());

    // Push images with different tags
    push_oci_image(&client, "tags-list", "app", "v1").await;
    push_oci_image(&client, "tags-list", "app", "v2").await;
    push_oci_image(&client, "tags-list", "app", "latest").await;

    let mut tags = client.docker_list_tags("tags-list", "app").await.unwrap();
    tags.sort();
    assert_eq!(tags, vec!["latest", "v1", "v2"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_tag_replacement() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "tag-replace").await;

    // Push image A as "latest"
    let (_, digest_a, _, _) = push_oci_image(&client, "tag-replace", "app", "latest").await;

    // Push image B (different content) as "latest"
    let config_b = serde_json::to_vec(&serde_json::json!({
        "architecture": "arm64",
        "os": "linux",
        "rootfs": { "type": "layers", "diff_ids": [] }
    }))
    .unwrap();
    let config_b_digest = sha256_digest(&config_b);
    client
        .docker_push_blob("tag-replace", "app", config_b.clone(), &config_b_digest)
        .await
        .unwrap();
    let manifest_b = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_b_digest,
            "size": config_b.len()
        },
        "layers": []
    }))
    .unwrap();
    let digest_b = client
        .docker_push_manifest(
            "tag-replace",
            "app",
            "latest",
            &manifest_b,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await
        .unwrap();

    assert_ne!(digest_a, digest_b);

    // Pull "latest" should get image B
    let (pulled, _, dcd) = client
        .docker_pull_manifest_accept(
            "tag-replace",
            "app",
            "latest",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await
        .unwrap();
    assert_eq!(dcd, digest_b);
    assert_eq!(pulled, manifest_b);

    // Image A still pullable by digest
    let (pulled_a, _, _) = client
        .docker_pull_manifest_accept(
            "tag-replace",
            "app",
            &digest_a,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await
        .unwrap();
    assert!(!pulled_a.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_catalog() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Create 2 docker repos + 1 raw repo
    make_docker_repo(&client, "cat-docker1").await;
    make_docker_repo(&client, "cat-docker2").await;
    client
        .create_repo(&CreateRepoRequest {
            name: "cat-raw".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    let mut catalog = client.docker_catalog().await.unwrap();
    catalog.sort();
    assert_eq!(catalog, vec!["cat-docker1", "cat-docker2"]);
}

// ============================================================
// Deletion & ref-counting tests
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_manifest_delete_simple() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "del-simple").await;

    let (_, digest, _, _) = push_oci_image(&client, "del-simple", "app", "v1").await;

    let status = client
        .docker_delete_manifest("del-simple", "app", &digest)
        .await
        .unwrap();
    assert_eq!(status, 202);

    // Pull by tag should fail
    let result = client
        .docker_pull_manifest_accept(
            "del-simple",
            "app",
            "v1",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await;
    assert!(result.is_err());

    // Pull by digest should also fail
    let result = client
        .docker_pull_manifest_accept(
            "del-simple",
            "app",
            &digest,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_manifest_delete_shared_blob() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "del-shared").await;

    // Push a shared layer
    let shared_layer = vec![99u8; 128];
    let shared_digest = sha256_digest(&shared_layer);
    client
        .docker_push_blob("del-shared", "app", shared_layer.clone(), &shared_digest)
        .await
        .unwrap();

    // Push image A with the shared layer
    let config_a = serde_json::to_vec(&serde_json::json!({
        "architecture": "amd64", "os": "linux",
        "rootfs": { "type": "layers", "diff_ids": [] }
    }))
    .unwrap();
    let config_a_digest = sha256_digest(&config_a);
    client
        .docker_push_blob("del-shared", "app", config_a.clone(), &config_a_digest)
        .await
        .unwrap();
    let manifest_a = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_a_digest,
            "size": config_a.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": shared_digest,
            "size": 128
        }]
    }))
    .unwrap();
    let digest_a = client
        .docker_push_manifest(
            "del-shared",
            "app",
            "a",
            &manifest_a,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await
        .unwrap();

    // Push image B with the same shared layer
    let config_b = serde_json::to_vec(&serde_json::json!({
        "architecture": "arm64", "os": "linux",
        "rootfs": { "type": "layers", "diff_ids": [] }
    }))
    .unwrap();
    let config_b_digest = sha256_digest(&config_b);
    client
        .docker_push_blob("del-shared", "app", config_b.clone(), &config_b_digest)
        .await
        .unwrap();
    let manifest_b = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_b_digest,
            "size": config_b.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": shared_digest,
            "size": 128
        }]
    }))
    .unwrap();
    let _digest_b = client
        .docker_push_manifest(
            "del-shared",
            "app",
            "b",
            &manifest_b,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await
        .unwrap();

    // Delete manifest A
    let status = client
        .docker_delete_manifest("del-shared", "app", &digest_a)
        .await
        .unwrap();
    assert_eq!(status, 202);

    // Shared layer should still be pullable (referenced by manifest B)
    let pulled = client
        .docker_pull_blob("del-shared", "app", &shared_digest)
        .await
        .unwrap();
    assert_eq!(pulled, shared_layer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_manifest_list_delete_cascading() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "del-cascade").await;

    // Push 2 child manifests (only by digest through tag, but we won't use the tags)
    let (m1_bytes, m1_digest, _, _) = push_oci_image(&client, "del-cascade", "app", "child1").await;
    let (m2_bytes, m2_digest, _, _) = push_oci_image(&client, "del-cascade", "app", "child2").await;

    // Build and push manifest list
    let manifest_list = build_manifest_list(&[
        (&m1_digest, m1_bytes.len(), "amd64"),
        (&m2_digest, m2_bytes.len(), "arm64"),
    ]);
    let list_digest = client
        .docker_push_manifest(
            "del-cascade",
            "app",
            "multi",
            &manifest_list,
            "application/vnd.docker.distribution.manifest.list.v2+json",
        )
        .await
        .unwrap();

    // Delete the manifest list
    let status = client
        .docker_delete_manifest("del-cascade", "app", &list_digest)
        .await
        .unwrap();
    assert_eq!(status, 202);

    // The child manifests had ref_count incremented by the list and by their tags.
    // Deleting the list decrements their ref_count. They still have tag refs,
    // so they should still be accessible by digest (tags keep them alive).
    // This tests the cascading ref-count logic works without crashing.
    // The children's tag refs keep them alive.
    let result = client
        .docker_pull_manifest_accept(
            "del-cascade",
            "app",
            &m1_digest,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await;
    assert!(
        result.is_ok(),
        "child manifest should still exist (tag ref)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_blob_dedup() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "dedup").await;

    let data = vec![77u8; 512];
    let digest = sha256_digest(&data);

    // Push same blob twice
    client
        .docker_push_blob("dedup", "img", data.clone(), &digest)
        .await
        .unwrap();
    client
        .docker_push_blob("dedup", "img", data.clone(), &digest)
        .await
        .unwrap();

    // Pull: should get the same data
    let pulled = client
        .docker_pull_blob("dedup", "img", &digest)
        .await
        .unwrap();
    assert_eq!(pulled, data);
}

// ============================================================
// Cross-repo mount & monolithic upload tests
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_blob_mount() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "mount-src").await;
    make_docker_repo(&client, "mount-dst").await;

    // Push blob to source
    let data = vec![55u8; 256];
    let digest = sha256_digest(&data);
    client
        .docker_push_blob("mount-src", "img", data.clone(), &digest)
        .await
        .unwrap();

    // Mount to destination
    let status = client
        .docker_mount_blob("mount-dst", "img", &digest, "mount-src")
        .await
        .unwrap();
    assert_eq!(status, 201);

    // Pull from destination
    let pulled = client
        .docker_pull_blob("mount-dst", "img", &digest)
        .await
        .unwrap();
    assert_eq!(pulled, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_monolithic_upload() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "mono-up").await;

    let data = vec![88u8; 1024];
    let digest = sha256_digest(&data);

    let status = client
        .docker_monolithic_upload("mono-up", "img", data.clone(), &digest)
        .await
        .unwrap();
    assert_eq!(status, 201);

    // Pull back
    let pulled = client
        .docker_pull_blob("mono-up", "img", &digest)
        .await
        .unwrap();
    assert_eq!(pulled, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_monolithic_upload_bad_digest() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "mono-bad").await;

    let data = vec![88u8; 64];
    let bad_digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    let status = client
        .docker_monolithic_upload("mono-bad", "img", data, bad_digest)
        .await
        .unwrap();
    assert_eq!(status, 400);
}

// ============================================================
// Error case tests
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_push_manifest_invalid_content_type() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "bad-ct").await;

    let manifest = serde_json::to_vec(&serde_json::json!({"schemaVersion": 2})).unwrap();
    let status = client
        .docker_push_manifest_raw("bad-ct", "img", "latest", &manifest, "text/plain")
        .await
        .unwrap();
    assert_eq!(status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_delete_manifest_by_tag_rejected() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "del-tag").await;

    push_oci_image(&client, "del-tag", "app", "latest").await;

    // Delete by tag should be rejected
    let status = client
        .docker_delete_manifest("del-tag", "app", "latest")
        .await
        .unwrap();
    assert_eq!(status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_pull_manifest_not_found() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_docker_repo(&client, "pull-404").await;

    let result = client
        .docker_pull_manifest_accept(
            "pull-404",
            "img",
            "nonexistent",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_docker_operations_on_non_docker_repo() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    client
        .create_repo(&CreateRepoRequest {
            name: "raw-repo".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    // Try Docker V2 ops on a raw repo
    let result = client
        .docker_pull_manifest_accept(
            "raw-repo",
            "img",
            "latest",
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await;
    assert!(result.is_err());

    let (status, _) = client
        .docker_head_blob("raw-repo", "img", "sha256:abc")
        .await
        .unwrap();
    assert_ne!(status, 200);
}

// ============================================================
// Demo tests
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_demo_run_small() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    demo::run(
        &client,
        demo::DemoConfig {
            repos: 1,
            docker_repos: 1,
            artifacts: 5,
            images: 1,
            tags: 1,
            clean: false,
            blob_root: _server
                ._temp_dir
                .path()
                .join("demo-blobs")
                .to_string_lossy()
                .into_owned(),
        },
    )
    .await
    .expect("demo run should succeed");

    // Verify repos were created
    let repos = client.list_repos().await.unwrap();
    assert!(
        repos.len() >= 2,
        "expected at least 2 repos (1 raw + 1 docker)"
    );

    // Verify raw artifacts were uploaded (browse returns dirs at root level)
    let raw_repo = repos.iter().find(|r| r.format == "raw").unwrap();
    // Use search to get a flat list of all artifacts
    let artifacts = client.search_artifacts(&raw_repo.name, "").await.unwrap();
    assert_eq!(artifacts.len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_demo_run_with_clean() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // First run: create some repos
    demo::run(
        &client,
        demo::DemoConfig {
            repos: 1,
            docker_repos: 1,
            artifacts: 3,
            images: 1,
            tags: 1,
            clean: false,
            blob_root: _server
                ._temp_dir
                .path()
                .join("demo-blobs")
                .to_string_lossy()
                .into_owned(),
        },
    )
    .await
    .unwrap();

    let repos_before = client.list_repos().await.unwrap();
    assert!(!repos_before.is_empty());

    // Second run with clean: should delete and recreate
    demo::run(
        &client,
        demo::DemoConfig {
            repos: 1,
            docker_repos: 0,
            artifacts: 2,
            images: 0,
            tags: 0,
            clean: true,
            blob_root: _server
                ._temp_dir
                .path()
                .join("demo-blobs")
                .to_string_lossy()
                .into_owned(),
        },
    )
    .await
    .expect("demo with clean should succeed");

    let repos_after = client.list_repos().await.unwrap();
    // 1 raw + 1 apt + 1 golang + 1 helm + 1 cargo + 1 yum + 1 npm
    assert_eq!(repos_after.len(), 7);
}

// ============================================================
// Bench tests
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bench_raw_upload_by_count() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    runner::run(
        &client,
        runner::BenchConfig {
            scenario: "raw-upload".to_string(),
            concurrency: 2,
            duration: 60,
            count: Some(10),
            artifact_size: 1024,
            layer_size: 4096,
            warmup: 0,
            json: true,
            store: "default".to_string(),
            random_data: false,
            pipeline: 1,
        },
    )
    .await
    .expect("bench raw-upload should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bench_unknown_scenario_errors() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    let result = runner::run(
        &client,
        runner::BenchConfig {
            scenario: "nonexistent-scenario".to_string(),
            concurrency: 1,
            duration: 1,
            count: None,
            artifact_size: 64,
            layer_size: 64,
            warmup: 0,
            json: true,
            store: "default".to_string(),
            random_data: false,
            pipeline: 1,
        },
    )
    .await;

    assert!(result.is_err(), "unknown scenario should error");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Unknown scenario"),
        "error should mention unknown scenario: {}",
        err
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bench_raw_mixed_by_count() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    runner::run(
        &client,
        runner::BenchConfig {
            scenario: "raw-mixed".to_string(),
            concurrency: 2,
            duration: 60,
            count: Some(20),
            artifact_size: 512,
            layer_size: 1024,
            warmup: 0,
            json: true,
            store: "default".to_string(),
            random_data: false,
            pipeline: 1,
        },
    )
    .await
    .expect("bench raw-mixed should succeed");
}

// ============================================================
// Bench: Docker scenarios
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bench_docker_push_by_count() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    runner::run(
        &client,
        runner::BenchConfig {
            scenario: "docker-push".to_string(),
            concurrency: 1,
            duration: 60,
            count: Some(4),
            artifact_size: 512,
            layer_size: 4096,
            warmup: 0,
            json: true,
            store: "default".to_string(),
            random_data: false,
            pipeline: 1,
        },
    )
    .await
    .expect("bench docker-push should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bench_docker_pull_by_count() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Seed a Docker image first so pulls have something to fetch
    let mut rng = rand_seeded(99);
    client
        .create_repo(&CreateRepoRequest {
            name: "bench-docker".to_string(),
            repo_type: "hosted".to_string(),
            format: "docker".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();
    for i in 0..3 {
        demo::docker::push_image(
            &client,
            "bench-docker",
            "bench-image",
            &format!("seed-{}", i),
            &mut rng,
        )
        .await
        .unwrap();
    }

    runner::run(
        &client,
        runner::BenchConfig {
            scenario: "docker-pull".to_string(),
            concurrency: 1,
            duration: 60,
            count: Some(4),
            artifact_size: 512,
            layer_size: 4096,
            warmup: 0,
            json: true,
            store: "default".to_string(),
            random_data: false,
            pipeline: 1,
        },
    )
    .await
    .expect("bench docker-pull should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bench_docker_mixed_by_count() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    runner::run(
        &client,
        runner::BenchConfig {
            scenario: "docker-mixed".to_string(),
            concurrency: 2,
            duration: 60,
            count: Some(6),
            artifact_size: 512,
            layer_size: 4096,
            warmup: 0,
            json: true,
            store: "default".to_string(),
            random_data: false,
            pipeline: 1,
        },
    )
    .await
    .expect("bench docker-mixed should succeed");
}

// ============================================================
// Bench: Raw download scenario
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bench_raw_download_by_count() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    runner::run(
        &client,
        runner::BenchConfig {
            scenario: "raw-download".to_string(),
            concurrency: 2,
            duration: 60,
            count: Some(10),
            artifact_size: 512,
            layer_size: 1024,
            warmup: 0,
            json: true,
            store: "default".to_string(),
            random_data: false,
            pipeline: 1,
        },
    )
    .await
    .expect("bench raw-download should succeed");
}

// ============================================================
// Bench: Warmup and text output paths
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bench_with_warmup() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    runner::run(
        &client,
        runner::BenchConfig {
            scenario: "raw-upload".to_string(),
            concurrency: 1,
            duration: 2,
            count: Some(10),
            artifact_size: 256,
            layer_size: 256,
            warmup: 1,
            json: true,
            store: "default".to_string(),
            random_data: false,
            pipeline: 1,
        },
    )
    .await
    .expect("bench with warmup should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bench_text_output() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    runner::run(
        &client,
        runner::BenchConfig {
            scenario: "raw-upload".to_string(),
            concurrency: 1,
            duration: 60,
            count: Some(5),
            artifact_size: 256,
            layer_size: 256,
            warmup: 0,
            json: false,
            store: "default".to_string(),
            random_data: false,
            pipeline: 1,
        },
    )
    .await
    .expect("bench text output should succeed");
}

// ============================================================
// Client: Error paths
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_upload_raw_to_nonexistent_repo() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    let result = client
        .upload_raw(
            "no-such-repo",
            "file.bin",
            vec![1, 2, 3],
            "application/octet-stream",
        )
        .await;
    assert!(result.is_err(), "upload to nonexistent repo should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("failed"),
        "error should mention failure: {}",
        err
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_download_raw_nonexistent() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Create a repo but don't upload anything
    client
        .create_repo(&CreateRepoRequest {
            name: "dl-err".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();

    let result = client.download_raw("dl-err", "nonexistent.bin").await;
    assert!(result.is_err(), "download of nonexistent path should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("failed"),
        "error should mention failure: {}",
        err
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bearer_token_auto_login() {
    let (_server, client) = setup().await;
    // Do NOT call login() — bearer_token() should auto-login on first API call
    let repos = client.list_repos().await.expect("auto-login should work");
    assert!(repos.is_empty(), "expected no repos initially");
}

// ============================================================
// Helpers
// ============================================================

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn sha256_digest(data: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(data))
}

fn rand_seeded(seed: u64) -> rand_chacha::ChaCha8Rng {
    use rand::SeedableRng;
    rand_chacha::ChaCha8Rng::seed_from_u64(seed)
}

async fn make_docker_repo(client: &DepotClient, name: &str) {
    client
        .create_repo(&CreateRepoRequest {
            name: name.to_string(),
            repo_type: "hosted".to_string(),
            format: "docker".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .expect("create docker repo should succeed");
}

/// Push a minimal OCI image (config + 1 layer + OCI manifest).
/// Returns (manifest_bytes, manifest_digest, config_digest, layer_digests).
async fn push_oci_image(
    client: &DepotClient,
    repo: &str,
    image: &str,
    tag: &str,
) -> (Vec<u8>, String, String, Vec<String>) {
    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "rootfs": { "type": "layers", "diff_ids": [] },
        "config": { "Env": [format!("TAG={tag}")] }
    }))
    .unwrap();
    let config_digest = sha256_digest(&config);
    client
        .docker_push_blob(repo, image, config.clone(), &config_digest)
        .await
        .unwrap();

    let layer = format!("oci-layer-{}-{}-{}", repo, image, tag).into_bytes();
    let layer_digest = sha256_digest(&layer);
    client
        .docker_push_blob(repo, image, layer.clone(), &layer_digest)
        .await
        .unwrap();

    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer_digest,
            "size": layer.len()
        }]
    }))
    .unwrap();
    let digest = client
        .docker_push_manifest(
            repo,
            image,
            tag,
            &manifest,
            "application/vnd.oci.image.manifest.v1+json",
        )
        .await
        .unwrap();

    (manifest, digest, config_digest, vec![layer_digest])
}

/// Push a minimal Docker V2 image (config + 1 layer + Docker V2 manifest).
async fn push_docker_v2_image(
    client: &DepotClient,
    repo: &str,
    image: &str,
    tag: &str,
) -> (Vec<u8>, String, String, Vec<String>) {
    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "rootfs": { "type": "layers", "diff_ids": [] },
        "config": { "Env": [format!("TAG={tag}")] }
    }))
    .unwrap();
    let config_digest = sha256_digest(&config);
    client
        .docker_push_blob(repo, image, config.clone(), &config_digest)
        .await
        .unwrap();

    let layer = format!("docker-v2-layer-{}-{}-{}", repo, image, tag).into_bytes();
    let layer_digest = sha256_digest(&layer);
    client
        .docker_push_blob(repo, image, layer.clone(), &layer_digest)
        .await
        .unwrap();

    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {
            "mediaType": "application/vnd.docker.container.image.v1+json",
            "digest": config_digest,
            "size": config.len()
        },
        "layers": [{
            "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
            "digest": layer_digest,
            "size": layer.len()
        }]
    }))
    .unwrap();
    let digest = client
        .docker_push_manifest(
            repo,
            image,
            tag,
            &manifest,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .await
        .unwrap();

    (manifest, digest, config_digest, vec![layer_digest])
}

fn build_manifest_list(children: &[(&str, usize, &str)]) -> Vec<u8> {
    let manifests: Vec<serde_json::Value> = children
        .iter()
        .map(|(digest, size, arch)| {
            serde_json::json!({
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": digest,
                "size": size,
                "platform": {
                    "architecture": arch,
                    "os": "linux"
                }
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
        "manifests": manifests
    }))
    .unwrap()
}

fn build_oci_index(children: &[(&str, usize, &str)]) -> Vec<u8> {
    let manifests: Vec<serde_json::Value> = children
        .iter()
        .map(|(digest, size, arch)| {
            serde_json::json!({
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": digest,
                "size": size,
                "platform": {
                    "architecture": arch,
                    "os": "linux"
                }
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": manifests
    }))
    .unwrap()
}

// ============================================================
// APT integration tests
// ============================================================

async fn make_apt_repo(client: &DepotClient, name: &str) {
    client
        .create_repo(&CreateRepoRequest {
            name: name.to_string(),
            repo_type: "hosted".to_string(),
            format: "apt".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .expect("create apt repo should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_apt_upload_and_metadata() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_apt_repo(&client, "apt-test").await;

    // Build a synthetic .deb
    let deb =
        depot_format_apt::build_synthetic_deb("hello", "1.0.0", "amd64", "A test package").unwrap();

    // Upload it
    client
        .apt_upload("apt-test", "main", "hello_1.0.0_amd64.deb", deb.clone())
        .await
        .expect("apt upload should succeed");

    // Fetch Release — should trigger on-demand metadata rebuild
    let release = client
        .apt_get("apt-test", "dists/stable/Release")
        .await
        .expect("should get Release");
    let release_text = String::from_utf8(release).unwrap();
    assert!(
        release_text.contains("Suite: stable"),
        "Release should contain Suite"
    );
    assert!(
        release_text.contains("main/binary-amd64/Packages"),
        "Release should reference Packages file"
    );

    // Fetch Packages
    let packages = client
        .apt_get("apt-test", "dists/stable/main/binary-amd64/Packages")
        .await
        .expect("should get Packages");
    let packages_text = String::from_utf8(packages.clone()).unwrap();
    assert!(
        packages_text.contains("Package: hello"),
        "Packages should contain hello"
    );
    assert!(
        packages_text.contains("Version: 1.0.0"),
        "Packages should contain version"
    );

    // Verify Release checksums match actual Packages content
    let packages_sha256 = sha256_hex(&packages);
    assert!(
        release_text.contains(&packages_sha256),
        "Release SHA256 should match Packages content: hash={} release={}",
        packages_sha256,
        release_text
    );

    // Fetch Packages.gz and verify it decompresses to same content
    let packages_gz = client
        .apt_get("apt-test", "dists/stable/main/binary-amd64/Packages.gz")
        .await
        .expect("should get Packages.gz");

    let mut decoder = flate2::read::GzDecoder::new(packages_gz.as_slice());
    let mut decompressed = String::new();
    std::io::Read::read_to_string(&mut decoder, &mut decompressed).unwrap();
    assert_eq!(
        decompressed, packages_text,
        "Packages.gz should decompress to Packages"
    );

    // Download the .deb from pool
    let downloaded = client
        .apt_get("apt-test", "pool/main/h/hello/hello_1.0.0_amd64.deb")
        .await
        .expect("should download .deb from pool");
    assert_eq!(downloaded, deb, "downloaded .deb should match uploaded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_apt_multiple_packages() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_apt_repo(&client, "apt-multi").await;

    // Upload two packages
    let deb1 =
        depot_format_apt::build_synthetic_deb("pkg-alpha", "1.0.0", "amd64", "First package")
            .unwrap();
    let deb2 =
        depot_format_apt::build_synthetic_deb("pkg-beta", "2.0.0", "amd64", "Second package")
            .unwrap();

    client
        .apt_upload("apt-multi", "main", "pkg-alpha_1.0.0_amd64.deb", deb1)
        .await
        .unwrap();
    client
        .apt_upload("apt-multi", "main", "pkg-beta_2.0.0_amd64.deb", deb2)
        .await
        .unwrap();

    // Fetch Packages — should contain both
    let packages = client
        .apt_get("apt-multi", "dists/stable/main/binary-amd64/Packages")
        .await
        .expect("should get Packages");
    let packages_text = String::from_utf8(packages.clone()).unwrap();
    assert!(
        packages_text.contains("Package: pkg-alpha"),
        "should contain pkg-alpha"
    );
    assert!(
        packages_text.contains("Package: pkg-beta"),
        "should contain pkg-beta"
    );

    // Verify Release checksums still match
    let release = client
        .apt_get("apt-multi", "dists/stable/Release")
        .await
        .expect("should get Release");
    let release_text = String::from_utf8(release).unwrap();
    let packages_sha256 = sha256_hex(&packages);
    assert!(
        release_text.contains(&packages_sha256),
        "Release SHA256 should match Packages with both packages"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_apt_proxy_repo() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Create two hosted APT repos
    make_apt_repo(&client, "apt-member1").await;
    make_apt_repo(&client, "apt-member2").await;

    // Upload different packages to each
    let deb1 =
        depot_format_apt::build_synthetic_deb("from-member1", "1.0.0", "amd64", "From member 1")
            .unwrap();
    let deb2 =
        depot_format_apt::build_synthetic_deb("from-member2", "1.0.0", "amd64", "From member 2")
            .unwrap();

    client
        .apt_upload("apt-member1", "main", "from-member1_1.0.0_amd64.deb", deb1)
        .await
        .unwrap();
    client
        .apt_upload("apt-member2", "main", "from-member2_1.0.0_amd64.deb", deb2)
        .await
        .unwrap();

    // Create proxy repo
    client
        .create_repo(&CreateRepoRequest {
            name: "apt-proxy".to_string(),
            repo_type: "proxy".to_string(),
            format: "apt".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: Some(vec!["apt-member1".to_string(), "apt-member2".to_string()]),
            listen: None,
        })
        .await
        .expect("create proxy should succeed");

    // Fetch merged Packages from proxy
    let packages = client
        .apt_get("apt-proxy", "dists/stable/main/binary-amd64/Packages")
        .await
        .expect("should get merged Packages from proxy");
    let packages_text = String::from_utf8(packages).unwrap();
    assert!(
        packages_text.contains("Package: from-member1"),
        "proxy Packages should contain from-member1"
    );
    assert!(
        packages_text.contains("Package: from-member2"),
        "proxy Packages should contain from-member2"
    );

    // Download via proxy
    let downloaded = client
        .apt_get(
            "apt-proxy",
            "pool/main/f/from-member1/from-member1_1.0.0_amd64.deb",
        )
        .await
        .expect("should download via proxy from member1");
    assert!(
        !downloaded.is_empty(),
        "downloaded .deb should not be empty"
    );

    let downloaded2 = client
        .apt_get(
            "apt-proxy",
            "pool/main/f/from-member2/from-member2_1.0.0_amd64.deb",
        )
        .await
        .expect("should download via proxy from member2");
    assert!(
        !downloaded2.is_empty(),
        "downloaded .deb from member2 should not be empty"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_apt_inrelease() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_apt_repo(&client, "apt-signed").await;

    let deb =
        depot_format_apt::build_synthetic_deb("signed-pkg", "1.0.0", "amd64", "A signed package")
            .unwrap();

    client
        .apt_upload("apt-signed", "main", "signed-pkg_1.0.0_amd64.deb", deb)
        .await
        .unwrap();

    // Fetch InRelease
    let in_release = client
        .apt_get("apt-signed", "dists/stable/InRelease")
        .await
        .expect("should get InRelease");
    let in_release_text = String::from_utf8(in_release).unwrap();

    // InRelease should contain Release content (Suite, Components, etc.)
    assert!(
        in_release_text.contains("Suite: stable"),
        "InRelease should contain Suite"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_apt_arch_all_packages() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_apt_repo(&client, "apt-arch-all").await;

    // Upload an "all" architecture package
    let deb_all = depot_format_apt::build_synthetic_deb(
        "common-data",
        "1.0.0",
        "all",
        "Architecture-independent data",
    )
    .unwrap();
    // And an amd64 package
    let deb_amd64 = depot_format_apt::build_synthetic_deb(
        "arch-specific",
        "1.0.0",
        "amd64",
        "Architecture-specific binary",
    )
    .unwrap();

    client
        .apt_upload("apt-arch-all", "main", "common-data_1.0.0_all.deb", deb_all)
        .await
        .unwrap();
    client
        .apt_upload(
            "apt-arch-all",
            "main",
            "arch-specific_1.0.0_amd64.deb",
            deb_amd64,
        )
        .await
        .unwrap();

    // amd64 Packages should contain both the amd64 package AND the "all" package
    let packages = client
        .apt_get("apt-arch-all", "dists/stable/main/binary-amd64/Packages")
        .await
        .expect("should get amd64 Packages");
    let packages_text = String::from_utf8(packages).unwrap();
    assert!(
        packages_text.contains("Package: arch-specific"),
        "amd64 Packages should contain arch-specific"
    );
    assert!(
        packages_text.contains("Package: common-data"),
        "amd64 Packages should contain common-data (arch: all)"
    );
}

// ============================================================
// Check task tests
// ============================================================

/// Helper to get an auth token for task API calls.
async fn get_auth_token(base_url: &str) -> String {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/v1/auth/login", base_url))
        .json(&serde_json::json!({"username": "admin", "password": "admin"}))
        .send()
        .await
        .expect("login request failed");
    let body: Value = resp.json().await.expect("parse login response");
    body["token"].as_str().expect("token field").to_string()
}

// ============================================================
// npm integration tests
// ============================================================

async fn make_npm_repo(client: &DepotClient, name: &str) {
    client
        .create_repo(&CreateRepoRequest {
            name: name.to_string(),
            repo_type: "hosted".to_string(),
            format: "npm".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .expect("create npm repo should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_publish_and_packument() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_npm_repo(&client, "npm-int-hosted").await;

    // Publish a package
    let tarball_data = b"fake-tarball-for-integration-test".to_vec();
    client
        .publish_npm("npm-int-hosted", "hello-world", "1.0.0", &tarball_data)
        .await
        .expect("npm publish should succeed");

    // Publish a second version
    let tarball_v2 = b"fake-tarball-v2".to_vec();
    client
        .publish_npm("npm-int-hosted", "hello-world", "2.0.0", &tarball_v2)
        .await
        .expect("npm publish v2 should succeed");

    // Fetch packument via HTTP
    let http = reqwest::Client::new();
    let token = get_auth_token(&_server.url).await;
    let resp = http
        .get(format!(
            "{}/repository/npm-int-hosted/hello-world",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let packument: Value = resp.json().await.unwrap();

    assert_eq!(packument["name"], "hello-world");
    assert_eq!(packument["dist-tags"]["latest"], "2.0.0");
    assert!(packument["versions"]["1.0.0"].is_object());
    assert!(packument["versions"]["2.0.0"].is_object());

    // Verify tarball URLs point to our server
    let tarball_url = packument["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert!(
        tarball_url.contains("/repository/npm-int-hosted/hello-world/-/"),
        "tarball URL should reference our server: {tarball_url}"
    );
    assert!(tarball_url.ends_with(".tgz"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_download_tarball() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_npm_repo(&client, "npm-int-dl").await;

    let tarball_data = b"integration-download-test-data".to_vec();
    client
        .publish_npm("npm-int-dl", "dl-pkg", "1.0.0", &tarball_data)
        .await
        .unwrap();

    // Download tarball
    let http = reqwest::Client::new();
    let token = get_auth_token(&_server.url).await;
    let resp = http
        .get(format!(
            "{}/repository/npm-int-dl/dl-pkg/-/dl-pkg-1.0.0.tgz",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(content_type, "application/octet-stream");

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), tarball_data.as_slice());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_scoped_package() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_npm_repo(&client, "npm-int-scoped").await;

    let tarball_data = b"scoped-package-content".to_vec();
    client
        .publish_npm("npm-int-scoped", "@myorg/widget", "1.0.0", &tarball_data)
        .await
        .expect("scoped publish should succeed");

    // Fetch scoped packument
    let http = reqwest::Client::new();
    let token = get_auth_token(&_server.url).await;
    let resp = http
        .get(format!(
            "{}/repository/npm-int-scoped/@myorg/widget",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let packument: Value = resp.json().await.unwrap();
    assert_eq!(packument["name"], "@myorg/widget");
    assert_eq!(packument["dist-tags"]["latest"], "1.0.0");

    // Download scoped tarball
    let resp = http
        .get(format!(
            "{}/repository/npm-int-scoped/@myorg/widget/-/widget-1.0.0.tgz",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), tarball_data.as_slice());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_search() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_npm_repo(&client, "npm-int-search").await;

    client
        .publish_npm("npm-int-search", "alpha-pkg", "1.0.0", b"a")
        .await
        .unwrap();
    client
        .publish_npm("npm-int-search", "beta-pkg", "1.0.0", b"b")
        .await
        .unwrap();
    client
        .publish_npm("npm-int-search", "gamma", "1.0.0", b"g")
        .await
        .unwrap();

    let http = reqwest::Client::new();
    let token = get_auth_token(&_server.url).await;

    // Search with text filter
    let resp = http
        .get(format!(
            "{}/repository/npm-int-search/-/v1/search?text=alpha",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let result: Value = resp.json().await.unwrap();
    let objects = result["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0]["package"]["name"], "alpha-pkg");

    // Search with no text (returns all)
    let resp = http
        .get(format!(
            "{}/repository/npm-int-search/-/v1/search",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let result: Value = resp.json().await.unwrap();
    let objects = result["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_proxy_repo() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_npm_repo(&client, "npm-int-member-a").await;
    make_npm_repo(&client, "npm-int-member-b").await;

    client
        .create_repo(&CreateRepoRequest {
            name: "npm-int-proxy".to_string(),
            repo_type: "proxy".to_string(),
            format: "npm".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: Some(vec![
                "npm-int-member-a".to_string(),
                "npm-int-member-b".to_string(),
            ]),
            listen: None,
        })
        .await
        .expect("create npm proxy repo");

    // Publish to member A
    client
        .publish_npm("npm-int-member-a", "shared", "1.0.0", b"from-a")
        .await
        .unwrap();
    // Publish different version to member B
    client
        .publish_npm("npm-int-member-b", "shared", "2.0.0", b"from-b")
        .await
        .unwrap();

    // Fetch through proxy
    let http = reqwest::Client::new();
    let token = get_auth_token(&_server.url).await;
    let resp = http
        .get(format!("{}/repository/npm-int-proxy/shared", _server.url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let packument: Value = resp.json().await.unwrap();
    assert!(packument["versions"]["1.0.0"].is_object());
    assert!(packument["versions"]["2.0.0"].is_object());

    // Download tarball through proxy (from member A)
    let resp = http
        .get(format!(
            "{}/repository/npm-int-proxy/shared/-/shared-1.0.0.tgz",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"from-a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_cache_repo_fetches_from_upstream() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Create a hosted repo as the "upstream"
    make_npm_repo(&client, "npm-upstream").await;

    // Publish packages to the upstream
    client
        .publish_npm("npm-upstream", "cached-pkg", "1.0.0", b"cached-v1-data")
        .await
        .unwrap();
    client
        .publish_npm("npm-upstream", "cached-pkg", "2.0.0", b"cached-v2-data")
        .await
        .unwrap();
    client
        .publish_npm("npm-upstream", "@org/lib", "3.0.0", b"scoped-data")
        .await
        .unwrap();

    // Create a cache repo pointing at the hosted repo as upstream,
    // with upstream_auth so the cache can authenticate to the upstream.
    let upstream_url = format!("{}/repository/npm-upstream", _server.url);
    let http = reqwest::Client::new();
    let token = get_auth_token(&_server.url).await;
    let resp = http
        .post(format!("{}/api/v1/repositories", _server.url))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "name": "npm-cache",
            "repo_type": "cache",
            "format": "npm",
            "store": "default",
            "upstream_url": upstream_url,
            "cache_ttl_secs": 3600,
            "upstream_auth": {
                "username": "admin",
                "password": "admin"
            }
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "create npm cache repo failed: {}",
        resp.status()
    );

    // Fetch packument through cache — should fetch from upstream and cache it
    let resp = http
        .get(format!("{}/repository/npm-cache/cached-pkg", _server.url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "cache packument fetch should succeed");
    let packument: Value = resp.json().await.unwrap();
    assert_eq!(packument["name"], "cached-pkg");
    assert!(
        packument["versions"]["1.0.0"].is_object(),
        "cache should have v1 from upstream"
    );
    assert!(
        packument["versions"]["2.0.0"].is_object(),
        "cache should have v2 from upstream"
    );
    // Tarball URLs should be rewritten to point through cache repo
    let tarball_url = packument["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert!(
        tarball_url.contains("/repository/npm-cache/"),
        "tarball URL should point through cache: {tarball_url}"
    );

    // Fetch scoped package through cache
    let resp = http
        .get(format!("{}/repository/npm-cache/@org/lib", _server.url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "scoped cache fetch should succeed");
    let scoped_packument: Value = resp.json().await.unwrap();
    assert_eq!(scoped_packument["name"], "@org/lib");

    // Package not in upstream → 404 through cache
    let resp = http
        .get(format!(
            "{}/repository/npm-cache/nonexistent-pkg",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        404,
        "missing package through cache should 404"
    );

    // Publish to cache repo should fail
    let result = client
        .publish_npm("npm-cache", "reject-me", "1.0.0", b"data")
        .await;
    assert!(result.is_err(), "publish to cache repo should fail");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_cache_tarball_download() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Create upstream with a package
    make_npm_repo(&client, "npm-up-dl").await;
    client
        .publish_npm("npm-up-dl", "dl-test", "1.0.0", b"upstream-tarball-bytes")
        .await
        .unwrap();

    // Create cache pointing at upstream, with auth
    let upstream_url = format!("{}/repository/npm-up-dl", _server.url);
    let http = reqwest::Client::new();
    let token = get_auth_token(&_server.url).await;
    let resp = http
        .post(format!("{}/api/v1/repositories", _server.url))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "name": "npm-cache-dl",
            "repo_type": "cache",
            "format": "npm",
            "store": "default",
            "upstream_url": upstream_url,
            "cache_ttl_secs": 3600,
            "upstream_auth": {
                "username": "admin",
                "password": "admin"
            }
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    // First, fetch packument to populate cache metadata. The tarball URL is now
    // absolute (npm requires it); simulate a reverse proxy forwarding to this
    // plain-HTTP test server so the emitted origin points back at it (production
    // sits behind a TLS-terminating proxy that forwards `https`).
    let server_port = _server.url.rsplit(':').next().unwrap_or("80");
    let resp = http
        .get(format!("{}/repository/npm-cache-dl/dl-test", _server.url))
        .bearer_auth(&token)
        .header("x-forwarded-proto", "http")
        .header("x-forwarded-port", server_port)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let packument: Value = resp.json().await.unwrap();

    // Extract the tarball URL from the packument — must be a fully-qualified URL.
    let tarball_url = packument["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert!(
        tarball_url.starts_with("http://") || tarball_url.starts_with("https://"),
        "tarball URL must be absolute, got: {tarball_url}"
    );

    // Download tarball through cache
    let full_url = if tarball_url.starts_with("http") {
        tarball_url.to_string()
    } else {
        format!("{}{}", _server.url, tarball_url)
    };
    let resp = http
        .get(&full_url)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "tarball download through cache should succeed"
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(
        body.as_ref(),
        b"upstream-tarball-bytes",
        "cached tarball content should match upstream"
    );

    // Second download should be served from cache (same result)
    let resp = http
        .get(&full_url)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "second download from cache should work");
    let body2 = resp.bytes().await.unwrap();
    assert_eq!(body2.as_ref(), b"upstream-tarball-bytes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_proxy_with_cache_member() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Create upstream hosted repo
    make_npm_repo(&client, "npm-prx-upstream").await;
    client
        .publish_npm("npm-prx-upstream", "from-cache", "1.0.0", b"via-cache")
        .await
        .unwrap();

    // Create a cache repo pointing at upstream, with auth
    let upstream_url = format!("{}/repository/npm-prx-upstream", _server.url);
    let http2 = reqwest::Client::new();
    let token2 = get_auth_token(&_server.url).await;
    let resp = http2
        .post(format!("{}/api/v1/repositories", _server.url))
        .bearer_auth(&token2)
        .json(&serde_json::json!({
            "name": "npm-prx-cache",
            "repo_type": "cache",
            "format": "npm",
            "store": "default",
            "upstream_url": upstream_url,
            "cache_ttl_secs": 3600,
            "upstream_auth": {
                "username": "admin",
                "password": "admin"
            }
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    // Create a hosted repo for direct packages
    make_npm_repo(&client, "npm-prx-local").await;
    client
        .publish_npm("npm-prx-local", "local-only", "1.0.0", b"local-data")
        .await
        .unwrap();

    // Create proxy with both cache and hosted members
    client
        .create_repo(&CreateRepoRequest {
            name: "npm-prx-group".to_string(),
            repo_type: "proxy".to_string(),
            format: "npm".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: Some(vec![
                "npm-prx-local".to_string(),
                "npm-prx-cache".to_string(),
            ]),
            listen: None,
        })
        .await
        .unwrap();

    let http = reqwest::Client::new();
    let token = get_auth_token(&_server.url).await;

    // Fetch local-only package through proxy
    let resp = http
        .get(format!(
            "{}/repository/npm-prx-group/local-only",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let packument: Value = resp.json().await.unwrap();
    assert_eq!(packument["name"], "local-only");

    // Download local package tarball through proxy
    let resp = http
        .get(format!(
            "{}/repository/npm-prx-group/local-only/-/local-only-1.0.0.tgz",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"local-data");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_not_found_cases() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();
    make_npm_repo(&client, "npm-int-nf").await;

    let http = reqwest::Client::new();
    let token = get_auth_token(&_server.url).await;

    // Packument not found
    let resp = http
        .get(format!("{}/repository/npm-int-nf/no-such-pkg", _server.url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Tarball not found
    let resp = http
        .get(format!(
            "{}/repository/npm-int-nf/no-such-pkg/-/no-such-pkg-1.0.0.tgz",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Nonexistent repo
    let resp = http
        .get(format!("{}/repository/no-such-repo/some-pkg", _server.url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_npm_demo_seeding() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Run the npm demo seeding function directly
    depot_bench::demo::npm::seed_npm_demo(&client)
        .await
        .expect("npm demo seeding should succeed");

    // Verify the npm-packages repo was created and has artifacts
    let http = reqwest::Client::new();
    let token = get_auth_token(&_server.url).await;

    // Check that packages were published
    let resp = http
        .get(format!(
            "{}/repository/npm-packages/-/v1/search",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let result: Value = resp.json().await.unwrap();
    let objects = result["objects"].as_array().unwrap();
    assert!(
        objects.len() >= 3,
        "demo should seed at least 3 packages, got {}",
        objects.len()
    );

    // Verify a specific package has multiple versions
    let resp = http
        .get(format!(
            "{}/repository/npm-packages/hello-world",
            _server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let packument: Value = resp.json().await.unwrap();
    let versions = packument["versions"].as_object().unwrap();
    assert!(
        versions.len() >= 2,
        "hello-world should have at least 2 versions"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_task_clean() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Seed some data so there are blobs to check.
    client
        .create_repo(&CreateRepoRequest {
            name: "check-test".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();
    client
        .upload_raw(
            "check-test",
            "file1.txt",
            b"hello world".to_vec(),
            "application/octet-stream",
        )
        .await
        .unwrap();
    client
        .upload_raw(
            "check-test",
            "file2.txt",
            b"second file".to_vec(),
            "application/octet-stream",
        )
        .await
        .unwrap();

    let token = get_auth_token(&_server.url).await;
    let http = reqwest::Client::new();

    // Start check.
    let resp = http
        .post(format!("{}/api/v1/tasks/check", _server.url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let task: Value = resp.json().await.unwrap();
    let task_id = task["id"].as_str().unwrap().to_string();

    // Poll until complete.
    let mut attempts = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let resp = http
            .get(format!("{}/api/v1/tasks/{}", _server.url, task_id))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let info: Value = resp.json().await.unwrap();
        let status = info["status"].as_str().unwrap();
        if status == "completed" {
            // Check result should be clean.
            let result = &info["result"];
            assert_eq!(result["type"], "check");
            assert_eq!(result["mismatched"], 0);
            assert_eq!(result["missing"], 0);
            assert!(result["passed"].as_u64().unwrap() > 0);
            break;
        }
        assert_ne!(status, "failed", "check task failed: {:?}", info["error"]);
        attempts += 1;
        assert!(attempts < 60, "check task did not complete in time");
    }

    // List tasks.
    let resp = http
        .get(format!("{}/api/v1/tasks", _server.url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(!list.is_empty());

    // Delete task.
    let resp = http
        .delete(format!("{}/api/v1/tasks/{}", _server.url, task_id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // Verify deleted.
    let resp = http
        .get(format!("{}/api/v1/tasks/{}", _server.url, task_id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_task_conflict() {
    let (_server, client) = setup().await;
    client.login().await.unwrap();

    // Seed data so the check takes a measurable amount of time.
    client
        .create_repo(&CreateRepoRequest {
            name: "conflict-test".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();
    for i in 0..20 {
        client
            .upload_raw(
                "conflict-test",
                &format!("file{}.txt", i),
                format!("content-{}", i).into_bytes(),
                "application/octet-stream",
            )
            .await
            .unwrap();
    }

    let token = get_auth_token(&_server.url).await;
    let http = reqwest::Client::new();

    // Start first check.
    let resp = http
        .post(format!("{}/api/v1/tasks/check", _server.url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Immediately try second — should be 409 since first is still pending/running.
    let resp = http
        .post(format!("{}/api/v1/tasks/check", _server.url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_check_task_detects_corruption() {
    let server = TestServer::start_with_kv(kv_config_from_env()).await;
    let client =
        DepotClient::new(&server.url, "admin", "admin", false).expect("failed to create client");
    client.login().await.unwrap();

    // Create repo and upload a file.
    client
        .create_repo(&CreateRepoRequest {
            name: "corrupt-test".to_string(),
            repo_type: "hosted".to_string(),
            format: "raw".to_string(),
            store: "default".to_string(),
            upstream_url: None,
            cache_ttl_secs: None,
            members: None,
            listen: None,
        })
        .await
        .unwrap();
    let data = b"this is the original content";
    client
        .upload_raw(
            "corrupt-test",
            "important.dat",
            data.to_vec(),
            "application/octet-stream",
        )
        .await
        .unwrap();

    // Corrupt the blob file on disk.
    // First, find the blob hash via the artifact metadata.
    let token = get_auth_token(&server.url).await;
    let http = reqwest::Client::new();
    let resp = http
        .get(format!(
            "{}/api/v1/repositories/corrupt-test/artifacts",
            server.url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    let browse: Value = resp.json().await.unwrap();
    let artifacts = browse["artifacts"].as_array().unwrap();
    assert!(!artifacts.is_empty());
    let blob_id = artifacts[0]["blob_id"].as_str().unwrap().to_string();

    // Find and corrupt the blob file in the temp dir.
    // Blob path layout uses 2-char prefixes at depth 2: ab/cd/<blob_id>
    let blob_dir = server._temp_dir.path().join("blobs");
    let blob_path = blob_dir
        .join(&blob_id[..2])
        .join(&blob_id[2..4])
        .join(&blob_id);
    assert!(blob_path.exists(), "blob file should exist");
    std::fs::write(&blob_path, b"CORRUPTED DATA").unwrap();

    // Start check.
    let resp = http
        .post(format!("{}/api/v1/tasks/check", server.url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let task: Value = resp.json().await.unwrap();
    let task_id = task["id"].as_str().unwrap().to_string();

    // Poll until complete.
    let mut attempts = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let resp = http
            .get(format!("{}/api/v1/tasks/{}", server.url, task_id))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let info: Value = resp.json().await.unwrap();
        let status = info["status"].as_str().unwrap();
        if status == "completed" {
            let result = &info["result"];
            assert_eq!(result["type"], "check");
            // Should detect the corruption.
            let mismatched = result["mismatched"].as_u64().unwrap();
            assert!(
                mismatched > 0,
                "check should detect mismatched blob, got: {:?}",
                result
            );
            assert!(!result["findings"].as_array().unwrap().is_empty());
            break;
        }
        assert_ne!(status, "failed", "check task failed: {:?}", info["error"]);
        attempts += 1;
        assert!(attempts < 60, "check task did not complete in time");
    }
}
