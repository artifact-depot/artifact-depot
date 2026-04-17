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
      openssh-client git npm curl wget vim make build-essential \
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

# CLI tooling, Rust toolchain, cargo workspace linters, reuse 3.x
# (Ubuntu 24.04 ships reuse 2.1 via apt, which predates .gitignore support).
RUN npm install -g @openai/codex && \
    curl -fsSL https://claude.ai/install.sh | bash && \
    echo 'export PATH="$HOME/.local/bin:$PATH"' >> /root/.bashrc && \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.94.1 && \
    ln -s /root/.cargo/bin/* /usr/local/bin/ && \
    cargo install cargo-deny cargo-about && \
    PIPX_BIN_DIR=/usr/local/bin pipx install reuse

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
