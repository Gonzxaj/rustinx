# Image with the dependency checkers used by `make security`, so nothing is installed on the host.
# Built locally as rustinx-tools:<versions>; never shipped or published.
ARG RUST_IMAGE=rust:1.99.1-trixie
ARG RUST_SLIM_IMAGE=rust:1.99.1-slim-trixie

# hadolint ignore=DL3006
FROM ${RUST_IMAGE} AS builder
ARG CARGO_AUDIT_VERSION
ARG CARGO_DENY_VERSION
RUN cargo install --locked --root /tools \
    "cargo-audit@${CARGO_AUDIT_VERSION:?}" "cargo-deny@${CARGO_DENY_VERSION:?}"

# cargo-deny needs cargo (cargo metadata) and git (advisory database), so the runtime is the slim
# Rust image plus git.
# hadolint ignore=DL3006
FROM ${RUST_SLIM_IMAGE}
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tools/bin/cargo-audit /tools/bin/cargo-deny /usr/local/cargo/bin/
