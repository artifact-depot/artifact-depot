#![deny(clippy::string_slice)]
// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! APT (Debian) package storage logic.
//!
//! Maps APT concepts onto our generic KV + blob store:
//!
//! - `.deb` packages stored at `pool/{component}/{prefix}/{package}/{filename}`
//! - `InRelease` at `dists/{dist}/InRelease` (single mutable commit point per dist)
//! - Metadata files at `dists/{dist}/{comp}/binary-{arch}/by-hash/SHA256/{hash}` (immutable)
//! - `Release` synthesized on demand from InRelease, `Release.gpg` generated on demand
//! - Stale flag at `_apt/metadata_stale/{dist}` triggers rebuild on next metadata fetch
//! - GPG signing key at `_apt/signing_key`, public key at `_apt/public_key`

mod metadata;
mod parse;
mod signing;
#[allow(clippy::module_inception)]
mod store;
mod synthetic;

pub use metadata::{
    extract_release_from_inrelease, generate_release as generate_release_pub, parse_release_sha256,
    ReleaseFileEntry,
};

/// Convenience constructor for ReleaseFileEntry.
pub fn metadata_entry(path: String, sha256: String, size: u64) -> ReleaseFileEntry {
    ReleaseFileEntry::new(path, sha256, size)
}
pub use parse::{parse_deb, pool_path, pool_prefix, DebControl};
pub use signing::{generate_gpg_keypair, public_key_from_secret, sign_release_pub};
pub use store::AptStore;
pub use synthetic::build_synthetic_deb;

/// Hex-encode helper (matches pypi.rs pattern).
pub(crate) mod hex {
    pub fn encode(data: impl AsRef<[u8]>) -> String {
        data.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests;
