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

# Pre-create the data directory so it exists in the runtime image with the
# correct ownership.  Without this, the empty `VOLUME /depot` declaration
# below would let Docker create the path on the fly at first mount, owned
# by root, and our non-root server (uid 65534) would fail with EACCES on
# its first redb open / blob write.  The placeholder file forces COPY to
# materialise the directory.
RUN mkdir -p /out/depot && touch /out/depot/.keep

# Generate a default config baked into the image so the container is usable
# with just `docker run`. Users can override by bind-mounting their own config
# at /etc/depot/depotd.toml.
RUN mkdir -p /out/etc/depot && cat > /out/etc/depot/depotd.toml <<'EOF'
# Default config baked into the Artifact Depot container image.
# Override by bind-mounting your own file at /etc/depot/depotd.toml.
#
# On first start a random admin password is generated and printed to the
# container log.  Set `default_admin_password = "..."` here (via your own
# config) to pick one yourself.

[http]
listen = "0.0.0.0:8080"

# Embedded redb KV store. Mount /depot as a volume for persistence.
[kv_store]
type = "redb"
path = "/depot/db"

# Blob stores are managed via the REST API, not this file.  On first start
# (from the UI or curl, using the admin password printed to the container
# log), create a filesystem blob store rooted at /depot/blobs:
#
#   curl -u admin:<password> -X POST http://localhost:8080/api/v1/stores \
#     -H 'Content-Type: application/json' \
#     -d '{"name":"default","store_type":"file","root":"/depot/blobs"}'
EOF

# --- Runtime stage ---
FROM scratch

LABEL org.opencontainers.image.title="Artifact Depot" \
      org.opencontainers.image.description="Scale-out artifact repository manager" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.vendor="Quantum"

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /depot /depot
COPY --from=builder --chown=65534:65534 /src/docker/depotd.toml /etc/depot/depotd.toml
COPY --from=builder --chown=65534:65534 /data-layout/ /data/

USER 65534:65534
EXPOSE 8080
VOLUME /data

ENTRYPOINT ["/depot"]
CMD ["-c", "/etc/depot/depotd.toml"]
