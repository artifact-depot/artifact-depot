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
    /// `reconcile`/`classify`: what to do when the tag is present in the check
    /// repo with a *different* digest. Default (`delete`): the tag joins the
    /// `mismatch` --apply group — deleted from source by `--apply mismatch`
    /// when the check copy is newer or the same build (a NEWER source is always
    /// kept for review). `leave` opts the rule out: every mismatch is parked
    /// for review, regardless of evidence.
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

/// Policy for a reconcile/classify rule when the source and check-repo digests
/// differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MismatchPolicy {
    /// Default: the tag is eligible for the `mismatch` --apply group. The
    /// source copy is deleted by `--apply mismatch` when the check copy is
    /// newer or the same build; a NEWER source is always kept for review.
    #[default]
    Delete,
    /// Never delete: park every mismatch for review, regardless of evidence.
    Leave,
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
/// *different* digest. Carries the evidence the plan shows and the `mismatch`
/// group acts on (delete only when the check copy is newer or the same build).
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

    /// A suggested action from the same evidence. `check_repo` is the canonical,
    /// customer-facing copy and the source is being drained, so unless the
    /// source is genuinely newer we recommend dropping the redundant source.
    fn recommendation(&self) -> &'static str {
        match (self.source_built.as_deref(), self.check_built.as_deref()) {
            (Some(s), Some(c)) if s < c => {
                "recommend: DELETE the source copy — the insight copy is newer"
            }
            (Some(s), Some(c)) if s > c => {
                "recommend: KEEP the source and review — it may be a newer build that never reached insight"
            }
            (Some(_), Some(_)) => {
                "recommend: DELETE the source copy — same build, keep the customer-facing insight copy"
            }
            _ => "recommend: inspect both manifests before deciding",
        }
    }

    /// Whether the `mismatch` group should delete this source copy: only when it
    /// is older than or the same build as the insight copy. A genuinely NEWER
    /// source is never deleted (stays for review), and an explicit
    /// `on_mismatch = "leave"` keeps the tag out of the group entirely.
    fn recommends_delete(&self) -> bool {
        if self.on_mismatch == MismatchPolicy::Leave {
            return false;
        }
        matches!(
            (self.source_built.as_deref(), self.check_built.as_deref()),
            (Some(s), Some(c)) if s <= c
        )
    }
}

/// A planned source-tag deletion together with the reason it's being removed,
/// so the dry-run can explain each delete instead of just listing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteItem {
    pub tag: TagRef,
    pub reason: String,
}

/// The `--apply` group a source-side deletion belongs to. Superseded copies are
/// their own group; every other entry in `deletes` is a redundant drop.
fn delete_group(d: &DeleteItem) -> &'static str {
    if d.reason.starts_with("superseded") {
        GROUP_SUPERSEDED
    } else {
        GROUP_REDUNDANT
    }
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
    /// Per-rule-class breakdown of `already_placed`, for the by-rule report.
    pub already_placed_by_class: HashMap<String, usize>,
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
                *plan
                    .already_placed_by_class
                    .entry(move_class(group, image, tag).to_string())
                    .or_default() += 1;
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
                    kept: "the existing copy already there".to_string(),
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
                    kept: format!("the newer copy in {winner_repo}"),
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
    /// Action groups to execute (see [`ALL_GROUPS`]); `None` => dry run only.
    /// A list containing `"all"` selects every group.
    pub apply: Option<Vec<String>>,
    /// Use `staging/copy` for moves and skip all destructive actions.
    pub copy: bool,
    /// Skip TLS verification (also used for the upstream authority client).
    pub insecure: bool,
    /// Print the full per-tag move list. Default summarizes moves by destination.
    pub verbose: bool,
}

/// The selectable action groups, in report order. Each names one kind of change
/// the reorg can make; `--apply <names>` runs exactly the ones listed.
pub const GROUP_MOVE: &str = "move";
pub const GROUP_REDUNDANT: &str = "redundant";
pub const GROUP_SUPERSEDED: &str = "superseded";
pub const GROUP_MISMATCH: &str = "mismatch";
pub const GROUP_RETIRED: &str = "retired";
pub const GROUP_NON_RELEASED: &str = "non-released";
pub const ALL_GROUPS: &[&str] = &[
    GROUP_MOVE,
    GROUP_REDUNDANT,
    GROUP_SUPERSEDED,
    GROUP_MISMATCH,
    GROUP_RETIRED,
    GROUP_NON_RELEASED,
];

impl ReorgConfig {
    /// True when this is a dry run (no `--apply`).
    fn dry_run(&self) -> bool {
        self.apply.is_none()
    }
    /// Whether the named action group was selected for execution.
    fn wants(&self, group: &str) -> bool {
        self.apply
            .as_ref()
            .is_some_and(|sel| sel.iter().any(|s| s == "all" || s == group))
    }
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
    /// First-party tags in the LOCAL insight cache that the UPSTREAM registry
    /// does NOT serve (cache pollution) and that are non-released (non x.y.z).
    /// The `non-released` group deletes these from the local cache — upstream
    /// lacks them, so they never re-populate. Nothing is deleted upstream.
    non_released: Vec<TagRef>,
    /// Released (x.y.z) first-party tags in the local insight cache that the
    /// UPSTREAM registry doesn't serve — these should be COPIED upstream (the
    /// upstream is missing a released version), never deleted. Review only.
    copy_upstream: Vec<TagRef>,
    /// Why the non-released diff could not be computed (missing UPSTREAM creds /
    /// unreachable upstream), shown in the plan instead of a count.
    non_released_note: Option<String>,
    /// Count of tags already in their correct repo (self-move skipped).
    already_placed: usize,
    /// Per-rule-class breakdown of `already_placed`, for the by-rule report.
    already_placed_by_class: HashMap<String, usize>,
    /// Create dates (config `created`, date only) resolved for the bounded
    /// decision sections (redundant drops, insight-absent), keyed by
    /// `(source_repo, image, tag)`. Looked up by the by-rule report.
    created: HashMap<(String, String, String), String>,
}

impl ResolvedPlan {
    /// The resolved create date for a tag, or `?` if unresolved.
    fn created_date(&self, t: &TagRef) -> String {
        self.created
            .get(&(t.source_repo.clone(), t.image.clone(), t.tag.clone()))
            .cloned()
            .unwrap_or_else(|| "?".to_string())
    }
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
    // Validate the requested --apply groups up front.
    if let Some(sel) = &cfg.apply {
        for g in sel {
            if g != "all" && !ALL_GROUPS.contains(&g.as_str()) {
                anyhow::bail!(
                    "unknown --apply group '{g}'; valid groups: {} (or 'all')",
                    ALL_GROUPS.join(", ")
                );
            }
        }
    }

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

    // The upstream authority ([check_authority]) — consulted DIRECTLY for
    // insight-class released-tag verification, triage evidence, and the
    // non-released diff. Classify rules require it: checking the local cache
    // instead would only reflect what happens to have been pulled through it,
    // and probing the cache would fetch-and-cache misses as a side effect.
    let classify_configured = groups
        .iter()
        .any(|g| g.rules.iter().any(|r| r.action == Action::Classify));
    let authority = match &rules.check_authority {
        Some(a) => match build_authority(a, cfg.insecure).await {
            Ok(auth) => Some(auth),
            Err(e) if classify_configured => {
                return Err(e.context(
                    "classify rules verify released tags against the upstream authority; \
                     it must be reachable",
                ));
            }
            Err(e) => {
                resolved.non_released_note = Some(e.to_string());
                None
            }
        },
        None if classify_configured => bail!(
            "classify rules require a [check_authority] section: released tags are \
             verified against the upstream registry, never the local cache"
        ),
        None => None,
    };

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
            // third-party in a regular source repo is none of our business and is
            // ignored. First-party is handled by build_group_plan below.
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
                }
                // third-party in a regular source repo: ignored (not reported).
            }
            let plan = build_group_plan(group, &inv, repo);
            resolved.already_placed += plan.already_placed;
            for (k, v) in plan.already_placed_by_class {
                *resolved.already_placed_by_class.entry(k).or_default() += v;
            }
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
            // check copy. Two shapes share the machinery:
            //  * MoveTo (`reconcile` action): the check copy lives in a LOCAL
            //    check repo; absent → move to dest (supplementary).
            //  * Flag (`classify` insight image): the check copy is resolved
            //    against the UPSTREAM AUTHORITY directly — never the local
            //    cache (cache contents are a usage record, and probing a cache
            //    repo would fetch-and-cache misses as a side effect). Present +
            //    same digest → delete the redundant source copy; different →
            //    mismatch review; absent → flag, never dropped (it may be the
            //    only copy anywhere).
            for rec in plan.reconciles {
                let check = match &rec.absent {
                    AbsentAction::Flag => {
                        let auth = authority
                            .as_ref()
                            .expect("classify requires [check_authority]; validated at startup");
                        auth.head(&rec.tag.image, &rec.tag.tag)
                            .await?
                            .map(|(repo, digest)| (repo, digest, true))
                    }
                    AbsentAction::MoveTo(_) => {
                        let (status, _, digest) = client
                            .docker_head_manifest(&rec.check_repo, &rec.tag.image, &rec.tag.tag)
                            .await
                            .with_context(|| {
                                format!(
                                    "reconcile check {}/{}:{}",
                                    rec.check_repo, rec.tag.image, rec.tag.tag
                                )
                            })?;
                        if status == 200 {
                            Some((rec.check_repo.clone(), digest, false))
                        } else {
                            None
                        }
                    }
                };
                let Some((check_repo, check_digest, via_authority)) = check else {
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
                                *resolved
                                    .already_placed_by_class
                                    .entry(
                                        move_class(group, &rec.tag.image, &rec.tag.tag).to_string(),
                                    )
                                    .or_default() += 1;
                            }
                        }
                        // Insight image the upstream lacks: don't move it to aux,
                        // and don't drop the only copy — flag for review. Record
                        // its create date for the report.
                        AbsentAction::Flag => {
                            if let Some(c) = resolve_built(
                                client,
                                &rec.tag.source_repo,
                                &rec.tag.image,
                                &rec.tag.tag,
                            )
                            .await
                            {
                                resolved.created.insert(
                                    (
                                        rec.tag.source_repo.clone(),
                                        rec.tag.image.clone(),
                                        rec.tag.tag.clone(),
                                    ),
                                    c,
                                );
                            }
                            resolved.insight_absent.push(rec.tag);
                        }
                    }
                    continue;
                };
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
                    // the check repo itself (it holds the canonical local copy);
                    // only drop copies that live elsewhere.
                    if rec.tag.source_repo == rec.check_repo {
                        resolved.kept.push(rec.tag);
                    } else {
                        let reason = if via_authority {
                            format!(
                                "identical digest already upstream in {check_repo} (redundant duplicate)"
                            )
                        } else {
                            format!(
                                "identical digest already in {check_repo} (redundant duplicate)"
                            )
                        };
                        if let Some(c) = resolve_built(
                            client,
                            &rec.tag.source_repo,
                            &rec.tag.image,
                            &rec.tag.tag,
                        )
                        .await
                        {
                            resolved.created.insert(
                                (
                                    rec.tag.source_repo.clone(),
                                    rec.tag.image.clone(),
                                    rec.tag.tag.clone(),
                                ),
                                c,
                            );
                        }
                        resolved.deletes.push(DeleteItem {
                            tag: rec.tag,
                            reason,
                        });
                    }
                } else {
                    // A real mismatch: gather the build-time evidence so the plan
                    // can explain it and the `mismatch` group can act on it. The
                    // check copy's build date comes from wherever the check copy
                    // lives — the upstream authority or the local check repo.
                    let source_built =
                        resolve_built(client, &rec.tag.source_repo, &rec.tag.image, &rec.tag.tag)
                            .await;
                    let check_built = if via_authority {
                        let auth = authority
                            .as_ref()
                            .expect("classify requires [check_authority]; validated at startup");
                        resolve_built(&auth.client, &check_repo, &rec.tag.image, &rec.tag.tag).await
                    } else {
                        resolve_built(client, &check_repo, &rec.tag.image, &rec.tag.tag).await
                    };
                    resolved.mismatched.push(MismatchInfo {
                        tag: rec.tag,
                        check_repo,
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
            // operator can decide which list each image belongs on. Never
            // touched. Evidence comes from the UPSTREAM AUTHORITY directly:
            // probing the local cache would fetch-and-cache misses (a cache is
            // a record of real pulls, not a probe target).
            for tag in plan.triage {
                let (present, same_digest) = if let Some(auth) = authority.as_ref() {
                    match auth.head(&tag.image, &tag.tag).await? {
                        Some((_, up_digest)) => {
                            let (_, _, src_digest) = client
                                .docker_head_manifest(&tag.source_repo, &tag.image, &tag.tag)
                                .await
                                .unwrap_or_default();
                            (true, !up_digest.is_empty() && up_digest == src_digest)
                        }
                        None => (false, false),
                    }
                } else {
                    (false, false)
                };
                resolved.needs_classification.push(TriageItem {
                    tag,
                    upstream_present: present,
                    upstream_same_digest: same_digest,
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
        // Build (create) dates for every colliding tag — both the source copy and
        // the kept copy — so the report compares the two by CREATE date (the
        // meaningful signal; last-pulled is not). Resolved only for tags actually
        // in a collision (bounded), not the whole move set.
        let mut ts: HashMap<(String, String, String), Option<String>> = HashMap::new();
        for m in &resolved.moves {
            let key = (m.dest.clone(), m.image.clone(), m.tag.clone());
            let collides =
                present.contains(&key) || srcs_per_key.get(&key).copied().unwrap_or(0) > 1;
            if !collides {
                continue;
            }
            let v = resolve_created_ts(client, &m.source_repo, &m.image, &m.tag).await;
            ts.insert((m.source_repo.clone(), m.image.clone(), m.tag.clone()), v);
        }
        // The kept dest copy of each no-clobber collision (one per `present` key).
        for (dest, image, tag) in &present {
            let v = resolve_created_ts(client, dest, image, tag).await;
            ts.insert((dest.clone(), image.clone(), tag.clone()), v);
        }
        let moves = std::mem::take(&mut resolved.moves);
        let (kept, superseded) = resolve_move_collisions(moves, &present, &ts);
        resolved.moves = kept;
        // A superseded copy — an older/duplicate copy whose winning copy is kept
        // elsewhere — is deleted on --apply. The one exception: if we can PROVE
        // the source copy is NEWER than the copy that was kept, we refuse to drop
        // it automatically (that would discard a newer build) and leave it flagged
        // for review instead. Record each deletable copy's build date so the
        // REMOVED listing can still show the age evidence.
        for s in superseded {
            let source_is_newer = matches!(
                (&s.own_built, &s.winner_built),
                (Some(o), Some(w)) if o > w
            );
            if source_is_newer {
                resolved.superseded.push(s);
            } else {
                resolved.created.insert(
                    (
                        s.tag.source_repo.clone(),
                        s.tag.image.clone(),
                        s.tag.tag.clone(),
                    ),
                    // Store the date only (drop the time), matching how the
                    // other REMOVED entries print their create date.
                    s.own_built
                        .as_deref()
                        .map(|d| d.split('T').next().unwrap_or(d).to_string())
                        .unwrap_or_else(|| "?".to_string()),
                );
                resolved.deletes.push(DeleteItem {
                    tag: s.tag,
                    reason: "superseded (a newer copy is kept elsewhere)".to_string(),
                });
            }
        }
    }

    // Last-accessed (usage) data from the configured usage repo, used ONLY by the
    // destination-retention `max_unaccessed` check (that policy is defined by
    // last-pull). Not shown per-tag anywhere — plan listings use create dates.
    // One browse per distinct MOVED image (the only tags retention examines).
    let mut atimes: HashMap<(String, String), String> = HashMap::new();
    if let Some(usage_repo) = rules.usage_repo.as_deref() {
        let mut images: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for m in &resolved.moves {
            images.insert(m.image.clone());
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
    // lets the AUTOMATIC zone explain *why* each destination's move count lands.
    let mut dest_classes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for g in &rules.group {
        for r in &g.rules {
            let class = class_label(&r.match_).to_string();
            // `move`/`reconcile` route via `dest`; `classify` routes aux images
            // via `aux_dest` — annotate that destination with this class too.
            if let Some(d) = r.dest.as_ref().or(r.aux_dest.as_ref()) {
                dest_classes.entry(d.clone()).or_default().push(class);
            }
        }
    }

    // Enumerate the check/insight repos' current contents (used both by the
    // dry-run reference and by the `non-released` group), and compute the
    // non-released (non x.y.z) first-party tags they serve — the `non-released`
    // group's targets. Done BEFORE print_plan so the overview counts it.
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
    let released_re = compile_match(&rules.patterns, "released").ok();
    // The `non-released` group is the LOCAL-cache-minus-UPSTREAM diff: tags the
    // local insight cache serves that the upstream registry does not. Non-x.y.z
    // ones are pollution to delete from the local cache; x.y.z ones are releases
    // the upstream is MISSING and should be copied up (review). Needs the
    // upstream (UPSTREAM_USERNAME / UPSTREAM_PASSWORD) to compute.
    match (&authority, &released_re) {
        (Some(auth), Some(re)) => {
            let is_fp = |image: &str| prefixes.iter().any(|pre| image.starts_with(pre.as_str()));
            for (repo, tags) in &cache_contents {
                let local: Vec<(String, String)> = tags
                    .iter()
                    .filter(|t| is_fp(&t.image))
                    .map(|t| (t.image.clone(), t.tag.clone()))
                    .collect();
                for (image, ts) in cache_pollution(&local, &auth.tag_set) {
                    for tag in ts {
                        let t = TagRef {
                            source_repo: repo.clone(),
                            image: image.clone(),
                            tag,
                        };
                        if re.is_match(&t.tag) {
                            resolved.copy_upstream.push(t);
                        } else {
                            resolved.non_released.push(t);
                        }
                    }
                }
            }
        }
        (None, _) => {
            // Keep a build-failure note from startup if one was recorded.
            if resolved.non_released_note.is_none() {
                resolved.non_released_note =
                    Some("no [check_authority] upstream configured".to_string());
            }
        }
        (_, None) => {
            resolved.non_released_note = Some("no `released` pattern in rules".to_string())
        }
    }

    print_plan(&resolved, cfg.copy, cfg.verbose, &dest_classes);

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

    if cfg.dry_run() {
        // Retention safety: warn if a destination's cleanup policy would expire
        // the tags we're about to move there (a re-file silently becoming a delete).
        print_destination_retention(client, &resolved, &atimes).await?;

        // Verbose appendix (large; the per-tag move list) at the very end, so
        // the plan up top is never buried under thousands of move lines.
        if cfg.verbose {
            print_verbose_moves(&resolved);
        }

        // Close with the current repo snapshot and a repeat of the one-screen
        // summary, so they're the last thing read after the long listings.
        if let Some(b) = &before_summary {
            print_repo_summary(b, None);
        }
        println!("\n================ Reorg plan — summary ================");
        print_summary(&resolved, cfg.copy);

        println!("\nDry run — no changes made. Re-run with --apply <groups> to execute.");
        return Ok(());
    }

    // ----------------------------------------------------------------------
    // Execute only the selected action groups (no prompts — the plan named each
    // group and `--apply` listed the ones to run).
    // ----------------------------------------------------------------------
    let mut ok = 0usize;
    let mut failed = 0usize;
    let verb = if cfg.copy { "copy" } else { "move" };
    println!(
        "\nApplying groups: {} — no prompts...",
        cfg.apply.as_ref().map(|s| s.join(", ")).unwrap_or_default()
    );

    // --- move ---
    if cfg.wants(GROUP_MOVE) {
        println!("\n[{}] {} tag(s)...", verb, resolved.moves.len());
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
    }

    // --- source-side deletes (redundant / superseded / mismatch / retired) ---
    // Never under --copy (non-destructive).
    if !cfg.copy {
        // Hard guard: NEVER delete out of a cache/check/insight repo.
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

        // Gather the tags to delete from every selected source-delete group.
        let mut to_delete: Vec<&TagRef> = Vec::new();
        for d in &resolved.deletes {
            if cfg.wants(delete_group(d)) {
                to_delete.push(&d.tag);
            }
        }
        if cfg.wants(GROUP_MISMATCH) {
            for m in resolved.mismatched.iter().filter(|m| m.recommends_delete()) {
                to_delete.push(&m.tag);
            }
        }
        if cfg.wants(GROUP_RETIRED) {
            for t in &resolved.prefix_deletes {
                to_delete.push(t);
            }
        }
        if !to_delete.is_empty() {
            println!("\n[delete] {} tag(s) from source...", to_delete.len());
            for d in to_delete {
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

        // --- non-released: delete LOCAL insight-cache pollution. These tags are
        // absent from the upstream registry (that diff is how they were found),
        // so deleting the cache copy is permanent and they never re-populate.
        // Nothing is ever deleted upstream. This intentionally targets the cache
        // repo, so it does NOT go through the protected-repo guard above.
        if cfg.wants(GROUP_NON_RELEASED) {
            if let Some(note) = &resolved.non_released_note {
                eprintln!("  [non-released] cannot run — {note}");
            } else if !resolved.non_released.is_empty() {
                println!(
                    "\n[non-released] deleting {} tag(s) from the local insight cache (absent upstream)...",
                    resolved.non_released.len()
                );
                for t in &resolved.non_released {
                    let r = client
                        .staging_delete(&t.source_repo, &t.image, &t.tag)
                        .await;
                    report(
                        &mut ok,
                        &mut failed,
                        "delete",
                        &t.source_repo,
                        &t.image,
                        &t.tag,
                        r,
                    );
                }
            }
        }
    } else {
        println!("\n--copy is non-destructive: skipping all delete groups.");
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
/// Per-group counts of what `--apply <group>` would change.
fn group_counts(p: &ResolvedPlan) -> [(&'static str, usize, &'static str); 6] {
    let redundant = p
        .deletes
        .iter()
        .filter(|d| delete_group(d) == GROUP_REDUNDANT)
        .count();
    let superseded = p
        .deletes
        .iter()
        .filter(|d| delete_group(d) == GROUP_SUPERSEDED)
        .count();
    let mismatch = p
        .mismatched
        .iter()
        .filter(|m| m.recommends_delete())
        .count();
    [
        (
            GROUP_MOVE,
            p.moves.len(),
            "re-home tags to their correct repository",
        ),
        (
            GROUP_REDUNDANT,
            redundant,
            "delete source copies identical to the insight copy",
        ),
        (
            GROUP_SUPERSEDED,
            superseded,
            "delete older source copies (a newer copy is kept)",
        ),
        (
            GROUP_MISMATCH,
            mismatch,
            "delete source copies that differ from insight (insight authoritative)",
        ),
        (
            GROUP_RETIRED,
            p.prefix_deletes.len(),
            "delete tags under retired namespaces",
        ),
        (
            GROUP_NON_RELEASED,
            p.non_released.len(),
            "delete non-x.y.z tags the upstream doesn't serve from the LOCAL insight cache",
        ),
    ]
}

/// Count of tags left for the operator to review — NOT touched by any `--apply`
/// group. (Mismatches whose source is newer, superseded-but-source-newer,
/// restore candidates, unclassified, unmatched, and already-correct-here.)
fn for_you_total(p: &ResolvedPlan) -> usize {
    let mismatch_review = p
        .mismatched
        .iter()
        .filter(|m| !m.recommends_delete())
        .count();
    mismatch_review
        + p.insight_absent.len()
        + p.superseded.len()
        + p.copy_upstream.len()
        + p.needs_classification.len()
        + p.leaves.len()
        + p.kept.len()
}

/// TL;DR up top: the named action groups (each independently selectable via
/// `--apply`), then what's left for the operator to review.
fn print_summary(p: &ResolvedPlan, copy: bool) {
    println!("Nothing is changed until you re-run with --apply. Each line below is one");
    println!("independent group — pass the ones you want (comma-separated), or 'all':");
    println!();
    // Lead every line with the literal command so it's obvious what to run.
    for (name, n, desc) in group_counts(p) {
        let cmd = format!("--apply {name}");
        println!("    {cmd:<22} {n:>6} tag(s)   {desc}");
    }
    if let Some(note) = &p.non_released_note {
        println!("    (non-released count unavailable — {note})");
    }
    println!();
    println!("    e.g.   --apply move,redundant        --apply all");
    if copy {
        println!("    (--copy makes 'move' a non-destructive copy and skips every delete group)");
    }
    let mismatch_review = p
        .mismatched
        .iter()
        .filter(|m| !m.recommends_delete())
        .count();
    println!(
        "\nNOT touched by any group (review only): {} — {} mismatch(source newer) · \
         {} superseded(source newer) · {} copy-upstream · {} restore-candidate · \
         {} unclassified · {} unmatched",
        for_you_total(p),
        mismatch_review,
        p.superseded.len(),
        p.copy_upstream.len(),
        p.insight_absent.len(),
        p.needs_classification.len(),
        p.leaves.len(),
    );
    println!(
        "Already in the right place (no change needed): {}",
        p.already_placed
    );
}

/// Print one itemized `repo/image:tag (created date)` line per delete, sorted.
fn print_delete_listing(p: &ResolvedPlan, items: &[&DeleteItem]) {
    let mut lines: Vec<String> = items
        .iter()
        .map(|d| {
            format!(
                "     {}/{}:{} (created {})",
                d.tag.source_repo,
                d.tag.image,
                d.tag.tag,
                p.created_date(&d.tag)
            )
        })
        .collect();
    lines.sort();
    for l in lines {
        println!("{l}");
    }
}

/// One `▸ \`--apply <group>\`` section per action group, each with its full
/// listing directly underneath — nothing about a group lives anywhere else.
/// Where to find the full per-tag move list, depending on whether the caller
/// already asked for it.
fn move_list_hint(verbose: bool) -> &'static str {
    if verbose {
        "(full per-tag list: Detail section at the end of this dry run)"
    } else {
        "(full per-tag list: --verbose)"
    }
}

fn print_group_sections(
    p: &ResolvedPlan,
    copy: bool,
    verbose: bool,
    dest_classes: &BTreeMap<String, Vec<String>>,
) {
    let counts = group_counts(p);
    let desc_of = |g: &str| {
        counts
            .iter()
            .find(|(n, _, _)| *n == g)
            .map(|(_, _, d)| *d)
            .unwrap_or("")
    };

    // ▸ --apply move
    let verb = if copy { "copied" } else { "moved" };
    println!(
        "\n▸ `--apply move` — {} — {} to be {verb}",
        desc_of(GROUP_MOVE),
        p.moves.len()
    );
    if !p.moves.is_empty() {
        let mut by_dest: BTreeMap<&str, usize> = BTreeMap::new();
        for m in &p.moves {
            *by_dest.entry(m.dest.as_str()).or_default() += 1;
        }
        for (dest, n) in &by_dest {
            let rules = dest_classes
                .get(*dest)
                .map(|cs| cs.join(", "))
                .filter(|s| !s.is_empty())
                .map(|s| format!("   ({s})"))
                .unwrap_or_default();
            println!("     → {dest:<28} {n:>7}{rules}");
        }
        let mut by_src: BTreeMap<&str, usize> = BTreeMap::new();
        for m in &p.moves {
            *by_src.entry(m.source_repo.as_str()).or_default() += 1;
        }
        let parts: Vec<String> = by_src.iter().map(|(r, n)| format!("{r} {n}")).collect();
        println!("     from: {}", parts.join(" · "));
        println!("     {}", move_list_hint(verbose));
    }

    // ▸ --apply redundant / --apply superseded — split the delete list.
    let redundant: Vec<&DeleteItem> = p
        .deletes
        .iter()
        .filter(|d| delete_group(d) == GROUP_REDUNDANT)
        .collect();
    let superseded: Vec<&DeleteItem> = p
        .deletes
        .iter()
        .filter(|d| delete_group(d) == GROUP_SUPERSEDED)
        .collect();
    println!(
        "\n▸ `--apply redundant` — {} — {} to be deleted",
        desc_of(GROUP_REDUNDANT),
        redundant.len()
    );
    print_delete_listing(p, &redundant);

    println!(
        "\n▸ `--apply superseded` — {} — {} to be deleted",
        desc_of(GROUP_SUPERSEDED),
        superseded.len()
    );
    print_delete_listing(p, &superseded);
    if !p.superseded.is_empty() {
        let created = |o: &Option<String>| {
            o.as_deref()
                .map(|s| s.get(0..10).unwrap_or(s).to_string())
                .unwrap_or_else(|| "?".to_string())
        };
        println!(
            "     plus {} kept for REVIEW — the source copy is NEWER than the kept copy:",
            p.superseded.len()
        );
        for s in &p.superseded {
            println!(
                "     {}/{}:{} (created {}) — kept {} (created {})",
                s.tag.source_repo,
                s.tag.image,
                s.tag.tag,
                created(&s.own_built),
                s.kept,
                created(&s.winner_built),
            );
        }
    }

    // ▸ --apply mismatch
    let del = p
        .mismatched
        .iter()
        .filter(|m| m.recommends_delete())
        .count();
    let review = p.mismatched.len() - del;
    println!(
        "\n▸ `--apply mismatch` — {} — {} to be deleted from source; {} kept for review",
        desc_of(GROUP_MISMATCH),
        del,
        review
    );
    for m in &p.mismatched {
        let action = if m.recommends_delete() {
            "DELETE on --apply mismatch"
        } else {
            "KEPT for review"
        };
        println!(
            "     {}/{}:{}  [{action}]",
            m.tag.source_repo, m.tag.image, m.tag.tag
        );
        print_mismatch_evidence(m);
    }

    // ▸ --apply retired
    println!(
        "\n▸ `--apply retired` — {} — {} to be deleted from source",
        desc_of(GROUP_RETIRED),
        p.prefix_deletes.len()
    );
    let mut by_repo: BTreeMap<&str, Vec<&TagRef>> = BTreeMap::new();
    for t in &p.prefix_deletes {
        by_repo.entry(t.source_repo.as_str()).or_default().push(t);
    }
    for (repo, tags) in &by_repo {
        println!("     in {repo}:");
        for (img, ts) in tags_by_image(tags) {
            println!("        {img}: {}", ts.join(", "));
        }
    }

    // ▸ --apply non-released — the local-cache-minus-upstream diff. Deletes are
    // LOCAL (cache pollution the upstream never serves); x.y.z tags missing
    // upstream are the opposite problem and are listed for copy-up review.
    println!(
        "\n▸ `--apply non-released` — {} — {} to be deleted from the local cache",
        desc_of(GROUP_NON_RELEASED),
        p.non_released.len()
    );
    if let Some(note) = &p.non_released_note {
        println!("     unavailable — {note}");
    } else {
        let refs: Vec<&TagRef> = p.non_released.iter().collect();
        for (img, ts) in tags_by_image(&refs) {
            println!("     {img}: {}", ts.join(", "));
        }
        if !p.copy_upstream.is_empty() {
            println!(
                "     plus {} released x.y.z tag(s) the UPSTREAM is missing → copy them upstream (review, never deleted):",
                p.copy_upstream.len()
            );
            let refs: Vec<&TagRef> = p.copy_upstream.iter().collect();
            for (img, ts) in tags_by_image(&refs) {
                println!("        {img}: {}", ts.join(", "));
            }
        }
    }
}

/// Review-only leftovers no `--apply` group touches: restore candidates,
/// unclassified images, unmatched tags. Small by design — everything actionable
/// lives in a group section above.
fn print_review_only(p: &ResolvedPlan) {
    if p.insight_absent.is_empty()
        && p.purges.is_empty()
        && p.needs_classification.is_empty()
        && p.leaves.is_empty()
        && p.kept.is_empty()
    {
        return;
    }
    println!("\n▸ Review only — no --apply group touches these:");

    // Restore-to-insight candidates — insight-classified, absent upstream.
    if !p.insight_absent.is_empty() {
        println!(
            "\n  Restore-to-insight candidates ({})  → push to insight, or delete as stale (may have been pruned)",
            p.insight_absent.len()
        );
        let mut by_repo: BTreeMap<&str, Vec<&TagRef>> = BTreeMap::new();
        for t in &p.insight_absent {
            by_repo.entry(t.source_repo.as_str()).or_default().push(t);
        }
        for (repo, tags) in &by_repo {
            println!("     in {repo}:");
            let mut by_img: BTreeMap<&str, Vec<String>> = BTreeMap::new();
            for t in tags {
                by_img.entry(t.image.as_str()).or_default().push(format!(
                    "{} (created {})",
                    t.tag,
                    p.created_date(t)
                ));
            }
            for (img, ts) in &by_img {
                println!("        {img}: {}", ts.join(", "));
            }
        }
    }

    // Purge-repo remainder — third-party in a dead cache.
    if !p.purges.is_empty() {
        let mut by_repo: BTreeMap<&str, usize> = BTreeMap::new();
        for t in &p.purges {
            *by_repo.entry(t.source_repo.as_str()).or_default() += 1;
        }
        let parts: Vec<String> = by_repo.iter().map(|(r, n)| format!("{r} ({n})")).collect();
        println!(
            "\n  Purge-repo remainder ({})  → dead cache; first-party already re-homed, delete the repo wholesale: {}",
            p.purges.len(),
            parts.join(", ")
        );
    }

    // Needs classification — released image on neither list.
    if !p.needs_classification.is_empty() {
        println!(
            "\n  Needs classification ({})  → add each image to insight_images or aux_images, then re-run",
            p.needs_classification.len()
        );
        let mut by_image: BTreeMap<&str, (Vec<&str>, &TriageItem)> = BTreeMap::new();
        for t in &p.needs_classification {
            let e = by_image
                .entry(t.tag.image.as_str())
                .or_insert_with(|| (Vec::new(), t));
            e.0.push(t.tag.tag.as_str());
        }
        for (image, (mut tags, sample)) in by_image {
            tags.sort();
            println!("     {image} — {} — {}", sample.hint(), tags.join(", "));
        }
    }

    // Unmatched — no rule matched (only when there's no catch-all).
    if !p.leaves.is_empty() {
        println!(
            "\n  Unmatched ({})  → no rule matched; add a rule or leave in place",
            p.leaves.len()
        );
        let refs: Vec<&TagRef> = p.leaves.iter().collect();
        for (img, ts) in tags_by_image(&refs) {
            println!("     {img}: {}", ts.join(", "));
        }
    }

    // Kept — reconcile redundant whose source IS the check repo (never dropped).
    if !p.kept.is_empty() {
        println!(
            "\n  Kept ({})  → already present in the check repo, left in place (informational)",
            p.kept.len()
        );
    }
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

fn print_plan(
    p: &ResolvedPlan,
    copy: bool,
    verbose: bool,
    dest_classes: &BTreeMap<String, Vec<String>>,
) {
    // Summary up top, then one ▸ section per --apply group (each with its full
    // listing directly underneath), then the review-only leftovers. The summary
    // repeats at the very end of the dry run, after the reference sections.
    println!("================ Reorg plan ================");
    print_summary(p, copy);
    print_group_sections(p, copy, verbose, dest_classes);
    print_review_only(p);
}

/// Group a set of tag refs by image, sorted tags per image.
fn tags_by_image(tags: &[&TagRef]) -> BTreeMap<String, Vec<String>> {
    let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in tags {
        m.entry(t.image.clone()).or_default().push(t.tag.clone());
    }
    for v in m.values_mut() {
        v.sort();
    }
    m
}

/// Verbose appendix: the full per-tag list of the automatic MOVES, grouped by
/// destination. Emitted only with --verbose — it's large (thousands of tags),
/// and being automatic and non-destructive it needs no review, so it sorts to
/// the very bottom. Removals are NOT here: deletes are itemized unconditionally
/// in the ① AUTOMATIC zone (see print_automatic), so they're always visible.
fn print_verbose_moves(p: &ResolvedPlan) {
    if p.moves.is_empty() {
        return;
    }
    println!("\n================ Detail (--verbose) — every automatic move ================");
    let mut by_dest: BTreeMap<&str, Vec<&PlannedMove>> = BTreeMap::new();
    for m in &p.moves {
        by_dest.entry(m.dest.as_str()).or_default().push(m);
    }
    for (dest, ms) in &by_dest {
        println!("\n→ {dest}  ({} tag(s)):", ms.len());
        let mut lines: Vec<String> = ms
            .iter()
            .map(|m| format!("     {}/{}:{}", m.source_repo, m.image, m.tag))
            .collect();
        lines.sort();
        for l in lines {
            println!("{l}");
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

/// Live handle to the authoritative upstream registry from `[check_authority]`:
/// a client, the configured repos, and their full `image:tag` inventory. Built
/// once per run. Every consult goes to the upstream directly — never through a
/// local cache repo: cache contents are a record of what the team actually
/// pulls, and a cache-miss HEAD would fetch-and-cache as a side effect.
struct UpstreamAuthority {
    url: String,
    client: DepotClient,
    repos: Vec<String>,
    /// `image:tag` keys of every tag the upstream serves across `repos`.
    tag_set: std::collections::HashSet<String>,
}

impl UpstreamAuthority {
    /// HEAD `image:tag` across the configured upstream repos; the first 200
    /// wins. Returns the serving repo and its manifest digest, or `None` when
    /// no upstream repo serves the tag.
    async fn head(&self, image: &str, tag: &str) -> Result<Option<(String, String)>> {
        for repo in &self.repos {
            let (status, _, digest) = self
                .client
                .docker_head_manifest(repo, image, tag)
                .await
                .with_context(|| {
                    format!("authority check {repo}/{image}:{tag} via {}", self.url)
                })?;
            if status == 200 {
                return Ok(Some((repo.clone(), digest)));
            }
        }
        Ok(None)
    }
}

/// Build the [`UpstreamAuthority`]. Requires `UPSTREAM_USERNAME` /
/// `UPSTREAM_PASSWORD` — verification against the authority is meaningless
/// without the upstream, so a missing credential is an error the caller
/// reports, not a silent skip.
async fn build_authority(authority: &CheckAuthority, insecure: bool) -> Result<UpstreamAuthority> {
    let (Some(user), Some(pass)) = (
        std::env::var("UPSTREAM_USERNAME").ok(),
        std::env::var("UPSTREAM_PASSWORD").ok(),
    ) else {
        anyhow::bail!(
            "set UPSTREAM_USERNAME / UPSTREAM_PASSWORD to consult the upstream authority {}",
            authority.upstream_url
        );
    };
    let client = DepotClient::new(&authority.upstream_url, &user, &pass, insecure)
        .with_context(|| format!("build upstream client for {}", authority.upstream_url))?;
    let mut tag_set: std::collections::HashSet<String> = Default::default();
    for repo in &authority.upstream_repos {
        let tags = list_repo_tags(&client, repo).await.with_context(|| {
            format!(
                "upstream repo '{repo}' unreachable via {}",
                authority.upstream_url
            )
        })?;
        tag_set.extend(tags.into_iter().map(|(i, t)| format!("{i}:{t}")));
    }
    Ok(UpstreamAuthority {
        url: authority.upstream_url.clone(),
        client,
        repos: authority.upstream_repos.clone(),
        tag_set,
    })
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
/// plus a non-authoritative assessment and recommendation, shown in the plan.
fn print_mismatch_evidence(m: &MismatchInfo) {
    println!(
        "            source ({}): created {}  [{}]",
        m.tag.source_repo,
        m.source_built.as_deref().unwrap_or("?"),
        short(&m.source_digest)
    );
    println!(
        "            check  ({}): created {}  [{}]",
        m.check_repo,
        m.check_built.as_deref().unwrap_or("?"),
        short(&m.check_digest)
    );
    println!("            -> {}", m.assessment());
    println!("            -> {}", m.recommendation());
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

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [patterns]
        released   = '\d+\.\d+\.\d+(-(linux|windows|darwin)_[a-z0-9_]+)?'
        prerelease = '\d+\.\d+\.\d+-(dev|rc)\.\d+(-(linux|windows|darwin)_[a-z0-9_]+)?'
        develop    = 'develop(-(linux|windows|darwin)_[a-z0-9_]+)?'
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
    fn released_regex_flags_non_xyz_tags() {
        // The `non-released` group uses the compiled `released` pattern to spot
        // non x.y.z tags an insight/release cache shouldn't serve.
        let rules = RulesFile::from_toml(SAMPLE).unwrap();
        let re = compile_match(&rules.patterns, "released").expect("released compiles");
        assert!(re.is_match("1.5.0"), "plain x.y.z is released");
        assert!(
            re.is_match("1.5.0-linux_amd64"),
            "platform-suffixed is released"
        );
        assert!(
            !re.is_match("1.4.0-dev.117"),
            "dev prerelease is NOT released"
        );
        assert!(
            !re.is_match("1.6.0-dev.242"),
            "dev prerelease is NOT released"
        );
        assert!(!re.is_match("develop"), "develop is NOT released");
        assert!(
            !re.is_match("1.0.0-alain-20260520T151747Z"),
            "one-off tag is NOT released"
        );
    }

    #[test]
    fn arch_suffixed_tags_route_like_their_base() {
        let g = sample_group();
        // A platform component tag routes exactly like its base tag, so a
        // multi-arch family stays together instead of falling to the catch-all.
        assert_eq!(
            decide(&g, "myriad/dev", "1.4.0-dev.80-linux_amd64"),
            Decision::Move("docker-prerelease")
        );
        assert_eq!(
            decide(&g, "myriad/dev", "1.4.0-dev.80-linux_arm64_v8"),
            Decision::Move("docker-prerelease")
        );
        assert_eq!(
            decide(&g, "qkp/leaf", "develop-linux_amd64"),
            Decision::Move("docker-prerelease")
        );
        // released x.y.z component → same reconcile as the bare release.
        assert_eq!(
            decide(&g, "myriad/api_server", "1.5.0-linux_amd64"),
            Decision::Reconcile {
                check: "docker-insight",
                absent: AbsentDest::MoveTo("docker-release-aux"),
                on_mismatch: MismatchPolicy::Delete,
            }
        );
        // The bare index tag still routes as before.
        assert_eq!(
            decide(&g, "myriad/dev", "1.4.0-dev.80"),
            Decision::Move("docker-prerelease")
        );
        // A non-platform suffix is NOT swallowed (still catch-all).
        assert_eq!(
            decide(&g, "myriad/dev", "1.4.0-dev.80-canal13"),
            Decision::Move("docker-development-local")
        );
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
                on_mismatch: MismatchPolicy::Delete,
            }
        );
        assert_eq!(
            decide(&g, "myriad/api_server", "1.2.3"),
            Decision::Reconcile {
                check: "docker-insight",
                absent: AbsentDest::MoveTo("docker-release-aux"),
                on_mismatch: MismatchPolicy::Delete,
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
            first_party_prefixes = ["acme-orchestrator/"]
            retired_prefixes = ["orchestrator/", "acme_orchestrator/"]
        "#;
        let g = compile_groups(&RulesFile::from_toml(toml).unwrap(), Path::new("."))
            .unwrap()
            .pop()
            .unwrap();
        // Retired namespaces match; the current (dash) one does not.
        assert!(g.is_retired("orchestrator/orchestrator"));
        assert!(g.is_retired("acme_orchestrator/orchestrator"));
        assert!(!g.is_retired("acme-orchestrator/orchestrator"));
        // The current name is first-party, not retired (disjoint).
        assert!(g.is_first_party("acme-orchestrator/orchestrator"));
        assert!(!g.is_first_party("orchestrator/orchestrator"));
    }

    #[test]
    fn check_authority_parses() {
        let toml = r#"
            [[group]]
            format = "docker"
            [check_authority]
            cache_repo = "docker-insight"
            upstream_url = "https://repository.example.com:8081"
            upstream_repos = ["docker-external", "docker-release"]
        "#;
        let r = RulesFile::from_toml(toml).unwrap();
        let a = r.check_authority.expect("check_authority present");
        assert_eq!(a.cache_repo, "docker-insight");
        assert_eq!(a.upstream_url, "https://repository.example.com:8081");
        assert_eq!(a.upstream_repos, vec!["docker-external", "docker-release"]);
    }

    #[test]
    fn cache_pollution_flags_only_tags_absent_upstream() {
        let local = vec![
            ("myriad/master".to_string(), "1.5.2".to_string()),
            ("myriad/master".to_string(), "1.6.0-dev.168".to_string()),
            ("myriad/master".to_string(), "1.4.0-dev.117".to_string()),
            (
                "acme-orchestrator/orchestrator".to_string(),
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
            p.get("acme-orchestrator/orchestrator").unwrap(),
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
    fn on_mismatch_defaults_to_delete_and_parses_leave() {
        // SAMPLE's reconcile rule omits on_mismatch → Delete (mismatch group).
        let g = sample_group();
        match decide(&g, "myriad/api_server", "1.2.3") {
            Decision::Reconcile { on_mismatch, .. } => {
                assert_eq!(on_mismatch, MismatchPolicy::Delete)
            }
            d => panic!("expected reconcile, got {d:?}"),
        }
        // Explicit on_mismatch = "leave" parses and flows through to the decision.
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
              on_mismatch = "leave"
        "#;
        let rules = RulesFile::from_toml(toml).unwrap();
        let g = compile_groups(&rules, Path::new("."))
            .unwrap()
            .pop()
            .unwrap();
        match decide(&g, "app/x", "1.2.3") {
            Decision::Reconcile { on_mismatch, .. } => {
                assert_eq!(on_mismatch, MismatchPolicy::Leave)
            }
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
            on_mismatch: MismatchPolicy::Delete,
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
                on_mismatch: MismatchPolicy::Delete,
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

    #[test]
    fn move_list_hint_matches_verbosity() {
        assert_eq!(move_list_hint(false), "(full per-tag list: --verbose)");
        assert!(
            move_list_hint(true).contains("Detail section"),
            "with --verbose already on, don't tell the user to pass --verbose"
        );
    }
}
