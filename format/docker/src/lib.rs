// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Docker / OCI registry format handler — both repository logic and HTTP routes.

pub mod api;
pub mod promote;
pub mod store;

pub use promote::{copy_tag, delete_tag, CopyTarget, PromoteOutcome};
pub use store::DockerStore;
