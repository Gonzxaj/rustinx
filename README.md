# rustinx

Docker image that runs Nginx supervised by a Rust manager. The goal (see `DEVELOPMENT.md`) is
automatic ACME certificate management; **the current state is Phase 1**: supervision, health
checks, persistence and graceful shutdown. There is no ACME, certificate issuance or renewal yet.

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

## Configuration

Environment variables:

| Variable | Default | Description |
|---|---|---|
| `RUSTINX_NGINX_BIN` | `/usr/sbin/nginx` | Nginx binary (absolute path) |
| `RUSTINX_NGINX_CONF` | `/etc/rustinx/nginx.conf` | Main Nginx configuration (absolute path) |
| `RUSTINX_DATA_DIR` | `/data` | Persistent data directory (absolute path, must exist) |
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
| `/etc/nginx/managed-conf.d/*.conf` | rustinx | Reserved for generated config (empty in Phase 1) |

`127.0.0.1:8081` inside the container is reserved for the health endpoint.

## Volumes and writable paths

| Path | Type | Content |
|---|---|---|
| `/data` | named volume | `accounts/`, `certificates/`, `challenges/` (reserved, `0700`), `state/runtime.json` (`0600`) |
| `/run/rustinx` | tmpfs | Nginx pid file |
| `/var/cache/rustinx` | tmpfs | Nginx temporary files |

State files are written atomically. A corrupt `runtime.json` is preserved as
`runtime.json.corrupt-<unix-time>` and a new one is created.

## Commands and exit codes

| Command | Description |
|---|---|
| `rustinx run` (default) | Supervise Nginx |
| `rustinx healthcheck` | Docker `HEALTHCHECK`: exit 0 if `/healthz` answers 200, 1 otherwise, 2 on invalid config |
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
| `make test-e2e` | Fails explicitly: no end-to-end ACME suite until Phase 2 |
| `make test-clean` | Removes leftovers of interrupted test runs (run-scoped only) |
| `make lint` | rustfmt, clippy `-D warnings`, shell syntax, shellcheck and hadolint (run in Docker) |
| `make security` | `cargo audit` and `cargo deny check` in a local `rustinx-tools` image |
| `make cache-clean CONFIRM=yes` | Removes `./target` only |
| `make release` | Builds `rustinx:<version>` locally; nothing is pushed |

See `TESTING.md` for the test isolation and cleanup rules.

## License

MIT. See `LICENSE`.
