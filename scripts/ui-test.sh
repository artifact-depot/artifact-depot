#!/usr/bin/env bash

# SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
#
# SPDX-License-Identifier: Apache-2.0
# Run Playwright UI tests inside a network namespace for isolation.
# All artifacts are kept under build/ so parallel worktrees never collide.
#
# Usage:
#   bash scripts/ui-test.sh              # standalone (builds everything)
#   bash scripts/ui-test.sh --skip-build # called from test.sh/coverage.sh (build already done)
#
# If LLVM_PROFILE_FILE is set, it is forwarded to the depot server so Rust
# code coverage is collected from UI-driven traffic.
set -euo pipefail

SKIP_BUILD=false
EXTRA_ARGS=()
for arg in "$@"; do
  case "$arg" in
    --skip-build) SKIP_BUILD=true ;;
    *) EXTRA_ARGS+=("$arg") ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FRONTEND_DIR="$ROOT/ui/frontend"
BUILD_DIR="$ROOT/build/test/ui"
DATA_DIR="$BUILD_DIR/server-data"

source "$SCRIPT_DIR/ns-helpers.sh"
sweep_leaked_namespaces

NETNS=$(make_netns_name "depot-ui-test")
PORT=8080
BASE_URL="http://127.0.0.1:${PORT}"

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    echo "Stopped depot server (pid $SERVER_PID)"
  fi
  ip netns del "$NETNS" 2>/dev/null || true
}
trap cleanup EXIT

# --- Build (unless caller already did it) ---
if [[ "$SKIP_BUILD" == "false" ]]; then
  echo "Building instrumented frontend..."
  (cd "$FRONTEND_DIR" && npm run build:test)

  echo "Rebuilding depot binary..."
  cargo build -p depot-server
fi

# --- Clean prior state ---
rm -rf "$FRONTEND_DIR/.nyc_output"
mkdir -p "$FRONTEND_DIR/.nyc_output"

# Fresh server data each run
rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR/blobs" "$BUILD_DIR"

# --- Create isolated network namespace ---
ip netns del "$NETNS" 2>/dev/null || true
ip netns add "$NETNS"
ip netns exec "$NETNS" ip link set lo up

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

# --- Start depot server inside namespace ---
# Forward LLVM_PROFILE_FILE if set so Rust coverage is collected.
echo "Starting depot server in namespace $NETNS..."
ip netns exec "$NETNS" env \
  ${LLVM_PROFILE_FILE:+LLVM_PROFILE_FILE="$LLVM_PROFILE_FILE"} \
  "$ROOT/target/debug/depot" -c "$DATA_DIR/depotd.toml" \
  > "$DATA_DIR/server.log" 2>&1 &
SERVER_PID=$!

# Wait for health
echo "Waiting for server health..."
for _ in $(seq 1 60); do
  if ip netns exec "$NETNS" curl -sf "${BASE_URL}/api/v1/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "Server exited unexpectedly. Log:"; cat "$DATA_DIR/server.log"; exit 1
  fi
  sleep 0.5
done
ip netns exec "$NETNS" curl -sf "${BASE_URL}/api/v1/health" >/dev/null || {
  echo "Server not healthy. Log:"; cat "$DATA_DIR/server.log"; exit 1
}
echo "Server is healthy."

# --- Bootstrap: change admin password (required) and create default store ---
_curl() { ip netns exec "$NETNS" curl -s "$@"; }

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
STORE_HTTP=$(echo "$STORE_RESP" | tail -1)
case "$STORE_HTTP" in
  2*) echo "Default store created." ;;
  409) echo "Default store already exists." ;;
  *) echo "Store creation failed (HTTP $STORE_HTTP): $(echo "$STORE_RESP" | head -1)"; exit 1 ;;
esac
echo "Bootstrap complete."

# --- Ensure Playwright browsers are installed ---
echo "Ensuring Playwright browsers are installed..."
(cd "$FRONTEND_DIR" && npx playwright install chromium)

# --- Run Playwright tests inside namespace ---
echo "Running Playwright tests..."
cd "$FRONTEND_DIR"
# Run only the functional projects by default. The `screenshots` and
# `observability` projects are doc-image generators driven on demand by
# scripts/screenshots.sh (they self-skip without their *_OUT env vars), so
# without this filter every run reports them as skipped. A caller that
# passes its own --project (e.g. to run one project) overrides this default.
PROJECT_ARGS=()
if ! printf '%s\n' "${EXTRA_ARGS[@]}" | grep -q -- '--project'; then
  PROJECT_ARGS=(--project=parallel --project=serial)
fi
ip netns exec "$NETNS" env \
  DEPOT_TEST_URL="${BASE_URL}" \
  DEPOT_UI_SKIP_SETUP=1 \
  npx playwright test "${PROJECT_ARGS[@]}" "${EXTRA_ARGS[@]}"
