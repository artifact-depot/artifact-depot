ARG DOCKER_MIRROR=
FROM ${DOCKER_MIRROR}ubuntu:24.04

ARG APT_MIRROR=
# Narrow the sources to the components and suites we actually consume
# (main + universe; no backports). This also lets APT_MIRROR point at
# mirrors that do not expose restricted, multiverse, or noble-backports.
RUN sed -Ei \
        -e 's/^Components:.*/Components: main universe/' \
        -e 's/ noble-backports//' \
        /etc/apt/sources.list.d/ubuntu.sources \
    && if [ -n "$APT_MIRROR" ]; then \
         sed -Ei "s#https?://(archive|security)\.ubuntu\.com/ubuntu/?#http://${APT_MIRROR}/#g" \
           /etc/apt/sources.list.d/ubuntu.sources; \
       fi

# Unminimize, then install everything from apt in a single pass.
RUN apt-get -y update && \
    apt-get -y --no-install-recommends upgrade && \
    apt-get -y --no-install-recommends install unminimize && \
    echo y | unminimize && \
    apt-get -y --no-install-recommends install \
      openssh-client git curl wget vim make build-essential \
      iproute2 python3 pipx default-jre-headless mold gh \
      docker.io containerd nginx-light openssl \
      libasound2t64 libatk-bridge2.0-0t64 libatk1.0-0t64 libatspi2.0-0t64 \
      libcairo2 libcups2t64 libdbus-1-3 libdrm2 libgbm1 libglib2.0-0t64 \
      libnspr4 libnss3 libpango-1.0-0 libx11-6 libxcb1 libxcomposite1 \
      libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2 \
      xvfb fonts-noto-color-emoji fonts-unifont libfontconfig1 libfreetype6 \
      xfonts-cyrillic xfonts-scalable fonts-liberation fonts-ipafont-gothic \
      fonts-wqy-zenhei fonts-tlwg-loma-otf fonts-freefont-ttf && \
    rm -rf /var/lib/apt/lists/*

# Node.js (matches the release image's node:24.14.0). Ubuntu 24.04's apt
# `npm` pulls Node 18, which is EOL and too old for the frontend toolchain
# (Vite 8 requires Node ^20.19 || >=22.12, and crashes on 18 with
# "CustomEvent is not defined"). Install the official tarball directly, the
# same way Helm/dynamodb-local are fetched below, rather than via apt.
ENV NODE_VERSION=v24.14.0
RUN case "$(uname -m)" in \
        x86_64)  NODE_ARCH=x64 ;; \
        aarch64) NODE_ARCH=arm64 ;; \
        *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;; \
    esac && \
    curl -fsSL "https://nodejs.org/dist/${NODE_VERSION}/node-${NODE_VERSION}-linux-${NODE_ARCH}.tar.gz" -o /tmp/node.tar.gz && \
    tar -xzf /tmp/node.tar.gz -C /usr/local --strip-components=1 && \
    rm -f /tmp/node.tar.gz && \
    node --version && npm --version

# CLI tooling, Rust toolchain, cargo workspace linters, reuse 3.x
# (Ubuntu 24.04 ships reuse 2.1 via apt, which predates .gitignore support).
RUN npm install -g @openai/codex && \
    curl -fsSL https://claude.ai/install.sh | bash && \
    echo 'export PATH="$HOME/.local/bin:$PATH"' >> /root/.bashrc && \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.94.1 && \
    ln -s /root/.cargo/bin/* /usr/local/bin/ && \
    cargo install cargo-deny cargo-about && \
    PIPX_BIN_DIR=/usr/local/bin pipx install 'reuse[charset-normalizer]'

# DynamoDB Local (for integration testing)
ENV DYNAMODB_LOCAL_DIR=/usr/local/lib/dynamodb-local
RUN mkdir -p "$DYNAMODB_LOCAL_DIR" && \
    wget -qO /tmp/dynamodb_local.tar.gz "https://d1ni2b6xgvw0s0.cloudfront.net/v2.x/dynamodb_local_latest.tar.gz" && \
    tar xzf /tmp/dynamodb_local.tar.gz -C "$DYNAMODB_LOCAL_DIR" && \
    rm -f /tmp/dynamodb_local.tar.gz

# Helm (for linting and templating the depot Helm chart)
ENV HELM_VERSION=v3.16.2
RUN curl -fsSL "https://get.helm.sh/helm-${HELM_VERSION}-linux-amd64.tar.gz" -o /tmp/helm.tar.gz && \
    tar xzf /tmp/helm.tar.gz -C /tmp && \
    mv /tmp/linux-amd64/helm /usr/local/bin/helm && \
    rm -rf /tmp/helm.tar.gz /tmp/linux-amd64

# Ruby + Bundler for the Jekyll docs site (just-the-docs theme).
# Ubuntu 24.04 ships Ruby 3.2 in `ruby-full`, which is sufficient for
# Jekyll 4 + just-the-docs 0.10.  The native-gem build tools (libffi-dev,
# zlib1g-dev, libyaml-dev) are needed because sass-embedded compiles native
# extensions during `bundle install`.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ruby-full ruby-bundler ruby-dev \
        libffi-dev zlib1g-dev libyaml-dev && \
    rm -rf /var/lib/apt/lists/*

# docker-compose v1 (standalone upstream binary) for docker/standalone +
# docker/distributed + the observability-screenshots harness.  The host's
# Docker Engine daemon is reached via the mounted /var/run/docker.sock.
#
# We grab the PyInstaller-bundled Linux x86_64 release directly from
# GitHub rather than the Ubuntu `docker-compose` apt package, which on
# 24.04 is broken: it imports `distutils` at load time and Python 3.12
# dropped `distutils` from stdlib.
RUN curl -fsSL -o /usr/local/bin/docker-compose \
        "https://github.com/docker/compose/releases/download/1.29.2/docker-compose-Linux-x86_64" && \
    chmod +x /usr/local/bin/docker-compose
