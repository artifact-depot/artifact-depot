#!/usr/bin/env bash

# SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
#
# SPDX-License-Identifier: Apache-2.0

# Generate npm dependency license JSON via license-checker.
# Output: target/npm-licenses.json
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET="$ROOT/target"
OUTPUT="$TARGET/npm-licenses.json"

mkdir -p "$TARGET"

# Skip if package-lock.json hasn't changed since last run.
if [ -f "$OUTPUT" ] && [ "$ROOT/ui/frontend/package-lock.json" -ot "$OUTPUT" ]; then
  exit 0
fi

cd "$ROOT/ui/frontend"

TMP="$(mktemp "$OUTPUT.XXXXXX")"
trap 'rm -f "$TMP"' EXIT

npx --yes license-checker --json --production 2>/dev/null > "$TMP"

# Reject empty or invalid output so a failed run can't poison the cache.
if [ ! -s "$TMP" ] || ! python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$TMP" 2>/dev/null; then
    echo "generate-notices-npm.sh: license-checker produced empty or invalid JSON" >&2
    exit 1
fi

mv "$TMP" "$OUTPUT"
