// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Server-side copy / move of a Helm chart version between repositories.
//!
//! Unlike Docker (a tag fans out into a manifest closure of many records), a
//! Helm chart version is a **single** artifact record — `{name}-{version}.tgz`
//! at the repo root — pointing at one content-addressable blob. So promotion
//! is one [`CopyTarget::copy_record`] call: verbatim in a shared store, or a
//! blob-restream across stores.
//!
//! The one Helm-specific concern is the stored `index.yaml`. Both the source
//! and destination repositories cache their index and rebuild it lazily when a
//! stale marker is present (see `store::set_stale_flag`). A move changes the
//! chart set of *both* repos, so both indexes must be invalidated: the source
//! loses the chart, the destination gains it. We set the flag on both after a
//! successful copy/delete; propagation to parent group/proxy repos is the
//! caller's (API layer's) responsibility, exactly as for uploads.

use depot_core::error::{self, DepotError};
use depot_core::service;
use depot_core::service::promote::CopyTarget;
use depot_core::store::kv::{ArtifactKind, KvStore};
use depot_core::update::UpdateSender;

use crate::store::{chart_path, set_stale_flag};

/// Outcome of a [`move_chart`] operation.
#[derive(Debug, Clone)]
pub struct MoveOutcome {
    /// The chart record path that was copied (`{name}-{version}.tgz`).
    pub chart_path: String,
    /// Number of artifact records written into the destination repo (0 or 1).
    pub copied_records: u64,
    /// Whether the source record was removed (a move rather than a copy).
    pub source_deleted: bool,
}

/// Copy — and optionally move — a single Helm chart version `name:version`
/// between the repositories described by `t`.
///
/// The chart's `.tgz` record is copied into the destination repo, preserving
/// `created_at` / `last_accessed_at` so the destination's retention policy is
/// applied against the chart's true age. Same-store copies move no bytes;
/// cross-store copies stream the blob into the destination store (and
/// de-duplicate there). On success the stored `index.yaml` of the destination
/// (and, for a move, the source) is invalidated so it rebuilds on next read.
///
/// When `delete_source` is `true` this is a *move*: the source record is
/// removed after the copy succeeds (copy-strictly-before-delete keeps the
/// shared blob referenced throughout; store-wide blob GC's two-pass grace
/// covers the window).
///
/// `overwrite` governs the destination collision: if the chart already exists
/// in the destination and `overwrite` is false, returns [`DepotError::Conflict`].
///
/// # Errors
/// - [`DepotError::NotFound`] if the source chart does not exist.
/// - [`DepotError::Conflict`] if the destination chart exists and `!overwrite`.
/// - [`DepotError::DataIntegrity`] if the source record is not a Helm chart.
pub async fn move_chart(
    t: &CopyTarget<'_>,
    name: &str,
    version: &str,
    delete_source: bool,
    overwrite: bool,
) -> error::Result<MoveOutcome> {
    let path = chart_path(name, version);

    // Resolve and validate the source record.
    let src = service::get_artifact(t.kv, t.source_repo, &path)
        .await?
        .ok_or_else(|| {
            DepotError::NotFound(format!(
                "chart '{name}-{version}' not found in repository '{}'",
                t.source_repo
            ))
        })?;
    if !matches!(src.kind, ArtifactKind::HelmChart { .. }) {
        return Err(DepotError::DataIntegrity(format!(
            "record at '{path}' in '{}' is not a Helm chart",
            t.source_repo
        )));
    }

    // Destination collision check.
    if !overwrite
        && service::get_artifact(t.kv, t.dest_repo, &path)
            .await?
            .is_some()
    {
        return Err(DepotError::Conflict(format!(
            "chart '{name}-{version}' already exists in repository '{}' (set overwrite to replace)",
            t.dest_repo
        )));
    }

    // Copy the single chart record (verbatim same-store, or blob-restream).
    let copied = t.copy_record(&path).await?;

    // Move: drop the source record after the copy has succeeded.
    let source_deleted = if delete_source {
        match service::delete_artifact(t.kv, t.source_repo, &path).await? {
            Some(old) => {
                t.updater
                    .dir_changed(t.source_repo, &path, -1, -(old.size as i64))
                    .await;
                true
            }
            None => false,
        }
    } else {
        false
    };

    // Invalidate the cached index.yaml on every repo whose chart set changed.
    if copied > 0 {
        set_stale_flag(t.kv, t.dest_repo).await?;
    }
    if source_deleted {
        set_stale_flag(t.kv, t.source_repo).await?;
    }

    Ok(MoveOutcome {
        chart_path: path,
        copied_records: copied,
        source_deleted,
    })
}

/// Delete a single Helm chart version from `repo` and invalidate its cached
/// `index.yaml`. Returns `true` if a chart was removed, `false` if it did not
/// exist. Used for the reorg's junk-deletion pass.
pub async fn delete_chart(
    kv: &dyn KvStore,
    updater: &UpdateSender,
    repo: &str,
    name: &str,
    version: &str,
) -> error::Result<bool> {
    let path = chart_path(name, version);
    match service::delete_artifact(kv, repo, &path).await? {
        Some(old) => {
            updater
                .dir_changed(repo, &path, -1, -(old.size as i64))
                .await;
            set_stale_flag(kv, repo).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}
