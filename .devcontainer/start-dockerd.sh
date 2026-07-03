#!/usr/bin/env bash

# SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
#
# SPDX-License-Identifier: Apache-2.0

# Start the in-container Docker daemon for the dev-non-root container.
#
# Debian's docker.io (installed in the Dockerfile's builder stage) provides
# dockerd, but no init system runs inside the container, so the daemon is
# launched here on every container start via devcontainer.json's
# postStartCommand. This replaces the docker-in-docker devcontainer feature,
# which could no longer be pinned to skip the (already-present) engine install.
#
# Idempotent: a no-op when dockerd is already accepting connections. Requires
# the container to be privileged (set in devcontainer.json), which dind needs.
# /var/lib/docker is a named volume (see mounts) so the storage driver runs on
# a real filesystem rather than nested overlay, and images survive rebuilds.
set -euo pipefail

# Root can always reach the socket, so probe with sudo to avoid depending on
# the caller's docker-group membership having taken effect yet.
if sudo docker info >/dev/null 2>&1; then
    exit 0
fi

# A container restart kills dockerd without removing its pid file, and the
# stale file makes the next dockerd refuse to start ("pid file found"). Clear
# it when no daemon is actually running.
if ! pgrep -x dockerd >/dev/null; then
    sudo rm -f /var/run/docker.pid
fi

# Launch dockerd detached, as root. daemon.json (baked into the image) marks
# the loopback registry insecure for the docker-auth integration test.
sudo -b sh -c 'dockerd >/var/log/dockerd.log 2>&1'

# Wait (up to ~30s) for the daemon to come up.
for _ in $(seq 1 30); do
    if sudo docker info >/dev/null 2>&1; then
        exit 0
    fi
    sleep 1
done

echo "dockerd did not become ready within 30s; see /var/log/dockerd.log" >&2
sudo tail -n 20 /var/log/dockerd.log >&2 || true
exit 1
