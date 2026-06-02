// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use tokio::sync::{mpsc, Semaphore};

use super::scenario::{pick_op, OpType, WeightedOp};
use super::stats::OpResult;
use crate::client::DepotClient;
use crate::demo;

pub struct WorkerConfig {
    pub worker_id: usize,
    pub ops: Vec<WeightedOp>,
    pub client: DepotClient,
    pub repo_name: String,
    pub docker_repo: String,
    pub docker_image: String,
    pub artifact_size: usize,
    pub layer_size: usize,
    pub cancel: Arc<AtomicBool>,
    pub op_counter: Arc<AtomicU64>,
    pub max_ops: Option<u64>,
    pub seeded_paths: Arc<Vec<String>>,
    pub seeded_tags: Arc<Vec<String>>,
    pub random_data: bool,
    pub pipeline: usize,
}

pub async fn run_worker(cfg: WorkerConfig, tx: mpsc::UnboundedSender<OpResult>) {
    if cfg.pipeline <= 1 {
        run_worker_sequential(cfg, tx).await;
    } else {
        run_worker_pipelined(cfg, tx).await;
    }
}

async fn run_worker_sequential(cfg: WorkerConfig, tx: mpsc::UnboundedSender<OpResult>) {
    let mut rng = ChaCha8Rng::seed_from_u64(cfg.worker_id as u64 * 1000 + 7);
    let mut op_index = 0u64;

    while !cfg.cancel.load(Ordering::Relaxed) {
        if let Some(max) = cfg.max_ops {
            let current = cfg.op_counter.fetch_add(1, Ordering::Relaxed);
            if current >= max {
                break;
            }
        }

        let op = pick_op(&cfg.ops, rng.random());
        let start = Instant::now();

        let (bytes, success) = match op {
            OpType::RawUpload => execute_raw_upload(&cfg, &mut rng, op_index).await,
            OpType::RawDownload => execute_raw_download(&cfg, &mut rng).await,
            OpType::RawDelete => execute_raw_delete(&cfg, &mut rng, op_index).await,
            OpType::RawList => execute_raw_list(&cfg).await,
            OpType::DockerPush => execute_docker_push(&cfg, &mut rng, op_index).await,
            OpType::DockerPull => execute_docker_pull(&cfg, &mut rng).await,
        };

        let duration = start.elapsed();
        let _ = tx.send(OpResult {
            op_type: op.name().to_string(),
            duration,
            bytes,
            success,
        });

        op_index += 1;
    }
}

async fn run_worker_pipelined(cfg: WorkerConfig, tx: mpsc::UnboundedSender<OpResult>) {
    let sem = Arc::new(Semaphore::new(cfg.pipeline));
    let cancel = cfg.cancel.clone();
    let op_counter = cfg.op_counter.clone();
    let max_ops = cfg.max_ops;
    let ops = cfg.ops.clone();
    let client = cfg.client.clone();
    let repo_name = cfg.repo_name.clone();
    let artifact_size = cfg.artifact_size;
    let random_data = cfg.random_data;
    let worker_id = cfg.worker_id;
    let seeded_paths = cfg.seeded_paths.clone();
    let seeded_tags = cfg.seeded_tags.clone();
    let docker_repo = cfg.docker_repo.clone();
    let docker_image = cfg.docker_image.clone();
    let layer_size = cfg.layer_size;

    // Pre-generate an RNG to produce per-request seeds.
    let mut seed_rng = ChaCha8Rng::seed_from_u64(worker_id as u64 * 1000 + 7);
    let op_index = AtomicU64::new(0);

    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if let Some(max) = max_ops {
            let current = op_counter.fetch_add(1, Ordering::Relaxed);
            if current >= max {
                break;
            }
        }

        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };

        let idx = op_index.fetch_add(1, Ordering::Relaxed);
        let op_pick: u32 = seed_rng.random();
        let request_seed: u64 = seed_rng.random();
        let op = pick_op(&ops, op_pick);
        let tx = tx.clone();
        let client = client.clone();
        let repo_name = repo_name.clone();
        let seeded_paths = seeded_paths.clone();
        let seeded_tags = seeded_tags.clone();
        let docker_repo = docker_repo.clone();
        let docker_image = docker_image.clone();

        tokio::spawn(async move {
            let mut rng = ChaCha8Rng::seed_from_u64(request_seed);
            let start = Instant::now();

            let inner_cfg = PipelinedRequestCtx {
                worker_id,
                client: &client,
                repo_name: &repo_name,
                artifact_size,
                random_data,
                seeded_paths: &seeded_paths,
                seeded_tags: &seeded_tags,
                docker_repo: &docker_repo,
                docker_image: &docker_image,
                layer_size,
            };

            let (bytes, success) = match op {
                OpType::RawUpload => pipe_raw_upload(&inner_cfg, &mut rng, idx).await,
                OpType::RawDownload => pipe_raw_download(&inner_cfg, &mut rng).await,
                OpType::RawDelete => pipe_raw_delete(&inner_cfg, &mut rng, idx).await,
                OpType::RawList => pipe_raw_list(&inner_cfg).await,
                OpType::DockerPush => pipe_docker_push(&inner_cfg, &mut rng, idx).await,
                OpType::DockerPull => pipe_docker_pull(&inner_cfg, &mut rng).await,
            };

            let duration = start.elapsed();
            let _ = tx.send(OpResult {
                op_type: op.name().to_string(),
                duration,
                bytes,
                success,
            });

            drop(permit);
        });
    }

    // Wait for all in-flight requests to finish.
    let _ = sem.acquire_many(cfg.pipeline as u32).await;
}

struct PipelinedRequestCtx<'a> {
    worker_id: usize,
    client: &'a DepotClient,
    repo_name: &'a str,
    artifact_size: usize,
    random_data: bool,
    seeded_paths: &'a [String],
    seeded_tags: &'a [String],
    docker_repo: &'a str,
    docker_image: &'a str,
    layer_size: usize,
}

async fn pipe_raw_upload(
    ctx: &PipelinedRequestCtx<'_>,
    rng: &mut ChaCha8Rng,
    index: u64,
) -> (u64, bool) {
    let path = format!("bench/w{}/artifact-{}.bin", ctx.worker_id, index);
    let data = if ctx.random_data {
        let mut buf = vec![0u8; ctx.artifact_size];
        rng.fill(&mut buf[..]);
        buf
    } else {
        let mut buf = vec![0xABu8; ctx.artifact_size];
        let header = format!("w{}:i{}\n", ctx.worker_id, index);
        let hdr = header.as_bytes();
        let len = hdr.len().min(buf.len());
        buf[..len].copy_from_slice(&hdr[..len]);
        buf
    };
    let size = data.len() as u64;
    match ctx
        .client
        .upload_raw(ctx.repo_name, &path, data, "application/octet-stream")
        .await
    {
        Ok(()) => (size, true),
        Err(_) => (0, false),
    }
}

async fn pipe_raw_download(ctx: &PipelinedRequestCtx<'_>, rng: &mut ChaCha8Rng) -> (u64, bool) {
    if ctx.seeded_paths.is_empty() {
        return (0, false);
    }
    let idx = rng.random_range(0..ctx.seeded_paths.len());
    let path = &ctx.seeded_paths[idx];
    match ctx.client.download_raw(ctx.repo_name, path).await {
        Ok(data) => (data.len() as u64, true),
        Err(_) => (0, false),
    }
}

async fn pipe_raw_delete(
    ctx: &PipelinedRequestCtx<'_>,
    _rng: &mut ChaCha8Rng,
    index: u64,
) -> (u64, bool) {
    let path = format!(
        "bench/w{}/artifact-{}.bin",
        ctx.worker_id,
        index.saturating_sub(1)
    );
    match ctx.client.delete_raw(ctx.repo_name, &path).await {
        Ok(()) => (0, true),
        Err(_) => (0, false),
    }
}

async fn pipe_raw_list(ctx: &PipelinedRequestCtx<'_>) -> (u64, bool) {
    match ctx.client.list_artifacts(ctx.repo_name).await {
        Ok(list) => (0, !list.is_empty()),
        Err(_) => (0, false),
    }
}

async fn pipe_docker_push(
    ctx: &PipelinedRequestCtx<'_>,
    rng: &mut ChaCha8Rng,
    index: u64,
) -> (u64, bool) {
    let tag = format!("bench-{}-{}", ctx.worker_id, index);
    match demo::docker::push_image(ctx.client, ctx.docker_repo, ctx.docker_image, &tag, rng).await {
        Ok(()) => (ctx.layer_size as u64, true),
        Err(_) => (0, false),
    }
}

async fn pipe_docker_pull(ctx: &PipelinedRequestCtx<'_>, rng: &mut ChaCha8Rng) -> (u64, bool) {
    if ctx.seeded_tags.is_empty() {
        return (0, false);
    }
    let idx = rng.random_range(0..ctx.seeded_tags.len());
    let tag = &ctx.seeded_tags[idx];
    match ctx
        .client
        .docker_pull_manifest(ctx.docker_repo, ctx.docker_image, tag)
        .await
    {
        Ok((manifest_bytes, _)) => {
            let mut total_bytes = manifest_bytes.len() as u64;
            if let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&manifest_bytes) {
                if let Some(layers) = manifest["layers"].as_array() {
                    for layer in layers {
                        if let Some(digest) = layer["digest"].as_str() {
                            match ctx
                                .client
                                .docker_pull_blob(ctx.docker_repo, ctx.docker_image, digest)
                                .await
                            {
                                Ok(data) => total_bytes += data.len() as u64,
                                Err(_) => return (total_bytes, false),
                            }
                        }
                    }
                }
            }
            (total_bytes, true)
        }
        Err(_) => (0, false),
    }
}

async fn execute_raw_upload(cfg: &WorkerConfig, rng: &mut ChaCha8Rng, index: u64) -> (u64, bool) {
    let path = format!("bench/w{}/artifact-{}.bin", cfg.worker_id, index);
    let data = if cfg.random_data {
        let mut buf = vec![0u8; cfg.artifact_size];
        rng.fill(&mut buf[..]);
        buf
    } else {
        // Cheap pattern: fill with a repeating byte, then stamp a unique
        // header so each object has a distinct BLAKE3 hash (defeats dedup).
        let mut buf = vec![0xABu8; cfg.artifact_size];
        let header = format!("w{}:i{}\n", cfg.worker_id, index);
        let hdr = header.as_bytes();
        let len = hdr.len().min(buf.len());
        buf[..len].copy_from_slice(&hdr[..len]);
        buf
    };
    let size = data.len() as u64;
    match cfg
        .client
        .upload_raw(&cfg.repo_name, &path, data, "application/octet-stream")
        .await
    {
        Ok(()) => (size, true),
        Err(_) => (0, false),
    }
}

async fn execute_raw_download(cfg: &WorkerConfig, rng: &mut ChaCha8Rng) -> (u64, bool) {
    if cfg.seeded_paths.is_empty() {
        return (0, false);
    }
    let idx = rng.random_range(0..cfg.seeded_paths.len());
    let path = &cfg.seeded_paths[idx];
    match cfg.client.download_raw(&cfg.repo_name, path).await {
        Ok(data) => (data.len() as u64, true),
        Err(_) => (0, false),
    }
}

async fn execute_raw_delete(cfg: &WorkerConfig, _rng: &mut ChaCha8Rng, index: u64) -> (u64, bool) {
    // Delete a previously uploaded bench artifact
    let path = format!(
        "bench/w{}/artifact-{}.bin",
        cfg.worker_id,
        index.saturating_sub(1)
    );
    match cfg.client.delete_raw(&cfg.repo_name, &path).await {
        Ok(()) => (0, true),
        Err(_) => (0, false),
    }
}

async fn execute_raw_list(cfg: &WorkerConfig) -> (u64, bool) {
    match cfg.client.list_artifacts(&cfg.repo_name).await {
        Ok(list) => (0, !list.is_empty()),
        Err(_) => (0, false),
    }
}

async fn execute_docker_push(cfg: &WorkerConfig, rng: &mut ChaCha8Rng, index: u64) -> (u64, bool) {
    let tag = format!("bench-{}-{}", cfg.worker_id, index);
    match demo::docker::push_image(&cfg.client, &cfg.docker_repo, &cfg.docker_image, &tag, rng)
        .await
    {
        Ok(()) => (cfg.layer_size as u64, true),
        Err(_) => (0, false),
    }
}

async fn execute_docker_pull(cfg: &WorkerConfig, rng: &mut ChaCha8Rng) -> (u64, bool) {
    if cfg.seeded_tags.is_empty() {
        return (0, false);
    }
    let idx = rng.random_range(0..cfg.seeded_tags.len());
    let tag = &cfg.seeded_tags[idx];
    match cfg
        .client
        .docker_pull_manifest(&cfg.docker_repo, &cfg.docker_image, tag)
        .await
    {
        Ok((manifest_bytes, _)) => {
            // Parse manifest to get layer digests and pull them
            let mut total_bytes = manifest_bytes.len() as u64;
            if let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&manifest_bytes) {
                if let Some(layers) = manifest["layers"].as_array() {
                    for layer in layers {
                        if let Some(digest) = layer["digest"].as_str() {
                            match cfg
                                .client
                                .docker_pull_blob(&cfg.docker_repo, &cfg.docker_image, digest)
                                .await
                            {
                                Ok(data) => total_bytes += data.len() as u64,
                                Err(_) => return (total_bytes, false),
                            }
                        }
                    }
                }
            }
            (total_bytes, true)
        }
        Err(_) => (0, false),
    }
}

/// Compute a reasonable sleep for back-pressure if needed (currently unused but available).
pub fn _backoff(attempt: u32) -> Duration {
    Duration::from_millis(10 * 2u64.pow(attempt.min(6)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_exponential() {
        assert_eq!(_backoff(0), Duration::from_millis(10));
        assert_eq!(_backoff(1), Duration::from_millis(20));
        assert_eq!(_backoff(2), Duration::from_millis(40));
        assert_eq!(_backoff(3), Duration::from_millis(80));
        assert_eq!(_backoff(4), Duration::from_millis(160));
        assert_eq!(_backoff(5), Duration::from_millis(320));
        assert_eq!(_backoff(6), Duration::from_millis(640));
    }

    #[test]
    fn backoff_capped_at_6() {
        // attempt > 6 should be capped at 2^6 = 64, so 10 * 64 = 640ms
        assert_eq!(_backoff(7), Duration::from_millis(640));
        assert_eq!(_backoff(100), Duration::from_millis(640));
        assert_eq!(_backoff(u32::MAX), Duration::from_millis(640));
    }
}
