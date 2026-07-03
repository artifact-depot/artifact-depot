# Single source of truth for the build/test/CI/dev/release toolchain.
#
# The `builder` stage holds everything `make` (lint + build + test + e2e)
# needs, so the Node/Rust toolchain is pinned in exactly one place and can
# never drift between CI, the dev container, and releases. Three consumers
# build on it:
#   - CI builds `--target builder` and runs `make` inside it
#     (.github/workflows/ci.yml).
#   - The local dev container builds `--target dev` — builder plus developer
#     conveniences — via .devcontainer/devcontainer.json.
#   - The release image compiles in `release-build` and ships `runtime`.
#
# `runtime` is intentionally the LAST stage so a bare `docker build .`
# (e.g. `make docker`) still produces the slim release image.

ARG DOCKER_MIRROR=
# Node is pinned in exactly one place: this image tag.
FROM ${DOCKER_MIRROR}node:24.14.0-bookworm-slim AS node

# --- Shared build / test / CI toolchain -----------------------------------
# Debian bookworm via the official Rust image (Rust 1.94.1 preinstalled). The
# Node binary is copied in from the `node` stage above. Everything `make`
# needs to lint, build, and run the full test suite lives here.
FROM ${DOCKER_MIRROR}rust:1.94.1-bookworm AS builder

ARG APT_MIRROR=
ARG TARGETARCH
# Repoint apt at an optional mirror (APT_MIRROR, empty by default), add the
# musl cross target for the static release binary, and install the toolchain.
# Package groups:
#   make/mold/musl-tools          -> compile (static musl release + debug)
#   python3/pipx                  -> reuse 3.x (license header lint)
#   default-jre-headless          -> DynamoDB Local (Java) integration test
#   docker.io/containerd          -> dockerd-in-container for the docker-auth
#                                    and format-compatibility e2e tests
#   iproute2                      -> network-namespace isolation (ui/ext tests)
#   nginx-light/openssl           -> ext-test fixtures
#   libasound2..libxrandr2 + xvfb -> Playwright (chromium) for the UI e2e tests
#   fonts-*                       -> font coverage for screenshot/UI tests
RUN if [ -n "$APT_MIRROR" ]; then \
      for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.sources /etc/apt/sources.list.d/*.list; do \
        [ -f "$f" ] || continue; \
        sed -Ei "s#https?://deb\.debian\.org/debian#http://${APT_MIRROR}#g; \
                 s#https?://security\.debian\.org/debian-security#http://${APT_MIRROR}#g" "$f"; \
      done; \
    fi \
    && case "$TARGETARCH" in \
      amd64) RUST_TARGET=x86_64-unknown-linux-musl ;; \
      arm64) RUST_TARGET=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && echo "$RUST_TARGET" > /rust-target \
    && rustup target add "$RUST_TARGET" \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
      make mold musl-tools \
      python3 pipx default-jre-headless \
      docker.io containerd iproute2 nginx-light openssl \
      libasound2 libatk-bridge2.0-0 libatk1.0-0 libatspi2.0-0 \
      libcairo2 libcups2 libdbus-1-3 libdrm2 libgbm1 libglib2.0-0 \
      libnspr4 libnss3 libpango-1.0-0 libx11-6 libxcb1 libxcomposite1 \
      libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2 \
      xvfb fonts-noto-color-emoji fonts-unifont libfontconfig1 libfreetype6 \
      xfonts-scalable fonts-liberation fonts-ipafont-gothic \
      fonts-wqy-zenhei fonts-tlwg-loma-otf fonts-freefont-ttf \
    && rm -rf /var/lib/apt/lists/*

# Node (pinned once via the `node` stage), with the bundled npm.
COPY --from=node /usr/local/bin/node /usr/local/bin/
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx

# Workspace linters / third-party-notice generators. cargo-about's binary is
# gated behind its `cli` feature, so it must be installed on its own with that
# feature (a bare `cargo install cargo-about` builds no binary yet exits 0).
# reuse is installed via pipx (Debian ships an older reuse); the
# charset-normalizer extra provides reuse's file-encoding backend. PIPX_HOME is
# a shared, world-readable path (not root's 0700 home) so the non-root dev user
# can run reuse too; chmod re-applies read/exec after the venv is created.
RUN cargo install cargo-deny \
    && cargo install cargo-about --features cli \
    && PIPX_HOME=/opt/pipx PIPX_BIN_DIR=/usr/local/bin pipx install 'reuse[charset-normalizer]' \
    && chmod -R a+rX /opt/pipx

# Symlink the Rust toolchain + cargo-installed tools onto the default PATH.
# The rust image exposes them via /usr/local/cargo/bin in $PATH, but Debian's
# /etc/profile resets PATH for login shells (which is how VS Code probes the
# container env), dropping that dir. /usr/local/bin is always on PATH, so
# linking here keeps cargo/rustc/clippy/cargo-deny/cargo-about available in
# interactive and login shells (e.g. `make` from the dev-container terminal).
RUN ln -sf /usr/local/cargo/bin/* /usr/local/bin/

# DynamoDB Local (Java) for the dynamodb integration test.
ENV DYNAMODB_LOCAL_DIR=/usr/local/lib/dynamodb-local
RUN mkdir -p "$DYNAMODB_LOCAL_DIR" \
    && wget -qO /tmp/dynamodb_local.tar.gz "https://d1ni2b6xgvw0s0.cloudfront.net/v2.x/dynamodb_local_latest.tar.gz" \
    && tar xzf /tmp/dynamodb_local.tar.gz -C "$DYNAMODB_LOCAL_DIR" \
    && rm -f /tmp/dynamodb_local.tar.gz

# CI mounts the runner-owned checkout into /workspace and runs `make` as root,
# so git flags "dubious ownership" and refuses to operate on the repo. Tools
# that shell out to git then misbehave -- notably `reuse lint`, whose
# .gitignore detection fails, so it scans the build output (target/,
# node_modules/) the debug build produced before lint runs and errors on it.
# Trust any bind-mounted repo so git-backed tooling works regardless of the
# checkout's owner. Dev containers UID-match the checkout, so this is a no-op
# there; the image is a single-purpose build/CI/dev container, so trust-all is
# acceptable.
RUN git config --system --add safe.directory '*'

# --- Community dev container layer (NOT built by CI or the release image) --
# Builder plus the developer conveniences most contributors want. Targeted by
# the default .devcontainer/devcontainer.json. CI targets `builder` and the
# release image targets `runtime`, so none of these tools reach those images.
FROM builder AS dev

# GitHub CLI is not in Debian; add its official apt repo. Then editor/shell
# conveniences, common dev utilities, and the docs-site (Ruby/Jekyll) build
# dependencies. (skopeo is handy here since depot is itself a registry.)
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
      openssh-client vim less gh \
      ripgrep jq shellcheck skopeo file bash-completion unzip \
      ruby-full ruby-bundler ruby-dev libffi-dev zlib1g-dev libyaml-dev \
    && rm -rf /var/lib/apt/lists/*

# hadolint (Dockerfile linter; backs the hadolint VS Code extension).
ENV HADOLINT_VERSION=2.12.0
RUN ARCH="$(uname -m | sed 's/aarch64/arm64/')" \
    && curl -fsSL -o /usr/local/bin/hadolint \
        "https://github.com/hadolint/hadolint/releases/download/v${HADOLINT_VERSION}/hadolint-Linux-${ARCH}" \
    && chmod 0755 /usr/local/bin/hadolint

# Helm (linting/templating the depot chart) and docker-compose v1 (the
# docker/standalone + distributed + observability-screenshots harnesses).
ENV HELM_VERSION=v3.16.2
RUN case "$(dpkg --print-architecture)" in \
        amd64) HELM_ARCH=amd64; COMPOSE_ARCH=x86_64 ;; \
        arm64) HELM_ARCH=arm64; COMPOSE_ARCH=aarch64 ;; \
        *) echo "unsupported architecture: $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac \
    && curl -fsSL "https://get.helm.sh/helm-${HELM_VERSION}-linux-${HELM_ARCH}.tar.gz" -o /tmp/helm.tar.gz \
    && tar xzf /tmp/helm.tar.gz -C /tmp \
    && mv "/tmp/linux-${HELM_ARCH}/helm" /usr/local/bin/helm \
    && rm -rf /tmp/helm.tar.gz "/tmp/linux-${HELM_ARCH}" \
    && curl -fsSL -o /usr/local/bin/docker-compose \
        "https://github.com/docker/compose/releases/download/1.29.2/docker-compose-Linux-${COMPOSE_ARCH}" \
    && chmod +x /usr/local/bin/docker-compose

# AI coding CLIs used in the dev container.
RUN npm install -g @openai/codex \
    && curl -fsSL https://claude.ai/install.sh | bash \
    && echo 'export PATH="$HOME/.local/bin:$PATH"' >> /root/.bashrc

# --- Full dev container layer (non-root, opt-in target) --------------------
# `dev` plus the fuller toolset and a sudo-capable non-root user, so the
# container runs as the host user (remoteUser + updateRemoteUserUID) with
# correct ownership on bind mounts. Targeted by .devcontainer/dev-non-root/.
# The baked UID/GID default to 1000; updateRemoteUserUID reconciles them to the
# host user at attach time. CI (builder) and the release image (runtime <-
# release-build <- builder) never include this stage.
FROM dev AS dev-non-root

# glab (GitLab CLI -- generic, arch-matched from gitlab.com) and the AWS CLI
# (for the S3 / DynamoDB storage backends). Installed as root before the
# USER switch below.
RUN ARCH="$(dpkg --print-architecture)" \
    && GLAB_TAG="$(curl -fsSL 'https://gitlab.com/api/v4/projects/gitlab-org%2Fcli/releases/permalink/latest' \
        | grep -o '"tag_name":"[^"]*"' | sed 's/"tag_name":"//;s/"//')" \
    && curl -fsSL "https://gitlab.com/gitlab-org/cli/-/releases/${GLAB_TAG}/downloads/glab_${GLAB_TAG#v}_linux_${ARCH}.tar.gz" \
        | tar -xz -C /tmp \
    && mv /tmp/bin/glab /usr/local/bin/glab \
    && rm -rf /tmp/bin
RUN curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-$(uname -m).zip" -o /tmp/awscliv2.zip \
    && unzip -q /tmp/awscliv2.zip -d /tmp \
    && /tmp/aws/install \
    && rm -rf /tmp/awscliv2.zip /tmp/aws

# Docker Buildx CLI plugin (needed for `make docker`). Debian's docker.io
# provides the engine and `docker` CLI but not the buildx plugin; the
# docker-in-docker devcontainer feature used to supply it. Drop the official
# plugin binary into the system cli-plugins dir so `docker buildx` resolves.
ENV BUILDX_VERSION=v0.35.0
RUN ARCH="$(dpkg --print-architecture)" \
    && mkdir -p /usr/local/lib/docker/cli-plugins \
    && curl -fsSL -o /usr/local/lib/docker/cli-plugins/docker-buildx \
        "https://github.com/docker/buildx/releases/download/${BUILDX_VERSION}/buildx-${BUILDX_VERSION}.linux-${ARCH}" \
    && chmod 0755 /usr/local/lib/docker/cli-plugins/docker-buildx

# Mark the loopback registry insecure for the in-container dockerd that
# .devcontainer/start-dockerd.sh launches on container start, so the
# docker-auth integration test can `docker login`/`pull` a depot bound on
# localhost without TLS chain validation against its self-signed cert --
# matching CI's `dockerd --insecure-registry 127.0.0.0/8`.
RUN mkdir -p /etc/docker \
    && printf '{"insecure-registries":["127.0.0.0/8"]}\n' > /etc/docker/daemon.json

# The rust base 0777s /usr/local/cargo so any uid can use the shared toolchain,
# but `cargo install` in the builder stage re-created the registry subdirs as
# root:root 0755 -- so the non-root user can't write the crate cache and
# `cargo build` fails with EACCES. Re-apply the base's intent recursively
# (world-writable, so it survives updateRemoteUserUID; keeps the prebaked
# registry usable without re-downloading). Root/CI are unaffected.
RUN chmod -R a+rwX /usr/local/cargo

ARG USERNAME=dev
ARG USER_UID=1000
ARG USER_GID=1000
RUN groupadd --gid "$USER_GID" "$USERNAME" \
    && useradd --uid "$USER_UID" --gid "$USER_GID" -m -s /bin/bash "$USERNAME" \
    && apt-get update \
    && apt-get install -y --no-install-recommends sudo \
    && rm -rf /var/lib/apt/lists/* \
    && echo "$USERNAME ALL=(ALL) NOPASSWD:ALL" > "/etc/sudoers.d/$USERNAME" \
    && chmod 0440 "/etc/sudoers.d/$USERNAME" \
    && groupadd -f docker \
    && usermod -aG docker "$USERNAME"
USER $USERNAME

# --- Release build ---------------------------------------------------------
# Compiles the static musl `depot` binary and stages the runtime filesystem.
FROM builder AS release-build

WORKDIR /src
COPY . .

# Build counter (commits on the built revision) for the `1.0.0+<counter>`
# version string. `.git/` is excluded by .dockerignore, so the counter cannot
# be derived from git here — CI passes it in via --build-arg. Empty arg leaves
# build.rs to fall back to the bare crate version.
ARG BUILD_COUNTER=
ENV DEPOT_BUILD_COUNTER=${BUILD_COUNTER}

RUN RUST_TARGET=$(cat /rust-target) \
    && cargo build --release --features dynamodb --bin depot --target "$RUST_TARGET" \
    && cp "target/$RUST_TARGET/release/depot" /depot

# Pre-create the /data layout so the runtime stage has directories owned
# by the unprivileged user (scratch has no mkdir/chown).
RUN mkdir -p /data-layout/kv /data-layout/blobs

# --- Runtime stage ---------------------------------------------------------
FROM scratch AS runtime

LABEL org.opencontainers.image.title="Artifact Depot" \
      org.opencontainers.image.description="Scale-out artifact repository manager" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.vendor="Quantum"

COPY --from=release-build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=release-build /depot /depot
COPY --from=release-build --chown=65534:65534 /src/docker/depotd.toml /etc/depot/depotd.toml
COPY --from=release-build --chown=65534:65534 /data-layout/ /data/

USER 65534:65534
EXPOSE 8080
VOLUME /data

ENTRYPOINT ["/depot"]
CMD ["-c", "/etc/depot/depotd.toml"]
