// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Rules-driven artifact repository reorganization.
//!
//! Everything is data-driven by a TOML rules file — the product compiles no
//! repo names, image prefixes, or tag conventions. The file defines:
//!
//! - `[patterns]` — reusable named regexes (referenced by name from any rule,
//!   or a rule may inline a regex). Patterns are auto-anchored (`^(?:…)$`).
//! - `[[group]]` — one per artifact `format` (docker today; helm/raw later
//!   reuse the same patterns). A group lists its source repos, the image-name
//!   prefixes considered first-party, repos to purge wholesale, and an ordered
//!   list of `[[group.rule]]`.
//!
//! Each rule matches a tag (by pattern name or inline regex) with an optional
//! exact-image filter, and carries an `action`:
//!
//! - `move` — relocate the tag into `dest` (staging/move)
//! - `delete_if_absent` — delete from source only if `check` repo lacks it
//! - `reconcile` — released-image reconcile against a canonical `check` repo:
//!   absent → move to `dest`; present+same-digest → delete; present+diff → leave
//! - `delete` — delete from source unconditionally
//! - `leave` — leave in place (reported)
//!
//! First matching rule wins; an unmatched tag is left in place (add a trailing
//! `match = '.*'` rule to route the remainder somewhere, e.g. development).
//!
//! Dry-run by default (prints the plan; changes nothing). `--apply` executes;
//! `--copy` uses staging/copy for moves and skips every destructive action.

use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Deserialize;

use crate::client::DepotClient;

// ---------------------------------------------------------------------------
// Rules file (TOML)
// ---------------------------------------------------------------------------

/// The action a rule applies to a matching `image:tag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Relocate the tag into `dest`.
    Move,
    /// Delete from source only if the `check` repo does not have the tag.
    DeleteIfAbsent,
    /// Released-image reconcile against a canonical `check` repo (e.g. insight):
    /// absent from `check` → move to `dest` (supplementary); present with the
    /// same content digest → delete the redundant source copy; present with a
    /// different digest → leave + flag for review (not the same image).
    Reconcile,
    /// Delete from source unconditionally.
    Delete,
    /// Leave in place (reported).
    Leave,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    /// Pattern name (from `[patterns]`) or an inline regex.
    #[serde(rename = "match")]
    pub match_: String,
    /// Optional exact image-name filter; empty = any image.
    #[serde(default)]
    pub images: Vec<String>,
    pub action: Action,
    /// Destination repo for `move`.
    #[serde(default)]
    pub dest: Option<String>,
    /// Presence-check repo for `delete_if_absent`.
    #[serde(default)]
    pub check: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Group {
    /// Artifact format this group applies to (`docker` is the only one
    /// currently supported by the server-side movers).
    pub format: String,
    #[serde(default)]
    pub source_repos: Vec<String>,
    /// Image-name prefixes considered first-party; only these are touched.
    #[serde(default)]
    pub first_party_prefixes: Vec<String>,
    /// Repos emptied wholesale (every image/tag), e.g. a dead cache.
    #[serde(default)]
    pub purge_repos: Vec<String>,
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RulesFile {
    /// Reusable named regexes.
    #[serde(default)]
    pub patterns: HashMap<String, String>,
    #[serde(default)]
    pub group: Vec<Group>,
    /// Repository to read last-accessed (usage) data from for the dry-run
    /// report — typically the docker group/proxy, so each tag's *live* atime is
    /// reported (the stale source member resolves last in the group). Report
    /// only; never filters what gets moved/deleted.
    #[serde(default)]
    pub usage_repo: Option<String>,
}

impl RulesFile {
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).context("parse reorg rules TOML")
    }
}

// ---------------------------------------------------------------------------
// Compiled form
// ---------------------------------------------------------------------------

struct CompiledRule {
    re: Regex,
    images: Vec<String>,
    action: Action,
    dest: Option<String>,
    check: Option<String>,
}

struct CompiledGroup {
    format: String,
    source_repos: Vec<String>,
    first_party_prefixes: Vec<String>,
    purge_repos: Vec<String>,
    rules: Vec<CompiledRule>,
}

impl CompiledGroup {
    fn is_first_party(&self, image: &str) -> bool {
        self.first_party_prefixes
            .iter()
            .any(|p| image.starts_with(p))
    }
}

/// What to do with one `image:tag`, resolved from the first matching rule.
#[derive(Debug, PartialEq, Eq)]
enum Decision<'a> {
    Move(&'a str),
    DeleteIfAbsent(&'a str),
    Reconcile { check: &'a str, dest: &'a str },
    Delete,
    Leave,
}

/// Resolve a rule's `match` (pattern name or inline regex) to an anchored regex.
fn compile_match(patterns: &HashMap<String, String>, match_: &str) -> Result<Regex> {
    let src = patterns.get(match_).map(String::as_str).unwrap_or(match_);
    Regex::new(&format!("^(?:{src})$"))
        .with_context(|| format!("compile regex for match '{match_}' (resolved: '{src}')"))
}

/// Compile and validate every group's rules.
fn compile_groups(rules: &RulesFile) -> Result<Vec<CompiledGroup>> {
    let mut out = Vec::new();
    for g in &rules.group {
        let mut crules = Vec::new();
        for r in &g.rules {
            match r.action {
                Action::Move if r.dest.is_none() => {
                    bail!("rule match='{}' action=move requires 'dest'", r.match_)
                }
                Action::DeleteIfAbsent if r.check.is_none() => {
                    bail!(
                        "rule match='{}' action=delete_if_absent requires 'check'",
                        r.match_
                    )
                }
                Action::Reconcile if r.check.is_none() || r.dest.is_none() => {
                    bail!(
                        "rule match='{}' action=reconcile requires 'check' and 'dest'",
                        r.match_
                    )
                }
                _ => {}
            }
            crules.push(CompiledRule {
                re: compile_match(&rules.patterns, &r.match_)?,
                images: r.images.clone(),
                action: r.action,
                dest: r.dest.clone(),
                check: r.check.clone(),
            });
        }
        out.push(CompiledGroup {
            format: g.format.clone(),
            source_repos: g.source_repos.clone(),
            first_party_prefixes: g.first_party_prefixes.clone(),
            purge_repos: g.purge_repos.clone(),
            rules: crules,
        });
    }
    Ok(out)
}

/// Apply the group's rules to an `image:tag` — first match (regex matches the
/// tag and the optional image filter passes) wins.
fn decide<'a>(group: &'a CompiledGroup, image: &str, tag: &str) -> Decision<'a> {
    for r in &group.rules {
        let image_ok = r.images.is_empty() || r.images.iter().any(|i| i == image);
        if image_ok && r.re.is_match(tag) {
            return match r.action {
                Action::Move => Decision::Move(r.dest.as_deref().unwrap_or_default()),
                Action::DeleteIfAbsent => {
                    Decision::DeleteIfAbsent(r.check.as_deref().unwrap_or_default())
                }
                Action::Reconcile => Decision::Reconcile {
                    check: r.check.as_deref().unwrap_or_default(),
                    dest: r.dest.as_deref().unwrap_or_default(),
                },
                Action::Delete => Decision::Delete,
                Action::Leave => Decision::Leave,
            };
        }
    }
    Decision::Leave
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRef {
    pub source_repo: String,
    pub image: String,
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMove {
    pub source_repo: String,
    pub image: String,
    pub tag: String,
    pub dest: String,
}

/// A delete gated on absence from a presence-check repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalDelete {
    pub tag: TagRef,
    pub check_repo: String,
}

/// A released-image reconcile pending a digest comparison against `check_repo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileItem {
    pub tag: TagRef,
    pub check_repo: String,
    pub dest: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub moves: Vec<PlannedMove>,
    pub conditional_deletes: Vec<ConditionalDelete>,
    pub reconciles: Vec<ReconcileItem>,
    pub deletes: Vec<TagRef>,
    pub leaves: Vec<TagRef>,
}

/// Build a plan for one group from its `(image, tag)` inventory. Pure: no I/O.
fn build_group_plan(
    group: &CompiledGroup,
    inventory: &[(String, String)],
    source_repo: &str,
) -> Plan {
    let mut plan = Plan::default();
    for (image, tag) in inventory {
        if !group.is_first_party(image) {
            continue;
        }
        let tagref = || TagRef {
            source_repo: source_repo.to_string(),
            image: image.clone(),
            tag: tag.clone(),
        };
        match decide(group, image, tag) {
            Decision::Move(dest) => plan.moves.push(PlannedMove {
                source_repo: source_repo.to_string(),
                image: image.clone(),
                tag: tag.clone(),
                dest: dest.to_string(),
            }),
            Decision::DeleteIfAbsent(check) => plan.conditional_deletes.push(ConditionalDelete {
                tag: tagref(),
                check_repo: check.to_string(),
            }),
            Decision::Reconcile { check, dest } => plan.reconciles.push(ReconcileItem {
                tag: tagref(),
                check_repo: check.to_string(),
                dest: dest.to_string(),
            }),
            Decision::Delete => plan.deletes.push(tagref()),
            Decision::Leave => plan.leaves.push(tagref()),
        }
    }
    plan
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

pub struct ReorgConfig {
    pub rules_path: String,
    /// Perform the actions. When false (default), only print the plan.
    pub apply: bool,
    /// Use `staging/copy` for moves and skip all destructive actions.
    pub copy: bool,
}

/// Resolved, ready-to-execute plan (after presence checks + purge enumeration).
#[derive(Default)]
struct ResolvedPlan {
    moves: Vec<PlannedMove>,
    deletes: Vec<TagRef>,
    kept: Vec<TagRef>,
    leaves: Vec<TagRef>,
    purges: Vec<TagRef>,
    /// Reconcile tags present in the check repo but with a *different* digest —
    /// not the same image, so left in place pending review.
    mismatched: Vec<TagRef>,
}

pub async fn run(client: &DepotClient, cfg: ReorgConfig) -> Result<()> {
    let text = std::fs::read_to_string(&cfg.rules_path)
        .with_context(|| format!("read rules file '{}'", cfg.rules_path))?;
    let rules = RulesFile::from_toml(&text)?;
    let groups = compile_groups(&rules)?;
    client.login().await?;

    let mut resolved = ResolvedPlan::default();

    for group in &groups {
        if group.format != "docker" {
            bail!(
                "format '{}' is not supported yet (only 'docker'); \
                 add server-side movers before using it",
                group.format
            );
        }

        for repo in &group.source_repos {
            let inv = list_repo_tags(client, repo).await?;
            let plan = build_group_plan(group, &inv, repo);
            resolved.moves.extend(plan.moves);
            resolved.deletes.extend(plan.deletes);
            resolved.leaves.extend(plan.leaves);

            // Resolve conditional deletes via a presence check.
            for cd in plan.conditional_deletes {
                if insight_has(client, &cd.check_repo, &cd.tag.image, &cd.tag.tag).await? {
                    resolved.kept.push(cd.tag);
                } else {
                    resolved.deletes.push(cd.tag);
                }
            }

            // Resolve reconciles: compare the source digest against the canonical
            // check repo. Absent → move to dest (supplementary); present + same
            // digest → delete the redundant source copy; present + different
            // digest → leave + flag (not the same image).
            for rec in plan.reconciles {
                let (check_status, _, check_digest) = client
                    .docker_head_manifest(&rec.check_repo, &rec.tag.image, &rec.tag.tag)
                    .await
                    .with_context(|| {
                        format!(
                            "reconcile check {}/{}:{}",
                            rec.check_repo, rec.tag.image, rec.tag.tag
                        )
                    })?;
                if check_status != 200 {
                    resolved.moves.push(PlannedMove {
                        source_repo: rec.tag.source_repo.clone(),
                        image: rec.tag.image.clone(),
                        tag: rec.tag.tag.clone(),
                        dest: rec.dest.clone(),
                    });
                    continue;
                }
                let (_, _, src_digest) = client
                    .docker_head_manifest(&rec.tag.source_repo, &rec.tag.image, &rec.tag.tag)
                    .await
                    .with_context(|| {
                        format!(
                            "reconcile source {}/{}:{}",
                            rec.tag.source_repo, rec.tag.image, rec.tag.tag
                        )
                    })?;
                if !check_digest.is_empty() && check_digest == src_digest {
                    resolved.deletes.push(rec.tag);
                } else {
                    resolved.mismatched.push(rec.tag);
                }
            }
        }

        // Purge repos: every image/tag.
        for repo in &group.purge_repos {
            for (image, tag) in list_repo_tags(client, repo).await? {
                resolved.purges.push(TagRef {
                    source_repo: repo.clone(),
                    image,
                    tag,
                });
            }
        }
    }

    // Optional: annotate the plan with last-accessed (usage) data from the
    // configured usage repo (e.g. the docker proxy). Report-only — never alters
    // what gets moved/deleted. One browse call per distinct image.
    let mut atimes: HashMap<(String, String), String> = HashMap::new();
    if let Some(usage_repo) = rules.usage_repo.as_deref() {
        let mut images: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for m in &resolved.moves {
            images.insert(m.image.clone());
        }
        for t in resolved
            .deletes
            .iter()
            .chain(&resolved.kept)
            .chain(&resolved.leaves)
            .chain(&resolved.mismatched)
        {
            images.insert(t.image.clone());
        }
        for image in images {
            match client.image_tag_atimes(usage_repo, &image).await {
                Ok(tags) => {
                    for (tag, at) in tags {
                        if let Some(at) = at {
                            let date = at.get(0..10).unwrap_or(&at).to_string();
                            atimes.insert((image.clone(), tag), date);
                        }
                    }
                }
                Err(e) => eprintln!("  (usage lookup failed for '{image}': {e})"),
            }
        }
    }

    print_plan(&resolved, cfg.copy, rules.usage_repo.as_deref(), &atimes);

    if !cfg.apply {
        println!("\nDry run — no changes made. Re-run with --apply to execute.");
        return Ok(());
    }

    let mut ok = 0usize;
    let mut failed = 0usize;
    let verb = if cfg.copy { "copy" } else { "move" };

    for m in &resolved.moves {
        let r = if cfg.copy {
            client
                .staging_copy(&m.source_repo, &m.dest, &m.image, &m.tag)
                .await
        } else {
            client
                .staging_move(&m.source_repo, &m.dest, &m.image, &m.tag)
                .await
        };
        report(
            &mut ok,
            &mut failed,
            verb,
            &m.source_repo,
            &m.image,
            &m.tag,
            r,
        );
    }

    if cfg.copy {
        let skipped = resolved.deletes.len() + resolved.purges.len();
        if skipped > 0 {
            println!("\n--copy is non-destructive: skipped {skipped} delete/purge action(s).");
        }
    } else {
        for d in resolved.deletes.iter().chain(resolved.purges.iter()) {
            let r = client
                .staging_delete(&d.source_repo, &d.image, &d.tag)
                .await;
            report(
                &mut ok,
                &mut failed,
                "delete",
                &d.source_repo,
                &d.image,
                &d.tag,
                r,
            );
        }
    }

    println!("\nApplied: {ok} succeeded, {failed} failed.");
    if failed > 0 {
        bail!("{failed} staging operation(s) failed");
    }
    Ok(())
}

async fn list_repo_tags(client: &DepotClient, repo: &str) -> Result<Vec<(String, String)>> {
    let images = client
        .docker_repo_catalog(repo)
        .await
        .with_context(|| format!("list images in '{repo}'"))?;
    let mut out = Vec::new();
    for image in images {
        let tags = client
            .docker_list_tags(repo, &image)
            .await
            .with_context(|| format!("list tags for '{repo}/{image}'"))?;
        for tag in tags {
            out.push((image.clone(), tag));
        }
    }
    Ok(out)
}

/// HEAD the tag against a repo; true if it is served there (the cache resolves
/// through to its upstream).
async fn insight_has(client: &DepotClient, repo: &str, image: &str, tag: &str) -> Result<bool> {
    let (status, _, _) = client
        .docker_head_manifest(repo, image, tag)
        .await
        .with_context(|| format!("presence check {repo}/{image}:{tag}"))?;
    Ok(status == 200)
}

#[allow(clippy::too_many_arguments)]
fn report(
    ok: &mut usize,
    failed: &mut usize,
    verb: &str,
    repo: &str,
    image: &str,
    tag: &str,
    result: Result<()>,
) {
    match result {
        Ok(()) => {
            *ok += 1;
            println!("  {verb} ok: {repo}/{image}:{tag}");
        }
        Err(e) => {
            *failed += 1;
            eprintln!("  {verb} FAILED: {repo}/{image}:{tag}: {e}");
        }
    }
}

/// Format the last-accessed annotation for a tag line. Empty when no usage repo
/// is configured; otherwise `[pulled DATE]` if the usage repo served the tag, or
/// `[absent from <repo>]` if it has no copy (a strong "stale, safe to drop" hint).
fn atime_note(
    usage_repo: Option<&str>,
    atimes: &HashMap<(String, String), String>,
    image: &str,
    tag: &str,
) -> String {
    match usage_repo {
        Some(ur) => match atimes.get(&(image.to_string(), tag.to_string())) {
            Some(date) => format!("  [pulled {date}]"),
            None => format!("  [absent from {ur}]"),
        },
        None => String::new(),
    }
}

fn print_plan(
    p: &ResolvedPlan,
    copy: bool,
    usage_repo: Option<&str>,
    atimes: &HashMap<(String, String), String>,
) {
    let verb = if copy { "copy" } else { "move" };
    let del_note = if copy { " (skipped: --copy)" } else { "" };
    if let Some(ur) = usage_repo {
        println!("(usage annotations show last-accessed from '{ur}')");
    }

    println!("Planned {verb}s ({} tag(s)):", p.moves.len());
    let mut by_dest: BTreeMap<&str, Vec<&PlannedMove>> = BTreeMap::new();
    for m in &p.moves {
        by_dest.entry(m.dest.as_str()).or_default().push(m);
    }
    for (dest, moves) in &by_dest {
        println!("  -> {dest} ({} tag(s)):", moves.len());
        for m in moves {
            let note = atime_note(usage_repo, atimes, &m.image, &m.tag);
            println!("       {}/{}:{}{note}", m.source_repo, m.image, m.tag);
        }
    }

    println!("\nDeletes{del_note} ({} tag(s)):", p.deletes.len());
    for d in &p.deletes {
        let note = atime_note(usage_repo, atimes, &d.image, &d.tag);
        println!("       {}/{}:{}{note}", d.source_repo, d.image, d.tag);
    }

    if !p.purges.is_empty() {
        let mut by_repo: BTreeMap<&str, usize> = BTreeMap::new();
        for t in &p.purges {
            *by_repo.entry(t.source_repo.as_str()).or_default() += 1;
        }
        println!(
            "\nPurge{del_note} ({} tag(s) across {} repo(s)):",
            p.purges.len(),
            by_repo.len()
        );
        for (repo, n) in &by_repo {
            println!("  {repo}: {n} tag(s)");
        }
    }

    if !p.kept.is_empty() {
        println!(
            "\nKept ({} tag(s)) — already present in the check repo, left in place:",
            p.kept.len()
        );
        for k in &p.kept {
            let note = atime_note(usage_repo, atimes, &k.image, &k.tag);
            println!("       {}/{}:{}{note}", k.source_repo, k.image, k.tag);
        }
    }

    if !p.mismatched.is_empty() {
        println!(
            "\nReconcile mismatch ({} tag(s)) — in the check repo but a DIFFERENT digest; \
             left in place for review:",
            p.mismatched.len()
        );
        for m in &p.mismatched {
            let note = atime_note(usage_repo, atimes, &m.image, &m.tag);
            println!("       {}/{}:{}{note}", m.source_repo, m.image, m.tag);
        }
    }

    if p.leaves.is_empty() {
        println!("\nUnclassified: none.");
    } else {
        println!(
            "\nUnclassified ({} tag(s)) — no rule matched, left in place:",
            p.leaves.len()
        );
        for l in &p.leaves {
            let note = atime_note(usage_repo, atimes, &l.image, &l.tag);
            println!("       {}/{}:{}{note}", l.source_repo, l.image, l.tag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atime_note_formats() {
        let mut m = HashMap::new();
        m.insert(
            ("myriad/dev".to_string(), "develop".to_string()),
            "2026-06-25".to_string(),
        );
        // No usage repo configured → no annotation.
        assert_eq!(atime_note(None, &m, "myriad/dev", "develop"), "");
        // Present in the usage repo → last-pull date.
        assert_eq!(
            atime_note(Some("docker"), &m, "myriad/dev", "develop"),
            "  [pulled 2026-06-25]"
        );
        // Absent from the usage repo → flagged (stale hint).
        assert_eq!(
            atime_note(Some("docker"), &m, "myriad/dev", "missing"),
            "  [absent from docker]"
        );
    }

    const SAMPLE: &str = r#"
        [patterns]
        released   = '\d+\.\d+\.\d+'
        prerelease = '\d+\.\d+\.\d+-(dev|rc)\.\d+'
        develop    = 'develop'
        ci         = 'ci-.+-\d+'
        developer  = '[a-z][a-z0-9]*-\d+'

        [[group]]
        format = "docker"
        source_repos = ["docker-internal", "docker-external"]
        first_party_prefixes = ["myriad/", "qkp/", "orchestrator/"]
        purge_repos = ["docker-upstream"]

          [[group.rule]]
          match = "prerelease"
          action = "move"
          dest = "docker-prerelease"

          [[group.rule]]
          match = "released"
          action = "reconcile"
          check = "docker-insight"
          dest = "docker-release-aux"

          [[group.rule]]
          match = "develop"
          action = "move"
          dest = "docker-prerelease"

          [[group.rule]]
          match = "ci"
          action = "move"
          dest = "docker-development-local"

          [[group.rule]]
          match = "developer"
          action = "move"
          dest = "docker-development-local"

          [[group.rule]]
          match = '.*'
          action = "move"
          dest = "docker-development"
    "#;

    fn sample_group() -> CompiledGroup {
        let rules = RulesFile::from_toml(SAMPLE).unwrap();
        compile_groups(&rules).unwrap().pop().unwrap()
    }

    #[test]
    fn patterns_parse_and_compile() {
        let rules = RulesFile::from_toml(SAMPLE).unwrap();
        assert_eq!(rules.patterns.len(), 5);
        let groups = compile_groups(&rules).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].rules.len(), 6);
    }

    #[test]
    fn decide_routes_every_shape() {
        let g = sample_group();
        assert_eq!(
            decide(&g, "myriad/api_server", "1.2.3-dev.7"),
            Decision::Move("docker-prerelease")
        );
        assert_eq!(
            decide(&g, "myriad/api_server", "1.5.0-rc.2"),
            Decision::Move("docker-prerelease")
        );
        assert_eq!(
            decide(&g, "qkp/leaf", "develop"),
            Decision::Move("docker-prerelease")
        );
        assert_eq!(
            decide(&g, "qkp/leaf", "ci-build-12"),
            Decision::Move("docker-development-local")
        );
        assert_eq!(
            decide(&g, "qkp/leaf", "slord-79"),
            Decision::Move("docker-development-local")
        );
        // any released x.y.z → reconcile against insight (image-independent;
        // the supplementary/primary split happens at digest-check time).
        assert_eq!(
            decide(&g, "myriad/test_exec_web", "1.2.3"),
            Decision::Reconcile {
                check: "docker-insight",
                dest: "docker-release-aux"
            }
        );
        assert_eq!(
            decide(&g, "myriad/api_server", "1.2.3"),
            Decision::Reconcile {
                check: "docker-insight",
                dest: "docker-release-aux"
            }
        );
        // unmatched (e.g. the real 1.4.2-canal13 tag, or latest) → catch-all
        assert_eq!(
            decide(&g, "myriad/api_server", "1.4.2-canal13"),
            Decision::Move("docker-development")
        );
        assert_eq!(
            decide(&g, "myriad/api_server", "latest"),
            Decision::Move("docker-development")
        );
    }

    #[test]
    fn catch_all_routes_unmatched_to_development() {
        let g = sample_group();
        // A specific rule still wins over the catch-all (ordering).
        assert_eq!(
            decide(&g, "myriad/x", "1.2.3-dev.1"),
            Decision::Move("docker-prerelease")
        );
        // Anything no specific rule matched falls to the catch-all.
        assert_eq!(
            decide(&g, "myriad/x", "some-branch-tag"),
            Decision::Move("docker-development")
        );
        assert_eq!(
            decide(&g, "myriad/x", "0-9-0"),
            Decision::Move("docker-development")
        );
    }

    #[test]
    fn build_group_plan_buckets_and_skips_third_party() {
        let g = sample_group();
        let inv = vec![
            ("myriad/api_server".to_string(), "1.2.3".to_string()), // reconcile
            ("myriad/api_server".to_string(), "1.2.3-dev.4".to_string()), // move → prerelease
            ("myriad/api_server".to_string(), "1.4.2-canal13".to_string()), // catch-all → development
            ("library/postgres".to_string(), "16.2".to_string()),           // third-party: skipped
        ];
        let plan = build_group_plan(&g, &inv, "docker-internal");
        // 1.2.3-dev.4 → prerelease, 1.4.2-canal13 → development
        assert_eq!(plan.moves.len(), 2);
        assert_eq!(plan.reconciles.len(), 1);
        assert_eq!(plan.reconciles[0].check_repo, "docker-insight");
        assert_eq!(plan.reconciles[0].dest, "docker-release-aux");
        // Catch-all means nothing is left unclassified.
        assert!(plan.leaves.is_empty());
        // Third-party image contributed nothing.
        assert!(!plan.moves.iter().any(|m| m.image == "library/postgres"));
        assert!(!plan
            .reconciles
            .iter()
            .any(|r| r.tag.image == "library/postgres"));
    }

    #[test]
    fn inline_regex_in_match_works() {
        let toml = r#"
            [[group]]
            format = "docker"
            source_repos = ["r"]
            first_party_prefixes = ["app/"]
              [[group.rule]]
              match = '^v\d+$'
              action = "move"
              dest = "tagged"
        "#;
        let rules = RulesFile::from_toml(toml).unwrap();
        let g = compile_groups(&rules).unwrap().pop().unwrap();
        assert_eq!(decide(&g, "app/x", "v3"), Decision::Move("tagged"));
        assert_eq!(decide(&g, "app/x", "v3.1"), Decision::Leave);
    }

    #[test]
    fn move_without_dest_is_rejected() {
        let toml = r#"
            [[group]]
            format = "docker"
              [[group.rule]]
              match = 'x'
              action = "move"
        "#;
        let rules = RulesFile::from_toml(toml).unwrap();
        assert!(compile_groups(&rules).is_err());
    }
}
