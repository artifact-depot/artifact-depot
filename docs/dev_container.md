---
title: Dev Container
nav_order: 9
---

# Dev Container

Artifact Depot ships **two** dev container configurations. Both build from the
single root [`Dockerfile`](https://github.com/artifact-depot/artifact-depot/blob/main/Dockerfile)
— the same image CI and the release build use — they just target different
build stages. In VS Code, **Dev Containers: Reopen in Container** shows a picker
when more than one config is present; pick the one you want.

| | **artifact-depot** (default) | **artifact-depot (dev-non-root)** |
|---|---|---|
| Config | `.devcontainer/devcontainer.json` | `.devcontainer/dev-non-root/devcontainer.json` |
| Build stage | `dev` | `dev-non-root` |
| Runs as | `root` | your host user (UID-matched) |
| Setup needed | none | a few host-side files (below) |

Both share the same named volumes (`/workspace`, `/worktrees`, `/build`), bind
the repo at `/workspace/artifact-depot`, run privileged (so the `make`
end-to-end tests that start `dockerd` work), and forward ports 4000 (docs) and
8080 (depot).

## artifact-depot (default / community)

The recommended starting point. **Just "Reopen in Container" — no setup.** It
contains the complete build/test toolchain plus the conveniences most
contributors want:

- **Toolchain** (from the shared `builder` stage): Rust 1.94.1, Node 24, the
  Playwright system libraries, DynamoDB Local, `cargo-deny`/`cargo-about`,
  `reuse`, `mold` — everything `make` (lint + build + test + e2e) needs.
- **Conveniences** (the `dev` stage): `gh`, `git`, `vim`, `less`, `ripgrep`,
  `jq`, `shellcheck`, `hadolint`, `skopeo`, `helm`, `docker-compose`, Ruby +
  Jekyll (docs site), and the `claude`/`codex` CLIs.
- **Extensions**: rust-analyzer, GitLens, shellcheck, hadolint, YAML, Volar,
  Claude Code.

It runs as **root**. The trade-off: files you create in the bind-mounted repo
are owned by root on your host. If that bothers you, use the dev-non-root container.

## artifact-depot (dev-non-root)

Everything in the default container, plus a **non-root user matched to your
host UID** (so bind-mounted files keep your ownership), `glab` and the `aws`
CLI, a real in-container Docker daemon, access to a few of your host config
folders, an env-file bridge, and an optional Beyond Compare install.

Because it runs its own in-container `dockerd` (like CI and the default
container), the **docker-based** integration tests run here — `test-docker-auth`
and `test-apt` launch containers against a depot on the dev container's
`localhost`. The Docker engine and the `buildx` plugin are baked into the image;
the daemon itself is started on every container launch by a `postStartCommand`
([`.devcontainer/start-dockerd.sh`](https://github.com/artifact-depot/artifact-depot/blob/main/.devcontainer/start-dockerd.sh)),
and `/var/lib/docker` is a named volume so pulled images survive rebuilds. The
dev-non-root stage bakes `/etc/docker/daemon.json` marking the loopback registry
insecure, matching CI's `dockerd --insecure-registry 127.0.0.0/8`.

**What passes here.** The full `make test` suite — lint, build, `cargo test`,
and the `test-ui`, `test-dynamodb`, and `test-docker-auth` integration suites —
passes non-root in this container, identically to root/CI. (`test-apt` is a
separate `scripts/ext-test.sh apt` mode, not part of `make test`, and is
maintained elsewhere.)

`test-ui` and `test-dynamodb` give each server a **free ephemeral port on the
host loopback** (see
[`scripts/net-helpers.sh`](https://github.com/artifact-depot/artifact-depot/blob/main/scripts/net-helpers.sh)),
which keeps parallel suites/worktrees from colliding with **no privilege**. They
previously used a network namespace for that isolation — which *did* work
non-root (via `sudo ip netns`, since this container grants passwordless sudo),
but it required privilege and leaked namespaces/processes when a run was
interrupted; free ports need neither.

`test-docker-auth` is the one suite with irreducibly privileged steps: it starts
a root-owned `containerd` and `dockerd` and installs a self-signed CA into the
system trust store. Those specific steps run via the container's passwordless
`sudo` (see the `as_root` helper in `net-helpers.sh`), while `ctr` and `docker`
themselves run as the dev user via the `docker` group. This is exactly why the
dev-non-root container is **privileged with NOPASSWD sudo** — not a workaround,
but what lets the daemon-based test run the same way whether root or non-root.

Select it with **Dev Containers: Reopen in Container → "artifact-depot (dev-non-root)"**.

### Host files it uses

`initializeCommand` creates these on your host if missing (empty), then binds
them in, so the container starts even if you have none of them:

- `~/.ssh` (read-only) — git push / ssh to remote hosts
- `~/.claude` — Claude session history persists across rebuilds
- `~/.aws` — AWS credentials for the S3 / DynamoDB backends
- `~/.artifact-depot.bash_history` — shell history persists across rebuilds

### `~/.dev_aliases` — your env + aliases

Create `~/.dev_aliases` on your host and `export` whatever you want available
**inside** the container:

```bash
# ~/.dev_aliases
export TZ=America/Chicago
export GITLAB_HOST=gitlab.example.com      # if you use glab
# export BC5_LICENSE_KEY_B64=...           # see Beyond Compare below
```

How it works: `initializeCommand` snapshots the variables this file exports
into `~/.artifact-depot.devenv`, and `runArgs: --env-file` imports them into the
container — so they reach **non-interactive** shells too (Claude Code's tool
shell, scripts), not just login shells. The same file is also mounted as
`~/.bash_aliases`, so interactive shells pick up any aliases/functions.

### Beyond Compare 5 (optional)

Set one variable to opt in; leave it unset and nothing happens.

1. Save your BC5 license **key block** (the multi-line text from Scooter) to a
   file, e.g. `~/bc5key.txt`.
2. Base64-encode it into `~/.dev_aliases`:
   ```bash
   # Linux
   echo "export BC5_LICENSE_KEY_B64=$(base64 -w0 ~/bc5key.txt)" >> ~/.dev_aliases
   # macOS
   echo "export BC5_LICENSE_KEY_B64=$(base64 -i ~/bc5key.txt | tr -d '\n')" >> ~/.dev_aliases
   ```
   (Base64 keeps the multi-line key on one line for the env-file and dodges
   quoting issues. **Never commit your key.**)
3. Rebuild/reopen the dev-non-root container. The `beyond-compare` `postCreateCommand`
   runs [`.devcontainer/install-bcompare.sh`](https://github.com/artifact-depot/artifact-depot/blob/main/.devcontainer/install-bcompare.sh),
   which installs Beyond Compare 5 and licenses it (writes `/etc/BC5Key.txt`).
   `git difftool` / `git mergetool` use it via `diff.tool=bc` / `merge.tool=bc`
   from your host `~/.gitconfig` (copied into the container by VS Code).

The **GUI** (`git difftool`) needs a forwarded display. On Linux, VS Code
forwards a Wayland socket automatically and the wrapper falls back to it. For
the crisp X11 (xcb) path, authorize the host X server for local clients once
with `xhost +local:` on the host — persist it via a GNOME autostart entry on
Wayland (`~/.config/autostart/`), or `~/.xprofile` on an X11 session. Otherwise
it uses Wayland. The install + license itself works with no display.

## Under the hood — the Dockerfile stages

```
node ─┐
      ▼
   builder ──► dev ──► dev-non-root      (CI: --target builder)
      │                                  (default container: dev)
      ▼                                  (dev-non-root container: dev-non-root)
 release-build ──► runtime               (release image; default build target)
```

One `Dockerfile`, one place that pins Node/Rust. `dev` is `FROM builder`,
`dev-non-root` is `FROM dev`, so each layer inherits the one before it. CI
targets `builder`; the release image is `runtime`. The dev tooling never
reaches CI or the shipped image because those target different stages.

## Notes

- **Git worktrees**: create them under `/worktrees` (a named volume that
  persists across rebuilds), or anywhere under `/workspace` (also a volume).
- The `/workspace` volume keeps sibling clones / scratch dirs across rebuilds;
  your checkout is bind-mounted into it at `/workspace/artifact-depot`.
