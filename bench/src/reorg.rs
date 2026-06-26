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
    /// `reconcile` only: what to do when the tag is present in the check repo
    /// with a *different* digest. The script never guesses which copy is correct.
    /// `leave` (default) parks it for review (report-only); `ask` shows the
    /// build-time/digest evidence and prompts the operator per tag during `--apply`.
    #[serde(default)]
    pub on_mismatch: Option<MismatchPolicy>,
}

/// Policy for a reconcile rule when the source and check-repo digests differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MismatchPolicy {
    /// Leave the source copy in place and flag it for review (safe default).
    #[default]
    Leave,
    /// Show the evidence and prompt the operator to delete the source or keep
    /// it. Interactive — only acts during `--apply`.
    Ask,
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
    /// Image-name prefixes (namespaces) to delete wholesale from the source
    /// repos — retired/renamed projects. Every tag under these is a delete.
    #[serde(default)]
    pub delete_prefixes: Vec<String>,
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
    /// Optional cache-integrity check: validate a local cache repo against the
    /// authoritative upstream registry it mirrors, flagging tags present locally
    /// but absent upstream (manually-pushed pollution). Report-only.
    #[serde(default)]
    pub check_authority: Option<CheckAuthority>,
}

/// Declares that `cache_repo` (on the depot being reorganized) should mirror the
/// `upstream_repos` on an authoritative upstream registry. The dry-run flags any
/// tag in the cache that the upstream doesn't have. Upstream credentials come
/// from `UPSTREAM_USERNAME` / `UPSTREAM_PASSWORD` env vars.
#[derive(Debug, Clone, Deserialize)]
pub struct CheckAuthority {
    /// Cache repo on the local depot to validate (e.g. `docker-insight`).
    pub cache_repo: String,
    /// Base URL of the authoritative upstream registry.
    pub upstream_url: String,
    /// Repos on the upstream that together form the source of truth
    /// (e.g. `docker-external` + `docker-release`).
    #[serde(default)]
    pub upstream_repos: Vec<String>,
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
    on_mismatch: MismatchPolicy,
}

struct CompiledGroup {
    format: String,
    source_repos: Vec<String>,
    first_party_prefixes: Vec<String>,
    purge_repos: Vec<String>,
    delete_prefixes: Vec<String>,
    rules: Vec<CompiledRule>,
}

impl CompiledGroup {
    fn is_first_party(&self, image: &str) -> bool {
        self.first_party_prefixes
            .iter()
            .any(|p| image.starts_with(p))
    }

    /// True if the image is under a retired namespace slated for wholesale delete.
    fn is_retired(&self, image: &str) -> bool {
        self.delete_prefixes.iter().any(|p| image.starts_with(p))
    }
}

/// What to do with one `image:tag`, resolved from the first matching rule.
#[derive(Debug, PartialEq, Eq)]
enum Decision<'a> {
    Move(&'a str),
    DeleteIfAbsent(&'a str),
    Reconcile {
        check: &'a str,
        dest: &'a str,
        on_mismatch: MismatchPolicy,
    },
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
                on_mismatch: r.on_mismatch.unwrap_or_default(),
            });
        }
        out.push(CompiledGroup {
            format: g.format.clone(),
            source_repos: g.source_repos.clone(),
            first_party_prefixes: g.first_party_prefixes.clone(),
            purge_repos: g.purge_repos.clone(),
            delete_prefixes: g.delete_prefixes.clone(),
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
                    on_mismatch: r.on_mismatch,
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
    pub on_mismatch: MismatchPolicy,
}

/// A confirmed reconcile mismatch: the tag exists in `check_repo` with a
/// *different* digest. Carries the evidence used to explain it (and, for the
/// `ask` policy, to prompt the operator) — never an automatic verdict.
#[derive(Debug, Clone)]
struct MismatchInfo {
    tag: TagRef,
    check_repo: String,
    source_digest: String,
    check_digest: String,
    /// Image build time (config `created`, date only); `None` if unresolved.
    source_built: Option<String>,
    check_built: Option<String>,
    on_mismatch: MismatchPolicy,
}

impl MismatchInfo {
    /// A one-line, non-authoritative read of the build-time evidence.
    fn assessment(&self) -> &'static str {
        match (self.source_built.as_deref(), self.check_built.as_deref()) {
            (Some(s), Some(c)) if s < c => {
                "source build is OLDER than the check copy — likely stale"
            }
            (Some(s), Some(c)) if s > c => {
                "source build is NEWER than the check copy — may be un-propagated; review carefully"
            }
            (Some(_), Some(_)) => {
                "same build date — likely the same image re-serialized (different manifest format)"
            }
            _ => "build time unavailable for one or both — review manually",
        }
    }
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
            Decision::Reconcile {
                check,
                dest,
                on_mismatch,
            } => plan.reconciles.push(ReconcileItem {
                tag: tagref(),
                check_repo: check.to_string(),
                dest: dest.to_string(),
                on_mismatch,
            }),
            Decision::Delete => plan.deletes.push(tagref()),
            Decision::Leave => plan.leaves.push(tagref()),
        }
    }
    plan
}

// ---------------------------------------------------------------------------
// Preflight repo validation
// ---------------------------------------------------------------------------

/// Validate every repo a rules file references against the live repo inventory
/// (`(name, repo_type, format)` triples). Pure — no I/O — so it is unit-tested.
///
/// Returns a list of human-readable problems (empty = OK):
/// - source / purge / check / dest repos must exist with the group's `format`;
/// - a move/copy **destination must be `hosted`** — staging/move re-homes records
///   into a concrete repo, so a `group`/`proxy` or `cache` dest is invalid (this
///   is what catches routing a catch-all at a `*-development` proxy);
/// - the usage repo, if set, must exist (its type is unconstrained — read-only).
fn validate_repo_refs(
    inventory: &[(String, String, String)],
    groups: &[CompiledGroup],
    usage_repo: Option<&str>,
) -> Vec<String> {
    let by_name: HashMap<&str, (&str, &str)> = inventory
        .iter()
        .map(|(n, t, f)| (n.as_str(), (t.as_str(), f.as_str())))
        .collect();
    let mut errs = Vec::new();

    let mut check = |name: &str, fmt: &str, must_be_hosted: bool, role: &str| match by_name
        .get(name)
    {
        None => errs.push(format!("{role} repo '{name}' does not exist")),
        Some((repo_type, repo_fmt)) => {
            if *repo_fmt != fmt {
                errs.push(format!(
                    "{role} repo '{name}' is format '{repo_fmt}', expected '{fmt}'"
                ));
            }
            if must_be_hosted && *repo_type != "hosted" {
                errs.push(format!(
                    "{role} repo '{name}' is type '{repo_type}', but a move/copy \
                     destination must be 'hosted'"
                ));
            }
        }
    };

    for g in groups {
        for s in &g.source_repos {
            check(s, &g.format, false, "source");
        }
        for p in &g.purge_repos {
            check(p, &g.format, false, "purge");
        }
        for r in &g.rules {
            if let Some(d) = &r.dest {
                check(d, &g.format, true, "destination");
            }
            if let Some(c) = &r.check {
                check(c, &g.format, false, "check");
            }
        }
    }
    if let Some(u) = usage_repo {
        if !by_name.contains_key(u) {
            errs.push(format!("usage_repo '{u}' does not exist"));
        }
    }
    // A repo referenced by several rules yields the same problem repeatedly;
    // report each distinct problem once, preserving first-seen order.
    let mut seen = std::collections::HashSet::new();
    errs.retain(|e| seen.insert(e.clone()));
    errs
}

/// Fetch the live repo inventory and validate every referenced repo, bailing
/// with all problems at once before any move/delete is planned or executed.
async fn preflight_repos(
    client: &DepotClient,
    groups: &[CompiledGroup],
    usage_repo: Option<&str>,
) -> Result<()> {
    let repos = client
        .list_repos()
        .await
        .context("list repositories for preflight")?;
    let inventory: Vec<(String, String, String)> = repos
        .into_iter()
        .map(|r| (r.name, r.repo_type, r.format))
        .collect();
    let errs = validate_repo_refs(&inventory, groups, usage_repo);
    if !errs.is_empty() {
        bail!(
            "reorg rules reference invalid repositories:\n  - {}",
            errs.join("\n  - ")
        );
    }
    Ok(())
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
    /// Skip TLS verification (also used for the upstream authority client).
    pub insecure: bool,
}

/// Resolved, ready-to-execute plan (after presence checks + purge enumeration).
#[derive(Default)]
struct ResolvedPlan {
    moves: Vec<PlannedMove>,
    deletes: Vec<TagRef>,
    kept: Vec<TagRef>,
    leaves: Vec<TagRef>,
    purges: Vec<TagRef>,
    /// Tags under a retired `delete_prefixes` namespace — deleted from source.
    prefix_deletes: Vec<TagRef>,
    /// Reconcile tags present in the check repo but with a *different* digest,
    /// carrying the evidence (digests + build times) needed to explain them.
    mismatched: Vec<MismatchInfo>,
    /// Tags in source repos the reorg never touches because their image is not
    /// first-party — they stay put. Tracked only to report the post-reorg residual.
    remaining_third_party: Vec<TagRef>,
}

pub async fn run(client: &DepotClient, cfg: ReorgConfig) -> Result<()> {
    let text = std::fs::read_to_string(&cfg.rules_path)
        .with_context(|| format!("read rules file '{}'", cfg.rules_path))?;
    let rules = RulesFile::from_toml(&text)?;
    let groups = compile_groups(&rules)?;
    client.login().await?;
    preflight_repos(client, &groups, rules.usage_repo.as_deref()).await?;

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
            // Classify the non-first-party tags: retired namespaces are deleted
            // wholesale; everything else is untouched third-party (reported as
            // residual). First-party tags are handled by build_group_plan below.
            for (image, tag) in &inv {
                if group.is_first_party(image) {
                    continue;
                }
                let tagref = TagRef {
                    source_repo: repo.clone(),
                    image: image.clone(),
                    tag: tag.clone(),
                };
                if group.is_retired(image) {
                    resolved.prefix_deletes.push(tagref);
                } else {
                    resolved.remaining_third_party.push(tagref);
                }
            }
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
                    // A real mismatch: gather the build-time evidence so the plan
                    // can explain it (and prompt, under the `ask` policy). The
                    // script never decides which copy is correct.
                    let source_built =
                        resolve_built(client, &rec.tag.source_repo, &rec.tag.image, &rec.tag.tag)
                            .await;
                    let check_built =
                        resolve_built(client, &rec.check_repo, &rec.tag.image, &rec.tag.tag).await;
                    resolved.mismatched.push(MismatchInfo {
                        tag: rec.tag,
                        check_repo: rec.check_repo,
                        source_digest: src_digest,
                        check_digest,
                        source_built,
                        check_built,
                        on_mismatch: rec.on_mismatch,
                    });
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
        {
            images.insert(t.image.clone());
        }
        for m in &resolved.mismatched {
            images.insert(m.tag.image.clone());
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

    // Snapshot of the repositories this run involves (sources, move dests, purge
    // repos, and check/cache repos), with current artifact counts + sizes.
    let mut scope: Vec<String> = Vec::new();
    {
        let mut add = |r: &str| {
            if !scope.iter().any(|x| x == r) {
                scope.push(r.to_string());
            }
        };
        for g in &groups {
            for r in &g.source_repos {
                add(r);
            }
        }
        for m in &resolved.moves {
            add(&m.dest);
        }
        for g in &groups {
            for r in &g.purge_repos {
                add(r);
            }
            for r in &g.rules {
                if let Some(c) = &r.check {
                    add(c);
                }
            }
        }
        if let Some(a) = &rules.check_authority {
            add(&a.cache_repo);
        }
    }
    let before_summary = match fetch_repo_summary(client, &scope).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("  (repository summary unavailable: {e})");
            None
        }
    };

    if !cfg.apply {
        if let Some(b) = &before_summary {
            print_repo_summary(b, None);
        }
        // Report the post-reorg residual: what stays in each source repo, plus
        // each check repo (a cache the reorg never writes to). Enumerate the
        // distinct check repos for their current contents.
        let mut check_repos: std::collections::BTreeSet<String> = Default::default();
        for group in &groups {
            for r in &group.rules {
                if let Some(c) = &r.check {
                    check_repos.insert(c.clone());
                }
            }
        }
        let mut cache_contents: BTreeMap<String, Vec<TagRef>> = BTreeMap::new();
        for repo in &check_repos {
            let tags = list_repo_tags(client, repo).await?;
            cache_contents.insert(
                repo.clone(),
                tags.into_iter()
                    .map(|(image, tag)| TagRef {
                        source_repo: repo.clone(),
                        image,
                        tag,
                    })
                    .collect(),
            );
        }
        let mut prefixes: std::collections::BTreeSet<String> = Default::default();
        for group in &groups {
            for p in &group.first_party_prefixes {
                prefixes.insert(p.clone());
            }
        }
        let prefixes: Vec<String> = prefixes.into_iter().collect();
        print_remaining(&resolved, &cache_contents, &prefixes);

        // Cache-integrity check: diff a local cache repo against the
        // authoritative upstream it mirrors, flagging tags present locally but
        // absent upstream (manually-pushed pollution that the cache would never
        // legitimately hold).
        if let Some(authority) = &rules.check_authority {
            check_cache_authority(client, authority, cfg.insecure).await?;
        }

        println!("\nDry run — no changes made. Re-run with --apply to execute.");
        return Ok(());
    }

    // ----------------------------------------------------------------------
    // Phase 1 — gather every operator decision UP FRONT, before any change, so
    // the long unattended modification phase never stops to ask a question.
    // ----------------------------------------------------------------------
    let mut approved_mismatch_deletes: Vec<&MismatchInfo> = Vec::new();
    if !cfg.copy {
        let asks: Vec<&MismatchInfo> = resolved
            .mismatched
            .iter()
            .filter(|m| m.on_mismatch == MismatchPolicy::Ask)
            .collect();
        if !asks.is_empty() {
            println!(
                "\n{} reconcile mismatch(es) need a decision before modifications begin:",
                asks.len()
            );
            for m in asks {
                print_mismatch_evidence(m);
                let q = format!(
                    "  Delete the source copy {}/{}:{}? (it stays if you decline)",
                    m.tag.source_repo, m.tag.image, m.tag.tag
                );
                if prompt_yes_no(&q)? {
                    approved_mismatch_deletes.push(m);
                } else {
                    println!(
                        "  will keep: {}/{}:{}",
                        m.tag.source_repo, m.tag.image, m.tag.tag
                    );
                }
            }
        }
    }

    // ----------------------------------------------------------------------
    // Phase 2 — execute. No further prompts from here on.
    // ----------------------------------------------------------------------
    let mut ok = 0usize;
    let mut failed = 0usize;
    let verb = if cfg.copy { "copy" } else { "move" };
    let total_deletes = if cfg.copy {
        0
    } else {
        resolved.deletes.len()
            + resolved.purges.len()
            + resolved.prefix_deletes.len()
            + approved_mismatch_deletes.len()
    };
    println!(
        "\nApplying {} {verb}(s) and {} delete(s) — no further prompts...",
        resolved.moves.len(),
        total_deletes
    );

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
        let skipped =
            resolved.deletes.len() + resolved.purges.len() + resolved.prefix_deletes.len();
        if skipped > 0 {
            println!("\n--copy is non-destructive: skipped {skipped} delete/purge action(s).");
        }
    } else {
        let mismatch_tags = approved_mismatch_deletes.iter().map(|m| &m.tag);
        for d in resolved
            .deletes
            .iter()
            .chain(resolved.purges.iter())
            .chain(resolved.prefix_deletes.iter())
            .chain(mismatch_tags)
        {
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

    // Re-measure (letting the counters settle) and show the before → after table.
    if let Some(before) = &before_summary {
        match fetch_settled_summary(client, &scope).await {
            Ok(after) => print_repo_summary(before, Some(&after)),
            Err(e) => eprintln!("  (after-summary unavailable: {e})"),
        }
    }

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

/// Report what remains after the reorg, restricted to **first-party** images
/// (those under a configured `first_party_prefixes` namespace) and listing each
/// artifact with its actual tags — so anything under your own projects that a
/// rule didn't route stands out (and you can tell whether more rules are needed).
/// Third-party images that stay are summarized as a single count, not listed.
/// Source repos show the tags left in place (reconcile-mismatch / conditional-
/// keep / unmatched); check repos (caches the reorg never writes to) show the
/// first-party artifacts they currently serve, for reference.
fn print_remaining(
    p: &ResolvedPlan,
    cache_contents: &BTreeMap<String, Vec<TagRef>>,
    first_party_prefixes: &[String],
) {
    let is_fp =
        |image: &str| first_party_prefixes.iter().any(|pre| image.starts_with(pre.as_str()));

    // Group a set of tag refs by image, with sorted tags per image.
    let by_image = |tags: &[&TagRef]| -> BTreeMap<String, Vec<String>> {
        let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for t in tags {
            m.entry(t.image.clone()).or_default().push(t.tag.clone());
        }
        for v in m.values_mut() {
            v.sort();
        }
        m
    };

    println!(
        "\n================ Remaining after reorg — first-party only ({}) ================",
        first_party_prefixes.join(", ")
    );

    let fp_left: Vec<&TagRef> = p
        .mismatched
        .iter()
        .map(|m| &m.tag)
        .chain(&p.kept)
        .chain(&p.leaves)
        .collect();

    let mut source_repos: std::collections::BTreeSet<&str> = Default::default();
    for t in p.remaining_third_party.iter().chain(fp_left.iter().copied()) {
        source_repos.insert(t.source_repo.as_str());
    }

    for repo in &source_repos {
        let fp: Vec<&TagRef> = fp_left
            .iter()
            .copied()
            .filter(|t| t.source_repo == *repo)
            .collect();
        let tp_count = p
            .remaining_third_party
            .iter()
            .filter(|t| t.source_repo == *repo)
            .count();
        if fp.is_empty() {
            println!(
                "\n{repo}: no first-party tags left ✓  ({tp_count} third-party tag(s) also stay, not shown)"
            );
            continue;
        }
        println!(
            "\n{repo}: {} first-party tag(s) left  ({tp_count} third-party tag(s) also stay, not shown):",
            fp.len()
        );
        for (img, tags) in by_image(&fp) {
            println!("       {img}: {}", tags.join(", "));
        }
    }

    for (repo, tags) in cache_contents {
        let fp: Vec<&TagRef> = tags.iter().filter(|t| is_fp(&t.image)).collect();
        println!(
            "\n{repo} (cache — untouched) — {} first-party tag(s):",
            fp.len()
        );
        for (img, tags) in by_image(&fp) {
            println!("       {img}: {}", tags.join(", "));
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

    if !p.prefix_deletes.is_empty() {
        // Retired namespaces deleted wholesale — list every tag, grouped by
        // source repo then image, since these are the ones to eyeball.
        println!(
            "\nDelete — retired namespaces{del_note} ({} tag(s)):",
            p.prefix_deletes.len()
        );
        let mut by_repo: BTreeMap<&str, BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
        for t in &p.prefix_deletes {
            by_repo
                .entry(t.source_repo.as_str())
                .or_default()
                .entry(t.image.as_str())
                .or_default()
                .push(t.tag.as_str());
        }
        for (repo, by_img) in &by_repo {
            let n: usize = by_img.values().map(|v| v.len()).sum();
            println!("  {repo} ({n} tag(s)):");
            for (img, tags) in by_img {
                let mut ts = tags.clone();
                ts.sort();
                println!("       {img}: {}", ts.join(", "));
            }
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
        let asks = p
            .mismatched
            .iter()
            .filter(|m| m.on_mismatch == MismatchPolicy::Ask)
            .count();
        println!(
            "\nReconcile mismatch ({} tag(s)) — in the check repo but a DIFFERENT digest. \
             {} will prompt for a decision on --apply; the rest are left for review:",
            p.mismatched.len(),
            asks
        );
        for m in &p.mismatched {
            let note = atime_note(usage_repo, atimes, &m.tag.image, &m.tag.tag);
            println!(
                "       {}/{}:{}{note}",
                m.tag.source_repo, m.tag.image, m.tag.tag
            );
            print_mismatch_evidence(m);
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

/// Format a byte count as gigabytes (decimal, 1 GB = 1e9 bytes).
fn fmt_gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// One repo's measured stats.
struct RepoStat {
    name: String,
    repo_type: String,
    artifacts: u64,
    bytes: u64,
}

/// A measured snapshot of the repos a run involves, plus store-wide totals.
struct RepoSummary {
    repos: Vec<RepoStat>,
    logical_sum: u64,
    docker_logical: u64,
    /// (store name, physical bytes, blob count) — deduplicated.
    stores: Vec<(String, u64, u64)>,
}

/// Measure the in-scope repos and store totals (no printing) so the same shape
/// can be snapshotted before and after a run.
async fn fetch_repo_summary(client: &DepotClient, scope: &[String]) -> Result<RepoSummary> {
    let all = client.list_repos().await.context("list repositories")?;
    let by_name: HashMap<&str, &crate::client::RepoResponse> =
        all.iter().map(|r| (r.name.as_str(), r)).collect();
    let repos = scope
        .iter()
        .filter_map(|n| by_name.get(n.as_str()))
        .map(|r| RepoStat {
            name: r.name.clone(),
            repo_type: r.repo_type.clone(),
            artifacts: r.artifact_count,
            bytes: r.total_bytes,
        })
        .collect();
    let logical_sum = all
        .iter()
        .filter(|r| r.repo_type != "proxy")
        .map(|r| r.total_bytes)
        .sum();
    let docker_logical = all
        .iter()
        .filter(|r| r.format == "docker" && r.repo_type != "proxy")
        .map(|r| r.total_bytes)
        .sum();
    let stores = client
        .list_stores()
        .await
        .map(|s| {
            s.into_iter()
                .map(|s| (s.name, s.total_bytes, s.blob_count))
                .collect()
        })
        .unwrap_or_default();
    Ok(RepoSummary {
        repos,
        logical_sum,
        docker_logical,
        stores,
    })
}

/// Re-read the in-scope summary until artifact counts stop changing between two
/// reads (the counter system can lag a beat after a large batch), capped so a
/// continuously-busy instance can't stall the report.
async fn fetch_settled_summary(client: &DepotClient, scope: &[String]) -> Result<RepoSummary> {
    let mut prev = fetch_repo_summary(client, scope).await?;
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let cur = fetch_repo_summary(client, scope).await?;
        let stable = cur.repos.len() == prev.repos.len()
            && cur
                .repos
                .iter()
                .zip(&prev.repos)
                .all(|(a, b)| a.artifacts == b.artifacts);
        if stable {
            return Ok(cur);
        }
        prev = cur;
    }
    Ok(prev)
}

/// Render a repo summary. With `after`, shows a before→after comparison
/// (per-repo and store-wide); without, a single current snapshot. Note: a blob
/// shared by N repos is counted in each repo's size, so the per-repo sizes sum
/// to more than the store's deduplicated physical total.
fn print_repo_summary(before: &RepoSummary, after: Option<&RepoSummary>) {
    let after_of = |name: &str| {
        after.and_then(|a| a.repos.iter().find(|r| r.name == name))
    };
    match after {
        None => {
            println!("\n================ Repository summary (current) ================");
            println!("  {:28} {:8} {:>12} {:>12}", "repo", "type", "artifacts", "size");
            for r in &before.repos {
                println!(
                    "  {:28} {:8} {:>12} {:>12}",
                    r.name,
                    r.repo_type,
                    r.artifacts,
                    fmt_gb(r.bytes)
                );
            }
            println!("\n  sum(all hosted/cache repos)    = {}", fmt_gb(before.logical_sum));
            println!("  sum(docker hosted/cache repos) = {}", fmt_gb(before.docker_logical));
            for (name, bytes, blobs) in &before.stores {
                println!("  store '{name}' physical total      = {} ({blobs} blobs, dedup)", fmt_gb(*bytes));
            }
        }
        Some(a) => {
            println!("\n================ Repository summary — before → after ================");
            println!(
                "  {:28} {:8} {:>21} {:>23}",
                "repo", "type", "artifacts", "size"
            );
            for b in &before.repos {
                let aft = after_of(&b.name);
                let (a_art, a_bytes) = aft.map(|r| (r.artifacts, r.bytes)).unwrap_or((b.artifacts, b.bytes));
                println!(
                    "  {:28} {:8} {:>9} → {:>9} {:>10} → {:>10}",
                    b.name,
                    b.repo_type,
                    b.artifacts,
                    a_art,
                    fmt_gb(b.bytes),
                    fmt_gb(a_bytes)
                );
            }
            println!(
                "\n  sum(all hosted/cache repos)    = {} → {}",
                fmt_gb(before.logical_sum),
                fmt_gb(a.logical_sum)
            );
            println!(
                "  sum(docker hosted/cache repos) = {} → {}",
                fmt_gb(before.docker_logical),
                fmt_gb(a.docker_logical)
            );
            for (name, b_bytes, b_blobs) in &before.stores {
                let (a_bytes, a_blobs) = a
                    .stores
                    .iter()
                    .find(|(n, _, _)| n == name)
                    .map(|(_, by, bl)| (*by, *bl))
                    .unwrap_or((*b_bytes, *b_blobs));
                println!(
                    "  store '{name}' physical total      = {} ({b_blobs} blobs) → {} ({a_blobs} blobs)",
                    fmt_gb(*b_bytes),
                    fmt_gb(a_bytes)
                );
            }
        }
    }
}

/// Validate a local cache repo against the authoritative upstream registry it
/// mirrors, flagging tags present locally but absent upstream — i.e. content
/// that was manually pushed into the cache and would never be served by the
/// upstream. Report-only, and non-fatal: a missing credential or unreachable
/// upstream prints a note and skips rather than failing the run.
async fn check_cache_authority(
    client: &DepotClient,
    authority: &CheckAuthority,
    insecure: bool,
) -> Result<()> {
    println!(
        "\n================ Cache integrity — '{}' vs upstream {} ================",
        authority.cache_repo, authority.upstream_url
    );
    let (Some(user), Some(pass)) = (
        std::env::var("UPSTREAM_USERNAME").ok(),
        std::env::var("UPSTREAM_PASSWORD").ok(),
    ) else {
        println!("  skipped: set UPSTREAM_USERNAME / UPSTREAM_PASSWORD to enable this check.");
        return Ok(());
    };
    let upstream = match DepotClient::new(&authority.upstream_url, &user, &pass, insecure) {
        Ok(c) => c,
        Err(e) => {
            println!("  skipped: cannot build upstream client: {e}");
            return Ok(());
        }
    };

    // Upstream source of truth (union of upstream_repos). Unreachable → skip.
    let mut upstream_set: std::collections::HashSet<String> = Default::default();
    for repo in &authority.upstream_repos {
        match list_repo_tags(&upstream, repo).await {
            Ok(tags) => upstream_set.extend(tags.into_iter().map(|(i, t)| format!("{i}:{t}"))),
            Err(e) => {
                println!("  skipped: upstream repo '{repo}' unreachable ({e}).");
                return Ok(());
            }
        }
    }

    let local = list_repo_tags(client, &authority.cache_repo)
        .await
        .with_context(|| format!("enumerate cache repo '{}'", authority.cache_repo))?;
    println!(
        "  cache: {} tag(s); upstream: {} tag(s)",
        local.len(),
        upstream_set.len()
    );

    let by_img = cache_pollution(&local, &upstream_set);
    if by_img.is_empty() {
        println!(
            "  clean ✓ — every tag in '{}' exists upstream.",
            authority.cache_repo
        );
        return Ok(());
    }
    let n: usize = by_img.values().map(|v| v.len()).sum();
    println!(
        "  {n} tag(s) in '{}' NOT upstream (manually injected — safe to delete from the cache):",
        authority.cache_repo
    );
    for (img, ts) in by_img {
        println!("       {img}: {}", ts.join(", "));
    }
    Ok(())
}

/// Local `(image, tag)` entries whose `image:tag` key is absent from the
/// `upstream` set, grouped by image with sorted tags. Pure — unit-tested.
fn cache_pollution(
    local: &[(String, String)],
    upstream: &std::collections::HashSet<String>,
) -> BTreeMap<String, Vec<String>> {
    let mut by_img: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (img, tag) in local {
        if !upstream.contains(&format!("{img}:{tag}")) {
            by_img.entry(img.clone()).or_default().push(tag.clone());
        }
    }
    for v in by_img.values_mut() {
        v.sort();
    }
    by_img
}

/// Print the two-sided evidence for a reconcile mismatch (build time + digest)
/// plus a non-authoritative assessment. Used by the dry-run plan and the
/// interactive `ask` prompt.
fn print_mismatch_evidence(m: &MismatchInfo) {
    println!(
        "            source ({}): built {}  [{}]",
        m.tag.source_repo,
        m.source_built.as_deref().unwrap_or("?"),
        short(&m.source_digest)
    );
    println!(
        "            check  ({}): built {}  [{}]",
        m.check_repo,
        m.check_built.as_deref().unwrap_or("?"),
        short(&m.check_digest)
    );
    println!("            -> {}", m.assessment());
}

/// Resolve an image's build time = its config blob's `created` field (date only).
/// Descends one level into a manifest list / OCI index. Best-effort: `None` on
/// any miss, so a mismatch is still reported (just without the date).
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
    let manifest = if let Some(child) = m
        .get("manifests")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("digest"))
        .and_then(|d| d.as_str())
    {
        let (cbody, _) = client
            .docker_get_manifest_path(repo, image, child)
            .await
            .ok()?;
        serde_json::from_slice::<serde_json::Value>(&cbody).ok()?
    } else {
        m
    };
    let cfg = manifest.get("config")?.get("digest")?.as_str()?;
    let blob = client.docker_get_blob_path(repo, image, cfg).await.ok()?;
    let c: serde_json::Value = serde_json::from_slice(&blob).ok()?;
    c.get("created")
        .and_then(|v| v.as_str())
        .map(|s| s.get(0..10).unwrap_or(s).to_string())
}

/// Shorten a `sha256:…` digest for display.
fn short(digest: &str) -> String {
    match digest.split_once(':') {
        Some((algo, hex)) => format!("{algo}:{}", &hex[..hex.len().min(12)]),
        None => digest.chars().take(12).collect(),
    }
}

/// Prompt the operator for a yes/no decision (defaults to no on empty/EOF).
fn prompt_yes_no(question: &str) -> Result<bool> {
    use std::io::Write;
    print!("{question} [y/N]: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
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
          dest = "docker-development-local"
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
                dest: "docker-release-aux",
                on_mismatch: MismatchPolicy::Leave,
            }
        );
        assert_eq!(
            decide(&g, "myriad/api_server", "1.2.3"),
            Decision::Reconcile {
                check: "docker-insight",
                dest: "docker-release-aux",
                on_mismatch: MismatchPolicy::Leave,
            }
        );
        // unmatched (e.g. the real 1.4.2-canal13 tag, or latest) → catch-all
        assert_eq!(
            decide(&g, "myriad/api_server", "1.4.2-canal13"),
            Decision::Move("docker-development-local")
        );
        assert_eq!(
            decide(&g, "myriad/api_server", "latest"),
            Decision::Move("docker-development-local")
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
            Decision::Move("docker-development-local")
        );
        assert_eq!(
            decide(&g, "myriad/x", "0-9-0"),
            Decision::Move("docker-development-local")
        );
    }

    #[test]
    fn build_group_plan_buckets_and_skips_third_party() {
        let g = sample_group();
        let inv = vec![
            ("myriad/api_server".to_string(), "1.2.3".to_string()), // reconcile
            ("myriad/api_server".to_string(), "1.2.3-dev.4".to_string()), // move → prerelease
            ("myriad/api_server".to_string(), "1.4.2-canal13".to_string()), // catch-all → development-local
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
    fn preflight_flags_nonhosted_dest_and_missing_repos() {
        let g = sample_group();
        let groups = std::slice::from_ref(&g);
        // docker-development-local is the catch-all dest here; model it as a
        // *proxy* in the inventory to prove the hosted-dest check fires.
        let inv = |dev_local_type: &str| {
            vec![
                ("docker-internal", "hosted", "docker"),
                ("docker-external", "hosted", "docker"),
                ("docker-prerelease", "hosted", "docker"),
                ("docker-release-aux", "hosted", "docker"),
                ("docker-insight", "cache", "docker"),
                ("docker-development-local", dev_local_type, "docker"),
                ("docker-upstream", "cache", "docker"),
                ("docker", "proxy", "docker"),
            ]
            .into_iter()
            .map(|(n, t, f)| (n.to_string(), t.to_string(), f.to_string()))
            .collect::<Vec<_>>()
        };

        // All hosted dests present → clean.
        assert!(validate_repo_refs(&inv("hosted"), groups, Some("docker")).is_empty());

        // Catch-all dest is a proxy → flagged as a non-hosted destination.
        let errs = validate_repo_refs(&inv("proxy"), groups, Some("docker"));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("docker-development-local"));
        assert!(errs[0].contains("must be 'hosted'"));

        // A missing repo (drop docker-prerelease) is reported too.
        let mut missing = inv("hosted");
        missing.retain(|(n, _, _)| n != "docker-prerelease");
        let errs = validate_repo_refs(&missing, groups, Some("docker"));
        assert!(errs.iter().any(|e| e.contains("docker-prerelease") && e.contains("does not exist")));

        // Missing usage repo is reported.
        let errs = validate_repo_refs(&inv("hosted"), groups, Some("nope"));
        assert!(errs.iter().any(|e| e.contains("usage_repo 'nope'")));
    }

    #[test]
    fn delete_prefixes_parse_and_classify() {
        let toml = r#"
            [[group]]
            format = "docker"
            first_party_prefixes = ["quantum-orchestrator/"]
            delete_prefixes = ["orchestrator/", "quantum_orchestrator/"]
        "#;
        let g = compile_groups(&RulesFile::from_toml(toml).unwrap())
            .unwrap()
            .pop()
            .unwrap();
        // Retired namespaces match; the current (dash) one does not.
        assert!(g.is_retired("orchestrator/orchestrator"));
        assert!(g.is_retired("quantum_orchestrator/orchestrator"));
        assert!(!g.is_retired("quantum-orchestrator/orchestrator"));
        // The current name is first-party, not retired (disjoint).
        assert!(g.is_first_party("quantum-orchestrator/orchestrator"));
        assert!(!g.is_first_party("orchestrator/orchestrator"));
    }

    #[test]
    fn check_authority_parses() {
        let toml = r#"
            [[group]]
            format = "docker"
            [check_authority]
            cache_repo = "docker-insight"
            upstream_url = "https://insight.quantum.com:8081"
            upstream_repos = ["docker-external", "docker-release"]
        "#;
        let r = RulesFile::from_toml(toml).unwrap();
        let a = r.check_authority.expect("check_authority present");
        assert_eq!(a.cache_repo, "docker-insight");
        assert_eq!(a.upstream_url, "https://insight.quantum.com:8081");
        assert_eq!(a.upstream_repos, vec!["docker-external", "docker-release"]);
    }

    #[test]
    fn cache_pollution_flags_only_tags_absent_upstream() {
        let local = vec![
            ("myriad/master".to_string(), "1.5.2".to_string()),
            ("myriad/master".to_string(), "1.6.0-dev.168".to_string()),
            ("myriad/master".to_string(), "1.4.0-dev.117".to_string()),
            (
                "quantum-orchestrator/orchestrator".to_string(),
                "develop".to_string(),
            ),
        ];
        // Upstream has the real release plus a legit -rc; the dev/develop tags don't exist there.
        let upstream: std::collections::HashSet<String> = [
            "myriad/master:1.5.2".to_string(),
            "myriad/master:1.5.0-rc.1".to_string(),
        ]
        .into_iter()
        .collect();
        let p = cache_pollution(&local, &upstream);
        // 1.5.2 is upstream → not flagged; the two dev tags are (sorted).
        assert_eq!(
            p.get("myriad/master").unwrap(),
            &vec!["1.4.0-dev.117".to_string(), "1.6.0-dev.168".to_string()]
        );
        assert_eq!(
            p.get("quantum-orchestrator/orchestrator").unwrap(),
            &vec!["develop".to_string()]
        );
        // A clean cache (all tags upstream) yields no entries.
        let clean = vec![("myriad/master".to_string(), "1.5.2".to_string())];
        assert!(cache_pollution(&clean, &upstream).is_empty());
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

    #[test]
    fn on_mismatch_defaults_to_leave_and_parses_ask() {
        // SAMPLE's reconcile rule omits on_mismatch → Leave.
        let g = sample_group();
        match decide(&g, "myriad/api_server", "1.2.3") {
            Decision::Reconcile { on_mismatch, .. } => {
                assert_eq!(on_mismatch, MismatchPolicy::Leave)
            }
            d => panic!("expected reconcile, got {d:?}"),
        }
        // Explicit on_mismatch = "ask" parses and flows through to the decision.
        let toml = r#"
            [patterns]
            released = '\d+\.\d+\.\d+'
            [[group]]
            format = "docker"
            source_repos = ["r"]
            first_party_prefixes = ["app/"]
              [[group.rule]]
              match = "released"
              action = "reconcile"
              check = "c"
              dest = "d"
              on_mismatch = "ask"
        "#;
        let rules = RulesFile::from_toml(toml).unwrap();
        let g = compile_groups(&rules).unwrap().pop().unwrap();
        match decide(&g, "app/x", "1.2.3") {
            Decision::Reconcile { on_mismatch, .. } => assert_eq!(on_mismatch, MismatchPolicy::Ask),
            d => panic!("expected reconcile, got {d:?}"),
        }
    }

    #[test]
    fn mismatch_assessment_reads_build_times() {
        let mk = |s: Option<&str>, c: Option<&str>| MismatchInfo {
            tag: TagRef {
                source_repo: "docker-internal".into(),
                image: "app/x".into(),
                tag: "1.0.0".into(),
            },
            check_repo: "docker-insight".into(),
            source_digest: "sha256:aaaaaaaaaaaa".into(),
            check_digest: "sha256:bbbbbbbbbbbb".into(),
            source_built: s.map(String::from),
            check_built: c.map(String::from),
            on_mismatch: MismatchPolicy::Ask,
        };
        assert!(mk(Some("2024-06-26"), Some("2024-08-12"))
            .assessment()
            .contains("OLDER"));
        assert!(mk(Some("2024-09-01"), Some("2024-08-12"))
            .assessment()
            .contains("NEWER"));
        assert!(mk(Some("2024-08-12"), Some("2024-08-12"))
            .assessment()
            .contains("same build date"));
        assert!(mk(None, Some("2024-08-12"))
            .assessment()
            .contains("unavailable"));
    }
}
