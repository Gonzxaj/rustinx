# TESTING.md

# nginx-rust — Testing and Validation Strategy

## 1. Purpose

This document defines how nginx-rust must be tested locally, against a real ACME Staging environment and on the user's Debian server.

The most important requirement is that tests must be reproducible and isolated.

The Debian server already hosts existing services. It must never be treated as a disposable test machine.

**No test may modify, stop, restart, replace or delete existing services or data without explicit authorization.**

## 2. Test environments

Maintain three independent environments.

| Environment | Purpose | ACME |
|---|---|---|
| Local Docker | Unit, integration and E2E tests | Local CA / Pebble |
| Real domain | DNS, challenge and certificate validation | Let's Encrypt Staging |
| Debian server | Deployment, persistence and long-running validation | Staging only |

Each environment must use separate configuration, credentials, certificates and persistent storage.

Never reuse production credentials or certificates in local tests.

## 3. Environment A — Local Docker

### Objective

Validate the application without depending on public DNS, router configuration or the user's home server.

### Requirements

- Docker Engine.
- Docker Compose.
- Rust toolchain for development.
- Local ACME test server such as Pebble.
- Isolated test networks.
- Temporary certificate and account storage.

### Test categories

#### Unit tests

Test individual components independently:

- Domain validation.
- Configuration parsing.
- Certificate metadata.
- Expiration calculations.
- Renewal scheduling.
- Retry policies.
- Storage operations.
- File permissions.
- State transitions.
- Challenge token generation.
- Error handling.

#### Integration tests

Test component interactions:

- Rust supervisor and Nginx.
- ACME client and local ACME server.
- Challenge manager and HTTP server.
- Certificate manager and storage.
- Configuration generator and Nginx validation.
- Renewal worker and certificate manager.
- Persistent storage across container restarts.

#### End-to-end tests

Validate complete workflows:

1. Start the isolated environment.
2. Start Nginx through the Rust supervisor.
3. Configure a test domain.
4. Create an ACME order.
5. Complete the challenge.
6. Obtain a test certificate.
7. Install the certificate.
8. Generate HTTPS configuration.
9. Validate Nginx configuration.
10. Reload Nginx.
11. Perform an HTTPS request.
12. Simulate certificate expiration.
13. Trigger renewal.
14. Verify the new certificate.
15. Restart the container.
16. Verify state recovery.

### Local HTTP-01

HTTP-01 must be tested locally before attempting public validation.

Use a local ACME server and isolated Docker networking.

Do not expose ports on the host unless the test explicitly requires it.

### Local DNS-01

When DNS-01 is implemented, use a mock DNS provider or a local test provider.

Do not use real Cloudflare credentials in ordinary unit or integration tests.

## 4. Environment B — Real domain and Let's Encrypt Staging

### Objective

Validate real DNS propagation, challenge completion, certificate issuance and renewal behavior.

### Domain

Primary domain:

`example.com`

DNS provider:

Cloudflare.

Dedicated test hostname:

`acme-test.example.com`

### DNS isolation

Only use the dedicated test hostname.

Do not modify:

- Root domain (`@`).
- `www`.
- Existing application subdomains.
- Existing MX, TXT or verification records.
- Nameservers.
- Existing Cloudflare Tunnel configuration.

Use a dedicated DNS record and dedicated ACME challenge records.

### Cloudflare credentials

Create a dedicated API token for the test environment.

Requirements:

- Restrict access to the `example.com` zone.
- Grant only the permissions required for DNS-01.
- Do not use a Global API Key.
- Never commit the token.
- Never print the token in logs.
- Store it through a secure environment mechanism or mounted secret.
- Rotate or revoke it when no longer required.

Do not include the token in command-line arguments if doing so would expose it in process listings.

### Let's Encrypt Staging

Use the official Staging ACME directory:

`https://acme-staging-v02.api.letsencrypt.org/directory`

Staging must be the default for real-domain validation.

Staging certificates are intentionally not trusted by normal browsers. This is expected and does not indicate a failure.

Never switch to the Production ACME endpoint automatically.

### DNS-01 validation

DNS-01 is the preferred method for real-domain validation on the Debian server because it does not require public access to TCP port 80.

Test:

- Cloudflare authentication.
- TXT record creation.
- TXT record propagation.
- ACME challenge validation.
- Certificate issuance.
- TXT record cleanup.
- Error handling.
- Retry behavior.
- Wildcard issuance, when supported.

Only delete TXT records created by the current challenge.

If an existing TXT record shares the same name, preserve it.

### HTTP-01 validation

HTTP-01 requires public access through TCP port 80.

Do not assume that a different host port is sufficient.

Before any public HTTP-01 test:

1. Inspect the Debian host's TCP port 80 listener.
2. Inspect existing Docker port mappings.
3. Identify the current reverse proxy or tunnel configuration.
4. Verify public routing.
5. Confirm whether the challenge can be served without changing existing infrastructure.

If port 80 is already used, do not stop or reconfigure the existing service.

Document the limitation and use DNS-01 for real-domain validation instead.

### Production protection

Production certificate issuance is not part of ordinary testing.

Any production test requires:

- Explicit user authorization.
- A clearly identified hostname.
- Confirmation of the intended ACME endpoint.
- Confirmation of the expected certificate storage location.
- A rollback plan.

Never use production issuance as an automated test.

## 5. Environment C — Debian server

### Objective

Validate that nginx-rust works on the user's actual Debian infrastructure without interfering with existing services.

### Non-interference policy

The Debian server is an existing environment, not a disposable test machine.

Without explicit authorization, tests must never:

- Stop existing Docker containers.
- Restart Docker.
- Restart or reconfigure host Nginx.
- Modify Cloudflare Tunnel.
- Change firewall rules.
- Change router port forwarding.
- Change existing DNS records.
- Delete existing Docker volumes.
- Delete existing Docker networks.
- Modify existing application directories.
- Install or remove system packages.
- Reboot the server.
- Run global cleanup commands.

If a test requires one of these actions, stop and request authorization.

### Initial preflight

Perform read-only inspection before deployment.

```bash
# Operating system
cat /etc/os-release

# Running processes and listeners
ss -lntup

# Running Docker containers and published ports
docker ps --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'

# Existing Docker networks
docker network ls

# Existing Docker volumes
docker volume ls

# Network interfaces
ip -br addr

# Routing table
ip route

# Docker version
docker version

# Compose version
docker compose version
```

These commands are inspection only.

Do not use their output to automatically modify the server.

### Isolated Docker deployment

The nginx-rust validation deployment must have:

- Its own Compose project name.
- Its own container names.
- Its own Docker network.
- Its own named volumes.
- Its own temporary directories.
- Its own configuration files.
- Its own certificate storage.
- Its own logs.

Use a dedicated directory outside existing application directories.

Example layout:

```text
/opt/nginx-rust-validation/
├── compose.yaml
├── .env
├── config/
├── data/
├── logs/
└── reports/
```

This is an example only. Do not create directories or deploy until the user authorizes the deployment location.

### Port isolation

Do not bind nginx-rust to host ports 80 or 443 if they are already occupied.

For local-only validation, prefer binding to loopback on unused alternative ports, for example:

```yaml
ports:
  - "127.0.0.1:18080:80"
  - "127.0.0.1:18443:443"
```

These ports are examples, not confirmed available ports.

Verify availability before using them.

Alternative ports are useful for local reverse proxy and HTTPS tests, but they do not make public HTTP-01 validation work.

DNS-01 should be used for real-domain certificate validation when the public HTTP challenge cannot be routed safely.

### Persistent storage

Use dedicated volumes for:

- ACME accounts.
- Certificates.
- Metadata.
- Application state.

Never mount existing application volumes.

Never reuse another project's data directory.

### Debian validation scenarios

- Container startup.
- Container restart.
- Rust supervisor recovery.
- Nginx health.
- Certificate persistence.
- Configuration persistence.
- DNS-01 Staging issuance.
- Certificate renewal simulation.
- Nginx reload.
- Invalid configuration recovery.
- Failed ACME request.
- Container shutdown.
- Multi-day stability.

## 6. Certificate lifecycle test matrix

| Scenario | Expected result |
|---|---|
| New certificate | Certificate issued and stored |
| Existing valid certificate | No unnecessary issuance |
| Certificate near expiration | Renewal scheduled |
| Successful renewal | New certificate installed |
| Failed renewal | Previous certificate remains active |
| Invalid private key | Operation rejected safely |
| Invalid certificate chain | Operation rejected safely |
| ACME unavailable | Retry with backoff |
| Rate limit | Retry policy respects CA response |
| Container restart | State recovered |
| Storage unavailable | Clear error without corrupting data |
| Nginx reload failure | Previous working configuration preserved |

## 7. Nginx safety tests

Validate:

- Valid configuration.
- Invalid configuration.
- Multiple domains.
- Multiple server blocks.
- Duplicate domain declarations.
- Reverse proxy behavior.
- HTTPS certificate installation.
- HTTP-to-HTTPS redirects.
- Reload without unnecessary downtime.
- Failed reload.
- Rollback after failure.
- User configuration remains unchanged.

A test must compare user-provided configuration before and after execution.

The application must only manage files that it owns.

## 8. Renewal testing

Do not wait for real certificate expiration.

Use an injectable clock and deterministic test fixtures.

Test:

- Certificate with more than 30 days remaining.
- Certificate with fewer than 30 days remaining.
- Expired certificate.
- Renewal success.
- Renewal failure.
- Worker restart.
- Duplicate renewal prevention.
- Retry behavior.
- ACME rate limits.

Use real Staging certificates for a controlled integration renewal test when appropriate.

Never accelerate the system clock on the Debian host.

## 9. Test isolation and resource ownership

Every test run must have a unique identifier.

Example:

`nginx-rust-test-<run-id>`

Resources created by the run must be explicitly tracked.

Track:

- Compose project.
- Containers.
- Networks.
- Volumes.
- Temporary directories.
- Generated configuration.
- Certificates.
- ACME accounts.
- Test logs.

Cleanup must operate only on resources recorded as belonging to the current run.

Never identify resources for deletion using broad name patterns alone.

### Cleanup behavior

Cleanup must:

1. Run after successful tests.
2. Run after failed tests.
3. Run after interrupted test runs whenever possible.
4. Collect logs before deleting test resources.
5. Remove only resources created by the current test run.
6. Preserve shared build caches.
7. Report cleanup failures.
8. Return a failed test-run status if cleanup fails.

Do not hide cleanup errors.

### Forbidden cleanup commands

Never use these as general test cleanup:

```bash
docker system prune
docker system prune -a
docker volume prune
docker network prune
docker container prune
```

Never run:

```bash
docker compose down -v
```

against an existing project or an unverified Compose project.

For isolated test projects, cleanup must explicitly identify the project and the resources created by the test run.

Do not delete shared Cargo caches or Docker build caches as part of normal tests.

## 10. Cache management

Cache cleanup is a separate, explicit operation.

Normal tests must preserve:

- Cargo registry cache.
- Cargo Git cache.
- Cargo build cache.
- Shared Docker build cache.

The `make cache-clean` target must:

- Explain what it will delete.
- Restrict deletion to project-owned caches.
- Never remove shared system caches.
- Never remove Docker resources unrelated to nginx-rust.
- Require explicit execution.

Cache cleanup must never run automatically after tests.

## 11. Makefile requirements

The project should expose these commands:

```bash
make build
make test
make test-unit
make test-integration
make test-e2e
make test-clean
make lint
make security
make cache-clean
make release
```

### Expected behavior

| Command | Purpose |
|---|---|
| `make build` | Build the application and Docker image |
| `make test` | Run the default test suite |
| `make test-unit` | Run unit tests |
| `make test-integration` | Run integration tests |
| `make test-e2e` | Run end-to-end tests |
| `make test-clean` | Clean resources created by test runs |
| `make lint` | Run formatting and static analysis |
| `make security` | Run dependency and security checks |
| `make cache-clean` | Explicitly clean project-owned caches |
| `make release` | Prepare a release build without publishing automatically |

No target may report success without executing its documented action.

## 12. Test reporting

Each test run must produce a report containing:

```text
Test run: <run-id>

Environment:
- Local Docker / Staging / Debian

Build:
- Result

Unit tests:
- Total
- Passed
- Failed
- Skipped

Integration tests:
- Total
- Passed
- Failed
- Skipped

E2E tests:
- Total
- Passed
- Failed
- Skipped

ACME:
- Provider
- Directory
- Challenge type
- Issuance result

Docker:
- Resources created
- Resource cleanup result

Logs:
- Location

Issues:
- Known failures
- Skipped scenarios
- Remaining limitations

Overall result:
- PASSED / FAILED / INCOMPLETE
```

Do not include secrets, private keys, API tokens or sensitive server configuration in reports.

A run is not considered successful if required cleanup failed.

## 13. Suggested test progression

### During Phase 1

- Build Docker image.
- Validate supervisor.
- Validate Nginx startup.
- Validate shutdown.
- Validate health checks.
- Validate container restart.

### During Phase 2

- Run local ACME tests.
- Validate HTTP-01.
- Validate certificate persistence.
- Validate failure handling.

### During Phase 3

- Validate domain discovery.
- Validate generated configuration.
- Validate safe reload.
- Validate rollback.

### During Phase 4

- Validate renewal scheduling.
- Simulate expiration.
- Validate recovery and retries.

### During Phase 5

- Validate DNS-01 with a mock provider.
- Validate Cloudflare integration.
- Issue a real-domain Staging certificate for `acme-test.example.com`.

### During Phase 6

- Run security checks.
- Validate Debian deployment in isolation.
- Validate persistence and long-running behavior.
- Verify no existing service was modified.

### During Phase 7

- Validate CI.
- Build AMD64 and ARM64.
- Validate release artifacts.
- Do not publish without authorization.

## 14. Final validation checklist

### Local

- [ ] Unit tests pass.
- [ ] Integration tests pass.
- [ ] E2E tests pass.
- [ ] Local ACME tests pass.
- [ ] HTTP-01 works locally.
- [ ] Renewal simulation works.
- [ ] Rollback works.
- [ ] Cleanup is successful.

### Cloudflare and real domain

- [ ] Dedicated hostname confirmed.
- [ ] Existing DNS records preserved.
- [ ] Dedicated API token configured securely.
- [ ] DNS-01 succeeds.
- [ ] Staging certificate issued.
- [ ] Certificate chain validated.
- [ ] TXT record cleanup verified.
- [ ] No production certificate issued accidentally.

### Debian

- [ ] Initial server state recorded.
- [ ] Existing listeners identified.
- [ ] Existing Docker resources preserved.
- [ ] Dedicated Compose project used.
- [ ] Dedicated volumes used.
- [ ] No existing service restarted.
- [ ] No existing firewall or routing rules modified.
- [ ] Persistence verified.
- [ ] Restart recovery verified.
- [ ] Cleanup verified.
- [ ] No interference with existing services.

### Release

- [ ] Security checks completed.
- [ ] Documentation updated.
- [ ] AMD64 build validated.
- [ ] ARM64 build validated.
- [ ] Git changes reviewed.
- [ ] No secrets committed.
- [ ] Release publication explicitly authorized.

## 16. Implemented tests and results

### Phase 1 suites

| Suite | Command | Location | What it uses |
|---|---|---|---|
| Unit | `make test-unit` | `src/*.rs` (`#[cfg(test)]`) | temp dirs, local TCP listeners |
| Host integration | `make test-host` | `tests/supervisor.rs` | real `rustinx` binary + `tests/fixtures/fake-nginx.sh`, temp dirs |
| Docker integration | `make test-docker` | `scripts/docker-integration.sh`, `tests/docker/compose.yaml` | real image with real Nginx |

Unit tests (24): configuration defaults/overrides/validation (relative paths, non-loopback health
address, timeouts), log format parsing, health probe (200, 500, refused, timeout), storage layout,
permissions, symlink rejection, atomic state, corrupt state preservation, future schema refusal,
explicit Nginx arguments, missing Nginx binary.

Host integration tests (12): SIGTERM and SIGINT graceful stop, invalid Nginx configuration
(exit 2), Nginx exiting during startup (exit 3), unexpected Nginx exit with an orphaned worker
that must be killed (exit 3), never healthy (exit 3), shutdown timeout escalating to SIGKILL,
persistence across restarts, missing data directory, invalid environment, `healthcheck` command.

Docker integration checks (40): build; startup and container health; test page served; health
endpoint not published; process tree (tini PID 1, one Nginx master, workers); uid 101, read-only
rootfs, `cap_drop ALL`; `/data` named volume, layout and start count; SIGTERM via `docker stop`
(exit 0, before the grace period, graceful SIGQUIT); container restart with persisted state;
SIGINT; stalled Nginx (SIGSTOP) reported unhealthy and recovered; Nginx master killed (exit 3,
leftover workers killed); container recreation keeps the volume; invalid user configuration
(exit 2, `nginx -t` error logged, never healthy); user configuration unchanged.

### Docker test isolation

- Run id `<UTC timestamp>-<random hex>`; Compose project `rustinx-test-<run-id>`; image
  `rustinx:test-<run-id>`.
- Every container, network, volume and image carries the label `rustinx.test-run=<run-id>`.
- Created resources are recorded in `target/test-runs/<run-id>/resources.manifest`.
- The host port is ephemeral and bound to `127.0.0.1`.
- On exit (success, failure, SIGINT/SIGTERM) logs are collected into
  `target/test-runs/<run-id>/logs/` (build, compose, per-container logs and inspect output),
  then cleanup removes only manifest entries whose label matches the run id, in the order
  containers → networks → volumes → images, and verifies that nothing labelled remains.
  Cleanup failures make the run fail. `cleanup.done` marks a cleaned run.
- `make test-clean` cleans runs without `cleanup.done` (e.g. a run killed with SIGKILL).
- No prune command and no `docker compose down -v` is used. The Cargo registry/target cache
  mounts and the Docker build cache are preserved.

### Results (2026-11-19, local Docker 29.8.2 rootless, linux/amd64)

| Check | Result |
|---|---|
| `make lint` | passed (rustfmt, clippy `-D warnings`, `bash -n`, shellcheck, hadolint in Docker) |
| `make test-unit` | 24 passed, 0 failed |
| `make test-host` | 12 passed, 0 failed (3 consecutive runs) |
| `make test-docker` | 40 passed, 0 failed (3 consecutive runs), cleanup success |
| Interrupted run (script SIGKILLed) + `make test-clean` | all recorded resources removed, nothing labelled left |
| `make build`, `make release` | passed (`rustinx:dev`, `rustinx:0.1.0`, about 244 MB) |
| `make dev-up` / `make dev-down` | container healthy, test page served on `127.0.0.1:18080` |
| `make security` | passed: cargo audit (58 crates, no advisories), cargo deny (advisories, bans, licenses, sources ok; duplicate `syn` 2/3 warning) |
| `make test-e2e` | NOT AVAILABLE: no ACME suite before Phase 2 (target fails explicitly) |

Issues found and fixed while testing: a host test expecting a startup-exit message while the
health endpoint was healthy; ANSI colour codes in container logs; Nginx workers surviving a
killed master; a false negative in log assertions (`grep -q` + `pipefail` SIGPIPE); shellcheck
SC2015 in the cleanup helper.

### Phase 2 local ACME suite

`make test-e2e` builds the runtime image and starts it with Pebble on an internal Docker network
without published ports. Pebble validates real HTTP-01 responses served by Nginx for reserved
`.test` names. The suite checks issuance for multiple names, private file modes, reuse of a current
certificate, account recovery after restarting rustinx, and rejection when Nginx returns 404.
It verifies that failure preserves the prior certificate and removes the HTTP-01 responses.
It also checks the effective configuration printed by `rustinx nginx-config`.

Each run records its labelled resources and writes logs under `target/test-runs/<run-id>/`.
Cleanup uses the same manifest mechanism as `make test-docker`. The test uses the
[Pebble test CA](https://github.com/letsencrypt/pebble/blob/main/test/certs/README.md).
Public Let's Encrypt Staging validation is a separate check using the dedicated host described
in section 4.

On 2026-11-24, Docker runs passed 37 Rust unit tests, 12 supervisor integration tests,
40 checks with real Nginx, and the Pebble suite. Rust formatting, Clippy with warnings denied
and ShellCheck on the ACME script passed. Both Docker integration suites cleaned up their
run-scoped resources.

### Phase 2 public Staging result

On 2026-11-24, the Docker development instance served the exact HTTP-01 probe body over
the public Cloudflare Tunnel route for `acme-test.example.com`. The same instance registered
an account and obtained a certificate from Let's Encrypt Staging. The full chain and private key
persisted in the Docker data volume, the key mode was `0600`, and the challenge directory was
removed after issuance.

### Phase 3 managed HTTPS result

On 2026-11-24, `make test-e2e` passed in Docker. It activated two Pebble-issued
certificates for three hostnames and verified the TLS chain and hostname on requests
through the managed Nginx servers. The shared reverse proxy served both HTTP and HTTPS.
The opt-in redirect preserved the path and query and left HTTP-01 reachable. The suite
rejected duplicate domains and an invalid Nginx snippet, then forced a reload timeout
and verified that the previous configuration kept serving HTTPS. A subsequent restart
restored the last confirmed HTTPS configuration.

The same revision passed 38 Rust unit tests, 12 supervisor integration tests, 40
real-Nginx Docker checks, Rust formatting, Clippy and ShellCheck on the integration script.

## 15. Final rule

When a test cannot be performed safely without modifying existing infrastructure, do not perform it.

Document the limitation, explain what would be required and request explicit authorization.

Protecting the user's existing services and data takes priority over completing a test.