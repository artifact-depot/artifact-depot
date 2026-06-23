// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Tag-based Docker repository reorganization.
//!
//! Scans a set of source Docker repositories for first-party images and
//! re-files each `image:tag` into the correct destination repository based on
//! the tag shape (released semver, prerelease, `develop`, CI build, developer
//! build). Classification destinations come from a TOML rules file; the tag
//! matchers themselves are built in.
//!
//! Runs as a dry-run by default — it prints the planned moves and an
//! "unclassified" bucket so nothing is moved without review — and performs the
//! moves only with `apply`. The moves are issued against the Nexus-compatible
//! staging endpoints (`staging/move`, or `staging/copy` when `copy` is set so
//! the source is left intact for a non-destructive first pass).

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::client::DepotClient;

/// Destination repository for each tag category.
#[derive(Debug, Clone, Deserialize)]
pub struct Dest {
    /// Released `x.y.z` images that customers download.
    pub released: String,
    /// Released `x.y.z` images that are supplementary (not customer-facing).
    pub released_aux: String,
    /// Prerelease `x.y.z-dev.n` / `-rc.n` images.
    pub prerelease: String,
    /// The rolling `develop` tag.
    pub develop: String,
    /// CI build images (`ci-*-<n>`).
    pub ci: String,
    /// Developer build images (`<name>-<n>`, e.g. `slord-79`).
    pub developer: String,
}

/// Reorg rules, loaded from a TOML file.
#[derive(Debug, Clone, Deserialize)]
pub struct Rules {
    /// Repositories to drain of first-party images.
    pub source_repos: Vec<String>,
    /// Only images whose name starts with one of these prefixes are touched
    /// (e.g. `myriad/`, `qkp/`, `orchestrator/`). Everything else is left in
    /// place (third-party content).
    pub first_party_prefixes: Vec<String>,
    /// Destination repositories per tag category.
    pub dest: Dest,
    /// Released images that are supplementary and route to `dest.released_aux`
    /// instead of `dest.released` (exact image-name match).
    #[serde(default)]
    pub aux_images: Vec<String>,
}

impl Rules {
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).context("parse reorg rules TOML")
    }

    fn is_first_party(&self, image: &str) -> bool {
        self.first_party_prefixes
            .iter()
            .any(|p| image.starts_with(p))
    }
}

/// A single planned relocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMove {
    pub source_repo: String,
    pub image: String,
    pub tag: String,
    pub dest: String,
}

/// An `image:tag` whose tag matched no category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unclassified {
    pub source_repo: String,
    pub image: String,
    pub tag: String,
}

/// Result of planning a reorg over an inventory.
#[derive(Debug, Default)]
pub struct Plan {
    pub moves: Vec<PlannedMove>,
    pub unclassified: Vec<Unclassified>,
}

// --- Tag classification (built-in matchers) ---

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `x.y.z` — three numeric, dot-separated components.
fn is_released_semver(tag: &str) -> bool {
    let parts: Vec<&str> = tag.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| all_digits(p))
}

/// `x.y.z-dev.n` or `x.y.z-rc.n`.
fn is_prerelease_semver(tag: &str) -> bool {
    let Some((base, suffix)) = tag.split_once('-') else {
        return false;
    };
    if !is_released_semver(base) {
        return false;
    }
    match suffix.split_once('.') {
        Some((kind, num)) => matches!(kind, "dev" | "rc") && all_digits(num),
        None => false,
    }
}

/// `ci-<something>-<n>` (e.g. `ci-build-4821`).
fn is_ci(tag: &str) -> bool {
    let Some((prefix, num)) = tag.rsplit_once('-') else {
        return false;
    };
    prefix.starts_with("ci-") && prefix.len() > 3 && all_digits(num)
}

/// `<name>-<n>` where name is a single lowercase-alphanumeric word starting
/// with a letter (e.g. `slord-79`).
fn is_developer(tag: &str) -> bool {
    let Some((name, num)) = tag.rsplit_once('-') else {
        return false;
    };
    if !all_digits(num) {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    name.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Classify a tag into its destination repository, or `None` if it matches no
/// known category. Order matters: prerelease is checked before released so the
/// `-dev.n` suffix isn't mistaken for plain semver.
pub fn classify<'a>(rules: &'a Rules, image: &str, tag: &str) -> Option<&'a str> {
    if is_prerelease_semver(tag) {
        Some(&rules.dest.prerelease)
    } else if is_released_semver(tag) {
        if rules.aux_images.iter().any(|a| a == image) {
            Some(&rules.dest.released_aux)
        } else {
            Some(&rules.dest.released)
        }
    } else if tag == "develop" {
        Some(&rules.dest.develop)
    } else if is_ci(tag) {
        Some(&rules.dest.ci)
    } else if is_developer(tag) {
        Some(&rules.dest.developer)
    } else {
        None
    }
}

/// Build the plan from a flat inventory of `(source_repo, image, tag)`.
/// Non-first-party images are skipped entirely (left in place); first-party
/// tags are either classified into a move or recorded as unclassified.
pub fn build_plan(rules: &Rules, inventory: &[(String, String, String)]) -> Plan {
    let mut plan = Plan::default();
    for (source_repo, image, tag) in inventory {
        if !rules.is_first_party(image) {
            continue;
        }
        match classify(rules, image, tag) {
            Some(dest) => plan.moves.push(PlannedMove {
                source_repo: source_repo.clone(),
                image: image.clone(),
                tag: tag.clone(),
                dest: dest.to_string(),
            }),
            None => plan.unclassified.push(Unclassified {
                source_repo: source_repo.clone(),
                image: image.clone(),
                tag: tag.clone(),
            }),
        }
    }
    plan
}

/// Configuration for a reorg run.
pub struct ReorgConfig {
    pub rules_path: String,
    /// Perform the moves. When false (default), only print the plan.
    pub apply: bool,
    /// Use `staging/copy` (leave source intact) instead of `staging/move`.
    pub copy: bool,
}

/// Fetch the full `(source_repo, image, tag)` inventory across the rules'
/// source repos.
async fn fetch_inventory(
    client: &DepotClient,
    rules: &Rules,
) -> Result<Vec<(String, String, String)>> {
    let mut inventory = Vec::new();
    for repo in &rules.source_repos {
        let images = client
            .docker_repo_catalog(repo)
            .await
            .with_context(|| format!("list images in '{repo}'"))?;
        for image in images {
            if !rules.is_first_party(&image) {
                continue;
            }
            let tags = client
                .docker_list_tags(repo, &image)
                .await
                .with_context(|| format!("list tags for '{repo}/{image}'"))?;
            for tag in tags {
                inventory.push((repo.clone(), image.clone(), tag));
            }
        }
    }
    Ok(inventory)
}

/// Run the reorg: load rules, fetch inventory, plan, print, and (if `apply`)
/// execute the moves.
pub async fn run(client: &DepotClient, cfg: ReorgConfig) -> Result<()> {
    let text = std::fs::read_to_string(&cfg.rules_path)
        .with_context(|| format!("read rules file '{}'", cfg.rules_path))?;
    let rules = Rules::from_toml(&text)?;
    client.login().await?;

    let inventory = fetch_inventory(client, &rules).await?;
    let plan = build_plan(&rules, &inventory);

    print_plan(&plan, cfg.copy);

    if !cfg.apply {
        println!(
            "\nDry run — no changes made. Re-run with --apply to {} {} tag(s).",
            if cfg.copy { "copy" } else { "move" },
            plan.moves.len()
        );
        return Ok(());
    }

    let verb = if cfg.copy { "copy" } else { "move" };
    let mut ok = 0usize;
    let mut failed = 0usize;
    for m in &plan.moves {
        let result = if cfg.copy {
            client
                .staging_copy(&m.source_repo, &m.dest, &m.image, &m.tag)
                .await
        } else {
            client
                .staging_move(&m.source_repo, &m.dest, &m.image, &m.tag)
                .await
        };
        match result {
            Ok(()) => {
                ok += 1;
                println!(
                    "  {verb} ok: {}/{}:{} -> {}",
                    m.source_repo, m.image, m.tag, m.dest
                );
            }
            Err(e) => {
                failed += 1;
                eprintln!(
                    "  {verb} FAILED: {}/{}:{} -> {}: {e}",
                    m.source_repo, m.image, m.tag, m.dest
                );
            }
        }
    }

    println!("\nApplied: {ok} succeeded, {failed} failed.");
    if failed > 0 {
        anyhow::bail!("{failed} staging operation(s) failed");
    }
    Ok(())
}

fn print_plan(plan: &Plan, copy: bool) {
    use std::collections::BTreeMap;

    let verb = if copy { "copy" } else { "move" };
    println!("Planned {verb}s ({} tag(s)):", plan.moves.len());

    // Group by destination for a readable summary.
    let mut by_dest: BTreeMap<&str, Vec<&PlannedMove>> = BTreeMap::new();
    for m in &plan.moves {
        by_dest.entry(m.dest.as_str()).or_default().push(m);
    }
    for (dest, moves) in &by_dest {
        println!("  -> {dest} ({} tag(s)):", moves.len());
        for m in moves {
            println!("       {}/{}:{}", m.source_repo, m.image, m.tag);
        }
    }

    if plan.unclassified.is_empty() {
        println!("\nUnclassified: none.");
    } else {
        println!(
            "\nUnclassified ({} tag(s)) — left in place, no rule matched:",
            plan.unclassified.len()
        );
        for u in &plan.unclassified {
            println!("       {}/{}:{}", u.source_repo, u.image, u.tag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rules() -> Rules {
        Rules {
            source_repos: vec!["docker-internal".into(), "docker-upstream".into()],
            first_party_prefixes: vec!["myriad/".into(), "qkp/".into(), "orchestrator/".into()],
            dest: Dest {
                released: "docker-insight".into(),
                released_aux: "docker-release-aux".into(),
                prerelease: "docker-prerelease".into(),
                develop: "docker-prerelease".into(),
                ci: "docker-development-local".into(),
                developer: "docker-development-local".into(),
            },
            aux_images: vec!["myriad/test_exec_web".into(), "myriad/uncrustifier".into()],
        }
    }

    #[test]
    fn released_semver_matches() {
        assert!(is_released_semver("1.2.3"));
        assert!(is_released_semver("10.0.44"));
        assert!(!is_released_semver("1.2"));
        assert!(!is_released_semver("1.2.3.4"));
        assert!(!is_released_semver("1.2.x"));
        assert!(!is_released_semver("1.2.3-dev.1"));
    }

    #[test]
    fn prerelease_semver_matches() {
        assert!(is_prerelease_semver("1.2.3-dev.7"));
        assert!(is_prerelease_semver("1.2.3-rc.1"));
        assert!(!is_prerelease_semver("1.2.3"));
        assert!(!is_prerelease_semver("1.2.3-beta.1"));
        assert!(!is_prerelease_semver("1.2.3-dev"));
        assert!(!is_prerelease_semver("1.2.3-dev.x"));
    }

    #[test]
    fn ci_and_developer_matches() {
        assert!(is_ci("ci-build-4821"));
        assert!(is_ci("ci-nightly-1"));
        assert!(!is_ci("ci--1")); // empty middle still has prefix "ci-" len 3 -> rejected
        assert!(!is_ci("build-12"));

        assert!(is_developer("slord-79"));
        assert!(is_developer("ab1-2"));
        assert!(!is_developer("ci-build-4821")); // contains hyphen in name
        assert!(!is_developer("Slord-79")); // uppercase
        assert!(!is_developer("slord-x"));
    }

    #[test]
    fn classify_routes_each_category() {
        let r = test_rules();
        assert_eq!(
            classify(&r, "myriad/api_server", "1.2.3"),
            Some("docker-insight")
        );
        assert_eq!(
            classify(&r, "myriad/test_exec_web", "1.2.3"),
            Some("docker-release-aux")
        );
        assert_eq!(
            classify(&r, "myriad/api_server", "1.2.3-dev.7"),
            Some("docker-prerelease")
        );
        assert_eq!(
            classify(&r, "qkp/quantum_leaf", "develop"),
            Some("docker-prerelease")
        );
        assert_eq!(
            classify(&r, "qkp/quantum_leaf", "ci-build-12"),
            Some("docker-development-local")
        );
        assert_eq!(
            classify(&r, "qkp/quantum_leaf", "slord-79"),
            Some("docker-development-local")
        );
        assert_eq!(classify(&r, "myriad/api_server", "weird-tag-format!"), None);
    }

    #[test]
    fn build_plan_filters_and_classifies() {
        let r = test_rules();
        let inventory = vec![
            (
                "docker-internal".into(),
                "myriad/api_server".into(),
                "1.2.3".into(),
            ),
            (
                "docker-internal".into(),
                "myriad/test_exec_web".into(),
                "1.2.3".into(),
            ),
            (
                "docker-internal".into(),
                "myriad/api_server".into(),
                "1.2.3-dev.4".into(),
            ),
            (
                "docker-internal".into(),
                "myriad/api_server".into(),
                "garbage".into(),
            ),
            // Third-party image — must be ignored entirely.
            (
                "docker-upstream".into(),
                "library/postgres".into(),
                "16.2".into(),
            ),
        ];
        let plan = build_plan(&r, &inventory);

        assert_eq!(plan.moves.len(), 3);
        assert!(plan.moves.contains(&PlannedMove {
            source_repo: "docker-internal".into(),
            image: "myriad/test_exec_web".into(),
            tag: "1.2.3".into(),
            dest: "docker-release-aux".into(),
        }));
        assert_eq!(plan.unclassified.len(), 1);
        assert_eq!(plan.unclassified[0].tag, "garbage");
        // The third-party image contributed nothing.
        assert!(!plan.moves.iter().any(|m| m.image == "library/postgres"));
    }

    #[test]
    fn rules_parse_from_toml() {
        let toml = r#"
            source_repos = ["docker-internal", "docker-upstream"]
            first_party_prefixes = ["myriad/", "qkp/"]
            aux_images = ["myriad/test_exec_web"]
            [dest]
            released = "docker-insight"
            released_aux = "docker-release-aux"
            prerelease = "docker-prerelease"
            develop = "docker-prerelease"
            ci = "docker-development-local"
            developer = "docker-development-local"
        "#;
        let rules = Rules::from_toml(toml).unwrap();
        assert_eq!(rules.source_repos.len(), 2);
        assert_eq!(rules.dest.released, "docker-insight");
        assert!(rules.is_first_party("myriad/api_server"));
        assert!(!rules.is_first_party("library/postgres"));
    }
}
