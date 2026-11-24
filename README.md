# rustinx

Docker image that runs Nginx supervised by a Rust manager. The goal (see `DEVELOPMENT.md`) is
automatic ACME certificate management. The image supervises Nginx and can issue certificates on
demand using ACME HTTP-01. HTTPS activation and background renewal are opt-in.

## How it works

```
tini (PID 1) ─ rustinx run ─ nginx master (own process group) ─ nginx workers
```

1. `rustinx` opens the data directory (`/data`) and records the start in `state/runtime.json`.
2. Runs `nginx -t -c /etc/rustinx/nginx.conf -e stderr`. Invalid configuration → exit code 2.
3. Starts `nginx -c /etc/rustinx/nginx.conf -e stderr -g 'daemon off;'` and waits until the
   internal endpoint `http://127.0.0.1:8081/healthz` answers 200.
4. On `SIGTERM`/`SIGINT` sends `SIGQUIT` to Nginx (graceful), `SIGKILL` after the shutdown
   timeout, then exits 0. If Nginx dies unexpectedly, rustinx kills any leftover Nginx process
   and exits 3, so the container restart policy decides what happens next.

`SIGHUP` is ignored for now (configuration reload is not implemented yet).

## Quick start (development)

```bash
make dev-up      # builds rustinx:dev and starts the rustinx-dev Compose project
curl http://127.0.0.1:18080/
make dev-logs
make dev-down    # stops and removes the containers; the data volume is kept
```

The dev Compose project publishes only on `127.0.0.1` (`RUSTINX_DEV_HTTP_PORT`, default 18080),
runs as uid 101 with a read-only root filesystem, `cap_drop: ALL` and `no-new-privileges`.

## Optional Cloudflare Tunnel

Copy `.env.example` to `.env` if you do not already have one. Set `TUNNEL_TOKEN` in
that file to the token from the Cloudflare dashboard. Git and the Docker build context
ignore `.env`. Compose mounts the token as a secret for the connector.

In Cloudflare, add a published application route for `acme-test.example.com` with
the service URL `http://rustinx:80`. Start the connector on the Rustinx Compose network:

```bash
docker compose -f docker-compose.yml -f docker-compose.tunnel.yml up -d --build
docker compose -f docker-compose.yml -f docker-compose.tunnel.yml logs -f cloudflared
```

Stop just the connector when the test is done:

```bash
docker compose -f docker-compose.yml -f docker-compose.tunnel.yml stop cloudflared
```

## Configuration

Environment variables:

| Variable | Default | Description |
|---|---|---|
| `RUSTINX_NGINX_BIN` | `/usr/sbin/nginx` | Nginx binary (absolute path) |
| `RUSTINX_NGINX_CONF` | `/etc/rustinx/nginx.conf` | Main Nginx configuration (absolute path) |
| `RUSTINX_DATA_DIR` | `/data` | Persistent data directory (absolute path, must exist) |
| `RUSTINX_CERTIFICATES_CONF` | `/etc/rustinx/certificates.toml` | Absolute path to the issuance configuration |
| `RUSTINX_HEALTH_ADDR` | `127.0.0.1:8081` | Internal health endpoint; must be a loopback address |
| `RUSTINX_STARTUP_TIMEOUT_SECS` | `10` | Time for Nginx to become healthy (1–300) |
| `RUSTINX_SHUTDOWN_TIMEOUT_SECS` | `8` | Graceful shutdown time before `SIGKILL` (1–300) |
| `RUSTINX_LOG` | `info` | Log filter (`tracing` `EnvFilter` syntax, e.g. `debug`) |
| `RUSTINX_LOG_FORMAT` | `text` | `text` or `json`; logs go to stderr |

Keep `RUSTINX_SHUTDOWN_TIMEOUT_SECS` below the container stop grace period (15 s in Compose).

Nginx configuration:

| Path | Owner | Description |
|---|---|---|
| `/etc/rustinx/nginx.conf` | image | Base config: pid, temp paths, health server, includes |
| `/etc/nginx/user_conf.d/*.conf` | user | Your `server` blocks; mount read-only. rustinx never modifies it |
| `/run/rustinx/https.conf` | rustinx | Active generated configuration, restored from `/data/state/https.conf` |

`127.0.0.1:8081` inside the container is reserved for the health endpoint.

## Issue a certificate with HTTP-01

Mount a TOML file at `/etc/rustinx/certificates.toml` in the running container. Start with
Let's Encrypt Staging and a hostname that you control:

```toml
[acme]
directory = "https://acme-staging-v02.api.letsencrypt.org/directory"
contact = ["mailto:admin@example.com"]
terms_of_service_agreed = true
challenge_timeout_secs = 90

[[certificate]]
id = "example"
domains = ["example.com", "www.example.com"]
```

Each domain must resolve to a port-80 virtual host that serves the challenge. In each relevant
`server` block, include `/etc/rustinx/snippets/acme-http01.conf`. The bundled example includes it
already. If you redirect traffic, put the redirect inside `location /` so it leaves the challenge
location reachable. When overriding `RUSTINX_DATA_DIR`, use a corresponding challenge alias for
`<data-dir>/challenges/http-01/` in your Nginx configuration.

Run `docker compose exec rustinx rustinx issue` in the Compose project that serves those domains.
The development Compose project listens on loopback and is suitable for local use. A public
Staging run requires HTTP access from the CA on port 80. For an isolated local run, use
`make test-e2e`, which uses Pebble and does not expose ports on the host.

The command reuses the persisted account and valid certificates for the same ACME directory and
domain list. Keep each certificate's `id` stable so its paths remain stable.

## Issue with DNS-01 and Cloudflare

DNS-01 validates through public TXT records, so the certificate issuer does not need to
reach Nginx on port 80. The HTTP-01 challenge remains the default per certificate.

1. In Cloudflare, create an API token with **Zone → DNS → Edit** restricted to the intended
   zone. Copy its Zone ID from the zone dashboard.
2. Create a local directory with `mkdir -m 0700 .rustinx-secrets` and save just the token
   in `.rustinx-secrets/cloudflare-dns-token`. Set the file mode to `0444` so the container's
   unprivileged user can read the mounted secret. The parent directory stays private and
   is excluded from Git and the Docker build context. This token is separate from the
   Cloudflare Tunnel token. Compose mounts it at `/run/secrets/cloudflare_dns_token`.
3. Copy `config/certificates.dns.example.toml` to `certificates.toml`, enter the Zone ID and
   replace the sample names in `domains` with your test hostname and its wildcard. The
   template uses Let's Encrypt Staging. Each DNS certificate uses
   `challenge = "dns-01"`.

```bash
docker compose -f docker-compose.yml -f docker-compose.dns.yml up -d --build
docker compose -f docker-compose.yml -f docker-compose.dns.yml exec rustinx rustinx issue
```

If you're also using the tunnel, include `-f docker-compose.tunnel.yml` on the Compose
commands. To mount another certificate config, set `RUSTINX_CERTIFICATES_FILE`.
Outside Compose, use `[cloudflare].token_file` to point at a readable secret file.

Rustinx waits until Cloudflare's public resolver returns the exact TXT value. This uses
the ACME `challenge_timeout_secs` budget. Rustinx tracks the Cloudflare records it creates
and retries cleanup on the next issuance attempt if a cleanup was interrupted. Keep the
same zone and token available while cleanup is pending.

Wildcard certificates can be issued with DNS-01 and referenced from a manually configured Nginx
server. Managed HTTPS continues to use exact names that match explicit port-80 servers.

## Enable managed HTTPS

Declare your domains with explicit `server_name` entries in a plain port-80 server under
`/etc/nginx/user_conf.d/`. Put the application's `location` and proxy directives in a separate
file, such as `sites/example.inc` under that mount. Include it in the HTTP server alongside
the HTTP-01 snippet:

```nginx
server {
    listen 80;
    server_name example.com www.example.com;
    include /etc/rustinx/snippets/acme-http01.conf;
    include /etc/rustinx/snippets/managed-redirect.conf;
    include /etc/nginx/user_conf.d/sites/example.inc;
}
```

After the corresponding `[[certificate]]` entry in `certificates.toml`, add:

```toml
[certificate.https]
include = "/etc/nginx/user_conf.d/sites/example.inc"
redirect_http = true
```

`redirect_http` defaults to `false`. To enable it, include the managed redirect snippet in the
port-80 server. The redirect preserves the hostname, path and query and leaves HTTP-01 reachable.
Keep application logic in the shared snippet and use absolute paths in Nginx `include` directives.
Use exact domain names on port 80 and give each managed domain a single HTTP server. Reserve port
443 for the generated server for those domains.

Run `docker compose exec rustinx rustinx issue` to obtain the certificate and activate HTTPS,
or `docker compose exec rustinx rustinx https` to activate using stored certificates. Rustinx
validates the candidate with `nginx -t`, then reloads Nginx and checks that workers serve the new
configuration before saving it. The last confirmed configuration is restored on restart.
To remove managed HTTPS, remove the `[certificate.https]` entries and run `rustinx https`.

Publish container TCP 443 in Docker if clients need direct HTTPS access to Nginx. The public
TLS connection at Cloudflare Tunnel is separate from the tunnel's configured origin protocol.

## Enable automatic renewal

Add a top-level `[renewal]` section to the mounted `certificates.toml` and restart the
Rustinx container. An empty section uses the following defaults:

```toml
[renewal]
renew_before_secs = 2592000
check_interval_secs = 43200
retry_initial_secs = 60
retry_max_secs = 3600
rate_limit_delay_secs = 172800
```

The worker runs after Nginx becomes ready and checks for missing, expired or soon-to-expire
certificates. The renewal window is capped at half of each certificate's lifetime to avoid
reissuing short-lived certificates immediately. HTTP-01 must remain reachable at renewal time.

The worker retries failures with bounded exponential backoff and persists its next attempt in
`/data/state/renewal.json`. Recognized ACME rate-limit errors use the configured longer delay.
`instant-acme` does not expose response headers on those errors, so this delay is a local setting.
For managed HTTPS, Rustinx reloads after changes to the certificate or effective configuration.
For HTTPS configured manually, the worker updates the PEM files and you manage the Nginx reload.

### Reference the PEM files in Nginx

Rustinx writes the files under `/data/certificates/<id>/`, where `<id>` is the value from
`certificates.toml`. For `id = "example"`, use these directives inside your HTTPS `server` block:

```nginx
ssl_certificate     /data/certificates/example/fullchain.pem;
ssl_certificate_key /data/certificates/example/privkey.pem;
```

Add the HTTPS block after the files have been issued. For direct HTTPS access, publish the
container's port 443 as part of your deployment. A Let's Encrypt Staging certificate is for
testing and is not trusted by ordinary browsers.

### Keep the PEM files in a host directory

To use a host directory like you would with a bind mount at `/etc/letsencrypt`, replace the
`data:/data` mount in your Compose service with the following entry. Keep your other mounts:

```yaml
services:
  rustinx:
    volumes:
      - ./.rustinx-data:/data
```

Create `.rustinx-data` before starting Compose and give container UID/GID `101:101` write access.
For a new directory, the initialization can run through Docker to apply the daemon's UID mapping.
Run the following after building the local `rustinx:dev` image:

```bash
mkdir -m 0700 .rustinx-data
docker run --rm --user 0:0 --entrypoint chown \
  -v "$PWD/.rustinx-data:/data" rustinx:dev 101:101 /data
```

After issuance, the host paths are `.rustinx-data/certificates/example/fullchain.pem` and
`.rustinx-data/certificates/example/privkey.pem`. Your Nginx directives still use the container
paths shown above. With this mount, the ACME account credentials also live in `.rustinx-data`.
Git and the Docker build context exclude this directory. Switching from an existing named volume
requires migrating its data if you want to preserve the same account and certificates.

### Copy PEM files from the default named volume

To copy just the chain and private key to a private temporary directory on your host, use the
following commands from the Compose project. Substitute the certificate ID if needed:

```bash
set -o pipefail
pem_dir="$(mktemp -d "${TMPDIR:-/tmp}/rustinx-pems.XXXXXXXX")"
docker compose exec -T rustinx tar -C /data/certificates/example -cf - fullchain.pem privkey.pem \
  | tar -xpf - -C "$pem_dir" &&
  printf 'PEM files copied to %s\n' "$pem_dir"
```

The private key has mode `0600`. If your host user cannot read the bind-mounted directory due
to Docker's UID mapping, use the same export command with the container running.

## Volumes and writable paths

| Path | Type | Content |
|---|---|---|
| `/data` | named volume | `accounts/`, `certificates/`, `challenges/` (private directories, `0700`), `state/runtime.json` (`0600`) |
| `/run/rustinx` | tmpfs | Nginx pid file |
| `/var/cache/rustinx` | tmpfs | Nginx temporary files |

State files are written atomically. A corrupt `runtime.json` is preserved as
`runtime.json.corrupt-<unix-time>` and a new one is created.

## Production image

The runtime uses the official `nginx:1.31.10-alpine3.24-slim` base. Rustinx is built for musl, and
the runtime adds only the CA certificates, curl and tini packages. The base omits optional
Nginx modules such as njs and XSLT. If you use those modules, maintain an image that
includes them and scan it separately. Certificate replacement uses Linux's atomic directory exchange. Use
a volume filesystem that supports `RENAME_EXCHANGE`, such as ext4 or XFS. Mount user
configuration read-only and keep the Compose privilege settings when deploying.

## Back up and restore certificate data

A backup contains private keys and ACME account credentials. Store the archive in an
access-controlled, encrypted location. Keep the Compose and Nginx configuration, certificate
requests, and any external token files in your deployment's separate configuration backup.

For a consistent snapshot, schedule a maintenance window and stop the Rustinx service before
archiving its volume. Use the same Compose options as the running deployment:

```bash
umask 077
backup="$(mktemp ./rustinx-data.XXXXXXXX.tar.gz)"
docker compose stop rustinx
docker compose run --rm -T --no-deps --entrypoint tar rustinx \
  -C /data -czf - accounts certificates state > "$backup"
docker compose start rustinx
```

Check that the archive was written successfully before relying on it. Restore into a **new,
empty volume** first. For the development Compose file, a distinct project name creates a
separate named volume. Use an unused project name and change the port before starting the
service if a development instance is already running:

```bash
backup=./rustinx-data.ABCDEFGH.tar.gz
docker compose -p rustinx-restore-check run --rm -T --no-deps --entrypoint sh rustinx -ec '
  test -z "$(find /data -mindepth 1 -print -quit)"
  tar --no-same-owner -C /data -xzf -
' < "$backup"
RUSTINX_DEV_HTTP_PORT=18081 docker compose -p rustinx-restore-check up -d --wait
```

Restore only an archive you created or otherwise trust. Before switching a live deployment
to the restored volume, verify Nginx health and the expected certificate files and expiry.
For a bind-mounted deployment, point the restore project at a new directory writable by the
container UID before running the restore. Keep the previous volume until verification succeeds.

## Commands and exit codes

| Command | Description |
|---|---|
| `rustinx run` (default) | Supervise Nginx |
| `rustinx healthcheck` | Docker `HEALTHCHECK`: exit 0 if `/healthz` answers 200, 1 otherwise, 2 on invalid config |
| `rustinx issue` | Issue missing or expired certificates, or replace certificates whose configured domains or ACME directory changed |
| `rustinx https` | Apply managed HTTPS configuration using stored certificates |
| `rustinx nginx-config` | Run `nginx -T` and print the effective configuration to stdout |
| `rustinx version` | Print the version |

| Exit code of `run` | Meaning |
|---|---|
| 0 | Stopped by `SIGTERM`/`SIGINT` |
| 1 | Internal error (data directory, process management, async runtime) |
| 2 | Invalid configuration (environment, logging settings or `nginx -t`) |
| 3 | Nginx exited during startup, never became healthy, or exited unexpectedly |

## Make targets

| Target | Description |
|---|---|
| `make build` | Release build and `rustinx:dev` image |
| `make test` | Unit + host integration + Docker integration tests |
| `make test-unit` / `make test-integration` | Subsets (`test-host`, `test-docker`) |
| `make test-e2e` | End-to-end HTTP-01 issuance with Pebble and real Nginx in Docker |
| `make test-clean` | Removes leftovers of interrupted test runs (run-scoped only) |
| `make lint` | rustfmt, clippy `-D warnings`, shell syntax, shellcheck and hadolint (run in Docker) |
| `make security` | `cargo audit` and `cargo deny check` in a local `rustinx-tools` image |
| `make image-security` | Scan the local image with Trivy, report all severities, fail on high or critical findings, and write a CycloneDX SBOM under `target/security/` |
| `make cache-clean CONFIRM=yes` | Removes `./target` only |
| `make release` | Builds `rustinx:<version>` locally; nothing is pushed |

See `TESTING.md` for the test isolation and cleanup rules.

## License

MIT. See `LICENSE`.
