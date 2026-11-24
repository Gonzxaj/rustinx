# Changelog

## Unreleased

- Add CI for formatting, linting, Rust tests, Nginx integration, ACME end-to-end tests,
  and dependency and runtime image security scans.
- Add local AMD64 and ARM64 OCI release builds with version and revision labels.
  Validate the architecture index before keeping the archive.

## 0.1.0

- Supervise Nginx with graceful shutdown, health checks and persistent state.
- Issue certificates with ACME HTTP-01 or Cloudflare DNS-01. Support wildcard DNS-01
  requests and optional certificate renewal and managed HTTPS.
- Use Nginx Alpine Slim, store credentials and keys privately, and replace certificate
  sets atomically.
