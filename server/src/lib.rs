#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::unreachable)]
#![deny(clippy::fallible_impl_from)]
#![deny(clippy::missing_panics_doc)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::string_slice)]
#![deny(clippy::get_unwrap)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::missing_panics_doc,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::get_unwrap,
        clippy::field_reassign_with_default,
        clippy::redundant_clone,
        clippy::cmp_owned,
        unused_imports
    )
)]

// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

pub mod server;

/// Full build version: the crate version plus a build counter that increments
/// with every merge to `main` (e.g. `1.0.0+47`). Computed in `build.rs`.
pub const VERSION: &str = env!("DEPOT_VERSION");
