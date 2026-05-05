#!/usr/bin/env bash

# SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
#
# SPDX-License-Identifier: Apache-2.0

# Generate Rust dependency license JSON via cargo-about.
# Output: target/cargo-about.json
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET="$ROOT/target"
OUTPUT="$TARGET/cargo-about.json"

mkdir -p "$TARGET"

# Skip if Cargo.lock hasn't changed since last run.
if [ -f "$OUTPUT" ] && [ "$ROOT/Cargo.lock" -ot "$OUTPUT" ]; then
  exit 0
fi

command -v cargo-about >/dev/null 2>&1 || cargo install cargo-about --locked --features cli

cargo about generate --workspace --format json -c "$ROOT/about.toml" \
    2>/dev/null > "$OUTPUT"
