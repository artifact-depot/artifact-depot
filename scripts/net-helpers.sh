#!/usr/bin/env bash

# SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
#
# SPDX-License-Identifier: Apache-2.0

# Shared helpers for running test servers on the host loopback.
# Source this file; do not execute it directly.
#
# This replaces the old network-namespace approach (scripts/ns-helpers.sh).
# Namespaces were only ever used here as a "private localhost" so parallel
# suites and worktrees could each bind a fixed port (8080 / 8000) without
# colliding. That ran non-root too (via `sudo ip netns` — the dev container
# grants NOPASSWD sudo), so it was never truly "root-only"; but it required
# privilege and leaked namespaces and orphaned server processes whenever a run
# was interrupted.
#
# Giving each server a free ephemeral port instead provides the same
# collision-free isolation with NO privilege and nothing to leak, so the
# port-based suites (ui, dynamodb) run identically as root (CI) or as the
# developer — no sudo, no namespaces, no runuser dance. The one irreducibly
# privileged integration test, docker-auth (it starts root-owned containerd /
# dockerd and installs a CA into the system trust store), uses as_root below.

# pick_free_port
# Echo a currently-unused TCP port. The kernel assigns one via bind(:0); we
# read it back and release it. There is a small TOCTOU window between this
# call and the server actually binding the port, but the caller starts the
# server promptly within the same script, so in practice it is safe — and it
# is the same assumption every "grab a free port" test harness makes.
pick_free_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

# as_root <cmd...>
# Run a command with real root privileges: directly when already root (CI runs
# `make` as root in the builder image), otherwise via passwordless sudo (the
# dev-non-root container grants the developer NOPASSWD: ALL by design). A few
# integration steps are irreducibly privileged — installing a self-signed CA
# into the system trust store and restarting the root-owned Docker daemon — and
# this lets those run unchanged whether invoked as root or as the developer.
# Server processes themselves do NOT use this: they bind a free port as the
# caller (see pick_free_port) so their files stay caller-owned.
if [ "$(id -u)" -eq 0 ]; then
  as_root() { "$@"; }
else
  as_root() { sudo -n "$@"; }
fi
