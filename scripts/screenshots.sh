#!/usr/bin/env bash

# SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
#
# SPDX-License-Identifier: Apache-2.0

# Boot a depot on a free loopback port, seed it with
# `depot-bench demo`, run `depot-bench trickle` in the background to
# generate live activity (so charts and last-modified columns have
# real values), then drive Chromium via the `screenshots` Playwright
# project to capture curated PNGs into docs/screenshots/.
#
# Mirrors the lifecycle of scripts/ui-test.sh so the same port handling
# and bootstrap apply.
#
# Usage:
#   bash scripts/screenshots.sh              # builds everything
#   bash scripts/screenshots.sh --skip-build # if caller already built

set -euo pipefail

SKIP_BUILD=false
for arg in "$@"; do
  case "$arg" in
    --skip-build) SKIP_BUILD=true ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FRONTEND_DIR="$ROOT/ui/frontend"
BUILD_DIR="$ROOT/build/screenshots"
DATA_DIR="$BUILD_DIR/server-data"
SHOTS_DIR="$BUILD_DIR/shots"
DOCS_OUT="$ROOT/docs/screenshots"

source "$SCRIPT_DIR/net-helpers.sh"

# A free ephemeral port keeps parallel runs from colliding without a network
# namespace (and therefore without root). See net-helpers.sh.
PORT=$(pick_free_port)
BASE_URL="http://127.0.0.1:${PORT}"

cleanup() {
  if [[ -n "${TRICKLE_PID:-}" ]]; then
    kill "$TRICKLE_PID" 2>/dev/null || true
    wait "$TRICKLE_PID" 2>/dev/null || true
  fi
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    echo "Stopped depot server (pid $SERVER_PID)"
  fi
}
trap cleanup EXIT

# --- Build (unless caller already did it) ---
if [[ "$SKIP_BUILD" == "false" ]]; then
  echo "Building depot + depot-bench..."
  cargo build -p depot-server -p depot-bench

  echo "Building frontend..."
  (cd "$FRONTEND_DIR" && npm install --silent && npm run build)
fi

# --- Clean prior state ---
rm -rf "$DATA_DIR" "$SHOTS_DIR"
mkdir -p "$DATA_DIR/blobs" "$SHOTS_DIR"

# --- Write server config ---
cat > "$DATA_DIR/depotd.toml" <<EOF
default_admin_password = "admin"
gc_min_interval_secs = 60

[http]
listen = "127.0.0.1:${PORT}"

[kv_store]
type = "redb"
path = "${DATA_DIR}/depot.redb"
EOF

# --- Start depot server ---
echo "Starting depot server on ${BASE_URL}..."
"$ROOT/target/debug/depot" -c "$DATA_DIR/depotd.toml" \
  > "$DATA_DIR/server.log" 2>&1 &
SERVER_PID=$!

# --- Wait for health ---
for _ in $(seq 1 60); do
  if curl -sf "${BASE_URL}/api/v1/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "Server exited unexpectedly. Log:"; cat "$DATA_DIR/server.log"; exit 1
  fi
  sleep 0.5
done
curl -sf "${BASE_URL}/api/v1/health" >/dev/null || {
  echo "Server not healthy. Log:"; cat "$DATA_DIR/server.log"; exit 1
}
echo "Server is healthy."

# --- Bootstrap admin + default store ---
_curl() { curl -s "$@"; }

TOKEN=$(_curl -X POST "${BASE_URL}/api/v1/auth/login" \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"admin"}' \
  | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')

FRESH_TOKEN=$(_curl -X POST "${BASE_URL}/api/v1/auth/change-password" \
  -H "Content-Type: application/json" -H "Authorization: Bearer ${TOKEN}" \
  -d '{"username":"admin","current_password":"admin","new_password":"admin"}' \
  | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
TOKEN="${FRESH_TOKEN:-$TOKEN}"

STORE_RESP=$(_curl -w "\n%{http_code}" -X POST "${BASE_URL}/api/v1/stores" \
  -H "Content-Type: application/json" -H "Authorization: Bearer ${TOKEN}" \
  -d "{\"name\":\"default\",\"store_type\":\"file\",\"root\":\"${DATA_DIR}/blobs\"}")
case "$(echo "$STORE_RESP" | tail -1)" in
  2*|409) echo "Default store ready." ;;
  *) echo "Store creation failed: $(echo "$STORE_RESP" | head -1)"; exit 1 ;;
esac

# --- Seed with depot-bench demo ---
echo "Seeding demo data..."
"$ROOT/target/debug/depot-bench" demo \
  --url "$BASE_URL" \
  > "$DATA_DIR/demo.log" 2>&1
echo "Demo data seeded."

# --- Kick off trickle in background for live activity ---
echo "Starting depot-bench trickle (background)..."
"$ROOT/target/debug/depot-bench" trickle \
  --url "$BASE_URL" \
  > "$DATA_DIR/trickle.log" 2>&1 &
TRICKLE_PID=$!

# Let trickle add a bit of activity before we capture.
echo "Letting trickle run for 20s..."
sleep 20

# --- Run the screenshots Playwright project ---
echo "Capturing screenshots..."
cd "$FRONTEND_DIR"
env \
  DEPOT_TEST_URL="${BASE_URL}" \
  DEPOT_UI_SKIP_SETUP=1 \
  SCREENSHOTS_OUT="$SHOTS_DIR" \
  npx playwright test --project=screenshots
cd "$ROOT"

# --- Publish into docs/ ---
mkdir -p "$DOCS_OUT"
shopt -s nullglob
PNGS=("$SHOTS_DIR"/*.png)
shopt -u nullglob
if (( ${#PNGS[@]} == 0 )); then
  echo "No screenshots produced!" >&2
  exit 1
fi
cp "${PNGS[@]}" "$DOCS_OUT/"

echo
echo "Wrote ${#PNGS[@]} screenshots to $DOCS_OUT/"
ls -la "$DOCS_OUT"
