// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Helm chart repository reorganization.
//!
//! The Helm analogue of [`crate::reorg`], but much simpler: a chart version is
//! a single record (`{name}-{version}.tgz`), so there is no manifest closure to
//! walk and no per-image insight/aux split. The driver:
//!
//! 1. reads each source repo's `index.yaml` (authoritative name/version list),
//! 2. classifies every first-party `(chart, version)` by ordered version-class
//!    rules (released / prerelease / branch / junk), leaving third-party charts
//!    and unknown patterns untouched,
//! 3. prints a grouped dry-run plan with an explicit **unclassified** bucket,
//! 4. on `--apply`, drives the Nexus staging move/delete endpoints.
//!
//! Released charts are verified against the upstream authority before deletion
//! (the "-insight repo follows the upstream" model): a released version is only
//! dropped locally if the upstream authority already serves the identical
//! digest, and the authority is queried directly, never through a local cache.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;

use crate::client::DepotClient;

/// A single `(chart_name, version, digest)` triple from an `index.yaml`.
#[derive(Debug, Clone)]
pub struct ChartVersion {
    pub name: String,
    pub version: String,
    /// `sha256:...` digest as advertised in the index (may be empty).
    pub digest: String,
}

/// What to do with a matched chart version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelmAction {
    /// Move to the named destination repo (same store).
    Move { dest: String },
    /// Delete outright (junk / superseded).
    Delete,
    /// Delete locally, but only after confirming the authority repo already
    /// serves the identical digest.
    DeleteIfOnAuthority,
}

/// One classification rule: a version-class regex → action.
#[derive(Debug, Deserialize)]
pub struct HelmRule {
    /// Named pattern (must exist in `[patterns]`) matched against the version.
    pub class: String,
    /// Destination repo for a move; omitted for delete actions.
    #[serde(default)]
    pub dest: Option<String>,
    /// Action: `move` (default when `dest` set), `delete`, or
    /// `delete-if-on-authority`.
    #[serde(default)]
    pub action: Option<String>,
}

/// Per-source-repo rule block.
#[derive(Debug, Deserialize)]
pub struct HelmGroup {
    /// Source repository to drain.
    pub source: String,
    /// Ordered rules (first match wins).
    pub rules: Vec<HelmRule>,
    /// Retire (expect empty afterwards) — informational only.
    #[serde(default)]
    pub retire: bool,
}

/// Where to check that a released chart already exists before deleting the
/// local copy. This MUST be the real upstream registry queried directly, never
/// a local cache repo: a cache's `index.yaml` only lists what has already been
/// pulled through it, so a genuinely-published chart could look absent (and be
/// wrongly kept) — and pulling through the cache to check would warm it, which
/// is forbidden (caches are usage records). Credentials come from the
/// `UPSTREAM_USERNAME` / `UPSTREAM_PASSWORD` environment variables.
#[derive(Debug, Deserialize)]
pub struct AuthorityConfig {
    /// Base URL of the upstream depot/registry (e.g. `https://insight.example.com`).
    pub url: String,
    /// Hosted repo on that upstream holding released charts (e.g. `helm-release`).
    pub repo: String,
}

/// The full helm reorg rules file.
#[derive(Debug, Deserialize)]
pub struct HelmRulesFile {
    /// Named version-class regexes (e.g. `released = '^\\d+\\.\\d+\\.\\d+$'`).
    pub patterns: BTreeMap<String, String>,
    /// First-party chart-name prefixes. Charts whose name does not start with
    /// one of these are third-party and are never touched.
    pub first_party: Vec<String>,
    /// Upstream authority for `delete-if-on-authority` (queried directly — see
    /// [`AuthorityConfig`]). Required only if a rule uses that action.
    #[serde(default)]
    pub authority: Option<AuthorityConfig>,
    /// Per-source-repo rule blocks.
    pub groups: Vec<HelmGroup>,
}

/// Parse the `entries` of a Helm `index.yaml` into a flat version list.
pub fn parse_index(yaml: &[u8]) -> Result<Vec<ChartVersion>> {
    #[derive(Deserialize)]
    struct Index {
        #[serde(default)]
        entries: BTreeMap<String, Vec<Entry>>,
    }
    #[derive(Deserialize)]
    struct Entry {
        name: String,
        version: String,
        #[serde(default)]
        digest: String,
    }
    let idx: Index = serde_yml::from_slice(yaml).context("parse index.yaml")?;
    let mut out = Vec::new();
    for versions in idx.entries.into_values() {
        for e in versions {
            out.push(ChartVersion {
                name: e.name,
                version: e.version,
                digest: e.digest,
            });
        }
    }
    Ok(out)
}

/// Compiled rules ready to classify versions.
pub struct Compiled {
    patterns: BTreeMap<String, Regex>,
    first_party: Vec<String>,
    authority: Option<AuthorityConfig>,
    groups: Vec<CompiledGroup>,
}

struct CompiledGroup {
    source: String,
    rules: Vec<(String, HelmAction)>, // (class name, action)
    retire: bool,
}

impl Compiled {
    pub fn compile(file: HelmRulesFile) -> Result<Self> {
        let mut patterns = BTreeMap::new();
        for (name, re) in &file.patterns {
            patterns.insert(
                name.clone(),
                Regex::new(re).with_context(|| format!("bad regex for pattern '{name}'"))?,
            );
        }
        let mut groups = Vec::new();
        for g in file.groups {
            let mut rules = Vec::new();
            for r in g.rules {
                if !patterns.contains_key(&r.class) {
                    anyhow::bail!(
                        "group '{}' references unknown pattern '{}'",
                        g.source,
                        r.class
                    );
                }
                let action = match r.action.as_deref() {
                    Some("delete") => HelmAction::Delete,
                    Some("delete-if-on-authority") => HelmAction::DeleteIfOnAuthority,
                    Some("move") | None => HelmAction::Move {
                        dest: r.dest.clone().ok_or_else(|| {
                            anyhow::anyhow!(
                                "group '{}' rule for class '{}' is a move but has no dest",
                                g.source,
                                r.class
                            )
                        })?,
                    },
                    Some(other) => anyhow::bail!("unknown action '{other}'"),
                };
                rules.push((r.class.clone(), action));
            }
            groups.push(CompiledGroup {
                source: g.source,
                rules,
                retire: g.retire,
            });
        }
        Ok(Self {
            patterns,
            first_party: file.first_party,
            authority: file.authority,
            groups,
        })
    }

    /// Is this chart name first-party?
    fn is_first_party(&self, name: &str) -> bool {
        self.first_party.iter().any(|p| name.starts_with(p))
    }

    /// Find the first rule whose class pattern matches the version.
    fn classify(&self, group: &CompiledGroup, version: &str) -> Option<(String, HelmAction)> {
        for (class, action) in &group.rules {
            if let Some(re) = self.patterns.get(class) {
                if re.is_match(version) {
                    return Some((class.clone(), action.clone()));
                }
            }
        }
        None
    }
}

/// One planned operation on a chart version.
#[derive(Debug, Clone)]
pub struct PlannedOp {
    pub source: String,
    pub name: String,
    pub version: String,
    pub digest: String,
    pub class: String,
    pub action: HelmAction,
}

/// The full reorg plan plus the buckets needed to report it honestly.
#[derive(Debug, Default)]
pub struct Plan {
    pub ops: Vec<PlannedOp>,
    /// First-party versions no rule matched (nothing is done to these).
    pub unclassified: Vec<(String, ChartVersion)>,
    /// Third-party charts skipped by name (informational count only).
    pub third_party_skipped: usize,
}

/// Build the reorg plan from the live indexes of each group's source repo.
pub async fn plan(client: &DepotClient, compiled: &Compiled) -> Result<Plan> {
    let mut plan = Plan::default();
    for group in &compiled.groups {
        let index = client
            .download_raw(&group.source, "index.yaml")
            .await
            .with_context(|| format!("fetch index.yaml for '{}'", group.source))?;
        let versions = parse_index(&index)?;
        for cv in versions {
            if !compiled.is_first_party(&cv.name) {
                plan.third_party_skipped += 1;
                continue;
            }
            match compiled.classify(group, &cv.version) {
                Some((class, action)) => plan.ops.push(PlannedOp {
                    source: group.source.clone(),
                    name: cv.name.clone(),
                    version: cv.version.clone(),
                    digest: cv.digest.clone(),
                    class,
                    action,
                }),
                None => plan.unclassified.push((group.source.clone(), cv)),
            }
        }
    }
    Ok(plan)
}

/// Print the dry-run plan: grouped by action, with the unclassified bucket and
/// third-party skip count spelled out so nothing is silently dropped.
pub fn print_plan(plan: &Plan, compiled: &Compiled) {
    use std::collections::BTreeMap as Map;
    let mut moves: Map<String, usize> = Map::new();
    let mut deletes = 0usize;
    let mut verify_deletes = 0usize;
    for op in &plan.ops {
        match &op.action {
            HelmAction::Move { dest } => *moves.entry(dest.clone()).or_default() += 1,
            HelmAction::Delete => deletes += 1,
            HelmAction::DeleteIfOnAuthority => verify_deletes += 1,
        }
    }
    println!("\n=== Helm reorg plan ===");
    for (dest, n) in &moves {
        println!("  MOVE  {n:>5} chart-versions -> {dest}");
    }
    if deletes > 0 {
        println!("  DELETE {deletes:>4} chart-versions (junk/superseded)");
    }
    if verify_deletes > 0 {
        let auth = compiled
            .authority
            .as_ref()
            .map(|a| format!("{}/repository/{}", a.url, a.repo))
            .unwrap_or_else(|| "<none configured>".to_string());
        println!(
            "  VERIFY+DELETE {verify_deletes:>4} released versions (drop locally iff \
             upstream authority {auth} serves the same digest)"
        );
    }
    println!(
        "  SKIP  {:>5} third-party chart-versions (untouched)",
        plan.third_party_skipped
    );
    if !plan.unclassified.is_empty() {
        println!(
            "\n  UNCLASSIFIED ({}) — first-party but no rule matched; NOTHING will be done:",
            plan.unclassified.len()
        );
        let mut shown = 0;
        for (src, cv) in &plan.unclassified {
            if shown < 40 {
                println!("    {src}: {}:{}", cv.name, cv.version);
            }
            shown += 1;
        }
        if shown > 40 {
            println!("    … and {} more", shown - 40);
        }
    }
    for g in &compiled.groups {
        if g.retire {
            // Every planned op removes its version from the source (a move and
            // both delete kinds all leave the source); only unclassified
            // first-party versions would remain and block retirement.
            let leaving = plan.ops.iter().filter(|o| o.source == g.source).count();
            let remaining = plan
                .unclassified
                .iter()
                .filter(|(s, _)| *s == g.source)
                .count();
            println!(
                "\n  RETIRE '{}' — {leaving} version(s) leave via move/delete; \
                 {remaining} unclassified would remain{}",
                g.source,
                if remaining == 0 {
                    " (repo will be empty of first-party charts)".to_string()
                } else {
                    " (handle these before deleting the repo)".to_string()
                }
            );
        }
    }
}

/// Which action groups to actually apply, parsed from `--apply`.
#[derive(Debug, Clone)]
pub struct ApplySelection {
    pub moves: bool,
    pub deletes: bool,
    pub verify_deletes: bool,
}

impl ApplySelection {
    pub fn parse(spec: &str) -> Self {
        let set: BTreeSet<&str> = spec.split(',').map(|s| s.trim()).collect();
        let all = set.contains("all");
        Self {
            moves: all || set.contains("move"),
            deletes: all || set.contains("delete"),
            verify_deletes: all || set.contains("verify-delete"),
        }
    }
}

/// Apply the selected action groups. Moves and plain deletes go straight to the
/// staging API; `delete-if-on-authority` first confirms the authority index
/// carries the identical digest, and **skips** (loudly) any version the
/// authority lacks — so a re-file never silently destroys the only copy.
pub async fn apply(
    client: &DepotClient,
    plan: &Plan,
    compiled: &Compiled,
    sel: &ApplySelection,
    insecure: bool,
) -> Result<()> {
    // Preload the authority digest set once if any verify-delete is selected.
    // Queried DIRECTLY against the upstream registry with its own credentials —
    // never through a local cache repo (which would both give an incomplete
    // answer and warm the cache, which is forbidden).
    let authority: Option<BTreeSet<String>> = if sel.verify_deletes
        && plan
            .ops
            .iter()
            .any(|o| o.action == HelmAction::DeleteIfOnAuthority)
    {
        let cfg = compiled.authority.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "verify-delete selected but no [authority] configured in the rules file"
            )
        })?;
        let user = std::env::var("UPSTREAM_USERNAME").context(
            "verify-delete needs UPSTREAM_USERNAME/UPSTREAM_PASSWORD to reach the authority",
        )?;
        let pass = std::env::var("UPSTREAM_PASSWORD").context(
            "verify-delete needs UPSTREAM_USERNAME/UPSTREAM_PASSWORD to reach the authority",
        )?;
        let upstream = DepotClient::new(&cfg.url, &user, &pass, insecure)?;
        let index = upstream
            .download_raw(&cfg.repo, "index.yaml")
            .await
            .with_context(|| {
                format!(
                    "fetch authority index.yaml from {}/repository/{}",
                    cfg.url, cfg.repo
                )
            })?;
        let set = parse_index(&index)?
            .into_iter()
            .map(|cv| format!("{}:{}:{}", cv.name, cv.version, cv.digest))
            .collect();
        Some(set)
    } else {
        None
    };

    let mut moved = 0u64;
    let mut deleted = 0u64;
    let mut verified_deleted = 0u64;
    let mut skipped_missing = 0u64;

    for op in &plan.ops {
        match &op.action {
            HelmAction::Move { dest } if sel.moves => {
                client
                    .helm_staging_move(&op.source, dest, &op.name, &op.version)
                    .await?;
                moved += 1;
            }
            HelmAction::Delete if sel.deletes => {
                client
                    .helm_staging_delete(&op.source, &op.name, &op.version)
                    .await?;
                deleted += 1;
            }
            HelmAction::DeleteIfOnAuthority if sel.verify_deletes => {
                let key = format!("{}:{}:{}", op.name, op.version, op.digest);
                let present = authority.as_ref().is_some_and(|a| a.contains(&key));
                if present {
                    client
                        .helm_staging_delete(&op.source, &op.name, &op.version)
                        .await?;
                    verified_deleted += 1;
                } else {
                    eprintln!(
                        "  SKIP delete {}:{} — authority does not serve this exact digest; \
                         republish upstream before removing the local copy",
                        op.name, op.version
                    );
                    skipped_missing += 1;
                }
            }
            _ => {}
        }
    }
    println!(
        "\nApplied: {moved} moved, {deleted} deleted, {verified_deleted} verified-deleted, \
         {skipped_missing} skipped (not on authority)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        first_party = ["myriad", "myriad-uui", "quantum-orchestrator", "observability", "perfsuite", "garage", "flexsync"]

        [authority]
        url = "https://insight.example.com"
        repo = "helm-release"

        [patterns]
        released   = '^\d+\.\d+\.\d+$'
        prerelease = '^\d+\.\d+\.\d+-(dev|rc)\.\d+$'
        branch     = '^\d+\.\d+\.\d+-(develop|main|master)$'
        junk       = '(deleteme|^1\.0\.0-0-9-0$)'

        [[groups]]
        source = "helm-external"
        retire = false

          [[groups.rules]]
          class = "junk"
          action = "delete"

          [[groups.rules]]
          class = "released"
          action = "delete-if-on-authority"

          [[groups.rules]]
          class = "prerelease"
          dest = "helm-prerelease"

          [[groups.rules]]
          class = "branch"
          dest = "helm-prerelease"

        [[groups]]
        source = "helm-internal"
        retire = true

          [[groups.rules]]
          class = "prerelease"
          dest = "helm-prerelease"
    "#;

    fn compiled() -> Compiled {
        let file: HelmRulesFile = toml::from_str(SAMPLE).unwrap();
        Compiled::compile(file).unwrap()
    }

    #[test]
    fn rules_compile_with_all_actions() {
        let c = compiled();
        assert_eq!(c.groups.len(), 2);
        assert_eq!(c.patterns.len(), 4);
        assert_eq!(c.authority.as_ref().unwrap().repo, "helm-release");
        // helm-external: junk→delete, released→verify, prerelease/branch→move
        assert_eq!(c.groups[0].rules.len(), 4);
    }

    #[test]
    fn first_party_prefix_match() {
        let c = compiled();
        assert!(c.is_first_party("myriad"));
        assert!(c.is_first_party("myriad-uui"));
        assert!(c.is_first_party("quantum-orchestrator"));
        // third-party charts are never first-party
        assert!(!c.is_first_party("cilium"));
        assert!(!c.is_first_party("ingress-nginx"));
        assert!(!c.is_first_party("minio"));
    }

    #[test]
    fn classify_each_class() {
        let c = compiled();
        let g = &c.groups[0]; // helm-external
        let action_for = |v: &str| c.classify(g, v).map(|(_, a)| a);

        assert_eq!(action_for("1.4.2"), Some(HelmAction::DeleteIfOnAuthority));
        assert_eq!(
            action_for("1.6.0-dev.145"),
            Some(HelmAction::Move {
                dest: "helm-prerelease".into()
            })
        );
        assert_eq!(
            action_for("1.0.0-develop"),
            Some(HelmAction::Move {
                dest: "helm-prerelease".into()
            })
        );
        assert_eq!(action_for("0.9.0-deleteme"), Some(HelmAction::Delete));
        assert_eq!(action_for("1.0.0-0-9-0"), Some(HelmAction::Delete));
        // CI-ish version no rule covers → unclassified
        assert_eq!(action_for("1.0.0-jss-dev-4"), None);
    }

    #[test]
    fn junk_rule_wins_before_released() {
        // Order matters: `1.0.0-0-9-0` must hit junk (listed first), not fall
        // through — and a plain released version must NOT match junk.
        let c = compiled();
        let g = &c.groups[0];
        assert_eq!(c.classify(g, "1.0.0-0-9-0").unwrap().0, "junk");
        assert_eq!(c.classify(g, "1.5.1").unwrap().0, "released");
    }

    #[test]
    fn parse_index_extracts_name_version_digest() {
        let yaml = br#"
apiVersion: v1
entries:
  myriad:
    - name: myriad
      version: "1.4.2"
      digest: "sha256:abc123"
    - name: myriad
      version: "1.6.0-dev.145"
      digest: "sha256:def456"
  myriad-uui:
    - name: myriad-uui
      version: "1.0.0-develop"
      digest: "sha256:aaa"
"#;
        let mut versions = parse_index(yaml).unwrap();
        versions.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].name, "myriad");
        assert_eq!(versions[0].version, "1.4.2");
        assert_eq!(versions[0].digest, "sha256:abc123");
        assert_eq!(versions[2].name, "myriad-uui");
    }

    #[test]
    fn plan_buckets_third_party_and_unclassified() {
        // Build a plan by hand from a parsed index to exercise bucketing
        // without a live server.
        let c = compiled();
        let g = &c.groups[0];
        let index: Vec<ChartVersion> = vec![
            ("myriad", "1.4.2"),           // released → verify-delete
            ("myriad", "1.6.0-dev.145"),   // prerelease → move
            ("myriad", "0.9.0-deleteme"),  // junk → delete
            ("myriad", "1.0.0-jss-dev-4"), // first-party, no rule → unclassified
            ("cilium", "1.15.0"),          // third-party → skip
        ]
        .into_iter()
        .map(|(n, v)| ChartVersion {
            name: n.into(),
            version: v.into(),
            digest: String::new(),
        })
        .collect();

        let mut plan = Plan::default();
        for cv in index {
            if !c.is_first_party(&cv.name) {
                plan.third_party_skipped += 1;
                continue;
            }
            match c.classify(g, &cv.version) {
                Some((class, action)) => plan.ops.push(PlannedOp {
                    source: g.source.clone(),
                    name: cv.name.clone(),
                    version: cv.version.clone(),
                    digest: cv.digest.clone(),
                    class,
                    action,
                }),
                None => plan.unclassified.push((g.source.clone(), cv)),
            }
        }
        assert_eq!(plan.ops.len(), 3, "released+prerelease+junk classified");
        assert_eq!(plan.third_party_skipped, 1, "cilium skipped");
        assert_eq!(plan.unclassified.len(), 1, "jss-dev unclassified");
        assert_eq!(plan.unclassified[0].1.version, "1.0.0-jss-dev-4");
    }

    #[test]
    fn apply_selection_parses() {
        let all = ApplySelection::parse("all");
        assert!(all.moves && all.deletes && all.verify_deletes);
        let moves_only = ApplySelection::parse("move");
        assert!(moves_only.moves && !moves_only.deletes && !moves_only.verify_deletes);
        let two = ApplySelection::parse("move, verify-delete");
        assert!(two.moves && !two.deletes && two.verify_deletes);
    }

    #[test]
    fn move_without_dest_is_rejected() {
        let bad = r#"
            first_party = ["myriad"]
            [patterns]
            prerelease = '^\d+\.\d+\.\d+-dev\.\d+$'
            [[groups]]
            source = "helm-external"
              [[groups.rules]]
              class = "prerelease"
              action = "move"
        "#;
        let file: HelmRulesFile = toml::from_str(bad).unwrap();
        assert!(
            Compiled::compile(file).is_err(),
            "move with no dest must error"
        );
    }

    #[test]
    fn unknown_pattern_reference_is_rejected() {
        let bad = r#"
            first_party = ["myriad"]
            [patterns]
            prerelease = '^\d+\.\d+\.\d+-dev\.\d+$'
            [[groups]]
            source = "helm-external"
              [[groups.rules]]
              class = "nonexistent"
              dest = "helm-prerelease"
        "#;
        let file: HelmRulesFile = toml::from_str(bad).unwrap();
        assert!(
            Compiled::compile(file).is_err(),
            "unknown pattern must error"
        );
    }
}

#[cfg(test)]
mod example_file_test {
    use super::*;
    #[test]
    fn shipped_example_parses_and_compiles() {
        let text = include_str!("../../etc/helm-reorg.example.toml");
        let file: HelmRulesFile = toml::from_str(text).expect("example toml parses");
        let c = Compiled::compile(file).expect("example compiles");
        assert!(c.authority.is_some());
        assert!(!c.groups.is_empty());
    }
}
