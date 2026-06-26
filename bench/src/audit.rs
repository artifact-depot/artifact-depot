// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Group (proxy) shadowing audit.
//!
//! A group repo resolves a pull by trying its members in declared order,
//! first-match-wins. When two members both hold the same `image:tag` but with
//! **different content digests**, the earlier member wins and the later one is
//! *cloaked* — its copy can never be served through the group. A losing member
//! whose digest is *identical* to the winner is just a redundant **mirror**
//! (harmless). The dangerous case is a **cache** member cloaked by an earlier
//! member with *different* content: a stale hosted copy hiding the canonical
//! registry image.
//!
//! For every genuine cloak this reports each copy's **build time** (the image
//! config's `created`) and **last-access time** (the tag's browse-tree atime),
//! so you can tell which copy is canonical and whether anyone still pulls the
//! shadowed one.
//!
//! Read-only. To stay cheap and avoid triggering upstream cache fetches, it only
//! issues a manifest HEAD for tags held by more than one member, and only against
//! members that already listed the tag in their own catalog.

use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Context, Result};

use crate::client::DepotClient;

pub struct AuditConfig {
    /// Name of the group/proxy repo whose member ordering to audit.
    pub group: String,
}

/// One member's copy of a shared tag.
struct Holder {
    /// Index in the group's member order (0 = highest priority / winner).
    order: usize,
    member: String,
    repo_type: String,
    digest: String,
    /// Image build time (config `created`), date only; `None` if unresolved.
    built: Option<String>,
    /// Tag last-access time (browse-tree atime), date only; `None` if unknown.
    accessed: Option<String>,
}

/// Decide, for holders sorted winner-first, whether this is a real content cloak
/// and how many **cache** copies are hidden by a different-digest winner. Pure.
fn classify(rt_digest: &[(&str, &str)]) -> (bool, usize) {
    if rt_digest.len() < 2 {
        return (false, 0);
    }
    let winner = rt_digest[0].1;
    let mut cloak = false;
    let mut cache_hidden = 0;
    for (rt, d) in &rt_digest[1..] {
        if *d != winner {
            cloak = true;
            if *rt == "cache" {
                cache_hidden += 1;
            }
        }
    }
    (cloak, cache_hidden)
}

pub async fn run(client: &DepotClient, cfg: AuditConfig) -> Result<()> {
    client.login().await?;

    let repos = client.list_repos().await.context("list repositories")?;
    let group = repos
        .iter()
        .find(|r| r.name == cfg.group)
        .with_context(|| format!("group repo '{}' not found", cfg.group))?;
    let members = group.members.clone().unwrap_or_default();
    if members.is_empty() {
        bail!(
            "'{}' has no members (type '{}'); not a group/proxy repo",
            cfg.group,
            group.repo_type
        );
    }
    let type_of = |name: &str| {
        repos
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.repo_type.clone())
            .unwrap_or_else(|| "?".to_string())
    };

    println!(
        "Auditing group '{}' resolution order:\n  {}",
        cfg.group,
        members
            .iter()
            .enumerate()
            .map(|(i, m)| format!("{}. {m} ({})", i + 1, type_of(m)))
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    // Enumerate every member's (image, tag) inventory (no HEADs yet).
    let mut holders: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    let mut member_tag_count = vec![0usize; members.len()];
    for (idx, member) in members.iter().enumerate() {
        let images = client
            .docker_repo_catalog(member)
            .await
            .with_context(|| format!("catalog '{member}'"))?;
        for image in images {
            let tags = client
                .docker_list_tags(member, &image)
                .await
                .with_context(|| format!("tags '{member}/{image}'"))?;
            for tag in tags {
                member_tag_count[idx] += 1;
                holders.entry((image.clone(), tag)).or_default().push(idx);
            }
        }
    }
    for (idx, member) in members.iter().enumerate() {
        println!("  ({}) holds {} tag(s)", member, member_tag_count[idx]);
    }

    // For every tag held by >1 member, fetch each holder's digest and compare.
    // Build-time / access-time are resolved lazily only for genuine cloaks.
    let mut atime_cache: HashMap<(String, String), HashMap<String, Option<String>>> =
        HashMap::new();
    let mut benign = 0usize;
    let mut cloaks: Vec<(String, String, Vec<Holder>, usize)> = Vec::new();
    for ((image, tag), idxs) in &holders {
        if idxs.len() < 2 {
            continue;
        }
        let mut hs: Vec<Holder> = Vec::new();
        for &idx in idxs {
            let member = &members[idx];
            let (status, _, digest) = client
                .docker_head_manifest(member, image, tag)
                .await
                .with_context(|| format!("HEAD {member}/{image}:{tag}"))?;
            if status == 200 {
                hs.push(Holder {
                    order: idx,
                    member: member.clone(),
                    repo_type: type_of(member),
                    digest,
                    built: None,
                    accessed: None,
                });
            }
        }
        if hs.len() < 2 {
            continue;
        }
        hs.sort_by_key(|h| h.order);

        let rt_digest: Vec<(&str, &str)> = hs
            .iter()
            .map(|h| (h.repo_type.as_str(), h.digest.as_str()))
            .collect();
        let (is_cloak, cache_hidden) = classify(&rt_digest);
        if !is_cloak {
            benign += 1;
            continue;
        }

        // Resolve build + access times for each copy of this cloaked tag.
        for h in &mut hs {
            h.built = resolve_built(client, &h.member, image, tag).await;
            let key = (h.member.clone(), image.clone());
            if !atime_cache.contains_key(&key) {
                let map = client
                    .image_tag_atimes(&h.member, image)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<HashMap<_, _>>();
                atime_cache.insert(key.clone(), map);
            }
            h.accessed = atime_cache
                .get(&key)
                .and_then(|m| m.get(tag).cloned())
                .flatten()
                .map(|s| date(&s));
        }
        cloaks.push((image.clone(), tag.clone(), hs, cache_hidden));
    }

    println!(
        "\n{} tag(s) held by >1 member; {benign} are identical (same digest, harmless).",
        cloaks.len() + benign
    );

    if cloaks.is_empty() {
        println!("\nNo cloaks: every multi-member tag resolves to identical content. ✓");
        return Ok(());
    }

    let total_cache_hidden: usize = cloaks.iter().map(|(_, _, _, c)| *c).sum();
    println!(
        "\n{} tag(s) cloaked — an earlier member hides a DIFFERENT-digest copy below \
         (a same-digest loser is just a 'mirror', shown for context):",
        cloaks.len()
    );
    for (image, tag, hs, _) in &cloaks {
        let winner_digest = hs[0].digest.clone();
        println!("  {image}:{tag}");
        for (i, h) in hs.iter().enumerate() {
            let role = if i == 0 {
                "winner "
            } else if h.digest == winner_digest {
                "mirror "
            } else {
                "cloaked"
            };
            let flag = if i != 0 && h.digest != winner_digest && h.repo_type == "cache" {
                "   <-- CACHE HIDDEN (canonical content overridden)"
            } else {
                ""
            };
            println!(
                "      {role} -> ({}) {} [{}]  built {}  last-pull {}{}",
                h.member,
                h.repo_type,
                short(&h.digest),
                h.built.as_deref().unwrap_or("?"),
                h.accessed.as_deref().unwrap_or("never/unknown"),
                flag
            );
        }
    }

    if total_cache_hidden > 0 {
        println!(
            "\n⚠  {total_cache_hidden} cache copy(ies) are cloaked by an earlier member with \
             different content. Resolve these (delete/repoint the hosted copy, or reorder) \
             before relying on the group to serve the canonical image."
        );
        bail!("{total_cache_hidden} cache tag(s) cloaked");
    }
    println!("\nNo cache members are cloaked by different content (the dangerous direction). ✓");
    Ok(())
}

/// Resolve an image's build time = its config blob's `created` field (date only).
/// Descends one level into a manifest list/index. Best-effort: `None` on any miss.
async fn resolve_built(
    client: &DepotClient,
    repo: &str,
    image: &str,
    reference: &str,
) -> Option<String> {
    let (body, _) = client
        .docker_get_manifest_path(repo, image, reference)
        .await
        .ok()?;
    let m: serde_json::Value = serde_json::from_slice(&body).ok()?;
    // Manifest list / OCI index → descend into the first child.
    if let Some(child) = m
        .get("manifests")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("digest"))
        .and_then(|d| d.as_str())
    {
        let (cbody, _) = client.docker_get_manifest_path(repo, image, child).await.ok()?;
        let cm: serde_json::Value = serde_json::from_slice(&cbody).ok()?;
        return created_from_config(client, repo, image, &cm).await;
    }
    created_from_config(client, repo, image, &m).await
}

async fn created_from_config(
    client: &DepotClient,
    repo: &str,
    image: &str,
    manifest: &serde_json::Value,
) -> Option<String> {
    let cfg = manifest.get("config")?.get("digest")?.as_str()?;
    let blob = client.docker_get_blob_path(repo, image, cfg).await.ok()?;
    let c: serde_json::Value = serde_json::from_slice(&blob).ok()?;
    c.get("created").and_then(|v| v.as_str()).map(date)
}

/// First 10 chars of an RFC3339 timestamp (the `YYYY-MM-DD` date).
fn date(ts: &str) -> String {
    ts.get(0..10).unwrap_or(ts).to_string()
}

/// Shorten a `sha256:…` digest for display.
fn short(digest: &str) -> String {
    match digest.split_once(':') {
        Some((algo, hex)) => format!("{algo}:{}", &hex[..hex.len().min(12)]),
        None => digest.chars().take(12).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_distinguishes_mirror_from_cloak() {
        // Single holder → never a cloak.
        assert_eq!(classify(&[("hosted", "sha256:a")]), (false, 0));
        // Two holders, same digest → mirror, not a cloak.
        assert_eq!(
            classify(&[("hosted", "sha256:a"), ("cache", "sha256:a")]),
            (false, 0)
        );
        // Winner + a different-digest hosted loser → cloak, no cache hidden.
        assert_eq!(
            classify(&[("hosted", "sha256:a"), ("hosted", "sha256:b")]),
            (true, 0)
        );
        // Cache loser with the SAME digest as the winner is a mirror, not hidden;
        // the different-digest hosted loser still makes it a cloak.
        assert_eq!(
            classify(&[
                ("hosted", "sha256:a"),
                ("hosted", "sha256:b"),
                ("cache", "sha256:a"),
            ]),
            (true, 0)
        );
        // Cache loser with a DIFFERENT digest than the winner → canonical hidden.
        assert_eq!(
            classify(&[("hosted", "sha256:a"), ("cache", "sha256:b")]),
            (true, 1)
        );
    }
}
