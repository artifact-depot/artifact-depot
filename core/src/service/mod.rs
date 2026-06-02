// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Service layer: typed helper functions over the KvStore trait.
//!
//! All domain logic lives here. Each function operates directly on the
//! non-transactional KvStore — no transactions, no ref counting, no
//! synchronous index maintenance.

mod artifact;
mod blob;
mod directory;
mod meta;
pub mod pagination;
mod repo;
pub mod scan;
mod store;
pub mod task;
mod typed;
mod upload;
mod user;

pub use artifact::{
    delete_artifact, delete_artifacts_paired_batch, fold_all_artifacts, get_artifact,
    list_artifacts, put_artifact, put_artifact_if_absent, search_artifacts, touch_artifact,
};
pub use blob::{
    count_blobs_for_store, delete_blob, fold_all_blobs, get_blob, list_blobs_for_store, put_blob,
    put_dedup_record,
};
pub use directory::{get_dir, get_repo_stats, list_children};
pub use meta::{delete_meta, get_gc_last_started_at, get_meta, put_meta, set_gc_last_started_at};
pub use repo::{
    clone_repo, delete_repo, drain_deleting_repo, get_repo, get_repo_raw, list_repos,
    list_repos_raw, propagate_staleness, put_repo,
};
pub use scan::{BG_SHARD_CONCURRENCY, SHARD_CONCURRENCY};
pub use store::{
    delete_store, delete_store_stats, get_store, get_store_stats, list_stores, put_store,
    put_store_stats,
};
pub use typed::{typed_get, typed_get_versioned, typed_list, typed_put, typed_put_if_absent};
pub use upload::{
    cleanup_upload_sessions, delete_upload_session, get_upload_session,
    get_upload_session_versioned, list_upload_sessions, put_upload_session,
    put_upload_session_if_version,
};
pub use user::{
    delete_role, delete_user, get_role, get_user, get_user_with_roles, list_roles, list_users,
    put_role, put_user,
};
