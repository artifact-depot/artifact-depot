// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

/// Computes the full build version string and exposes it as the `DEPOT_VERSION`
/// compile-time env var (read via `env!("DEPOT_VERSION")`).
///
/// The version is `<crate version>+<counter>`, where the counter is a
/// monotonically increasing build number incremented on every merge to `main`.
/// The counter is the number of commits on the built revision
/// (`git rev-list --count HEAD`), so it advances by one with each merged PR.
///
/// Resolution order for the counter:
///   1. `DEPOT_BUILD_COUNTER` env var, if set and non-empty. CI/Docker set this
///      explicitly because the release image build has no `.git` available
///      (`.git/` is excluded by `.dockerignore`).
///   2. `git rev-list --count HEAD`, for local/dev builds inside a checkout.
///   3. Omitted entirely — `DEPOT_VERSION` falls back to the bare crate version.
fn main() {
    println!("cargo:rerun-if-env-changed=DEPOT_BUILD_COUNTER");
    // A new commit (PR merge) changes HEAD, so rebuild to pick up the new count.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads/main");

    let base = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());

    let version = match build_counter() {
        Some(counter) => format!("{base}+{counter}"),
        None => base,
    };

    println!("cargo:rustc-env=DEPOT_VERSION={version}");
}

fn build_counter() -> Option<String> {
    if let Ok(counter) = std::env::var("DEPOT_BUILD_COUNTER") {
        let counter = counter.trim();
        if !counter.is_empty() {
            return Some(counter.to_string());
        }
    }

    let output = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let counter = String::from_utf8(output.stdout).ok()?;
    let counter = counter.trim();
    if counter.is_empty() {
        None
    } else {
        Some(counter.to_string())
    }
}
