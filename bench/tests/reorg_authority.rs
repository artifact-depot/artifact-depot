// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! End-to-end test for `reorg` classify verification against the upstream
//! authority. Boots two real depot instances over HTTP: a local depot with
//! source repos and a cache repo, and a second depot acting as the upstream
//! authority ([check_authority]). Verifies that released insight-class tags
//! are resolved against the authority directly — deleting digest-identical
//! source copies, leaving upstream-absent ones — and that the local cache
//! repo is never warmed as a side effect.

use depot_bench::client::DepotClient;
use depot_bench::reorg::{run, ReorgConfig};
use depot_core::store::kv::Pagination;
use depot_test_support::{test_tempdir, TestApp};

/// Serve a `TestApp`'s router on an ephemeral local port; returns its base URL.
async fn serve(app: &TestApp) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router.clone();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// Count a repo's artifact records straight from KV — no HTTP, so the check
/// itself cannot warm a cache repo.
async fn artifact_count(app: &TestApp, repo: &str) -> usize {
    depot_core::service::list_artifacts(
        app.state.repo.kv.as_ref(),
        repo,
        "",
        &Pagination::default(),
    )
    .await
    .unwrap()
    .items
    .len()
}

async fn head_status(client: &DepotClient, repo: &str, image: &str, tag: &str) -> u16 {
    let (status, _, _) = client.docker_head_manifest(repo, image, tag).await.unwrap();
    status
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn classify_verifies_against_authority_without_warming_cache() {
    // --- Upstream authority: a second depot holding the canonical repos.
    let authority = TestApp::new().await;
    authority.create_docker_repo("docker-external").await;
    authority.create_docker_repo("docker-release").await;

    let released_cfg: &[u8] = br#"{"created":"2026-01-02T03:04:05Z"}"#;
    let cfg_digest = authority
        .push_docker_blob_ns("docker-external", "acme/app", released_cfg)
        .await;
    let released_manifest = TestApp::make_manifest(&cfg_digest, &[]);
    authority
        .push_docker_manifest_ns("docker-external", "acme/app", "1.2.3", &released_manifest)
        .await;

    let authority_url = serve(&authority).await;

    // --- Local depot: a source repo holding (a) a copy of the released tag
    // with the identical digest, (b) a released tag the upstream lacks, and
    // (c) an aux-class image — plus the cache repo that must stay cold.
    let local = TestApp::new().await;
    local.create_docker_repo("docker-internal").await;
    local.create_docker_repo("docker-release-aux").await;
    local
        .create_docker_cache_repo("docker-insight", &authority_url, 300)
        .await;

    let local_cfg_digest = local
        .push_docker_blob_ns("docker-internal", "acme/app", released_cfg)
        .await;
    assert_eq!(cfg_digest, local_cfg_digest);
    local
        .push_docker_manifest_ns("docker-internal", "acme/app", "1.2.3", &released_manifest)
        .await;

    let missing_cfg: &[u8] = br#"{"created":"2026-02-03T04:05:06Z"}"#;
    let missing_digest = local
        .push_docker_blob_ns("docker-internal", "acme/app", missing_cfg)
        .await;
    let missing_manifest = TestApp::make_manifest(&missing_digest, &[]);
    local
        .push_docker_manifest_ns("docker-internal", "acme/app", "9.9.9", &missing_manifest)
        .await;

    let builder_cfg_digest = local
        .push_docker_blob_ns("docker-internal", "acme/builder", missing_cfg)
        .await;
    let builder_manifest = TestApp::make_manifest(&builder_cfg_digest, &[]);
    local
        .push_docker_manifest_ns(
            "docker-internal",
            "acme/builder",
            "1.2.3",
            &builder_manifest,
        )
        .await;

    let local_url = serve(&local).await;

    // --- Rules: a single classify rule. Released insight-class tags verify
    // against the authority; aux-class images move to the aux repo.
    let dir = test_tempdir();
    let rules_path = dir.path().join("rules.toml");
    std::fs::write(
        &rules_path,
        format!(
            r#"
[patterns]
released = '\d+\.\d+\.\d+'

[check_authority]
cache_repo     = "docker-insight"
upstream_url   = "{authority_url}"
upstream_repos = ["docker-external", "docker-release"]

[[group]]
format               = "docker"
source_repos         = ["docker-internal"]
first_party_prefixes = ["acme/"]

  [[group.rule]]
  match          = "released"
  action         = "classify"
  aux_dest       = "docker-release-aux"
  insight_repo   = "docker-insight"
  insight_images = ["acme/app"]
  aux_images     = ["acme/builder"]
"#
        ),
    )
    .unwrap();

    // Authority credentials come from the environment (never the rules file).
    std::env::set_var("UPSTREAM_USERNAME", "admin");
    std::env::set_var("UPSTREAM_PASSWORD", "password");

    let client = DepotClient::new(&local_url, "admin", "password", false).unwrap();
    let cfg = |apply: Option<Vec<String>>| ReorgConfig {
        rules_path: rules_path.to_str().unwrap().to_string(),
        apply,
        copy: false,
        insecure: false,
        verbose: true,
    };

    // --- Dry run: plans only. Nothing may change, and in particular the
    // authority consult must not have pulled anything through the cache.
    run(&client, cfg(None)).await.unwrap();

    assert_eq!(
        head_status(&client, "docker-internal", "acme/app", "1.2.3").await,
        200
    );
    assert_eq!(
        head_status(&client, "docker-internal", "acme/app", "9.9.9").await,
        200
    );
    assert_eq!(
        head_status(&client, "docker-internal", "acme/builder", "1.2.3").await,
        200
    );
    assert_eq!(
        artifact_count(&local, "docker-insight").await,
        0,
        "dry run must not warm the cache repo"
    );

    // --- Apply everything the plan allows.
    run(&client, cfg(Some(vec!["all".to_string()])))
        .await
        .unwrap();

    // Digest-identical released copy: verified redundant against the
    // authority and deleted from the source repo.
    assert_eq!(
        head_status(&client, "docker-internal", "acme/app", "1.2.3").await,
        404,
        "redundant copy (identical digest upstream) should be deleted"
    );
    // Upstream-absent released copy: possibly the only copy anywhere —
    // flagged, never deleted.
    assert_eq!(
        head_status(&client, "docker-internal", "acme/app", "9.9.9").await,
        200,
        "upstream-absent copy must be left in place"
    );
    // Aux-class image: moved to the aux repo.
    assert_eq!(
        head_status(&client, "docker-release-aux", "acme/builder", "1.2.3").await,
        200,
        "aux-class image should move to the aux repo"
    );
    assert_eq!(
        head_status(&client, "docker-internal", "acme/builder", "1.2.3").await,
        404,
        "aux-class image should leave the source repo"
    );
    // The no-warming guarantee holds through apply as well.
    assert_eq!(
        artifact_count(&local, "docker-insight").await,
        0,
        "authority verification must never pull through the cache repo"
    );
}
