# syntax=docker/dockerfile:1

ARG RUST_IMAGE=rust:1.99.1-alpine3.24
ARG NGINX_IMAGE=nginx:1.31.10-alpine3.24-slim

# Use the same Alpine release for the musl builder and runtime.
# hadolint ignore=DL3006
FROM ${RUST_IMAGE} AS builder
# hadolint ignore=DL3018
RUN apk add --no-cache build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Cache mounts keep the Cargo registry and build artifacts between builds without
# storing them in image layers. They are never removed by the test suite.
RUN --mount=type=cache,id=rustinx-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=rustinx-target,target=/src/target \
    cargo build --release --locked \
    && install -m 0755 target/release/rustinx /usr/local/bin/rustinx

# hadolint ignore=DL3006
FROM ${NGINX_IMAGE} AS runtime

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
COPY docker/nginx.conf /etc/rustinx/nginx.conf
COPY docker/snippets/ /etc/rustinx/snippets/
COPY config/nginx/default.conf /etc/nginx/user_conf.d/default.conf
COPY docker/html/ /usr/share/rustinx/html/

# Unprivileged user of the base image (uid/gid 101). Binding ports 80/443 relies on
# net.ipv4.ip_unprivileged_port_start=0, which Docker sets by default for bridge networks.
LABEL org.opencontainers.image.title="rustinx" \
      org.opencontainers.image.licenses="MIT"

USER 101:101

EXPOSE 80 443
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=5s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/rustinx", "healthcheck"]

ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/rustinx"]
CMD ["run"]
