// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

// ---------------------------------------------------------------------------
// Docker-specific GC
// ---------------------------------------------------------------------------

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bloomfilter::Bloom;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use depot_core::store::keys;
use depot_core::store::kv::KvStore;
use depot_core::store_registry::StoreRegistry;
use depot_core::update::UpdateSender;

use super::DOCKER_GC_GRACE_SECS;

/// Docker GC for a single repo: delete unreferenced layer blobs and
/// optionally unreferenced manifests.
///
/// Docker artifacts use a path convention:
///   `_blobs/sha256:...`         — layer/config blob references
///   `_manifests/sha256:...`     — manifest records (blob = JSON)
///   `_tags/{name}`              — tag records (point to a manifest digest)
///   `{image}/_blobs/...`        — namespaced variants
///   `{image}/_manifests/...`
///   `{image}/_tags/...`
///
/// Phase A: Build bloom filter of layer/config digests referenced by manifests.
///          Delete `_blobs/sha256:*` artifacts not in the filter.
/// Phase B: If `cleanup_untagged_manifests` is enabled, build bloom filter of
///          manifest digests referenced by tags. Delete `_manifests/sha256:*`
///          artifacts not in the filter.
pub(super) async fn docker_gc(
    kv: Arc<dyn KvStore>,
    stores: &Arc<StoreRegistry>,
    repo: &depot_core::store::kv::RepoConfig,
    dry_run: bool,
    cancel: Option<&CancellationToken>,
    updater: &UpdateSender,
) -> depot_core::error::Result<u64> {
    use depot_core::store::kv::ArtifactRecord;

    let mut deleted = 0u64;
    let repo_name = &repo.name;
    let grace_cutoff = chrono::Utc::now() - chrono::Duration::seconds(DOCKER_GC_GRACE_SECS as i64);

    // Collect all Docker artifacts across all shards in parallel, grouped by path prefix.
    struct ShardResult {
        manifests: Vec<(String, u16, ArtifactRecord)>,
        blob_refs: Vec<(String, u16, chrono::DateTime<chrono::Utc>, u64)>,
        tag_digests: Vec<String>,
    }

    let mut js = JoinSet::new();
    for shard in 0..keys::NUM_SHARDS {
        let kv = kv.clone();
        let repo_name = repo_name.clone();
        let cancel = cancel.cloned();
        js.spawn(async move {
            let pk = keys::artifact_shard_pk(&repo_name, shard);
            let mut sr = ShardResult {
                manifests: Vec::new(),
                blob_refs: Vec::new(),
                tag_digests: Vec::new(),
            };
            let mut cursor: Option<String> = None;
            loop {
                if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                    return Err(depot_core::error::DepotError::Internal(
                        "Docker GC cancelled".to_string(),
                    ));
                }

                let start = match &cursor {
                    Some(c) => c.as_str(),
                    None => "",
                };
                let result = kv
                    .scan_range(
                        keys::TABLE_ARTIFACTS,
                        Cow::Borrowed(pk.as_str()),
                        Cow::Borrowed(start),
                        None,
                        1000,
                    )
                    .await?;

                for (sk, value) in &result.items {
                    let path = sk.as_str();
                    let record: ArtifactRecord = match rmp_serde::from_slice(value) {
                        Ok(r) => r,
                        Err(_) => continue,
                    };

                    if path.contains("/_blobs/") || path.starts_with("_blobs/") {
                        sr.blob_refs
                            .push((sk.to_string(), shard, record.created_at, record.size));
                    } else if path.contains("/_manifests/") || path.starts_with("_manifests/") {
                        sr.manifests.push((sk.to_string(), shard, record));
                    } else if path.contains("/_tags/") || path.starts_with("_tags/") {
                        if let depot_core::store::kv::ArtifactKind::DockerTag {
                            ref digest, ..
                        } = record.kind
                        {
                            sr.tag_digests.push(digest.clone());
                        }
                    }
                }

                if result.done {
                    break;
                }
                if let Some((last_sk, _)) = result.items.last() {
                    cursor = Some(format!("{}\0", last_sk));
                } else {
                    break;
                }
                tokio::task::yield_now().await;
            }
            Ok::<_, depot_core::error::DepotError>(sr)
        });
    }
    let mut shard_results = Vec::new();
    while let Some(res) = js.join_next().await {
        shard_results.push(res??);
    }

    let mut manifests: Vec<(String, u16, ArtifactRecord)> = Vec::new();
    let mut blob_refs: Vec<(String, u16, chrono::DateTime<chrono::Utc>, u64)> = Vec::new();
    let mut tag_digests: Vec<String> = Vec::new();
    for sr in shard_results {
        manifests.extend(sr.manifests);
        blob_refs.extend(sr.blob_refs);
        tag_digests.extend(sr.tag_digests);
    }

    // --- Phase A: Delete unreferenced layer blob artifacts ---

    // Resolve blob store for this repo to read manifest JSON.
    let blobs = crate::server::infra::store_registry::resolve_blob_store(stores, &repo.store).await;

    // Build bloom filter of digests referenced by manifests.
    let manifest_count = manifests.len().max(1);
    let mut layer_bf = Bloom::new_for_fp_rate(manifest_count * 10, 0.01); // ~10 layers per manifest

    for (_, _, ref record) in &manifests {
        if let Some(json) = read_manifest_json(&blobs, record).await {
            for digest in extract_manifest_refs(&json) {
                layer_bf.set(digest.as_bytes());
            }
        }
    }

    // Delete blob ref artifacts whose digest is not in the layer BF.
    let mut blob_del_sks: Vec<&str> = Vec::new();
    let mut blob_del_sizes: Vec<u64> = Vec::new();
    for (sk, _shard, created_at, size) in &blob_refs {
        // Skip recently created records to avoid racing concurrent manifest pushes.
        if *created_at > grace_cutoff {
            continue;
        }

        // Extract digest from path like "_blobs/sha256:abc" or "img/_blobs/sha256:abc"
        let digest = if let Some((_, d)) = sk.rsplit_once("/_blobs/") {
            // Namespaced: "img/_blobs/sha256:abc" → "sha256:abc"
            d
        } else if let Some(d) = sk.strip_prefix("_blobs/") {
            d
        } else {
            continue;
        };

        if !layer_bf.check(digest.as_bytes()) {
            blob_del_sks.push(sk);
            blob_del_sizes.push(*size);
        }
    }
    if !blob_del_sks.is_empty() && !dry_run {
        depot_core::service::delete_artifacts_paired_batch(kv.as_ref(), repo_name, &blob_del_sks)
            .await?;
        deleted += blob_del_sks.len() as u64;
        for (&sk, &size) in blob_del_sks.iter().zip(blob_del_sizes.iter()) {
            updater.dir_changed(repo_name, sk, -1, -(size as i64)).await;
        }
        // No store_changed: deleting blob-ref artifact records orphans the
        // blobs but doesn't remove them — the orphan sweep reclaims them (and
        // recomputes store stats) once unreferenced.
    }

    // --- Phase B: Delete unreferenced manifests (if policy enabled) ---

    if repo.format_config.cleanup_untagged_manifests() {
        // A manifest is live iff it is reachable from a tag. Tags point at a
        // manifest digest; a manifest list/index points at child manifest
        // digests. Build the reachable closure starting from the tag digests
        // and follow children **only through manifests that are themselves
        // reachable** — so an untagged orphan index does NOT protect its
        // children. Both the orphan index and its now-unreferenced children are
        // reclaimed in the SAME pass (no second pass required for multi-arch).
        //
        // Exact sets, not a bloom filter: manifest counts per repo are modest
        // (thousands, vs. millions of layer blobs in Phase A), so an exact
        // membership test is cheap and — unlike a bloom — has zero false
        // positives, so no untagged manifest is ever wrongly protected.
        let digest_of = |sk: &str| -> Option<String> {
            if let Some((_, d)) = sk.rsplit_once("/_manifests/") {
                // Namespaced: "img/_manifests/sha256:abc" → "sha256:abc"
                Some(d.to_string())
            } else {
                sk.strip_prefix("_manifests/").map(|d| d.to_string())
            }
        };

        // Map each present list/index manifest's own digest → its child digests.
        let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
        for (sk, _shard, record) in &manifests {
            let ct = &record.content_type;
            if ct.contains("manifest.list") || ct.contains("image.index") {
                if let (Some(digest), Some(json)) =
                    (digest_of(sk), read_manifest_json(&blobs, record).await)
                {
                    children_of.insert(digest, extract_manifest_refs(&json));
                }
            }
        }

        // BFS the reachable set from the tags, descending through reachable
        // lists only (handles nested indexes; orphan indexes are never entered).
        let mut reachable: HashSet<String> = HashSet::new();
        let mut queue: Vec<String> = tag_digests.clone();
        while let Some(d) = queue.pop() {
            if !reachable.insert(d.clone()) {
                continue;
            }
            if let Some(children) = children_of.get(&d) {
                for c in children {
                    if !reachable.contains(c) {
                        queue.push(c.clone());
                    }
                }
            }
        }

        let mut manifest_del_sks: Vec<&str> = Vec::new();
        let mut manifest_del_sizes: Vec<u64> = Vec::new();
        for (sk, _shard, record) in &manifests {
            // Skip recently created records to avoid racing concurrent tag pushes.
            if record.created_at > grace_cutoff {
                continue;
            }

            let digest = match digest_of(sk) {
                Some(d) => d,
                None => continue,
            };

            if !reachable.contains(&digest) {
                manifest_del_sks.push(sk);
                manifest_del_sizes.push(record.size);
            }
        }
        if !manifest_del_sks.is_empty() && !dry_run {
            depot_core::service::delete_artifacts_paired_batch(
                kv.as_ref(),
                repo_name,
                &manifest_del_sks,
            )
            .await?;
            deleted += manifest_del_sks.len() as u64;
            for (&sk, &size) in manifest_del_sks.iter().zip(manifest_del_sizes.iter()) {
                updater.dir_changed(repo_name, sk, -1, -(size as i64)).await;
            }
            // No store_changed: same as above — orphaned, not yet reclaimed.
        }
    }

    Ok(deleted)
}

/// Extract all referenced digests from a Docker/OCI manifest JSON.
/// For single manifests: config digest + layer digests.
/// For manifest lists/indexes: child manifest digests.
pub(super) fn extract_manifest_refs(json: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let parsed: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return refs,
    };

    // Single manifest: {"config": {"digest": "..."}, "layers": [{"digest": "..."}, ...]}
    if let Some(config) = parsed.get("config") {
        if let Some(digest) = config.get("digest").and_then(|d| d.as_str()) {
            refs.push(digest.to_string());
        }
    }
    if let Some(layers) = parsed.get("layers").and_then(|l| l.as_array()) {
        for layer in layers {
            if let Some(digest) = layer.get("digest").and_then(|d| d.as_str()) {
                refs.push(digest.to_string());
            }
        }
    }

    // Manifest list/index: {"manifests": [{"digest": "..."}, ...]}
    if let Some(manifests) = parsed.get("manifests").and_then(|m| m.as_array()) {
        for m in manifests {
            if let Some(digest) = m.get("digest").and_then(|d| d.as_str()) {
                refs.push(digest.to_string());
            }
        }
    }

    refs
}

/// Read a Docker manifest's JSON from the blob store.
pub(super) async fn read_manifest_json(
    blobs: &Result<
        std::sync::Arc<dyn depot_core::store::blob::BlobStore>,
        depot_core::error::DepotError,
    >,
    record: &depot_core::store::kv::ArtifactRecord,
) -> Option<String> {
    let blobs = match blobs {
        Ok(b) => b,
        Err(_) => return None,
    };
    let blob_id = record.blob_id.as_deref()?;
    if blob_id.is_empty() {
        return None;
    }
    let data = blobs.get(blob_id).await.ok()??;
    String::from_utf8(data).ok()
}
