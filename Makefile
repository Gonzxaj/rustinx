# rustinx developer commands. See TESTING.md section 11.
# No target pushes, publishes or removes Docker resources it did not create.

SHELL := /usr/bin/env bash
.SHELLFLAGS := -euo pipefail -c

VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
DEV_IMAGE := rustinx:dev
RELEASE_TAG := $(subst +,_,$(VERSION))
RELEASE_IMAGE := rustinx:$(RELEASE_TAG)
RELEASE_ARCHIVE := target/release/rustinx-$(VERSION).oci.tar
REVISION := $(shell git rev-parse --verify HEAD 2>/dev/null || echo unknown)$(shell test -z "$$(git status --porcelain 2>/dev/null)" || echo -dirty)
CI_RUST_IMAGE := rust:1.99.1-trixie
RELEASE_ARGS := --build-arg BUILD_VERSION=$(VERSION) --build-arg BUILD_REVISION=$(REVISION)
DEV_COMPOSE := docker compose -p rustinx-dev -f docker-compose.yml
# Linters and dependency checkers run in throwaway containers; nothing is installed on the host.
SHELLCHECK_IMAGE := koalaman/shellcheck:v0.10.0
HADOLINT_IMAGE := hadolint/hadolint:v2.12.0
TRIVY_IMAGE ?= aquasec/trivy:0.76.2
SECURITY_IMAGE := rustinx:security-local
SECURITY_REPORTS := $(CURDIR)/target/security
CARGO_AUDIT_VERSION := 0.23.0
CARGO_DENY_VERSION := 0.21.0
TOOLS_IMAGE := rustinx-tools:audit-$(CARGO_AUDIT_VERSION)-deny-$(CARGO_DENY_VERSION)
DOCKER_TOOL := docker run --rm --label rustinx.tools=1
SCRIPTS := $(wildcard scripts/*.sh scripts/lib/*.sh tests/fixtures/*.sh)

.PHONY: help build test test-unit test-integration test-host test-docker test-e2e test-clean \
	lint ci security image-security tools-image cache-clean release release-oci dev-up dev-down dev-logs

help:
	@echo "build             cargo release build + $(DEV_IMAGE) image"
	@echo "test              unit + integration tests (host and Docker)"
	@echo "test-unit         unit tests"
	@echo "test-integration  host integration tests (fake nginx) + Docker integration tests (real nginx)"
	@echo "test-e2e          end-to-end ACME HTTP-01 tests in Docker with Pebble"
	@echo "test-clean        remove leftover resources of interrupted test runs (run-scoped only)"
	@echo "lint              rustfmt, clippy, shell syntax, shellcheck and hadolint (in Docker)"
	@echo "ci                format, lint, test and audit in Docker"
	@echo "security          cargo audit + cargo deny (in a local tools image)"
	@echo "image-security    scan a local image and generate an SBOM in target/security"
	@echo "cache-clean       remove project-owned caches (requires CONFIRM=yes)"
	@echo "release           build $(RELEASE_IMAGE) locally with OCI metadata"
	@echo "release-oci       export and validate a local AMD64/ARM64 OCI archive"
	@echo "dev-up/dev-down/dev-logs  manage the rustinx-dev Compose project"

build:
	cargo build --release --locked
	docker build -t $(DEV_IMAGE) .

test: test-unit test-integration

test-unit:
	cargo test --locked --lib --bins

test-integration: test-host test-docker

test-host:
	cargo test --locked --test supervisor

test-docker:
	scripts/docker-integration.sh

test-e2e:
	scripts/acme-integration.sh

test-clean:
	scripts/test-clean.sh

lint:
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings
	for script in $(SCRIPTS); do bash -n "$$script"; done
	$(DOCKER_TOOL) -v "$(CURDIR):/mnt:ro" -w /mnt $(SHELLCHECK_IMAGE) -x $(SCRIPTS)
	for file in Dockerfile docker/tools.Dockerfile; do $(DOCKER_TOOL) -i $(HADOLINT_IMAGE) hadolint - < "$$file"; done

ci:
	$(DOCKER_TOOL) --init -v "$(CURDIR):/src:ro" -w /src -e CARGO_TARGET_DIR=/tmp/rustinx-target \
		$(CI_RUST_IMAGE) bash -ec 'rustup component add rustfmt clippy && cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked'
	$(DOCKER_TOOL) -v "$(CURDIR):/mnt:ro" -w /mnt $(SHELLCHECK_IMAGE) -x $(SCRIPTS)
	$(DOCKER_TOOL) -i $(HADOLINT_IMAGE) hadolint - < Dockerfile
	$(MAKE) security
	$(MAKE) test-docker
	$(MAKE) test-e2e
	$(MAKE) image-security

security: tools-image
	$(DOCKER_TOOL) -v "$(CURDIR):/src:ro" -w /src $(TOOLS_IMAGE) cargo audit --deny warnings
	$(DOCKER_TOOL) -v "$(CURDIR):/src:ro" -w /src $(TOOLS_IMAGE) cargo deny --locked check

image-security:
	mkdir -p "$(SECURITY_REPORTS)"
	docker build --pull -t $(SECURITY_IMAGE) .
	@archive="$$(mktemp "$(SECURITY_REPORTS)/image.XXXXXXXX")"; \
	trap 'rm -f "$$archive"' EXIT; \
	docker save -o "$$archive" $(SECURITY_IMAGE); \
	scan=( $(DOCKER_TOOL) -v "$$archive:/image.tar:ro" -v rustinx-trivy-cache:/cache $(TRIVY_IMAGE) image --no-progress --cache-dir /cache --input /image.tar ); \
	"$${scan[@]}" --format cyclonedx > "$(SECURITY_REPORTS)/image.cdx.json"; \
	"$${scan[@]}" --scanners vuln > "$(SECURITY_REPORTS)/vulnerabilities-all.txt"; \
	"$${scan[@]}" --scanners vuln --severity HIGH,CRITICAL --exit-code 1 > "$(SECURITY_REPORTS)/vulnerabilities.txt"

tools-image:
	docker build --label rustinx.tools=$(TOOLS_IMAGE) \
		--build-arg CARGO_AUDIT_VERSION=$(CARGO_AUDIT_VERSION) --build-arg CARGO_DENY_VERSION=$(CARGO_DENY_VERSION) \
		-t $(TOOLS_IMAGE) -f docker/tools.Dockerfile docker

cache-clean:
	@echo "cache-clean removes ONLY project-owned caches:"
	@echo "  - ./target (Cargo build output and test-run logs of this checkout)"
	@echo "It keeps ~/.cargo, Docker images and the shared Docker build cache."
	@if [ "$(CONFIRM)" != "yes" ]; then echo "Re-run with CONFIRM=yes to proceed." >&2; exit 1; fi
	@if [ -d target/test-runs ] && find target/test-runs -mindepth 1 -maxdepth 1 -type d \
		-exec test ! -e '{}/cleanup.done' ';' -print | grep -q .; then \
		echo "cache-clean: some test runs were not cleaned up; run 'make test-clean' first." >&2; exit 1; fi
	cargo clean

release:
	docker build --pull $(RELEASE_ARGS) -t $(RELEASE_IMAGE) .
	@echo "release: built $(RELEASE_IMAGE) locally"

release-oci:
	mkdir -p "$(dir $(RELEASE_ARCHIVE))"
	@archive="$$(mktemp "$(RELEASE_ARCHIVE).XXXXXXXX")"; \
	trap 'rm -f "$$archive"' EXIT; \
	docker buildx build --pull --platform linux/amd64,linux/arm64 --provenance=false \
		$(RELEASE_ARGS) -t $(RELEASE_IMAGE) --output "type=oci,dest=$$archive" .; \
	python3 scripts/verify-oci.py "$$archive" "$(VERSION)" "$(REVISION)"; \
	mv "$$archive" "$(RELEASE_ARCHIVE)"
	@echo "release: saved $(RELEASE_ARCHIVE) locally"

dev-up:
	$(DEV_COMPOSE) up -d --build --wait

dev-down:
	$(DEV_COMPOSE) down

dev-logs:
	$(DEV_COMPOSE) logs -f
