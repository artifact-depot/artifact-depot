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
//! - `classify` — route a released (x.y.z) tag by an explicit per-image class
//!   list (from a `classes_file`): an `aux_images` image moves to `aux_dest`;
//!   an `insight_images` image is verified against `insight_repo` (same digest →
//!   drop the redundant source copy, never out of the cache itself; different →
//!   review; absent upstream → flag, never dropped); an image on neither list is
//!   reported as "needs classification" with evidence — never moved or deleted.
//! - `delete` — delete from source unconditionally
//! - `leave` — leave in place (reported)
//!
//! First matching rule wins; an unmatched tag is left in place (add a trailing
//! `match = '.*'` rule to route the remainder somewhere, e.g. development).
//!
//! Dry-run by default (prints the plan; changes nothing). `--apply` executes;
//! `--copy` uses staging/copy for moves and skips every destructive action.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

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
    /// Route a released (x.y.z) tag by an explicit per-image class list loaded
    /// from `classes_file` (and/or inline `insight_images`/`aux_images`). An aux
    /// image moves to `aux_dest`; an insight image is reconciled against
    /// `insight_repo` (same digest → drop, different → review, absent → flag,
    /// never dropped); an image on neither list → needs-classification triage.
    Classify,
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
    /// For `classify`, this governs the insight-image *different-digest* case.
    #[serde(default)]
    pub on_mismatch: Option<MismatchPolicy>,
    /// `classify` only: destination for an image on the `aux_images` list.
    #[serde(default)]
    pub aux_dest: Option<String>,
    /// `classify` only: the canonical insight repo (a cache) to verify an
    /// `insight_images` image against before dropping the redundant source copy.
    #[serde(default)]
    pub insight_repo: Option<String>,
    /// `classify` only: path (relative to the rules file) to a TOML file with
    /// `insight_images` / `aux_images` arrays. Merged with any inline lists below.
    #[serde(default)]
    pub classes_file: Option<String>,
    /// `classify` only: inline insight-image list (merged with `classes_file`).
    #[serde(default)]
    pub insight_images: Vec<String>,
    /// `classify` only: inline aux-image list (merged with `classes_file`).
    #[serde(default)]
    pub aux_images: Vec<String>,
}

/// The two image arrays a `classify` rule reads from its `classes_file`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageClassesFile {
    #[serde(default)]
    pub insight_images: Vec<String>,
    #[serde(default)]
    pub aux_images: Vec<String>,
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
    /// Image-name prefixes (namespaces) to retire — removed wholesale from the
    /// source repos. Every tag under these is deleted. (Why is the operator's
    /// call; the tool just removes them.) `delete_prefixes` accepted as an alias.
    #[serde(default, alias = "delete_prefixes")]
    pub retired_prefixes: Vec<String>,
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
    /// Friendly class label (the rule's `match` name; `.*` → "catch-all").
    class: String,
    re: Regex,
    images: Vec<String>,
    action: Action,
    dest: Option<String>,
    check: Option<String>,
    on_mismatch: MismatchPolicy,
    /// `classify` only: aux-image destination and insight verify repo.
    aux_dest: Option<String>,
    insight_repo: Option<String>,
    /// `classify` only: the resolved (file + inline) image-class membership.
    insight_images: BTreeSet<String>,
    aux_images: BTreeSet<String>,
}

struct CompiledGroup {
    format: String,
    source_repos: Vec<String>,
    first_party_prefixes: Vec<String>,
    purge_repos: Vec<String>,
    retired_prefixes: Vec<String>,
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
        self.retired_prefixes.iter().any(|p| image.starts_with(p))
    }
}

/// What a reconcile does when the tag is ABSENT from the check repo.
#[derive(Debug, PartialEq, Eq)]
enum AbsentDest<'a> {
    /// Move the source copy to this repo (supplementary). Used by `reconcile`.
    MoveTo(&'a str),
    /// Flag it for review and leave it — never drop. Used by `classify` for an
    /// insight image the upstream unexpectedly lacks (don't lose the only copy).
    Flag,
}

/// What to do with one `image:tag`, resolved from the first matching rule.
#[derive(Debug, PartialEq, Eq)]
enum Decision<'a> {
    Move(&'a str),
    DeleteIfAbsent(&'a str),
    Reconcile {
        check: &'a str,
        absent: AbsentDest<'a>,
        on_mismatch: MismatchPolicy,
    },
    /// A released tag whose image is on neither classify list — report with
    /// evidence for triage; never moved or deleted.
    Triage,
    Delete,
    Leave,
}

/// Resolve a rule's `match` (pattern name or inline regex) to an anchored regex.
fn compile_match(patterns: &HashMap<String, String>, match_: &str) -> Result<Regex> {
    let src = patterns.get(match_).map(String::as_str).unwrap_or(match_);
    Regex::new(&format!("^(?:{src})$"))
        .with_context(|| format!("compile regex for match '{match_}' (resolved: '{src}')"))
}

/// Load and merge a `classify` rule's image-class membership: the optional
/// `classes_file` (resolved relative to the rules file's directory) unioned with
/// any inline `insight_images`/`aux_images`. Errors if an image lands on both
/// lists (the insight/aux split must be disjoint).
fn load_image_classes(r: &Rule, base_dir: &Path) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let mut insight: BTreeSet<String> = r.insight_images.iter().cloned().collect();
    let mut aux: BTreeSet<String> = r.aux_images.iter().cloned().collect();
    if let Some(rel) = &r.classes_file {
        let path = base_dir.join(rel);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read classes_file '{}'", path.display()))?;
        let f: ImageClassesFile = toml::from_str(&text)
            .with_context(|| format!("parse classes_file '{}'", path.display()))?;
        insight.extend(f.insight_images);
        aux.extend(f.aux_images);
    }
    let overlap: Vec<&String> = insight.intersection(&aux).collect();
    if !overlap.is_empty() {
        bail!(
            "classify rule match='{}': image(s) on BOTH insight and aux lists: {}",
            r.match_,
            overlap
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok((insight, aux))
}

/// Compile and validate every group's rules. `base_dir` is the directory of the
/// rules file, used to resolve a `classify` rule's relative `classes_file`.
fn compile_groups(rules: &RulesFile, base_dir: &Path) -> Result<Vec<CompiledGroup>> {
    let mut out = Vec::new();
    for g in &rules.group {
        let mut crules = Vec::new();
        for r in &g.rules {
            let (mut insight_images, mut aux_images) = (BTreeSet::new(), BTreeSet::new());
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
                Action::Classify => {
                    if r.aux_dest.is_none() || r.insight_repo.is_none() {
                        bail!(
                            "rule match='{}' action=classify requires 'aux_dest' and 'insight_repo'",
                            r.match_
                        )
                    }
                    let (i, a) = load_image_classes(r, base_dir)?;
                    if i.is_empty() && a.is_empty() {
                        bail!(
                            "rule match='{}' action=classify has no images (set classes_file or inline lists)",
                            r.match_
                        )
                    }
                    insight_images = i;
                    aux_images = a;
                }
                _ => {}
            }
            crules.push(CompiledRule {
                class: class_label(&r.match_).to_string(),
                re: compile_match(&rules.patterns, &r.match_)?,
                images: r.images.clone(),
                action: r.action,
                dest: r.dest.clone(),
                check: r.check.clone(),
                on_mismatch: r.on_mismatch.unwrap_or_default(),
                aux_dest: r.aux_dest.clone(),
                insight_repo: r.insight_repo.clone(),
                insight_images: std::mem::take(&mut insight_images),
                aux_images: std::mem::take(&mut aux_images),
            });
        }
        out.push(CompiledGroup {
            format: g.format.clone(),
            source_repos: g.source_repos.clone(),
            first_party_prefixes: g.first_party_prefixes.clone(),
            purge_repos: g.purge_repos.clone(),
            retired_prefixes: g.retired_prefixes.clone(),
            rules: crules,
        });
    }
    Ok(out)
}

/// Apply the group's rules to an `image:tag` — first match (regex matches the
/// tag and the optional image filter passes) wins.
/// First rule whose image filter + tag regex match this `image:tag`.
fn matched_rule<'a>(group: &'a CompiledGroup, image: &str, tag: &str) -> Option<&'a CompiledRule> {
    group.rules.iter().find(|r| {
        (r.images.is_empty() || r.images.iter().any(|i| i == image)) && r.re.is_match(tag)
    })
}

/// The class label of the rule routing this tag (for grouping moves by rule).
fn move_class<'a>(group: &'a CompiledGroup, image: &str, tag: &str) -> &'a str {
    matched_rule(group, image, tag)
        .map(|r| r.class.as_str())
        .unwrap_or("unmatched")
}

fn decide<'a>(group: &'a CompiledGroup, image: &str, tag: &str) -> Decision<'a> {
    match matched_rule(group, image, tag) {
        Some(r) => match r.action {
            Action::Move => Decision::Move(r.dest.as_deref().unwrap_or_default()),
            Action::DeleteIfAbsent => {
                Decision::DeleteIfAbsent(r.check.as_deref().unwrap_or_default())
            }
            Action::Reconcile => Decision::Reconcile {
                check: r.check.as_deref().unwrap_or_default(),
                absent: AbsentDest::MoveTo(r.dest.as_deref().unwrap_or_default()),
                on_mismatch: r.on_mismatch,
            },
            // Route by explicit per-image class. aux → move; insight → reconcile
            // against the cache (absent → flag, never drop); neither → triage.
            Action::Classify => {
                if r.aux_images.contains(image) {
                    Decision::Move(r.aux_dest.as_deref().unwrap_or_default())
                } else if r.insight_images.contains(image) {
                    Decision::Reconcile {
                        check: r.insight_repo.as_deref().unwrap_or_default(),
                        absent: AbsentDest::Flag,
                        on_mismatch: r.on_mismatch,
                    }
                } else {
                    Decision::Triage
                }
            }
            Action::Delete => Decision::Delete,
            Action::Leave => Decision::Leave,
        },
        None => Decision::Leave,
    }
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
    /// The rule (tag-class) that matched and routed this move — for grouping
    /// the verbose plan by rule.
    pub class: String,
}

/// A move that was dropped because executing it would have clobbered a copy
/// already in the destination, or duplicated a sibling source whose copy was
/// kept instead. Left in source, reported — never deleted. Carries the build
/// dates of both the dropped copy and the winning copy so the dry-run can show
/// the age evidence behind the auto-resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersededItem {
    pub tag: TagRef,
    pub dest: String,
    /// Where the winning copy lives (the dest, or the source repo that was kept).
    pub kept: String,
    /// Build date (config `created`, date only) of this dropped copy; `None` if
    /// unresolved.
    pub own_built: Option<String>,
    /// Build date of the winning copy that was kept instead.
    pub winner_built: Option<String>,
}

/// A delete gated on absence from a presence-check repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalDelete {
    pub tag: TagRef,
    pub check_repo: String,
}

/// Owned form of [`AbsentDest`]: what to do if the tag is absent from `check_repo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbsentAction {
    /// Move the source copy here (supplementary) — `reconcile`.
    MoveTo(String),
    /// Flag for review, never drop — `classify` insight image absent upstream.
    Flag,
}

/// A released-image reconcile pending a digest comparison against `check_repo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileItem {
    pub tag: TagRef,
    pub check_repo: String,
    pub absent: AbsentAction,
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

/// A planned source-tag deletion together with the reason it's being removed,
/// so the dry-run can explain each delete instead of just listing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteItem {
    pub tag: TagRef,
    pub reason: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub moves: Vec<PlannedMove>,
    pub conditional_deletes: Vec<ConditionalDelete>,
    pub reconciles: Vec<ReconcileItem>,
    pub deletes: Vec<DeleteItem>,
    pub leaves: Vec<TagRef>,
    /// Released tags whose image is on neither classify list — pending evidence
    /// gathering, then reported as "needs classification" (never moved/deleted).
    pub triage: Vec<TagRef>,
    /// Tags already in their correct repo (self-move skipped) — convergence signal.
    pub already_placed: usize,
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
            // A repo can be both a source and a destination (so already-misfiled
            // artifacts get re-homed). A tag already in its correct repo needs no
            // move — skip the self-move rather than churn it.
            Decision::Move(dest) if dest == source_repo => {
                plan.already_placed += 1;
            }
            Decision::Move(dest) => plan.moves.push(PlannedMove {
                source_repo: source_repo.to_string(),
                image: image.clone(),
                tag: tag.clone(),
                dest: dest.to_string(),
                class: move_class(group, image, tag).to_string(),
            }),
            Decision::DeleteIfAbsent(check) => plan.conditional_deletes.push(ConditionalDelete {
                tag: tagref(),
                check_repo: check.to_string(),
            }),
            Decision::Reconcile {
                check,
                absent,
                on_mismatch,
            } => plan.reconciles.push(ReconcileItem {
                tag: tagref(),
                check_repo: check.to_string(),
                absent: match absent {
                    AbsentDest::MoveTo(d) => AbsentAction::MoveTo(d.to_string()),
                    AbsentDest::Flag => AbsentAction::Flag,
                },
                on_mismatch,
            }),
            Decision::Triage => plan.triage.push(tagref()),
            Decision::Delete => plan.deletes.push(DeleteItem {
                tag: tagref(),
                reason: "matched a delete rule".to_string(),
            }),
            Decision::Leave => plan.leaves.push(tagref()),
        }
    }
    plan
}

/// Resolve move collisions so a move never clobbers a copy already in the
/// destination, and several sources never clobber each other at one dest. Pure
/// (no I/O) so it is unit-tested; the caller supplies the precomputed inputs.
///
/// For each distinct `(dest, image, tag)`:
/// - if `present` contains it (the dest already holds that tag) → **supersede
///   every move** (never clobber the authoritative dest copy, e.g. a pipeline-
///   published rolling `develop`);
/// - else if a single source moves it → keep the move;
/// - else (several sources, dest empty) → keep the **newest** by `created`
///   timestamp from `ts` (RFC3339; a missing entry sorts oldest) and supersede
///   the rest.
///
/// `present` is keyed by `(dest, image, tag)`; `ts` by `(repo, image, tag)` and
/// must cover every source candidate plus, for a present collision, the dest
/// copy (keyed by the dest repo). Superseded moves are returned separately (left
/// in source, reported) carrying both copies' build dates for the report.
fn resolve_move_collisions(
    moves: Vec<PlannedMove>,
    present: &std::collections::HashSet<(String, String, String)>,
    ts: &HashMap<(String, String, String), Option<String>>,
) -> (Vec<PlannedMove>, Vec<SupersededItem>) {
    let mut by_key: BTreeMap<(String, String, String), Vec<PlannedMove>> = BTreeMap::new();
    for m in moves {
        by_key
            .entry((m.dest.clone(), m.image.clone(), m.tag.clone()))
            .or_default()
            .push(m);
    }
    let age = |repo: &str, image: &str, tag: &str| -> Option<String> {
        ts.get(&(repo.to_string(), image.to_string(), tag.to_string()))
            .cloned()
            .flatten()
    };
    let mut kept = Vec::new();
    let mut superseded = Vec::new();
    for ((dest, image, tag), mut cands) in by_key {
        // No-clobber: the dest already holds this tag → keep it, drop every move.
        if present.contains(&(dest.clone(), image.clone(), tag.clone())) {
            let winner_built = age(&dest, &image, &tag);
            for m in cands {
                let own_built = age(&m.source_repo, &m.image, &m.tag);
                superseded.push(SupersededItem {
                    dest: m.dest,
                    kept: format!("existing copy in {dest}"),
                    own_built,
                    winner_built: winner_built.clone(),
                    tag: TagRef {
                        source_repo: m.source_repo,
                        image: m.image,
                        tag: m.tag,
                    },
                });
            }
            continue;
        }
        if cands.len() == 1 {
            kept.push(cands.pop().unwrap());
            continue;
        }
        // Several sources, dest empty: keep the newest by build date (None sorts
        // oldest); first wins ties for determinism.
        let mut best = 0usize;
        for i in 1..cands.len() {
            let a = age(&cands[i].source_repo, &cands[i].image, &cands[i].tag).unwrap_or_default();
            let b = age(
                &cands[best].source_repo,
                &cands[best].image,
                &cands[best].tag,
            )
            .unwrap_or_default();
            if a > b {
                best = i;
            }
        }
        let winner_repo = cands[best].source_repo.clone();
        let winner_built = age(&winner_repo, &image, &tag);
        for (i, m) in cands.into_iter().enumerate() {
            if i == best {
                kept.push(m);
            } else {
                let own_built = age(&m.source_repo, &m.image, &m.tag);
                superseded.push(SupersededItem {
                    dest: m.dest,
                    kept: format!("newer copy in {winner_repo}"),
                    own_built,
                    winner_built: winner_built.clone(),
                    tag: TagRef {
                        source_repo: m.source_repo,
                        image: m.image,
                        tag: m.tag,
                    },
                });
            }
        }
    }
    (kept, superseded)
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

    let mut check =
        |name: &str, fmt: &str, must_be_hosted: bool, role: &str| match by_name.get(name) {
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
            // classify: aux destination must be hosted; insight repo just exists.
            if let Some(d) = &r.aux_dest {
                check(d, &g.format, true, "aux destination");
            }
            if let Some(c) = &r.insight_repo {
                check(c, &g.format, false, "insight");
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
    /// Print the full per-tag move list. Default summarizes moves by destination.
    pub verbose: bool,
}

/// Resolved, ready-to-execute plan (after presence checks + purge enumeration).
#[derive(Default)]
struct ResolvedPlan {
    moves: Vec<PlannedMove>,
    deletes: Vec<DeleteItem>,
    kept: Vec<TagRef>,
    leaves: Vec<TagRef>,
    purges: Vec<TagRef>,
    /// Tags under a retired `retired_prefixes` namespace — deleted from source.
    prefix_deletes: Vec<TagRef>,
    /// Reconcile tags present in the check repo but with a *different* digest,
    /// carrying the evidence (digests + build times) needed to explain them.
    mismatched: Vec<MismatchInfo>,
    /// Insight-classified images whose x.y.z tag is unexpectedly ABSENT upstream.
    /// Flagged (never dropped) — the source copy may be the only one.
    insight_absent: Vec<TagRef>,
    /// Released tags whose image is on neither classify list — reported with
    /// evidence so the operator can assign each to insight or aux. Never touched.
    needs_classification: Vec<TriageItem>,
    /// Moves dropped to avoid clobbering an existing dest copy / duplicating a
    /// sibling source (auto-resolved: keep dest, else newest). Left in source.
    superseded: Vec<SupersededItem>,
    /// Tags in source repos the reorg never touches because their image is not
    /// first-party — they stay put. Tracked only to report the post-reorg residual.
    remaining_third_party: Vec<TagRef>,
    /// Count of tags already in their correct repo (self-move skipped).
    already_placed: usize,
}

/// A released tag whose image is on neither classify list, with the evidence the
/// operator needs to decide which list it belongs on. Never an automatic verdict.
#[derive(Debug, Clone)]
struct TriageItem {
    tag: TagRef,
    /// Present in the insight repo (cache → upstream)? Strong "insight image" hint.
    upstream_present: bool,
    /// True when present upstream AND the digest matches the source copy.
    upstream_same_digest: bool,
    /// Source image build date (config `created`, date only); `None` if unresolved.
    source_built: Option<String>,
}

impl TriageItem {
    /// A one-line, non-authoritative hint at which list the image belongs on.
    fn hint(&self) -> &'static str {
        match (self.upstream_present, self.upstream_same_digest) {
            (true, true) => "present upstream, same digest → likely an INSIGHT image",
            (true, false) => "present upstream, different digest → likely INSIGHT (review digest)",
            (false, _) => "absent upstream → likely an AUX image",
        }
    }
}

pub async fn run(client: &DepotClient, cfg: ReorgConfig) -> Result<()> {
    let text = std::fs::read_to_string(&cfg.rules_path)
        .with_context(|| format!("read rules file '{}'", cfg.rules_path))?;
    let rules = RulesFile::from_toml(&text)?;
    // Resolve a classify rule's relative `classes_file` against the rules dir.
    let base_dir = Path::new(&cfg.rules_path)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let groups = compile_groups(&rules, base_dir)?;
    client.login().await?;
    preflight_repos(client, &groups, rules.usage_repo.as_deref()).await?;

    let mut resolved = ResolvedPlan::default();
    // Each scanned repo's current `(image, tag)` set — reused to detect, with no
    // extra I/O, whether a move's destination already holds the tag (no-clobber).
    let mut inventories: HashMap<String, std::collections::HashSet<(String, String)>> =
        HashMap::new();

    for group in &groups {
        if group.format != "docker" {
            bail!(
                "format '{}' is not supported yet (only 'docker'); \
                 add server-side movers before using it",
                group.format
            );
        }

        // Scan source repos AND purge repos. Purge repos are drained like
        // sources: their first-party tags get re-homed by the rules, and their
        // non-first-party remainder is FLAGGED for review (never auto-deleted).
        let scan: Vec<(&String, bool)> = group
            .source_repos
            .iter()
            .map(|r| (r, false))
            .chain(group.purge_repos.iter().map(|r| (r, true)))
            .collect();
        for (repo, is_purge) in scan {
            let inv = list_repo_tags(client, repo).await?;
            inventories
                .entry(repo.clone())
                .or_default()
                .extend(inv.iter().cloned());
            // Non-first-party tags. Retired-prefix tags and the remainder in a
            // purge repo are FLAGGED for the operator (reported, not deleted);
            // third-party in a regular source repo stays untouched. First-party
            // is handled by build_group_plan below.
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
                } else if is_purge {
                    resolved.purges.push(tagref);
                } else {
                    resolved.remaining_third_party.push(tagref);
                }
            }
            let plan = build_group_plan(group, &inv, repo);
            resolved.already_placed += plan.already_placed;
            resolved.moves.extend(plan.moves);
            resolved.deletes.extend(plan.deletes);
            resolved.leaves.extend(plan.leaves);

            // Resolve conditional deletes via a presence check.
            for cd in plan.conditional_deletes {
                if insight_has(client, &cd.check_repo, &cd.tag.image, &cd.tag.tag).await? {
                    resolved.kept.push(cd.tag);
                } else {
                    let reason = format!("absent from {} (delete-if-absent)", cd.check_repo);
                    resolved.deletes.push(DeleteItem {
                        tag: cd.tag,
                        reason,
                    });
                }
            }

            // Resolve reconciles: compare the source digest against the canonical
            // check repo. Absent → MoveTo dest (supplementary) or Flag (insight
            // image missing upstream — never dropped); present + same digest →
            // delete the redundant source copy; present + different → flag/review.
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
                    match &rec.absent {
                        // Supplementary → move to dest (unless already there — a
                        // repo may be its own source).
                        AbsentAction::MoveTo(dest) => {
                            if *dest != rec.tag.source_repo {
                                resolved.moves.push(PlannedMove {
                                    source_repo: rec.tag.source_repo.clone(),
                                    image: rec.tag.image.clone(),
                                    tag: rec.tag.tag.clone(),
                                    dest: dest.clone(),
                                    class: move_class(group, &rec.tag.image, &rec.tag.tag)
                                        .to_string(),
                                });
                            } else {
                                resolved.already_placed += 1;
                            }
                        }
                        // Insight image the upstream lacks: don't move it to aux,
                        // and don't drop the only copy — flag for review.
                        AbsentAction::Flag => resolved.insight_absent.push(rec.tag),
                    }
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
                    // Redundant duplicate. Drop the source copy — but NEVER out of
                    // the check repo itself (the cache holds the canonical copy);
                    // only drop copies that live elsewhere.
                    if rec.tag.source_repo == rec.check_repo {
                        resolved.kept.push(rec.tag);
                    } else {
                        let reason = format!(
                            "identical digest already in {} (redundant duplicate)",
                            rec.check_repo
                        );
                        resolved.deletes.push(DeleteItem {
                            tag: rec.tag,
                            reason,
                        });
                    }
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

            // Resolve triage (classify: image on neither list). Gather evidence —
            // upstream presence + digest match + source build date — so the
            // operator can decide which list each image belongs on. Never touched.
            // The insight repo to probe is the classify rule's `insight_repo`.
            let insight_repo = group
                .rules
                .iter()
                .find(|r| r.action == Action::Classify)
                .and_then(|r| r.insight_repo.clone());
            for tag in plan.triage {
                let (present, same_digest) = if let Some(ir) = &insight_repo {
                    let (st, _, up_digest) = client
                        .docker_head_manifest(ir, &tag.image, &tag.tag)
                        .await
                        .with_context(|| {
                            format!("triage check {}/{}:{}", ir, tag.image, tag.tag)
                        })?;
                    if st == 200 {
                        let (_, _, src_digest) = client
                            .docker_head_manifest(&tag.source_repo, &tag.image, &tag.tag)
                            .await
                            .unwrap_or_default();
                        (true, !up_digest.is_empty() && up_digest == src_digest)
                    } else {
                        (false, false)
                    }
                } else {
                    (false, false)
                };
                let source_built =
                    resolve_built(client, &tag.source_repo, &tag.image, &tag.tag).await;
                resolved.needs_classification.push(TriageItem {
                    tag,
                    upstream_present: present,
                    upstream_same_digest: same_digest,
                    source_built,
                });
            }
        }
    }

    // No-clobber + de-dup of moves (auto-resolved; the operator audits the plan).
    // A move must never overwrite a copy already in the destination (e.g. a
    // pipeline-published rolling `develop`), and several sources must not clobber
    // each other at one dest. Decide which single copy "wins" using build dates,
    // and report the rest as superseded (left in source, never deleted).
    {
        // Which (dest, image, tag) the destination already holds — from the
        // scanned inventories (no extra I/O for repos we already listed; HEAD as
        // a fallback for any dest that wasn't scanned).
        let mut present: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        let mut dest_keys: BTreeSet<(String, String, String)> = BTreeSet::new();
        for m in &resolved.moves {
            dest_keys.insert((m.dest.clone(), m.image.clone(), m.tag.clone()));
        }
        for (dest, image, tag) in &dest_keys {
            let here = match inventories.get(dest) {
                Some(set) => set.contains(&(image.clone(), tag.clone())),
                None => insight_has(client, dest, image, tag).await.unwrap_or(false),
            };
            if here {
                present.insert((dest.clone(), image.clone(), tag.clone()));
            }
        }
        // Build dates needed to explain/resolve collisions: every source copy of
        // a colliding tag, plus the dest copy when the dest already holds it.
        // Count sources per (dest, image, tag) to find the multi-source collisions.
        let mut srcs_per_key: HashMap<(String, String, String), usize> = HashMap::new();
        for m in &resolved.moves {
            *srcs_per_key
                .entry((m.dest.clone(), m.image.clone(), m.tag.clone()))
                .or_default() += 1;
        }
        // Build dates are only needed to DECIDE the multi-source-empty-dest case
        // (pick the newest); the no-clobber case is decided without them (keep the
        // dest). So resolve build dates ONLY for genuine multi-source collisions
        // where the dest is empty — bounded to the ambiguous tags (e.g. rolling
        // `develop`), not the potentially huge no-clobber set. No-clobber age is
        // shown cheaply via the existing `[pulled DATE]` usage annotation.
        let mut ts: HashMap<(String, String, String), Option<String>> = HashMap::new();
        for m in &resolved.moves {
            let key = (m.dest.clone(), m.image.clone(), m.tag.clone());
            let multi = !present.contains(&key) && srcs_per_key.get(&key).copied().unwrap_or(0) > 1;
            if !multi {
                continue;
            }
            let v = resolve_created_ts(client, &m.source_repo, &m.image, &m.tag).await;
            ts.insert((m.source_repo.clone(), m.image.clone(), m.tag.clone()), v);
        }
        let moves = std::mem::take(&mut resolved.moves);
        let (kept, superseded) = resolve_move_collisions(moves, &present, &ts);
        resolved.moves = kept;
        resolved.superseded = superseded;
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
        for d in &resolved.deletes {
            images.insert(d.tag.image.clone());
        }
        for t in resolved.kept.iter().chain(&resolved.leaves) {
            images.insert(t.image.clone());
        }
        for m in &resolved.mismatched {
            images.insert(m.tag.image.clone());
        }
        for t in &resolved.insight_absent {
            images.insert(t.image.clone());
        }
        for t in &resolved.needs_classification {
            images.insert(t.tag.image.clone());
        }
        for s in &resolved.superseded {
            images.insert(s.tag.image.clone());
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

    // Which tag classes (rule `match` names) route to each move destination —
    // lets the summary explain *why* the move counts land where they do. Also
    // capture the classes in rule order (catch-all last), so the verbose move
    // list prints groups in the same order the rules are evaluated.
    let mut dest_classes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut class_order: Vec<String> = Vec::new();
    let mut class_patterns: BTreeMap<String, String> = BTreeMap::new();
    for g in &rules.group {
        for r in &g.rules {
            let class = class_label(&r.match_).to_string();
            if !class_order.contains(&class) {
                class_order.push(class.clone());
            }
            // Resolve the rule's `match` to its regex source (a named pattern, or
            // the inline regex itself) so the verbose plan can show it.
            class_patterns.entry(class.clone()).or_insert_with(|| {
                rules
                    .patterns
                    .get(&r.match_)
                    .cloned()
                    .unwrap_or_else(|| r.match_.clone())
            });
            // `move`/`reconcile` route via `dest`; `classify` routes aux images
            // via `aux_dest` — annotate that destination with this class too.
            if let Some(d) = r.dest.as_ref().or(r.aux_dest.as_ref()) {
                dest_classes.entry(d.clone()).or_default().push(class);
            }
        }
    }

    print_plan(
        &resolved,
        cfg.copy,
        rules.usage_repo.as_deref(),
        &atimes,
        cfg.verbose,
        &dest_classes,
        &class_order,
        &class_patterns,
    );

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
                if let Some(c) = &r.insight_repo {
                    add(c);
                }
                if let Some(d) = &r.aux_dest {
                    add(d);
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
                if let Some(c) = &r.insight_repo {
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

        // Retention safety: warn if a destination's cleanup policy would expire
        // the tags we're about to move there (a re-file silently becoming a delete).
        print_destination_retention(client, &resolved, &atimes).await?;

        // Cache-integrity check: diff a local cache repo against the
        // authoritative upstream it mirrors, flagging tags present locally but
        // absent upstream (manually-pushed pollution that the cache would never
        // legitimately hold).
        if let Some(authority) = &rules.check_authority {
            check_cache_authority(client, authority, cfg.insecure).await?;
        }

        // Repeat the plan summary down here next to the repository/remaining
        // data, so a reader who scrolled the detail doesn't have to jump back to
        // the top to recall the counts.
        print_overview(
            &resolved,
            cfg.copy,
            rules.usage_repo.as_deref(),
            &dest_classes,
        );

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
    // Flagged tags (purge/retired zones) are reported, never auto-deleted (b).
    let flagged = resolved.purges.len() + resolved.prefix_deletes.len();
    let total_deletes = if cfg.copy {
        0
    } else {
        resolved.deletes.len() + approved_mismatch_deletes.len()
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
        if !resolved.deletes.is_empty() {
            println!(
                "\n--copy is non-destructive: skipped {} delete(s).",
                resolved.deletes.len()
            );
        }
    } else {
        // Hard guard: NEVER delete out of a cache/check/insight repo. The
        // canonical copy of a redundant insight tag lives in the cache; we only
        // drop the copies that live elsewhere. This is belt-and-suspenders over
        // the resolve-time guard (a redundant tag whose source IS the check repo
        // is kept, not dropped) — refuse here too, regardless of how it arose.
        let mut protected: BTreeSet<String> = BTreeSet::new();
        for g in &groups {
            for r in &g.rules {
                if let Some(c) = &r.check {
                    protected.insert(c.clone());
                }
                if let Some(c) = &r.insight_repo {
                    protected.insert(c.clone());
                }
            }
        }
        if let Some(a) = &rules.check_authority {
            protected.insert(a.cache_repo.clone());
        }
        // Only verified-duplicate Drops + operator-approved mismatch deletes are
        // removed. Flagged tags (purge/retired zones) are never auto-deleted.
        let mismatch_tags = approved_mismatch_deletes.iter().map(|m| &m.tag);
        for d in resolved.deletes.iter().map(|d| &d.tag).chain(mismatch_tags) {
            if protected.contains(&d.source_repo) {
                eprintln!(
                    "  delete REFUSED (cache/insight repo never deleted): {}/{}:{}",
                    d.source_repo, d.image, d.tag
                );
                continue;
            }
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
    if flagged > 0 {
        println!(
            "\n{flagged} tag(s) flagged for review were left untouched (purge/retired zones); \
             delete them in a separate explicit step when ready."
        );
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
    let is_fp = |image: &str| {
        first_party_prefixes
            .iter()
            .any(|pre| image.starts_with(pre.as_str()))
    };

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
        .chain(&p.insight_absent)
        .chain(p.needs_classification.iter().map(|t| &t.tag))
        .chain(p.superseded.iter().map(|s| &s.tag))
        .collect();

    let mut source_repos: std::collections::BTreeSet<&str> = Default::default();
    for t in p
        .remaining_third_party
        .iter()
        .chain(fp_left.iter().copied())
    {
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

/// Friendly label for a rule's raw `match` pattern in the summary.
fn class_label(m: &str) -> &str {
    if m == ".*" {
        "catch-all"
    } else {
        m
    }
}

/// One-screen plan summary: moves by destination (with the tag classes that
/// route there), the three distinct removal kinds spelled out, then
/// review/kept/unmatched. Printed at the top and again at the bottom (beside the
/// repository/remaining data) so the counts are visible without scrolling.
fn print_overview(
    p: &ResolvedPlan,
    copy: bool,
    usage_repo: Option<&str>,
    dest_classes: &BTreeMap<String, Vec<String>>,
) {
    let verb = if copy { "copy" } else { "move" };
    println!("================ Reorg plan ({verb}) — summary ================");
    if let Some(ur) = usage_repo {
        println!("Usage annotations '[pulled DATE]' show last access from '{ur}'.");
    }

    // Moves grouped by destination, annotated with the routing tag classes so
    // the per-destination counts are self-explanatory.
    let mut by_dest: BTreeMap<&str, usize> = BTreeMap::new();
    for m in &p.moves {
        *by_dest.entry(m.dest.as_str()).or_default() += 1;
    }
    println!(
        "  Moves     {:>6} tag(s) — re-homed by tag class:",
        p.moves.len()
    );
    for (dest, n) in &by_dest {
        match dest_classes.get(*dest) {
            Some(cs) if !cs.is_empty() => {
                println!("        {n:>6}  → {dest:<26} ({})", cs.join(", "))
            }
            _ => println!("        {n:>6}  → {dest}"),
        }
    }

    // Drop — the only auto-delete: a verified duplicate already in the check repo.
    if !p.deletes.is_empty() {
        println!(
            "  Drop      {:>6} tag(s) — verified duplicate (already in the check repo); deleted on --apply",
            p.deletes.len()
        );
    }
    if !p.mismatched.is_empty() {
        println!(
            "  Review    {:>6} tag(s) — reconcile mismatch (different digest); prompts on --apply",
            p.mismatched.len()
        );
    }
    // Triage — classify: image on neither insight nor aux list. Never touched.
    if !p.needs_classification.is_empty() {
        println!(
            "  Needs cls {:>6} tag(s) — released image on NEITHER classify list; triage (not moved/deleted)",
            p.needs_classification.len()
        );
    }
    // Insight images the upstream unexpectedly lacks — flagged, never dropped.
    if !p.insight_absent.is_empty() {
        println!(
            "  Insight ⚠ {:>6} tag(s) — insight image ABSENT upstream; NOT dropped (may be the only copy)",
            p.insight_absent.len()
        );
    }

    // Flagged — purge/retired drain remainder: reported, NEVER auto-deleted.
    let flagged = p.purges.len() + p.prefix_deletes.len();
    if flagged > 0 {
        println!(
            "  Flagged   {:>6} tag(s) — left for you to review, NOT deleted:",
            flagged
        );
        if !p.prefix_deletes.is_empty() {
            let mut ns: BTreeSet<String> = BTreeSet::new();
            for t in &p.prefix_deletes {
                ns.insert(format!(
                    "{}/",
                    t.image.split('/').next().unwrap_or(t.image.as_str())
                ));
            }
            println!(
                "        retired   {:>6} — under retired_prefixes: {}",
                p.prefix_deletes.len(),
                ns.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        if !p.purges.is_empty() {
            let mut repos: BTreeSet<&str> = BTreeSet::new();
            for t in &p.purges {
                repos.insert(t.source_repo.as_str());
            }
            println!(
                "        remainder {:>6} — third-party left in drained purge repo(s): {}",
                p.purges.len(),
                repos.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
    }
    if !p.kept.is_empty() {
        println!(
            "  Kept      {:>6} tag(s) — already in the check repo, left in place",
            p.kept.len()
        );
    }
    if p.already_placed > 0 {
        println!(
            "  Placed    {:>6} tag(s) — already in their correct repo, no move (convergence)",
            p.already_placed
        );
    }
    // Superseded — moves dropped to avoid clobbering the dest / duplicating a
    // sibling source. Auto-resolved (keep dest, else newest); left in source.
    if !p.superseded.is_empty() {
        let clobber = p
            .superseded
            .iter()
            .filter(|s| s.kept.starts_with("existing copy"))
            .count();
        let dup = p.superseded.len() - clobber;
        println!(
            "  Superseded{:>6} tag(s) — move skipped, left in source (not deleted): \
             {clobber} would-clobber-dest, {dup} duplicate-of-newer-source",
            p.superseded.len()
        );
    }
    println!(
        "  Unmatched {:>6} tag(s) — no rule matched{}",
        p.leaves.len(),
        if p.leaves.is_empty() { " ✓" } else { "" }
    );

    // Moves by source repo — shows where the misfiling lives (especially the
    // destinations-as-sources and the drained purge repos).
    if !p.moves.is_empty() {
        let mut by_src: BTreeMap<&str, usize> = BTreeMap::new();
        for m in &p.moves {
            *by_src.entry(m.source_repo.as_str()).or_default() += 1;
        }
        let parts: Vec<String> = by_src.iter().map(|(r, n)| format!("{r} {n}")).collect();
        println!("  Moves by source: {}", parts.join(" · "));
    }

    // One-line preview of what --apply will (and won't) do.
    let asks = p
        .mismatched
        .iter()
        .filter(|m| m.on_mismatch == MismatchPolicy::Ask)
        .count();
    let flagged = p.purges.len() + p.prefix_deletes.len();
    println!(
        "  On --apply: {} move(s) · {} drop(s) · {} prompt(s) · {} flagged · {} superseded (untouched)",
        p.moves.len(),
        p.deletes.len(),
        asks,
        flagged,
        p.superseded.len(),
    );
}

/// Warn when a move's destination has a cleanup policy that would expire the
/// moved tag — so a re-file doesn't silently become a delete. For
/// `cleanup_max_unaccessed_days` we count tags whose last pull is already older
/// than the window (using the atime annotations). `cleanup_max_age_days` is by
/// creation date, which we don't fetch per tag, so we report the policy + count
/// for review rather than a precise expiry tally.
async fn print_destination_retention(
    client: &DepotClient,
    resolved: &ResolvedPlan,
    atimes: &HashMap<(String, String), String>,
) -> Result<()> {
    if resolved.moves.is_empty() {
        return Ok(());
    }
    let cleanup: HashMap<String, (Option<u32>, Option<u32>)> = client
        .list_repos()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            (
                r.name,
                (r.cleanup_max_age_days, r.cleanup_max_unaccessed_days),
            )
        })
        .collect();
    let mut by_dest: BTreeMap<&str, Vec<&PlannedMove>> = BTreeMap::new();
    for m in &resolved.moves {
        by_dest.entry(m.dest.as_str()).or_default().push(m);
    }
    println!(
        "\n================ Destination retention — will the moved tags survive? ================"
    );
    let now = chrono::Utc::now();
    for (dest, moves) in &by_dest {
        let (age, unacc) = cleanup.get(*dest).copied().unwrap_or((None, None));
        if age.is_none() && unacc.is_none() {
            println!(
                "  {dest}: no cleanup policy — {} move(s) are permanent ✓",
                moves.len()
            );
            continue;
        }
        let mut policy = Vec::new();
        if let Some(a) = age {
            policy.push(format!("max_age={a}d (creation)"));
        }
        if let Some(u) = unacc {
            policy.push(format!("max_unaccessed={u}d (last pull)"));
        }
        let note = if let Some(u) = unacc {
            let stale = moves
                .iter()
                .filter(|m| {
                    let fresh = atimes
                        .get(&(m.image.clone(), m.tag.clone()))
                        .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
                        .map(|d| (now - d.with_timezone(&chrono::Utc)).num_days() <= u as i64)
                        .unwrap_or(false);
                    !fresh
                })
                .count();
            format!(
                " — {} move(s); {stale} last pulled older than {u}d → likely to expire after the move",
                moves.len()
            )
        } else {
            format!(
                " — {} move(s); expiry is by creation age (not checked here) — review",
                moves.len()
            )
        };
        println!("  ⚠ {dest}: {}{note}", policy.join(", "));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn print_plan(
    p: &ResolvedPlan,
    copy: bool,
    usage_repo: Option<&str>,
    atimes: &HashMap<(String, String), String>,
    verbose: bool,
    dest_classes: &BTreeMap<String, Vec<String>>,
    class_order: &[String],
    class_patterns: &BTreeMap<String, String>,
) {
    let verb = if copy { "copy" } else { "move" };
    let del_note = if copy { " (skipped: --copy)" } else { "" };

    // One-screen summary up top (the caller repeats it at the bottom, beside the
    // repository/remaining data, so you needn't scroll back up).
    print_overview(p, copy, usage_repo, dest_classes);

    // The per-tag move list is large (often thousands of lines). Show it only
    // with --verbose, so the consequential sections (removals, mismatches) stay
    // visible without scrolling. The summary already breaks moves down by dest.
    if verbose {
        // Group by the matching rule (class) so you can audit each rule's exact
        // effect. Print groups in rule-evaluation order (catch-all last), not
        // alphabetically. Each class routes to one destination.
        let mut by_class: BTreeMap<&str, Vec<&PlannedMove>> = BTreeMap::new();
        for m in &p.moves {
            by_class.entry(m.class.as_str()).or_default().push(m);
        }
        // Rule order first, then any leftover classes (e.g. "unmatched") after.
        let mut ordered: Vec<&str> = class_order
            .iter()
            .map(|c| c.as_str())
            .filter(|c| by_class.contains_key(c))
            .collect();
        for c in by_class.keys() {
            if !ordered.contains(c) {
                ordered.push(c);
            }
        }
        println!(
            "\nPlanned {verb}s ({} tag(s)) — grouped by matching rule:",
            p.moves.len()
        );
        for class in ordered {
            let moves = &by_class[class];
            let dest = moves.first().map(|m| m.dest.as_str()).unwrap_or("?");
            let pat = class_patterns
                .get(class)
                .map(|p| format!(" /{p}/"))
                .unwrap_or_default();
            println!("  rule '{class}'{pat} → {dest} ({} tag(s)):", moves.len());
            for m in moves {
                let note = atime_note(usage_repo, atimes, &m.image, &m.tag);
                println!("       {}/{}:{}{note}", m.source_repo, m.image, m.tag);
            }
        }
    } else if !p.moves.is_empty() {
        println!(
            "\nPlanned {verb}s: {} tag(s) — summarized by destination above \
             (re-run with --verbose for the full per-tag list, grouped by rule).",
            p.moves.len()
        );
    }

    println!(
        "\nDrop — redundant duplicates{del_note} ({} tag(s)):",
        p.deletes.len()
    );
    // Group by the recorded reason so each delete explains itself (redundant
    // duplicate already in the check repo / absent from the check repo / matched
    // an explicit delete rule) rather than appearing as an unexplained list.
    let mut by_reason: BTreeMap<&str, Vec<&DeleteItem>> = BTreeMap::new();
    for d in &p.deletes {
        by_reason.entry(d.reason.as_str()).or_default().push(d);
    }
    for (reason, items) in &by_reason {
        println!("  {reason} — {} tag(s):", items.len());
        for d in items {
            let note = atime_note(usage_repo, atimes, &d.tag.image, &d.tag.tag);
            println!(
                "       {}/{}:{}{note}",
                d.tag.source_repo, d.tag.image, d.tag.tag
            );
        }
    }

    if !p.purges.is_empty() {
        let mut by_repo: BTreeMap<&str, usize> = BTreeMap::new();
        for t in &p.purges {
            *by_repo.entry(t.source_repo.as_str()).or_default() += 1;
        }
        println!(
            "\nFlagged — third-party remainder in drained purge repo(s) ({} tag(s) across {} repo(s)) \
             — NOT deleted; first-party was re-homed, this is what's left to review:",
            p.purges.len(),
            by_repo.len()
        );
        for (repo, n) in &by_repo {
            println!("  {repo}: {n} tag(s)");
        }
    }

    if !p.prefix_deletes.is_empty() {
        // Retired-prefix tags — flagged for review, NOT deleted. Listed in full
        // (grouped by repo then image) since these are the ones to eyeball.
        println!(
            "\nFlagged — retired-prefix tags (retired_prefixes) ({} tag(s)) — NOT deleted, review:",
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

    // Superseded — moves auto-dropped to avoid a clobber/duplicate, with the age
    // evidence behind each decision (this copy's build date vs the kept copy's).
    if !p.superseded.is_empty() {
        println!(
            "\nSuperseded ({} tag(s)) — move skipped to avoid a clobber/duplicate; left in source \
             (not deleted). 'kept' is the copy that wins; build dates shown for audit:",
            p.superseded.len()
        );
        // Show build dates when resolved (the multi-source tie-break); otherwise
        // the appended [pulled DATE] usage annotation carries the age.
        let built = |o: &Option<String>| {
            o.as_deref()
                .map(|s| format!(" (built {})", s.get(0..10).unwrap_or(s)))
                .unwrap_or_default()
        };
        for s in &p.superseded {
            let note = atime_note(usage_repo, atimes, &s.tag.image, &s.tag.tag);
            println!(
                "       {}/{}:{}{} -> {} dest {} — kept {}{}{note}",
                s.tag.source_repo,
                s.tag.image,
                s.tag.tag,
                built(&s.own_built),
                verb,
                s.dest,
                s.kept,
                built(&s.winner_built),
            );
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

    // Insight images the upstream unexpectedly lacks — flagged, never dropped.
    if !p.insight_absent.is_empty() {
        println!(
            "\nInsight absent ({} tag(s)) — classified INSIGHT but absent upstream; NOT dropped \
             (the source copy may be the only one) — review:",
            p.insight_absent.len()
        );
        let mut by_repo: BTreeMap<&str, BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
        for t in &p.insight_absent {
            by_repo
                .entry(t.source_repo.as_str())
                .or_default()
                .entry(t.image.as_str())
                .or_default()
                .push(t.tag.as_str());
        }
        for (repo, by_img) in &by_repo {
            println!("  {repo}:");
            for (img, tags) in by_img {
                let mut ts = tags.clone();
                ts.sort();
                println!("       {img}: {}", ts.join(", "));
            }
        }
    }

    // Needs classification — released image on neither classify list. Reported
    // with evidence (upstream presence + a hint) so each can be assigned to the
    // insight or aux list. Never moved or deleted.
    if !p.needs_classification.is_empty() {
        println!(
            "\nNeeds classification ({} tag(s)) — released image on NEITHER classify list; \
             add each image to insight_images or aux_images, then re-run (not moved/deleted):",
            p.needs_classification.len()
        );
        // Group by image so the operator decides per image, not per tag, with the
        // shared hint shown once.
        let mut by_image: BTreeMap<&str, (Vec<&TriageItem>, &TriageItem)> = BTreeMap::new();
        for t in &p.needs_classification {
            let e = by_image
                .entry(t.tag.image.as_str())
                .or_insert_with(|| (Vec::new(), t));
            e.0.push(t);
        }
        for (image, (items, sample)) in &by_image {
            println!("  {image} — {} ({}):", sample.hint(), items.len());
            for t in items {
                let built = t
                    .source_built
                    .as_deref()
                    .map(|b| format!("  [built {b}]"))
                    .unwrap_or_default();
                let note = atime_note(usage_repo, atimes, &t.tag.image, &t.tag.tag);
                println!(
                    "       {}/{}:{}{built}{note}",
                    t.tag.source_repo, t.tag.image, t.tag.tag
                );
            }
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
    let after_of = |name: &str| after.and_then(|a| a.repos.iter().find(|r| r.name == name));
    match after {
        None => {
            println!("\n================ Repository summary (current) ================");
            println!(
                "  {:28} {:8} {:>12} {:>12}",
                "repo", "type", "artifacts", "size"
            );
            for r in &before.repos {
                println!(
                    "  {:28} {:8} {:>12} {:>12}",
                    r.name,
                    r.repo_type,
                    r.artifacts,
                    fmt_gb(r.bytes)
                );
            }
            println!(
                "\n  sum(all hosted/cache repos)    = {}",
                fmt_gb(before.logical_sum)
            );
            println!(
                "  sum(docker hosted/cache repos) = {}",
                fmt_gb(before.docker_logical)
            );
            for (name, bytes, blobs) in &before.stores {
                println!(
                    "  store '{name}' physical total      = {} ({blobs} blobs, dedup)",
                    fmt_gb(*bytes)
                );
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
                let (a_art, a_bytes) = aft
                    .map(|r| (r.artifacts, r.bytes))
                    .unwrap_or((b.artifacts, b.bytes));
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
    // Candidates for deletion (NEVER auto-deleted). Split by release format: a
    // non-x.y.z tag in a release registry is high-confidence pollution; an x.y.z
    // tag that's merely absent upstream warrants a closer look before removal.
    let xyz = Regex::new(r"^\d+\.\d+\.\d+$").expect("static regex");
    let mut non_release: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut release_fmt: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (img, ts) in &by_img {
        for t in ts {
            if xyz.is_match(t) {
                release_fmt
                    .entry(img.as_str())
                    .or_default()
                    .push(t.as_str());
            } else {
                non_release
                    .entry(img.as_str())
                    .or_default()
                    .push(t.as_str());
            }
        }
    }
    let nr: usize = non_release.values().map(|v| v.len()).sum();
    let rf: usize = release_fmt.values().map(|v| v.len()).sum();
    println!(
        "  {n} tag(s) in '{}' NOT upstream — candidates for deletion (NOT auto-deleted, review):",
        authority.cache_repo
    );
    if nr > 0 {
        println!("    non-release ({nr}) — not x.y.z; high-confidence pollution to delete:");
        for (img, ts) in &non_release {
            println!("       {img}: {}", ts.join(", "));
        }
    }
    if rf > 0 {
        println!("    release-format ({rf}) — x.y.z but absent upstream; review before deleting:");
        for (img, ts) in &release_fmt {
            println!("       {img}: {}", ts.join(", "));
        }
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

/// Resolve an image's full build timestamp = its config blob's `created` field
/// (RFC3339). Descends one level into a manifest list / OCI index. Best-effort:
/// `None` on any miss. RFC3339 sorts lexically = chronologically, so the raw
/// string is directly comparable for "newest".
async fn resolve_created_ts(
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
        .map(|s| s.to_string())
}

/// Resolve an image's build time as a date only (`YYYY-MM-DD`) for display.
async fn resolve_built(
    client: &DepotClient,
    repo: &str,
    image: &str,
    reference: &str,
) -> Option<String> {
    resolve_created_ts(client, repo, image, reference)
        .await
        .map(|s| s.get(0..10).unwrap_or(&s).to_string())
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
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
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
        compile_groups(&rules, Path::new("."))
            .unwrap()
            .pop()
            .unwrap()
    }

    #[test]
    fn patterns_parse_and_compile() {
        let rules = RulesFile::from_toml(SAMPLE).unwrap();
        assert_eq!(rules.patterns.len(), 5);
        let groups = compile_groups(&rules, Path::new(".")).unwrap();
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
                absent: AbsentDest::MoveTo("docker-release-aux"),
                on_mismatch: MismatchPolicy::Leave,
            }
        );
        assert_eq!(
            decide(&g, "myriad/api_server", "1.2.3"),
            Decision::Reconcile {
                check: "docker-insight",
                absent: AbsentDest::MoveTo("docker-release-aux"),
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
        assert_eq!(
            plan.reconciles[0].absent,
            AbsentAction::MoveTo("docker-release-aux".to_string())
        );
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
        let g = compile_groups(&rules, Path::new("."))
            .unwrap()
            .pop()
            .unwrap();
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
        assert!(errs
            .iter()
            .any(|e| e.contains("docker-prerelease") && e.contains("does not exist")));

        // Missing usage repo is reported.
        let errs = validate_repo_refs(&inv("hosted"), groups, Some("nope"));
        assert!(errs.iter().any(|e| e.contains("usage_repo 'nope'")));
    }

    #[test]
    fn retired_prefixes_parse_and_classify() {
        let toml = r#"
            [[group]]
            format = "docker"
            first_party_prefixes = ["quantum-orchestrator/"]
            retired_prefixes = ["orchestrator/", "quantum_orchestrator/"]
        "#;
        let g = compile_groups(&RulesFile::from_toml(toml).unwrap(), Path::new("."))
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
        assert!(compile_groups(&rules, Path::new(".")).is_err());
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
        let g = compile_groups(&rules, Path::new("."))
            .unwrap()
            .pop()
            .unwrap();
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

    // The classify action routes a released x.y.z tag by explicit per-image list:
    // aux image → move to aux_dest; insight image → reconcile against insight_repo
    // with absent→Flag (never dropped); image on neither list → Triage.
    const CLASSIFY: &str = r#"
        [patterns]
        released = '\d+\.\d+\.\d+'
        [[group]]
        format = "docker"
        source_repos = ["docker-internal"]
        first_party_prefixes = ["myriad/"]
          [[group.rule]]
          match        = "released"
          action       = "classify"
          aux_dest     = "docker-release-aux"
          insight_repo = "docker-insight"
          insight_images = ["myriad/api_internal", "myriad/master"]
          aux_images     = ["myriad/api_internal_debug", "myriad/test"]
          on_mismatch  = "ask"
    "#;

    fn classify_group() -> CompiledGroup {
        compile_groups(&RulesFile::from_toml(CLASSIFY).unwrap(), Path::new("."))
            .unwrap()
            .pop()
            .unwrap()
    }

    #[test]
    fn classify_routes_by_explicit_image_list() {
        let g = classify_group();
        // aux image → plain move to aux_dest (so the self-move guard & grouping
        // reuse the move path).
        assert_eq!(
            decide(&g, "myriad/api_internal_debug", "1.5.0"),
            Decision::Move("docker-release-aux")
        );
        assert_eq!(
            decide(&g, "myriad/test", "1.5.0"),
            Decision::Move("docker-release-aux")
        );
        // insight image → reconcile against insight_repo, absent → Flag (never
        // moved to aux, never auto-dropped).
        assert_eq!(
            decide(&g, "myriad/api_internal", "0.9.5"),
            Decision::Reconcile {
                check: "docker-insight",
                absent: AbsentDest::Flag,
                on_mismatch: MismatchPolicy::Ask,
            }
        );
        // image on NEITHER list → Triage.
        assert_eq!(decide(&g, "myriad/brand_new", "1.0.0"), Decision::Triage);
        // a non-x.y.z tag doesn't match the released rule at all (left for other
        // rules; here there are none → Leave).
        assert_eq!(
            decide(&g, "myriad/api_internal", "develop"),
            Decision::Leave
        );
    }

    #[test]
    fn classify_build_plan_buckets_insight_aux_and_triage() {
        let g = classify_group();
        let inv = vec![
            ("myriad/api_internal".to_string(), "0.9.5".to_string()), // insight → reconcile
            ("myriad/api_internal_debug".to_string(), "1.5.0".to_string()), // aux → move
            ("myriad/brand_new".to_string(), "1.0.0".to_string()),    // neither → triage
        ];
        let plan = build_group_plan(&g, &inv, "docker-internal");
        assert_eq!(plan.moves.len(), 1, "aux image moves");
        assert_eq!(plan.moves[0].dest, "docker-release-aux");
        assert_eq!(plan.reconciles.len(), 1, "insight image reconciles");
        assert_eq!(plan.reconciles[0].absent, AbsentAction::Flag);
        assert_eq!(plan.triage.len(), 1, "unlisted image triaged");
        assert_eq!(plan.triage[0].image, "myriad/brand_new");
    }

    #[test]
    fn classify_rejects_overlap_and_missing_targets() {
        // An image on BOTH lists is a config error.
        let overlap = r#"
            [patterns]
            released = '\d+\.\d+\.\d+'
            [[group]]
            format = "docker"
              [[group.rule]]
              match = "released"
              action = "classify"
              aux_dest = "a"
              insight_repo = "i"
              insight_images = ["myriad/x"]
              aux_images = ["myriad/x"]
        "#;
        let err = match compile_groups(&RulesFile::from_toml(overlap).unwrap(), Path::new(".")) {
            Ok(_) => panic!("expected overlap error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("BOTH"), "{err}");

        // classify without aux_dest/insight_repo is rejected.
        let missing = r#"
            [[group]]
            format = "docker"
              [[group.rule]]
              match = "x"
              action = "classify"
              insight_images = ["a/b"]
        "#;
        assert!(compile_groups(&RulesFile::from_toml(missing).unwrap(), Path::new(".")).is_err());
    }

    #[test]
    fn triage_hint_reads_upstream_evidence() {
        let mk = |present: bool, same: bool| TriageItem {
            tag: TagRef {
                source_repo: "docker-internal".into(),
                image: "myriad/x".into(),
                tag: "1.0.0".into(),
            },
            upstream_present: present,
            upstream_same_digest: same,
            source_built: None,
        };
        assert!(mk(true, true).hint().contains("INSIGHT"));
        assert!(mk(true, false).hint().contains("INSIGHT"));
        assert!(mk(false, false).hint().contains("AUX"));
    }

    // Move-collision resolution: never clobber an existing dest copy; when the
    // dest is empty but several sources collide, keep the newest by build date.
    #[test]
    fn move_collisions_never_clobber_and_keep_newest() {
        let mv = |src: &str, img: &str, tag: &str, dest: &str| PlannedMove {
            source_repo: src.into(),
            image: img.into(),
            tag: tag.into(),
            dest: dest.into(),
            class: "develop".into(),
        };
        let moves = vec![
            // (a) dest already has master:develop -> both sources superseded.
            mv(
                "docker-internal",
                "myriad/master",
                "develop",
                "docker-prerelease",
            ),
            mv(
                "docker-development-local",
                "myriad/master",
                "develop",
                "docker-prerelease",
            ),
            // (b) dest empty for client:develop; two sources -> keep the newer.
            mv(
                "docker-internal",
                "myriad/client",
                "develop",
                "docker-prerelease",
            ),
            mv(
                "docker-development-local",
                "myriad/client",
                "develop",
                "docker-prerelease",
            ),
            // (c) dest empty, single source -> kept as-is.
            mv(
                "docker-internal",
                "myriad/lonely",
                "develop",
                "docker-prerelease",
            ),
        ];
        let mut present = std::collections::HashSet::new();
        present.insert((
            "docker-prerelease".to_string(),
            "myriad/master".to_string(),
            "develop".to_string(),
        ));
        let mut ts: HashMap<(String, String, String), Option<String>> = HashMap::new();
        // client: development-local copy is newer than the internal copy.
        ts.insert(
            (
                "docker-internal".into(),
                "myriad/client".into(),
                "develop".into(),
            ),
            Some("2026-05-01T00:00:00Z".into()),
        );
        ts.insert(
            (
                "docker-development-local".into(),
                "myriad/client".into(),
                "develop".into(),
            ),
            Some("2026-06-20T00:00:00Z".into()),
        );

        let (kept, superseded) = resolve_move_collisions(moves, &present, &ts);

        // Kept: client (from development-local, newer) + lonely. master: none.
        assert_eq!(kept.len(), 2, "kept: {kept:?}");
        assert!(kept
            .iter()
            .any(|m| m.image == "myriad/client" && m.source_repo == "docker-development-local"));
        assert!(kept.iter().any(|m| m.image == "myriad/lonely"));
        assert!(!kept.iter().any(|m| m.image == "myriad/master"));

        // Superseded: 2 master (clobber) + 1 client (older).
        assert_eq!(superseded.len(), 3);
        let master_clobber = superseded
            .iter()
            .filter(|s| s.tag.image == "myriad/master")
            .count();
        assert_eq!(master_clobber, 2);
        assert!(superseded
            .iter()
            .all(|s| s.tag.image != "myriad/master" || s.kept.contains("existing copy")));
        let client_loser = superseded
            .iter()
            .find(|s| s.tag.image == "myriad/client")
            .expect("client loser");
        assert_eq!(client_loser.tag.source_repo, "docker-internal");
        assert!(client_loser
            .kept
            .contains("newer copy in docker-development-local"));
    }
}
