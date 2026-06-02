// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use depot_core::error::{self, DepotError};

/// Build a synthetic RPM file for testing/demo purposes using the rpm crate's builder API.
pub fn build_synthetic_rpm(
    name: &str,
    version: &str,
    release: &str,
    arch: &str,
    summary: &str,
) -> error::Result<Vec<u8>> {
    let config = rpm::BuildConfig::default().compression(rpm::CompressionType::None);
    let pkg = rpm::PackageBuilder::new(name, version, "MIT", arch, summary)
        .release(release.to_string())
        .description(format!("{summary} package"))
        .with_file_contents(
            b"#!/bin/sh\necho hello\n".to_vec(),
            rpm::FileOptions::new("/usr/bin/hello").permissions(0o755),
        )
        .map_err(|e| DepotError::BadRequest(format!("failed to add file: {e}")))?
        .add_changelog_entry(
            "Test Author <test@example.com>",
            "Initial build",
            rpm::Timestamp::now(),
        )
        .using_config(config)
        .build()
        .map_err(|e| DepotError::BadRequest(format!("failed to build RPM: {e}")))?;

    let mut buf = Vec::new();
    pkg.write(&mut buf)
        .map_err(|e| DepotError::BadRequest(format!("failed to write RPM: {e}")))?;
    Ok(buf)
}
