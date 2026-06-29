#!/usr/bin/env bash

# SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
#
# SPDX-License-Identifier: Apache-2.0

# Combined coverage: redb tests + DynamoDB tests + UI tests.
# Compiles once with -Cinstrument-coverage, then runs all test suites in
# parallel with separate profraw output dirs. Profraw data is merged into a
# single Rust coverage report. TypeScript coverage is collected separately
# via Istanbul/NYC and printed alongside.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
source "$SCRIPT_DIR/net-helpers.sh"

DATA_DIR=$(mktemp -d)
DYNAMO_PID=""

LLVM_TOOLS_DIR="$(dirname "$(find "$HOME/.rustup" -name llvm-profdata | head -1)")"
LLVM_PROFDATA="$LLVM_TOOLS_DIR/llvm-profdata"
LLVM_COV="$LLVM_TOOLS_DIR/llvm-cov"

cleanup() {
  [ -n "$DYNAMO_PID" ] && kill "$DYNAMO_PID" 2>/dev/null || true
  [ -n "$DYNAMO_PID" ] && wait "$DYNAMO_PID" 2>/dev/null || true
  rm -rf "$DATA_DIR"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# --- Compile instrumented test binaries (once) ---
# Setting DEPOT_INSTRUMENT_FRONTEND makes build.rs use npm run build:test
# (Istanbul-instrumented Vite build) instead of the normal build.
export DEPOT_INSTRUMENT_FRONTEND=1
echo "=== Compiling instrumented binaries ==="
cargo llvm-cov clean --workspace
eval "$(cargo llvm-cov show-env --sh 2>/dev/null)"
# Force recompile of workspace crates so they pick up -Cinstrument-coverage.
cargo clean -p depot-server 2>/dev/null || true
cargo test --features dynamodb --no-run --message-format=json 2>/dev/null \
  | python3 -c "
import json, sys
for line in sys.stdin:
    try:
        msg = json.loads(line)
        if msg.get('reason') == 'compiler-artifact' and msg.get('executable'):
            if msg.get('profile', {}).get('test'):
                print(msg['executable'])
    except: pass
" > "$DATA_DIR/test-binaries.txt"

# Also build the depot server binary with instrumentation (for UI tests).
cargo build -p depot-server

echo "Test binaries:"
cat "$DATA_DIR/test-binaries.txt"

# Profraw output dirs
REDB_PROFRAW_DIR="$DATA_DIR/profraw-redb"
DYNAMO_PROFRAW_DIR="$DATA_DIR/profraw-dynamodb"
UI_PROFRAW_DIR="$DATA_DIR/profraw-ui"
mkdir -p "$REDB_PROFRAW_DIR" "$DYNAMO_PROFRAW_DIR" "$UI_PROFRAW_DIR"

# --- Run redb tests ---
echo "=== Running redb tests ==="
REDB_LOG="$DATA_DIR/redb.log"
(
  while IFS= read -r bin; do
    LLVM_PROFILE_FILE="$REDB_PROFRAW_DIR/redb-%p-%m.profraw" "$bin" -q
  done < "$DATA_DIR/test-binaries.txt"
) > "$REDB_LOG" 2>&1 &
REDB_PID=$!

# --- Run UI tests ---
echo "=== Running UI tests ==="
UI_LOG="$DATA_DIR/ui.log"
LLVM_PROFILE_FILE="$UI_PROFRAW_DIR/ui-%p-%m.profraw" \
  bash scripts/ui-test.sh --skip-build > "$UI_LOG" 2>&1 &
UI_PID=$!

# --- Start DynamoDB Local infrastructure ---
DYNAMODB_LOCAL_DIR="${DYNAMODB_LOCAL_DIR:-$HOME/.local/lib/dynamodb-local}"
DYNAMODB_LOCAL_JAR="$DYNAMODB_LOCAL_DIR/DynamoDBLocal.jar"
DYNAMO_TEST_PID=""

if [ -f "$DYNAMODB_LOCAL_JAR" ]; then
  echo "=== Starting DynamoDB Local infrastructure ==="
  # DynamoDB Local only takes -port (it binds all interfaces), so a free
  # ephemeral port is what keeps parallel suites/worktrees from colliding —
  # no namespace, no root. See net-helpers.sh.
  DYNAMO_PORT=$(pick_free_port)
  DYNAMO_ENDPOINT="http://127.0.0.1:${DYNAMO_PORT}"

  java \
    -Djava.library.path="$DYNAMODB_LOCAL_DIR/DynamoDBLocal_lib" \
    -jar "$DYNAMODB_LOCAL_JAR" \
    -port "$DYNAMO_PORT" -inMemory \
    > "$DATA_DIR/dynamodb.log" 2>&1 &
  DYNAMO_PID=$!

  echo "Waiting for DynamoDB Local..."
  for i in $(seq 1 60); do
    if ss -tln "sport = :${DYNAMO_PORT}" 2>/dev/null | grep -q LISTEN; then
      break
    fi
    if [ "$i" -eq 60 ]; then
      echo "ERROR: DynamoDB Local did not bind port ${DYNAMO_PORT} within 60s"
      exit 1
    fi
    sleep 1
  done
  echo "DynamoDB Local ready."

  echo "=== Running DynamoDB tests ==="
  DYNAMO_LOG="$DATA_DIR/dynamodb-test.log"
  (
    while IFS= read -r bin; do
      env \
        DEPOT_TEST_KV=dynamodb \
        DEPOT_TEST_DYNAMODB_ENDPOINT="$DYNAMO_ENDPOINT" \
        AWS_ACCESS_KEY_ID=fakeAccessKeyId \
        AWS_SECRET_ACCESS_KEY=fakeSecretAccessKey \
        AWS_DEFAULT_REGION=us-east-1 \
        LLVM_PROFILE_FILE="$DYNAMO_PROFRAW_DIR/dynamodb-%p-%m.profraw" \
        "$bin" -q
    done < "$DATA_DIR/test-binaries.txt"
  ) > "$DYNAMO_LOG" 2>&1 &
  DYNAMO_TEST_PID=$!
else
  echo "=== Skipping DynamoDB tests (JAR not found at $DYNAMODB_LOCAL_JAR) ==="
fi

# --- Wait for all test suites ---
FAILED=0
if ! wait "$REDB_PID"; then
  echo "=== redb tests FAILED ==="
  cat "$REDB_LOG"
  FAILED=1
else
  echo "=== redb tests passed ==="
fi

if ! wait "$UI_PID"; then
  echo "=== UI tests FAILED ==="
  cat "$UI_LOG"
  FAILED=1
else
  echo "=== UI tests passed ==="
fi

if [ -n "$DYNAMO_TEST_PID" ]; then
  if ! wait "$DYNAMO_TEST_PID"; then
    echo "=== dynamodb tests FAILED ==="
    cat "$DYNAMO_LOG"
    FAILED=1
  else
    echo "=== dynamodb tests passed ==="
  fi
fi

[ "$FAILED" -ne 0 ] && exit 1

# --- Merge profraw and generate Rust coverage report ---
echo ""
echo "=== Rust coverage ==="

PROFRAW_DIRS=("$REDB_PROFRAW_DIR" "$UI_PROFRAW_DIR")
[ -d "$DYNAMO_PROFRAW_DIR" ] && PROFRAW_DIRS+=("$DYNAMO_PROFRAW_DIR")

PROFRAW_FILES=()
while IFS= read -r f; do PROFRAW_FILES+=("$f"); done \
  < <(find "${PROFRAW_DIRS[@]}" -name '*.profraw' 2>/dev/null)

if [ "${#PROFRAW_FILES[@]}" -eq 0 ]; then
  echo "ERROR: no profraw files found"
  exit 1
fi

MERGED_PROFDATA="$DATA_DIR/combined.profdata"
"$LLVM_PROFDATA" merge -sparse "${PROFRAW_FILES[@]}" -o "$MERGED_PROFDATA"

# Collect all instrumented objects: test binaries + depot server binary
OBJECTS=()
while IFS= read -r bin; do
  OBJECTS+=("--object=$bin")
done < "$DATA_DIR/test-binaries.txt"
OBJECTS+=("--object=$ROOT/target/debug/depot")

mkdir -p build/coverage/html
"$LLVM_COV" show \
  --format=html \
  --output-dir=build/coverage/html \
  --instr-profile="$MERGED_PROFDATA" \
  --ignore-filename-regex='/.cargo/registry|/rustc/' \
  "${OBJECTS[@]}"

"$LLVM_COV" report \
  --instr-profile="$MERGED_PROFDATA" \
  --ignore-filename-regex='/.cargo/registry|/rustc/' \
  "${OBJECTS[@]}"

echo ""
echo "Rust HTML report: build/coverage/html/index.html"

# --- TypeScript coverage ---
echo ""
echo "=== TypeScript coverage ==="
(cd "$ROOT/ui/frontend" && npx nyc report \
  --reporter=html --reporter=text \
  --report-dir=../../build/test/ui/coverage)
echo ""
echo "TypeScript HTML report: build/test/ui/coverage/index.html"
