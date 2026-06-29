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
# colliding. `ip netns` needs CAP_SYS_ADMIN, which the non-root devcontainer
# cannot obtain (setcap is stripped on exec under the nested overlay, and
# unprivileged user namespaces are blocked from writing uid_map), so those
# suites could only run as root.
#
# Giving each server a free ephemeral port instead provides the same
# collision-free isolation with zero privilege, so the suites now run
# identically whether invoked as root (CI) or as the developer (devcontainer).
# No sudo, no namespaces, no runuser environment dance.

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
