# --- Build stage ---
ARG DOCKER_MIRROR=
FROM ${DOCKER_MIRROR}node:24.14.0-bookworm-slim AS node
FROM ${DOCKER_MIRROR}rust:1.94.1-bookworm AS builder

ARG TARGETARCH
ARG APT_MIRROR=
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
    && apt-get update && apt-get install -y --no-install-recommends musl-tools mold \
    && rm -rf /var/lib/apt/lists/*

COPY --from=node /usr/local/bin/node /usr/local/bin/
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm

WORKDIR /src
COPY . .

RUN RUST_TARGET=$(cat /rust-target) \
    && cargo build --release --features dynamodb --bin depot --target "$RUST_TARGET" \
    && cp "target/$RUST_TARGET/release/depot" /depot

# --- Runtime stage ---
FROM scratch

LABEL org.opencontainers.image.title="Artifact Depot" \
      org.opencontainers.image.description="Scale-out artifact repository manager" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.vendor="Quantum"

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /depot /depot

USER 65534:65534
EXPOSE 8080
VOLUME /data

ENTRYPOINT ["/depot"]
CMD ["-c", "/etc/depot/depotd.toml"]
