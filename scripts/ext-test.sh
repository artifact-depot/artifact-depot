#!/usr/bin/env bash

# SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
#
# SPDX-License-Identifier: Apache-2.0

# External-dependency integration test runner.
# Starts infrastructure on free loopback ports, runs tests, cleans up.
# No network namespaces, so every mode runs as root (CI) or non-root.
#
# Usage:
#   bash scripts/ext-test.sh dynamodb [extra cargo test args...]
#   bash scripts/ext-test.sh apt
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/net-helpers.sh"

case "${1:-}" in
  apt)
    # Verify Docker is available
    if ! command -v docker &>/dev/null; then
      echo "ERROR: docker is not available. Install docker.io or ensure Docker is in PATH."
      exit 1
    fi

    PORT=18080
    TMP_DIR=$(mktemp -d)
    SERVER_PID=""
    CONTAINER_NAME="depot-apt-test-$$"

    cleanup() {
      echo "Cleaning up..."
      docker rm -f "$CONTAINER_NAME" 2>/dev/null || true
      [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
      [ -n "$SERVER_PID" ] && wait "$SERVER_PID" 2>/dev/null || true
      rm -rf "$TMP_DIR"
    }
    trap cleanup EXIT

    # Build depot
    echo "=== Building depot ==="
    cargo build -p depot-server

    # Write temp config
    cat > "$TMP_DIR/depot.toml" <<TOML
listen = "0.0.0.0:${PORT}"
default_admin_password = "admin"

[kv_store]
type = "redb"
path = "${TMP_DIR}/depot.redb"

[blob_store]
type = "file"
root = "${TMP_DIR}/blobs"
TOML

    # Start depot server
    echo "=== Starting depot server on port ${PORT} ==="
    cargo run --bin depot -- -c "$TMP_DIR/depot.toml" &
    SERVER_PID=$!

    echo "Waiting for depot server..."
    for i in $(seq 1 30); do
      if curl -sf "http://localhost:${PORT}/api/v1/health" >/dev/null 2>&1; then
        echo "Depot server ready."
        break
      fi
      if [ "$i" -eq 30 ]; then
        echo "ERROR: Depot server did not become ready in 30s"
        exit 1
      fi
      sleep 1
    done

    # Login to get JWT token
    echo "=== Logging in ==="
    TOKEN=$(curl -sf -X POST "http://localhost:${PORT}/api/v1/auth/login" \
      -H "Content-Type: application/json" \
      -d '{"username":"admin","password":"admin"}' | python3 -c "import sys,json; print(json.load(sys.stdin)['token'])")
    echo "Got auth token."

    # Grant anonymous user read-only access so apt-get works without auth
    echo "=== Granting anonymous read-only access ==="
    curl -sf -X PUT "http://localhost:${PORT}/api/v1/users/anonymous" \
      -H "Authorization: Bearer ${TOKEN}" \
      -H "Content-Type: application/json" \
      -d '{"roles":["read-only"]}' >/dev/null
    echo "Anonymous read-only access granted."

    # Seed demo data (creates apt-packages repo with 4 synthetic .debs)
    echo "=== Seeding demo data ==="
    cargo run --bin depot-bench -- demo --url "http://localhost:${PORT}"

    # Pre-check: verify InRelease and public key are accessible
    echo "=== Pre-checks ==="
    echo -n "InRelease: "
    curl -sf -o /dev/null -w "%{http_code}" "http://localhost:${PORT}/repository/apt-packages/dists/stable/InRelease"
    echo ""
    echo -n "public.key: "
    curl -sf -o /dev/null -w "%{http_code}" "http://localhost:${PORT}/repository/apt-packages/public.key"
    echo ""

    # Fetch GPG key
    curl -sf "http://localhost:${PORT}/repository/apt-packages/public.key" > "$TMP_DIR/depot.asc"
    echo "GPG key fetched ($(wc -c < "$TMP_DIR/depot.asc") bytes)."

    # Run Docker container with apt-get test
    echo "=== Running apt-get test in Docker container ==="
    docker run --name "$CONTAINER_NAME" --network=host \
      -v "$TMP_DIR/depot.asc:/tmp/depot.asc:ro" \
      debian:bookworm-slim \
      /bin/bash -exc "
        # Install gnupg for key dearmoring
        apt-get update -qq
        apt-get install -y -qq gnupg >/dev/null 2>&1

        # Import depot GPG key
        gpg --dearmor < /tmp/depot.asc > /etc/apt/trusted.gpg.d/depot.gpg

        # Add depot APT source
        echo 'deb [signed-by=/etc/apt/trusted.gpg.d/depot.gpg] http://localhost:${PORT}/repository/apt-packages stable main' \
          > /etc/apt/sources.list.d/depot.list

        # Update package lists (exercises InRelease + Packages.gz)
        apt-get update

        # Verify depot repo is visible
        apt-cache policy hello

        # Install packages from depot (downloads .debs from pool)
        apt-get install -y --allow-unauthenticated hello goodbye libfoo

        # Verify all three are installed
        dpkg -l hello goodbye libfoo
        echo '=== All APT packages installed successfully ==='
      "

    echo "=== APT integration test PASSED ==="
    ;;

  dynamodb)
    # DynamoDB Local JAR location
    DYNAMODB_LOCAL_DIR="${DYNAMODB_LOCAL_DIR:-$HOME/.local/lib/dynamodb-local}"
    DYNAMODB_LOCAL_JAR="$DYNAMODB_LOCAL_DIR/DynamoDBLocal.jar"
    if [ ! -f "$DYNAMODB_LOCAL_JAR" ]; then
      echo "Skipping DynamoDB tests (JAR not found at $DYNAMODB_LOCAL_JAR)"
      exit 0
    fi

    DATA_DIR=$(mktemp -d)
    JAVA_PID=""

    cleanup() {
      echo "Cleaning up..."
      [ -n "$JAVA_PID" ] && kill "$JAVA_PID" 2>/dev/null || true
      [ -n "$JAVA_PID" ] && wait "$JAVA_PID" 2>/dev/null || true
      rm -rf "$DATA_DIR"
    }
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    # Build test binary BEFORE starting infra.
    echo "Building DynamoDB test binary..."
    DEPOT_INSTRUMENT_FRONTEND=1 cargo test -q --features dynamodb --no-run 2>&1 | tail -1

    # DynamoDB Local only takes -port (it binds all interfaces), so a free
    # ephemeral port is what keeps parallel suites/worktrees from colliding —
    # no namespace, no root. See net-helpers.sh.
    DYNAMO_PORT=$(pick_free_port)
    DYNAMO_ENDPOINT="http://127.0.0.1:${DYNAMO_PORT}"

    # Start DynamoDB Local
    java \
      -Djava.library.path="$DYNAMODB_LOCAL_DIR/DynamoDBLocal_lib" \
      -jar "$DYNAMODB_LOCAL_JAR" \
      -port "$DYNAMO_PORT" -inMemory \
      > "$DATA_DIR/dynamodb.log" 2>&1 &
    JAVA_PID=$!

    # Wait for DynamoDB Local readiness
    echo "Waiting for DynamoDB Local..."
    for i in $(seq 1 30); do
      # DynamoDB Local returns 400 on bare GET (missing auth token), so
      # check for any HTTP response rather than requiring 2xx.
      HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "${DYNAMO_ENDPOINT}/" 2>/dev/null || echo "000")
      if [ "$HTTP_CODE" != "000" ]; then
        echo "DynamoDB Local ready."
        break
      fi
      if [ "$i" -eq 30 ]; then
        echo "ERROR: DynamoDB Local did not become ready in 30s"
        echo "=== Last 30 lines of dynamodb.log ==="
        tail -30 "$DATA_DIR/dynamodb.log" 2>/dev/null || echo "(no log file)"
        exit 1
      fi
      sleep 1
    done

    # Run tests against DynamoDB Local
    shift
    echo "Running integration tests against DynamoDB Local..."
    env \
      DEPOT_TEST_KV=dynamodb \
      DEPOT_TEST_DYNAMODB_ENDPOINT="$DYNAMO_ENDPOINT" \
      AWS_ACCESS_KEY_ID=fakeAccessKeyId \
      AWS_SECRET_ACCESS_KEY=fakeSecretAccessKey \
      AWS_DEFAULT_REGION=us-east-1 \
      DEPOT_INSTRUMENT_FRONTEND=1 cargo test -q --features dynamodb "$@"
    ;;

  pypi)
    echo "PyPI external tests not yet implemented"
    exit 1
    ;;

  docker-auth)
    # Verify that Docker and containerd can pull images that require
    # authentication, and that the OCI v1.1 referrers endpoint returns a
    # valid empty image index.  Tests both the GET (Docker/Basic) and POST
    # (containerd/OAuth2) token flows, with an NGINX reverse proxy
    # doing HTTPS termination in front of an HTTP-only depot.
    for cmd in docker ctr containerd nginx openssl; do
      if ! command -v "$cmd" &>/dev/null; then
        echo "Skipping Docker auth tests ($cmd not available)"
        exit 0
      fi
    done

    ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    PORT_HTTP=18080
    PORT_HTTPS=18443
    REGISTRY="localhost:${PORT_HTTPS}"
    TMP_DIR=$(mktemp -d)
    SERVER_PID=""
    CTR_PID=""

    SYSTEM_CA="/usr/local/share/ca-certificates/depot-test.crt"
    DOCKERD_ARGS=""
    cleanup() {
      echo "Cleaning up..."
      nginx -s stop -c "$TMP_DIR/nginx.conf" 2>/dev/null || true
      [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
      [ -n "$SERVER_PID" ] && wait "$SERVER_PID" 2>/dev/null || true
      [ -n "$CTR_PID" ] && kill "$CTR_PID" 2>/dev/null || true
      [ -n "$CTR_PID" ] && wait "$CTR_PID" 2>/dev/null || true
      docker logout "$REGISTRY" 2>/dev/null || true
      docker rmi "$REGISTRY/docker-auth-test/testimg:v1" 2>/dev/null || true
      if [ -f "$SYSTEM_CA" ]; then
        rm -f "$SYSTEM_CA"
        update-ca-certificates --fresh >/dev/null 2>&1 || true
        if [ -n "$DOCKERD_ARGS" ]; then
          restart_dockerd
        fi
      fi
      rm -rf "$TMP_DIR"
    }
    trap cleanup EXIT

    restart_dockerd() {
      local pid
      pid=$(pgrep -x dockerd | head -1)
      if [ -n "$pid" ]; then
        kill "$pid" 2>/dev/null || true
        while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
      fi
      # shellcheck disable=SC2086
      nohup dockerd $DOCKERD_ARGS >/var/log/dockerd.log 2>&1 &
      for _ in $(seq 1 30); do
        docker info >/dev/null 2>&1 && return 0
        sleep 1
      done
      echo "  FAIL: dockerd did not become ready after restart"
      tail -30 /var/log/dockerd.log
      return 1
    }

    # Generate self-signed TLS cert
    echo "=== Generating TLS certificate ==="
    openssl req -x509 -newkey rsa:2048 \
      -keyout "$TMP_DIR/key.pem" -out "$TMP_DIR/cert.pem" \
      -days 1 -nodes -subj '/CN=localhost' \
      -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1' 2>/dev/null

    # Write depot config (HTTP only — NGINX does TLS termination)
    cat > "$TMP_DIR/depot.toml" <<TOML
default_admin_password = "admin"

[http]
listen = "127.0.0.1:${PORT_HTTP}"

[kv_store]
type = "redb"
path = "${TMP_DIR}/depot.redb"

[blob_store]
type = "file"
root = "${TMP_DIR}/blobs"
TOML

    # Start depot
    echo "=== Starting depot on HTTP :${PORT_HTTP} ==="
    "$ROOT/target/debug/depot" -c "$TMP_DIR/depot.toml" > "$TMP_DIR/depot.log" 2>&1 &
    SERVER_PID=$!

    for i in $(seq 1 30); do
      if curl -sf "http://127.0.0.1:${PORT_HTTP}/api/v1/health" >/dev/null 2>&1; then
        echo "Depot ready."
        break
      fi
      if [ "$i" -eq 30 ]; then
        echo "ERROR: Depot did not start in 30s"
        tail -20 "$TMP_DIR/depot.log"
        exit 1
      fi
      sleep 1
    done

    # Start NGINX as HTTPS reverse proxy
    echo "=== Starting NGINX HTTPS proxy on :${PORT_HTTPS} ==="
    cat > "$TMP_DIR/nginx.conf" <<NGINX
worker_processes 1;
error_log ${TMP_DIR}/nginx-error.log warn;
pid ${TMP_DIR}/nginx.pid;
events { worker_connections 64; }
http {
    access_log ${TMP_DIR}/nginx-access.log;
    server {
        listen ${PORT_HTTPS} ssl;
        server_name localhost;
        ssl_certificate ${TMP_DIR}/cert.pem;
        ssl_certificate_key ${TMP_DIR}/key.pem;
        client_max_body_size 500m;

        location / {
            proxy_pass http://127.0.0.1:${PORT_HTTP};
            proxy_set_header Host \$host:\$server_port;
            proxy_set_header X-Forwarded-Proto \$scheme;
            proxy_set_header X-Forwarded-Port \$server_port;
            proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
            proxy_set_header X-Real-IP \$remote_addr;
        }
    }
}
NGINX
    nginx -c "$TMP_DIR/nginx.conf"
    sleep 1
    if ! curl -sfk "https://${REGISTRY}/api/v1/health" >/dev/null 2>&1; then
      echo "ERROR: NGINX proxy not responding"
      exit 1
    fi
    echo "NGINX proxy ready."

    BASE="https://${REGISTRY}"
    K="-sfk"

    # Bootstrap: store, repo, user
    echo "=== Bootstrapping ==="
    TOKEN=$(curl $K -X POST "$BASE/api/v1/auth/login" \
      -H "Content-Type: application/json" \
      -d '{"username":"admin","password":"admin"}' | python3 -c "import sys,json; print(json.load(sys.stdin)['token'])")

    curl $K -X POST "$BASE/api/v1/stores" \
      -H "Authorization: Bearer $TOKEN" \
      -H "Content-Type: application/json" \
      -d "{\"name\":\"default\",\"store_type\":\"file\",\"root\":\"$TMP_DIR/blobs\"}" > /dev/null

    curl $K -X POST "$BASE/api/v1/repositories" \
      -H "Authorization: Bearer $TOKEN" \
      -H "Content-Type: application/json" \
      -d '{"name":"docker-auth-test","repo_type":"hosted","format":"docker","store":"default"}' > /dev/null

    curl $K -X POST "$BASE/api/v1/users" \
      -H "Authorization: Bearer $TOKEN" \
      -H "Content-Type: application/json" \
      -d '{"username":"testpull","password":"pullpass123","roles":["read-only"]}' > /dev/null

    echo "Store, repo, user created."

    # Push a minimal OCI image via Docker V2 API
    echo "=== Pushing test image ==="
    DOCKER_TOKEN=$(curl $K -u "admin:admin" "$BASE/v2/token" | python3 -c "import sys,json; print(json.load(sys.stdin)['token'])")

    mkdir -p "$TMP_DIR/layer"
    echo "hello from docker-auth-test" > "$TMP_DIR/layer/hello.txt"
    tar -C "$TMP_DIR/layer" -cf - . | gzip > "$TMP_DIR/layer.tar.gz"
    LAYER_FILE="$TMP_DIR/layer.tar.gz"
    LAYER_SHA256=$(sha256sum "$LAYER_FILE" | cut -d' ' -f1)
    LAYER_DIGEST="sha256:$LAYER_SHA256"
    LAYER_SIZE=$(wc -c < "$LAYER_FILE")
    DIFF_ID="sha256:$(tar -C "$TMP_DIR/layer" -cf - . | sha256sum | cut -d' ' -f1)"

    # Upload layer blob
    UPLOAD_LOC=$(curl $K -D- -o /dev/null -X POST \
      -H "Authorization: Bearer $DOCKER_TOKEN" \
      "$BASE/v2/docker-auth-test/testimg/blobs/uploads/" \
      | grep -i '^location:' | tr -d '\r' | sed 's/^[Ll]ocation: *//')
    curl $K -X PUT \
      -H "Authorization: Bearer $DOCKER_TOKEN" \
      -H "Content-Type: application/octet-stream" \
      --data-binary @"$LAYER_FILE" \
      "${BASE}${UPLOAD_LOC}?digest=${LAYER_DIGEST}" -o /dev/null

    # Upload config blob
    printf '{"architecture":"amd64","os":"linux","config":{},"rootfs":{"type":"layers","diff_ids":["%s"]}}' "$DIFF_ID" > "$TMP_DIR/config.json"
    CONFIG_SHA256=$(sha256sum "$TMP_DIR/config.json" | cut -d' ' -f1)
    CONFIG_DIGEST="sha256:$CONFIG_SHA256"
    CONFIG_SIZE=$(wc -c < "$TMP_DIR/config.json")
    UPLOAD_LOC=$(curl $K -D- -o /dev/null -X POST \
      -H "Authorization: Bearer $DOCKER_TOKEN" \
      "$BASE/v2/docker-auth-test/testimg/blobs/uploads/" \
      | grep -i '^location:' | tr -d '\r' | sed 's/^[Ll]ocation: *//')
    curl $K -X PUT \
      -H "Authorization: Bearer $DOCKER_TOKEN" \
      -H "Content-Type: application/octet-stream" \
      --data-binary @"$TMP_DIR/config.json" \
      "${BASE}${UPLOAD_LOC}?digest=${CONFIG_DIGEST}" -o /dev/null

    # Push Docker V2 manifest (accepted by both Docker and containerd)
    printf '{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{"mediaType":"application/vnd.docker.container.image.v1+json","digest":"%s","size":%d},"layers":[{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","digest":"%s","size":%d}]}' \
      "$CONFIG_DIGEST" "$CONFIG_SIZE" "$LAYER_DIGEST" "$LAYER_SIZE" > "$TMP_DIR/manifest.json"
    HTTP_CODE=$(curl $K -X PUT \
      -H "Authorization: Bearer $DOCKER_TOKEN" \
      -H "Content-Type: application/vnd.docker.distribution.manifest.v2+json" \
      --data-binary @"$TMP_DIR/manifest.json" \
      "$BASE/v2/docker-auth-test/testimg/manifests/v1" -o /dev/null -w "%{http_code}")
    if [ "$HTTP_CODE" != "201" ]; then
      echo "ERROR: Manifest push failed (HTTP $HTTP_CODE)"
      exit 1
    fi
    echo "Test image pushed."

    # --- Test 1: POST /v2/token (containerd OAuth2 flow) ---
    echo ""
    echo "=== Test 1: POST /v2/token (OAuth2 form credentials) ==="
    HTTP_CODE=$(curl $K -X POST \
      -d "grant_type=password&username=testpull&password=pullpass123&service=depot&scope=repository:docker-auth-test/testimg:pull&client_id=containerd" \
      "$BASE/v2/token" -o "$TMP_DIR/post-token.json" -w "%{http_code}")
    if [ "$HTTP_CODE" = "200" ]; then
      echo "  PASS: POST /v2/token returned 200"
    else
      echo "  FAIL: POST /v2/token returned $HTTP_CODE"
      cat "$TMP_DIR/post-token.json"
      exit 1
    fi

    # Verify the token works
    POST_JWT=$(python3 -c "import sys,json; print(json.load(open('$TMP_DIR/post-token.json'))['token'])")
    HTTP_CODE=$(curl $K -H "Authorization: Bearer $POST_JWT" \
      -H "Accept: application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json" \
      "$BASE/v2/docker-auth-test/testimg/manifests/v1" -o /dev/null -w "%{http_code}")
    if [ "$HTTP_CODE" = "200" ]; then
      echo "  PASS: Bearer token from POST works for manifest GET"
    else
      echo "  FAIL: Bearer token from POST returned $HTTP_CODE on manifest GET"
      exit 1
    fi

    # --- Test 2: OCI v1.1 referrers API ---
    # Querying an unreferenced digest must return a valid *empty* OCI image
    # index -- not a 404, and not the SPA fallback HTML. The HTML-fallback
    # regression surfaces in real clients (docker 24+) as "failed to decode
    # referrers index: invalid character '<'", so assert both the
    # Content-Type and that the body parses as schemaVersion 2 with an empty
    # manifests array. Pure curl + python3 against the existing proxy, so it
    # needs no docker/containerd and runs before the dockerd-gated tests.
    echo ""
    echo "=== Test 2: OCI v1.1 referrers API ==="
    PROBE_DIGEST="sha256:0000000000000000000000000000000000000000000000000000000000000000"
    REF_HEADERS="$TMP_DIR/referrers-headers.txt"
    REF_BODY="$TMP_DIR/referrers-body.json"
    HTTP_CODE=$(curl -sk -o "$REF_BODY" -D "$REF_HEADERS" -w "%{http_code}" \
      -H "Authorization: Bearer $DOCKER_TOKEN" \
      "$BASE/v2/docker-auth-test/testimg/referrers/$PROBE_DIGEST")
    if [ "$HTTP_CODE" != "200" ]; then
      echo "  FAIL: referrers returned HTTP $HTTP_CODE"
      head -c 200 "$REF_BODY"; echo
      exit 1
    fi
    if grep -qi 'content-type:.*application/vnd\.oci\.image\.index\.v1+json' "$REF_HEADERS"; then
      echo "  PASS: referrers Content-Type is OCI image index"
    else
      echo "  FAIL: referrers Content-Type wrong: $(grep -i '^content-type:' "$REF_HEADERS" | tr -d '\r')"
      exit 1
    fi
    if python3 -c "import json,sys; d=json.load(open('$REF_BODY')); sys.exit(0 if d.get('schemaVersion')==2 and d.get('manifests')==[] else 1)" 2>/dev/null; then
      echo "  PASS: referrers body is a valid empty OCI image index"
    else
      echo "  FAIL: referrers body invalid: $(head -c 200 "$REF_BODY")"
      exit 1
    fi

    # --- Test 3: containerd ctr pull ---
    echo ""
    echo "=== Test 3: containerd (ctr) pull with auth ==="
    CTR_ROOT="$TMP_DIR/containerd"
    CTR_STATE="$TMP_DIR/containerd-state"
    CTR_SOCK="$TMP_DIR/containerd.sock"
    CTR_HOSTS="$TMP_DIR/hosts.d"
    mkdir -p "$CTR_ROOT" "$CTR_STATE" "$CTR_HOSTS/${REGISTRY}"

    # Modern ctr restricts --skip-verify and --tlscacert to --local mode,
    # so daemon-driven pulls have to advertise the self-signed CA via the
    # hosts.d directory format passed with --hosts-dir.
    cat > "$CTR_HOSTS/${REGISTRY}/hosts.toml" <<HOSTS
server = "https://${REGISTRY}"

[host."https://${REGISTRY}"]
  capabilities = ["pull", "resolve"]
  ca = "$TMP_DIR/cert.pem"
HOSTS

    cat > "$TMP_DIR/containerd-config.toml" <<CTR_TOML
version = 2
root = "$CTR_ROOT"
state = "$CTR_STATE"
[grpc]
  address = "$CTR_SOCK"
CTR_TOML
    containerd -c "$TMP_DIR/containerd-config.toml" > "$TMP_DIR/containerd.log" 2>&1 &
    CTR_PID=$!

    for i in $(seq 1 10); do
      [ -S "$CTR_SOCK" ] && break
      if [ "$i" -eq 10 ]; then
        echo "  FAIL: containerd did not start"
        tail -10 "$TMP_DIR/containerd.log"
        exit 1
      fi
      sleep 1
    done

    if ctr -a "$CTR_SOCK" images pull \
        --hosts-dir "$CTR_HOSTS" \
        -u testpull:pullpass123 \
        "${REGISTRY}/docker-auth-test/testimg:v1" >/dev/null 2>&1; then
      echo "  PASS: ctr pull succeeded"
    else
      echo "  FAIL: ctr pull failed"
      ctr -a "$CTR_SOCK" images pull \
        --hosts-dir "$CTR_HOSTS" \
        -u testpull:pullpass123 \
        "${REGISTRY}/docker-auth-test/testimg:v1" 2>&1 || true
      exit 1
    fi

    # --- Test 4: docker pull ---
    echo ""
    echo "=== Test 4: docker pull with auth ==="
    # Docker daemon's `insecure-registries` only covers the registry's
    # /v2/ API path; OAuth token requests (the realm URL returned in
    # WWW-Authenticate) still go through normal TLS verification. Install
    # the self-signed cert as a system CA and restart dockerd, since Go
    # caches the system cert pool at process start.
    DOCKERD_PID=$(pgrep -x dockerd | head -1)
    if [ -z "$DOCKERD_PID" ]; then
      echo "  SKIP: dockerd is not running (start it before running this test)"
      exit 0
    fi
    DOCKERD_ARGS=$(tr '\0' ' ' < "/proc/$DOCKERD_PID/cmdline" | sed 's/^[^ ]* //; s/ *$//')
    cp "$TMP_DIR/cert.pem" "$SYSTEM_CA"
    update-ca-certificates >/dev/null 2>&1
    restart_dockerd || exit 1

    if echo "pullpass123" | docker login -u testpull --password-stdin "$REGISTRY" >/dev/null 2>&1; then
      echo "  PASS: docker login succeeded"
    else
      echo "  FAIL: docker login failed"
      exit 1
    fi

    if docker pull "${REGISTRY}/docker-auth-test/testimg:v1" >/dev/null 2>&1; then
      echo "  PASS: docker pull succeeded"
    else
      echo "  FAIL: docker pull failed"
      docker pull "${REGISTRY}/docker-auth-test/testimg:v1" 2>&1 || true
      exit 1
    fi

    echo ""
    echo "=== Docker auth integration test PASSED ==="
    ;;

  *)
    echo "Usage: $0 {dynamodb|apt|docker-auth|pypi} [extra args...]"
    exit 1
    ;;
esac
