#!/usr/bin/env bash

# SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
#
# SPDX-License-Identifier: Apache-2.0

# Optional Beyond Compare 5 install for the dev-non-root dev container (opt-in, per
# developer). Run from the dev-non-root config's postCreateCommand.
#
# Gate + license are a single env var: BC5_LICENSE_KEY_B64, the base64 encoding
# of your BC5 license key block. Deliver it the way the dev-non-root dev container
# delivers other env -- export it from ~/.dev_aliases, which initializeCommand
# snapshots into the --env-file (~/.artifact-depot.devenv):
#
#     export BC5_LICENSE_KEY_B64=<base64 of your full BC5 key block>
#
# Produce that value on the host with (key block saved to ~/bc5key.txt):
#     base64 -w0 ~/bc5key.txt              # Linux
#     base64 -i  ~/bc5key.txt | tr -d '\n' # macOS
#
# See docs/dev_container.md ("Beyond Compare 5").
#
# When the var is set, this installs Beyond Compare 5 and licenses it by writing
# the decoded key to /etc/BC5Key.txt (Scooter's unattended-license location, all
# users). When it is unset, this is a no-op -- developers without a license get
# no Beyond Compare and the rebuild does not fail.
#
# git's bc diff/merge tool invokes `bcompare` (installed here), so once this has
# run `git difftool` / `git mergetool` open Beyond Compare (a forwarded display
# is required for the GUI; see the docs).
#
# Optional, also opt-in: XAUTH_B64. When the container forwards the host X
# socket but not its auth cookie, the crisp xcb path is auth-walled and BCompare
# falls back to (fuzzier) Wayland. Set XAUTH_B64 to the base64 of a
# wildcard-family X cookie extracted on the host (see the docs) and this writes
# it to ~/.artifact-depot.xauth and points BCompare's XAUTHORITY at it, so xcb
# authorizes. Unset -> Wayland. Kept here (not in devcontainer.json) so only BC
# users incur any of it.
set -euo pipefail

if [ -z "${BC5_LICENSE_KEY_B64:-}" ]; then
    echo "[bcompare] BC5_LICENSE_KEY_B64 not set -- skipping Beyond Compare 5 install"
    exit 0
fi

BC5_DEB_URL="https://www.scootersoftware.com/files/bcompare-5.2.2.32209_amd64.deb"

sudo apt-get update

# Runtime deps Scooter's .deb does not pull, but BCompare needs here. BCompare
# links the *system* Qt6 and ships no bundled platform plugins:
#   qt6-wayland       -- the Qt6 'wayland' platform plugin (this container can
#                        forward a Wayland socket); without it BCompare aborts
#                        with "Could not find the Qt platform plugin wayland".
#   fonts-dejavu-core -- a proper Latin UI font, else text renders with the
#                        wrong fallback.
sudo apt-get install -y qt6-wayland fonts-dejavu-core

if ! command -v bcompare >/dev/null 2>&1; then
    echo "[bcompare] installing Beyond Compare 5 ..."
    tmpdeb="$(mktemp --suffix=.deb)"
    curl -fsSL "$BC5_DEB_URL" -o "$tmpdeb"
    # Pre-create an empty /etc/default/bcompare so the .deb postinst does NOT
    # add Scooter's apt source for auto-updates -- unwanted in an ephemeral
    # dev container. (Scooter KB: kb/linux_install.)
    sudo touch /etc/default/bcompare
    sudo apt-get install -y "$tmpdeb"
    rm -f "$tmpdeb"
    sudo apt-get clean
else
    echo "[bcompare] Beyond Compare already installed"
fi

# Scooter's /usr/bin/bcompare wrapper hardcodes `export QT_QPA_PLATFORM=xcb`.
# Replace it with a fallback list so the backend is chosen by an actual runtime
# connection test rather than guessed:
#   QT_QPA_PLATFORM="xcb;wayland" -- Qt uses xcb when the X server (DISPLAY) is
#       reachable AND authorized (see the cookie block); otherwise it falls
#       through to wayland automatically.
#   LIBGL_ALWAYS_SOFTWARE=1 -- on the wayland path the GPU render node is often
#       not accessible to the dev user, so force Mesa software rendering;
#       harmless on the xcb path.
exports='export LIBGL_ALWAYS_SOFTWARE=1; export QT_QPA_PLATFORM="xcb;wayland"'

# Optional X11 authorization for the crisp xcb path (opt-in via XAUTH_B64).
if [ -n "${XAUTH_B64:-}" ]; then
    if printf '%s' "$XAUTH_B64" | base64 -d > "$HOME/.artifact-depot.xauth" 2>/dev/null; then
        chmod 0600 "$HOME/.artifact-depot.xauth"
        exports="$exports; export XAUTHORITY=\"$HOME/.artifact-depot.xauth\""
        echo "[bcompare] wrote X11 auth cookie to ~/.artifact-depot.xauth (xcb authorized)"
    else
        rm -f "$HOME/.artifact-depot.xauth"
        echo "[bcompare] WARN: XAUTH_B64 is not valid base64 -- ignoring (will use Wayland)"
    fi
fi

# Idempotent in both directions: match Scooter's pristine line (restored by the
# .deb on install/upgrade) OR a line we patched on a previous run, and rewrite
# it to the freshly computed $exports.
if grep -qE '^(export QT_QPA_PLATFORM=xcb$|export LIBGL_ALWAYS_SOFTWARE=1; export QT_QPA_PLATFORM=)' /usr/bin/bcompare; then
    case "$exports" in *XAUTHORITY*) cookie_note=" + X11 cookie";; *) cookie_note="";; esac
    echo "[bcompare] patching wrapper: QT platform fallback + software GL${cookie_note}"
    # '|' is the regex alternation, so use '@' as the s/// delimiter.
    sudo sed -i -E "s@^(export QT_QPA_PLATFORM=xcb\$|export LIBGL_ALWAYS_SOFTWARE=1; export QT_QPA_PLATFORM=).*@$exports@" /usr/bin/bcompare
fi

# License non-interactively: decode the key into Scooter's all-users key file.
echo "[bcompare] writing license to /etc/BC5Key.txt"
printf '%s' "$BC5_LICENSE_KEY_B64" | base64 -d | sudo tee /etc/BC5Key.txt >/dev/null
sudo chmod 0644 /etc/BC5Key.txt

# Point git's diff/merge tool at Beyond Compare.
git config --global diff.tool bc
git config --global merge.tool bc
git config --global difftool.bc.path /usr/bin/bcompare
git config --global mergetool.bc.path /usr/bin/bcompare

echo "[bcompare] done -- 'git difftool' / 'git mergetool' will use Beyond Compare"
