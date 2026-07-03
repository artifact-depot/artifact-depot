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
#       reachable AND authorized; otherwise it falls through to wayland
#       automatically. For the crisp xcb path, authorize the host X server for
#       local clients once with `xhost +local:` (see docs/dev_container.md).
#   LIBGL_ALWAYS_SOFTWARE=1 -- on the wayland path the GPU render node is often
#       not accessible to the dev user, so force Mesa software rendering;
#       harmless on the xcb path.
exports='export LIBGL_ALWAYS_SOFTWARE=1; export QT_QPA_PLATFORM="xcb;wayland"'

# Idempotent in both directions: match Scooter's pristine line (restored by the
# .deb on install/upgrade) OR a line we patched on a previous run, and rewrite
# it to the freshly computed $exports.
if grep -qE '^(export QT_QPA_PLATFORM=xcb$|export LIBGL_ALWAYS_SOFTWARE=1; export QT_QPA_PLATFORM=)' /usr/bin/bcompare; then
    echo "[bcompare] patching wrapper: QT platform fallback + software GL"
    # '|' is the regex alternation, so use '@' as the s/// delimiter.
    sudo sed -i -E "s@^(export QT_QPA_PLATFORM=xcb\$|export LIBGL_ALWAYS_SOFTWARE=1; export QT_QPA_PLATFORM=).*@$exports@" /usr/bin/bcompare
fi

# License non-interactively: decode the key into Scooter's all-users key file.
echo "[bcompare] writing license to /etc/BC5Key.txt"
printf '%s' "$BC5_LICENSE_KEY_B64" | base64 -d | sudo tee /etc/BC5Key.txt >/dev/null
sudo chmod 0644 /etc/BC5Key.txt

# Deliberately NO `git config` here. Running `git config --global` at
# postCreate creates ~/.gitconfig, which makes VS Code's copyGitConfig skip
# copying the host ~/.gitconfig (it won't overwrite an existing file) -- so the
# developer's identity and aliases never make it into the container. The host
# gitconfig already sets diff.tool/merge.tool=bc; with the bcompare binary on
# PATH that is all git needs. (The qb dev container works exactly this way.)

echo "[bcompare] done -- 'git difftool' / 'git mergetool' use Beyond Compare via your host ~/.gitconfig"
