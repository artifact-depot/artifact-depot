// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use anyhow::Result;
use rand::seq::IndexedRandom;
use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use super::names;
use super::raw;
use crate::client::DepotClient;

type PathGen = fn(&mut dyn rand::RngCore, usize) -> (String, &'static str);

#[derive(Clone, Copy)]
enum Op {
    RawGet,
    RawHead,
    RawPut,
    RawDelete,
    RawList,
    DockerPull,
    DockerHead,
    DockerHeadBlob,
}

const WEIGHTED_OPS: &[(Op, u32)] = &[
    (Op::RawGet, 30),
    (Op::RawHead, 15),
    (Op::RawPut, 15),
    (Op::RawList, 10),
    (Op::DockerPull, 10),
    (Op::DockerHead, 10),
    (Op::RawDelete, 5),
    (Op::DockerHeadBlob, 5),
];

fn pick_op(rng: &mut ChaCha8Rng) -> Op {
    let total: u32 = WEIGHTED_OPS.iter().map(|(_, w)| w).sum();
    let mut pick = rng.random_range(0..total);
    for (op, weight) in WEIGHTED_OPS {
        if pick < *weight {
            return *op;
        }
        pick -= weight;
    }
    WEIGHTED_OPS.last().map(|(op, _)| *op).unwrap_or(Op::RawGet)
}

struct DockerTarget {
    repo: String,
    image: String,
    tag: String,
}

/// Detect 401/auth errors, clear the stale JWT, and try to re-login.
/// Updates `auth_failures` so the caller can back off on persistent failures.
async fn handle_auth_error(client: &DepotClient, e: &anyhow::Error, auth_failures: &mut u32) {
    let msg = e.to_string();
    if msg.contains("401") || msg.contains("Unauthorized") {
        client.clear_token().await;
        match client.login().await {
            Ok(()) => {
                tracing::info!("trickle: re-authenticated");
                *auth_failures = 0;
            }
            Err(_) => {
                *auth_failures += 1;
            }
        }
    }
}

/// Run slow background activity against a seeded depot instance, forever.
pub async fn run_trickle(client: &DepotClient) -> Result<()> {
    client.login().await?;
    tracing::info!("trickle: authenticated, discovering seeded state...");

    // Discover raw repos and their artifacts (search returns all recursively)
    let repos = client.list_repos().await?;

    let mut raw_repos: Vec<String> = Vec::new();
    let mut raw_pool: Vec<(String, String)> = Vec::new(); // (repo, path)

    for repo in &repos {
        if repo.format == "raw" {
            raw_repos.push(repo.name.clone());
            if let Ok(artifacts) = client.search_artifacts(&repo.name, "").await {
                for a in artifacts {
                    raw_pool.push((repo.name.clone(), a.path));
                }
            }
        }
    }

    // Discover docker repos, images, and tags. The per-repo `_catalog`
    // returns image names within that repo, per Docker Registry V2 semantics.
    let mut docker_pool: Vec<DockerTarget> = Vec::new();

    for repo in &repos {
        if repo.format == "docker" {
            if let Ok(images) = client.docker_repo_catalog(&repo.name).await {
                for image in &images {
                    if let Ok(tags) = client.docker_list_tags(&repo.name, image).await {
                        for tag in tags {
                            docker_pool.push(DockerTarget {
                                repo: repo.name.clone(),
                                image: image.clone(),
                                tag,
                            });
                        }
                    }
                }
            }
        }
    }

    tracing::info!(
        "trickle: found {} raw artifacts across {} repos, {} docker tags",
        raw_pool.len(),
        raw_repos.len(),
        docker_pool.len(),
    );

    let has_docker = !docker_pool.is_empty();
    let mut rng = ChaCha8Rng::from_os_rng();

    let path_gens: Vec<PathGen> = vec![
        names::gen_release_path,
        names::gen_config_path,
        names::gen_doc_path,
    ];
    let mut put_index: usize = 0;
    let mut auth_failures: u32 = 0;

    loop {
        // Back off when authentication is persistently failing
        if auth_failures > 0 {
            let backoff = Duration::from_secs(30.min(5 * auth_failures as u64));
            tokio::time::sleep(backoff).await;
            client.clear_token().await;
            match client.login().await {
                Ok(()) => {
                    tracing::info!("trickle: re-authenticated after backoff");
                    auth_failures = 0;
                }
                Err(_) => {
                    auth_failures += 1;
                    continue;
                }
            }
        }

        let op = pick_op(&mut rng);

        // Skip docker ops if no docker content exists
        if !has_docker && matches!(op, Op::DockerPull | Op::DockerHead | Op::DockerHeadBlob) {
            tokio::time::sleep(Duration::from_secs(rng.random_range(2..=5))).await;
            continue;
        }

        // Skip raw read/delete ops if pool is empty
        if raw_pool.is_empty()
            && matches!(op, Op::RawGet | Op::RawHead | Op::RawDelete | Op::RawList)
        {
            tokio::time::sleep(Duration::from_secs(rng.random_range(2..=5))).await;
            continue;
        }

        match op {
            Op::RawGet => {
                let (repo, path) = raw_pool.choose(&mut rng).expect("pool not empty").clone();
                match client.download_raw(&repo, &path).await {
                    Ok(data) => {
                        tracing::info!("trickle: GET {}/{} ({} bytes)", repo, path, data.len())
                    }
                    Err(e) => {
                        handle_auth_error(client, &e, &mut auth_failures).await;
                        tracing::warn!("trickle: GET {}/{} failed: {}", repo, path, e);
                    }
                }
            }

            Op::RawHead => {
                let (repo, path) = raw_pool.choose(&mut rng).expect("pool not empty").clone();
                match client.head_raw(&repo, &path).await {
                    Ok((status, size)) => {
                        tracing::info!(
                            "trickle: HEAD {}/{} -> {} ({} bytes)",
                            repo,
                            path,
                            status,
                            size
                        )
                    }
                    Err(e) => {
                        handle_auth_error(client, &e, &mut auth_failures).await;
                        tracing::warn!("trickle: HEAD {}/{} failed: {}", repo, path, e);
                    }
                }
            }

            Op::RawPut => {
                if let Some(repo) = raw_repos.choose(&mut rng) {
                    let gen = path_gens[put_index % path_gens.len()];
                    put_index += 1;
                    let (path, content_type) = gen(&mut rng, put_index);
                    let content = raw::generate_content(&mut rng, content_type, &path);
                    let size = content.len();
                    let repo = repo.clone();
                    match client.upload_raw(&repo, &path, content, content_type).await {
                        Ok(()) => {
                            tracing::info!("trickle: PUT {}/{} ({} bytes)", repo, path, size);
                            raw_pool.push((repo, path));
                        }
                        Err(e) => {
                            handle_auth_error(client, &e, &mut auth_failures).await;
                            tracing::warn!("trickle: PUT {}/{} failed: {}", repo, path, e);
                        }
                    }
                }
            }

            Op::RawDelete => {
                if raw_pool.len() < 20 {
                    tokio::time::sleep(Duration::from_secs(rng.random_range(2..=5))).await;
                    continue;
                }
                let idx = rng.random_range(0..raw_pool.len());
                let (repo, path) = raw_pool[idx].clone();
                match client.delete_raw(&repo, &path).await {
                    Ok(()) => {
                        tracing::info!("trickle: DELETE {}/{}", repo, path);
                        raw_pool.swap_remove(idx);
                    }
                    Err(e) => {
                        handle_auth_error(client, &e, &mut auth_failures).await;
                        tracing::warn!("trickle: DELETE {}/{} failed: {}", repo, path, e);
                    }
                }
            }

            Op::RawList => {
                if let Some(repo) = raw_repos.choose(&mut rng) {
                    match client.search_artifacts(repo, "").await {
                        Ok(list) => {
                            tracing::info!("trickle: LIST {} ({} artifacts)", repo, list.len())
                        }
                        Err(e) => {
                            handle_auth_error(client, &e, &mut auth_failures).await;
                            tracing::warn!("trickle: LIST {} failed: {}", repo, e);
                        }
                    }
                }
            }

            Op::DockerPull => {
                let target = docker_pool.choose(&mut rng).expect("docker pool not empty");
                match client
                    .docker_pull_manifest(&target.repo, &target.image, &target.tag)
                    .await
                {
                    Ok((manifest, _)) => tracing::info!(
                        "trickle: DOCKER PULL {}/{}:{} ({} bytes)",
                        target.repo,
                        target.image,
                        target.tag,
                        manifest.len()
                    ),
                    Err(e) => tracing::warn!(
                        "trickle: DOCKER PULL {}/{}:{} failed: {}",
                        target.repo,
                        target.image,
                        target.tag,
                        e
                    ),
                }
            }

            Op::DockerHead => {
                let target = docker_pool.choose(&mut rng).expect("docker pool not empty");
                match client
                    .docker_head_manifest(&target.repo, &target.image, &target.tag)
                    .await
                {
                    Ok((status, ct, digest)) => tracing::info!(
                        "trickle: DOCKER HEAD {}/{}:{} -> {} {} {}",
                        target.repo,
                        target.image,
                        target.tag,
                        status,
                        ct,
                        digest
                    ),
                    Err(e) => tracing::warn!(
                        "trickle: DOCKER HEAD {}/{}:{} failed: {}",
                        target.repo,
                        target.image,
                        target.tag,
                        e
                    ),
                }
            }

            Op::DockerHeadBlob => {
                let target = docker_pool.choose(&mut rng).expect("docker pool not empty");
                match client
                    .docker_pull_manifest(&target.repo, &target.image, &target.tag)
                    .await
                {
                    Ok((manifest_bytes, _)) => {
                        if let Ok(manifest) =
                            serde_json::from_slice::<serde_json::Value>(&manifest_bytes)
                        {
                            if let Some(layers) = manifest["layers"].as_array() {
                                if let Some(digest) =
                                    layers.choose(&mut rng).and_then(|l| l["digest"].as_str())
                                {
                                    match client
                                        .docker_head_blob(&target.repo, &target.image, digest)
                                        .await
                                    {
                                        Ok((status, size)) => tracing::info!(
                                            "trickle: DOCKER HEAD blob {}/{} {} -> {} ({} bytes)",
                                            target.repo,
                                            target.image,
                                            digest,
                                            status,
                                            size
                                        ),
                                        Err(e) => tracing::warn!(
                                            "trickle: DOCKER HEAD blob {}/{} failed: {}",
                                            target.repo,
                                            target.image,
                                            e
                                        ),
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => tracing::warn!(
                        "trickle: DOCKER HEAD blob {}/{} manifest fetch failed: {}",
                        target.repo,
                        target.image,
                        e
                    ),
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(rng.random_range(2..=5))).await;
    }
}
