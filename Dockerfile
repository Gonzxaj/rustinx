# syntax=docker/dockerfile:1

ARG RUST_IMAGE=rust:1.99.1-trixie@sha256:8a4e90c7946b74626488b1446f712c0fe0ed1a596cbd3d871684bc9c5433ae07
ARG NGINX_IMAGE=nginx:1.31.10-alpine3.24-slim@sha256:c293b28729906d97fa5302fce3bd79bd27a3ea9a5d9604e21c6ff0ebf14549fc

# Compile Rust on the builder's native architecture, even when exporting a
# foreign runtime. The Rust musl target provides its own runtime and linker.
# hadolint ignore=DL3006
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS builder
ARG BUILDARCH
ARG TARGETARCH
WORKDIR /src
# hadolint ignore=DL3008
RUN set -eu; \
    case "$TARGETARCH" in \
        amd64) cpu=x86_64; cross_gcc=gcc-x86-64-linux-gnu; cross_headers=libc6-dev-amd64-cross ;; \
        arm64) cpu=aarch64; cross_gcc=gcc-aarch64-linux-gnu; cross_headers=libc6-dev-arm64-cross ;; \
        *) echo "Unsupported target architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    target="$cpu-unknown-linux-musl"; \
    rustup target add "$target"; \
    compiler='gcc'; archiver='ar'; \
    if [ "$BUILDARCH" != "$TARGETARCH" ]; then \
        apt-get update; \
        apt-get install -y --no-install-recommends "$cross_gcc" "$cross_headers"; \
        rm -rf /var/lib/apt/lists/*; \
        compiler="$cpu-linux-gnu-gcc"; archiver="$cpu-linux-gnu-ar"; \
    fi; \
    printf '%s\n' "$target" "$compiler" "$archiver" > /rustinx-target

COPY Cargo.toml Cargo.lock ./
COPY src ./src
# The pipeline inputs are fixed Rust target names validated by the case above.
# hadolint ignore=DL4006
RUN --mount=type=cache,id=rustinx-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=rustinx-cross-${BUILDARCH}-${TARGETARCH},target=/src/target \
    set -eu; \
    { read -r target; read -r compiler; read -r archiver; } < /rustinx-target; \
    export "CC_$(printf '%s' "$target" | tr '-' '_')=$compiler"; \
    export "AR_$(printf '%s' "$target" | tr '-' '_')=$archiver"; \
    export "CARGO_TARGET_$(printf '%s' "$target" | tr 'a-z-' 'A-Z_')_LINKER=rust-lld"; \
    cargo build --release --locked --target "$target"; \
    install -m 0755 "target/$target/release/rustinx" /usr/local/bin/rustinx

# hadolint ignore=DL3006
FROM ${NGINX_IMAGE} AS runtime
ARG BUILD_VERSION=dev
ARG BUILD_REVISION=unknown

LABEL org.opencontainers.image.title="rustinx" \
      org.opencontainers.image.description="Nginx supervised by a Rust manager"

# tini runs as PID 1 and reaps zombies; rustinx supervises Nginx.
# hadolint ignore=DL3018
RUN apk upgrade --no-cache \
    && apk add --no-cache ca-certificates curl tini \
    && install -d -o nginx -g nginx -m 0700 /data /run/rustinx /var/cache/rustinx \
    && install -d -o root -g root -m 0755 /etc/rustinx /etc/nginx/user_conf.d \
        /etc/nginx/managed-conf.d /usr/share/rustinx/html

COPY --from=builder /usr/local/bin/rustinx /usr/local/bin/rustinx
RUN /usr/local/bin/rustinx version
COPY docker/nginx.conf /etc/rustinx/nginx.conf
COPY docker/snippets/ /etc/rustinx/snippets/
COPY config/nginx/default.conf /etc/nginx/user_conf.d/default.conf
COPY docker/html/ /usr/share/rustinx/html/

# Unprivileged user of the base image (uid/gid 101). Binding ports 80/443 relies on
# net.ipv4.ip_unprivileged_port_start=0, which Docker sets by default for bridge networks.
LABEL org.opencontainers.image.title="rustinx" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${BUILD_VERSION}" \
      org.opencontainers.image.revision="${BUILD_REVISION}"

USER 101:101

EXPOSE 80 443
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=5s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/rustinx", "healthcheck"]

ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/rustinx"]
CMD ["run"]
