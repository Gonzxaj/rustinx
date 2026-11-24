#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/resources.sh
source "$ROOT/scripts/lib/resources.sh"

RUN_ID="$(new_run_id)"
PROJECT="rustinx-test-$RUN_ID"
IMAGE="rustinx:test-$RUN_ID"
RUN_DIR="$ROOT/target/test-runs/$RUN_ID"
MANIFEST="$RUN_DIR/resources.manifest"
LOG_DIR="$RUN_DIR/logs"
FIXTURES="$RUN_DIR/fixtures"
mkdir -p "$LOG_DIR" "$FIXTURES"
: > "$MANIFEST"

export RUSTINX_TEST_RUN_ID="$RUN_ID" RUSTINX_TEST_IMAGE="$IMAGE" RUSTINX_ACME_FIXTURES="$FIXTURES"
compose=(docker compose -p "$PROJECT" -f "$ROOT/tests/acme/compose.yaml")

finish() {
    local status=$?
    trap - EXIT INT TERM
    set +e
    record_project_resources "$MANIFEST" "$PROJECT" "$RUN_ID"
    "${compose[@]}" logs --no-color > "$LOG_DIR/compose.log" 2>&1
    if cleanup_manifest "$MANIFEST" "$RUN_ID" > "$RUN_DIR/cleanup.log" 2>&1; then
        touch "$RUN_DIR/cleanup.done"
    else
        status=1
    fi
    printf 'ACME test run %s exited %s. Logs: %s\n' "$RUN_ID" "$status" "$LOG_DIR"
    exit "$status"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

record_resource "$MANIFEST" image "$IMAGE"
docker build --label "$RUN_LABEL=$RUN_ID" -t "$IMAGE" "$ROOT" > "$LOG_DIR/build.log" 2>&1
"${compose[@]}" up -d --wait --wait-timeout 60 rustinx
record_project_resources "$MANIFEST" "$PROJECT" "$RUN_ID"
"${compose[@]}" cp pebble:/test/certs/pebble.minica.pem "$FIXTURES/pebble-ca.pem"
chmod a+r "$FIXTURES/pebble-ca.pem"

in_rustinx() { "${compose[@]}" exec -T rustinx "$@"; }

deadline=$((SECONDS + 20))
until in_rustinx bash -c 'true </dev/tcp/127.0.0.1/14000' 2>/dev/null; do
    [ "$SECONDS" -lt "$deadline" ] || exit 1
    sleep 1
done

write_config() {
    cat > "$FIXTURES/$1.toml" <<CONFIG
[acme]
directory = "https://localhost:14000/dir"
terms_of_service_agreed = true
ca_root = "/fixtures/pebble-ca.pem"
challenge_timeout_secs = 30

[[certificate]]
id = "$2"
domains = [$3]
CONFIG
    chmod a+r "$FIXTURES/$1.toml"
}

issue() {
    in_rustinx env "RUSTINX_CERTIFICATES_CONF=/fixtures/$1.toml" rustinx issue 2>&1 | tee "$LOG_DIR/$1.log"
}

write_config first site '"rustinx.example.test", "www.rustinx.example.test"'
issue first
grep -qF 'ACME account registered' "$LOG_DIR/first.log"
grep -qF 'certificate issued' "$LOG_DIR/first.log"
# Expand these variables inside the test container.
# shellcheck disable=SC2016
in_rustinx sh -ec '
    dir=/data/certificates/site
    for name in cert.pem chain.pem fullchain.pem privkey.pem metadata.json; do
        test -s "$dir/$name"
        test "$(stat -c %a "$dir/$name")" = 600
    done
    set -- /data/accounts/*.json
    test "$#" -eq 1
    test "$(stat -c %a "$1")" = 600
    test ! -e /data/challenges/http-01
'
before="$(in_rustinx sha256sum /data/certificates/site/fullchain.pem)"

write_config repeat site '"rustinx.example.test", "www.rustinx.example.test"'
issue repeat
grep -qF 'existing certificate is current' "$LOG_DIR/repeat.log"
test "$before" = "$(in_rustinx sha256sum /data/certificates/site/fullchain.pem)"

"${compose[@]}" restart rustinx
"${compose[@]}" up -d --wait --wait-timeout 60 rustinx
write_config recover second '"other.rustinx.example.test"'
issue recover
grep -qF 'ACME account restored' "$LOG_DIR/recover.log"
grep -qF 'certificate issued' "$LOG_DIR/recover.log"

write_config rejected site '"invalid.rustinx.example.test"'
if issue rejected; then
    echo "Expected the CA to reject the challenge" >&2
    exit 1
fi
grep -qF 'polling HTTP-01 authorization' "$LOG_DIR/rejected.log"
test "$before" = "$(in_rustinx sha256sum /data/certificates/site/fullchain.pem)"
in_rustinx test ! -e /data/challenges/http-01

in_rustinx rustinx nginx-config > "$LOG_DIR/nginx-config.log"
grep -qF 'server_name rustinx.example.test www.rustinx.example.test;' "$LOG_DIR/nginx-config.log"
grep -qF 'alias /data/challenges/http-01/;' "$LOG_DIR/nginx-config.log"
echo "ACME issuance, restart, rejected challenge, cleanup and Nginx configuration checks passed"

in_rustinx curl --noproxy '*' --silent --show-error --fail --max-time 10 \
    --cacert /fixtures/pebble-ca.pem https://localhost:15000/roots/0 > "$FIXTURES/issuing-root.pem"
chmod a+r "$FIXTURES/issuing-root.pem"

write_config https site '"rustinx.example.test", "www.rustinx.example.test"'
cat >> "$FIXTURES/https.toml" <<'CONFIG'

[certificate.https]
include = "/etc/nginx/test-site.inc"
redirect_http = true

[[certificate]]
id = "second"
domains = ["other.rustinx.example.test"]

[certificate.https]
include = "/etc/nginx/test-site.inc"
CONFIG

http() {
    local host="$1" url_path="$2"
    shift 2
    in_rustinx curl --noproxy '*' --silent --show-error --max-time 5 \
        --resolve "$host:80:127.0.0.1" "http://$host$url_path" "$@"
}

https() {
    in_rustinx curl --noproxy '*' --silent --show-error --fail --cacert /fixtures/issuing-root.pem --max-time 5 \
        --resolve "$1:443:127.0.0.1" "https://$1/"
}

test "$(http rustinx.example.test /)" = 'upstream: http'
issue https
grep -qF 'managed HTTPS configuration loaded' "$LOG_DIR/https.log"
for host in rustinx.example.test www.rustinx.example.test other.rustinx.example.test; do
    test "$(https "$host")" = 'upstream: https'
done
test "$(http other.rustinx.example.test /)" = 'upstream: http'
test "$(http www.rustinx.example.test '/test?q=1' -o /dev/null -w '%{http_code} %{redirect_url}')" \
    = '308 https://www.rustinx.example.test/test?q=1'

in_rustinx sh -ec 'mkdir -p /data/challenges/http-01; printf probe > /data/challenges/http-01/phase-three-test'
test "$(http www.rustinx.example.test /.well-known/acme-challenge/phase-three-test)" = probe
in_rustinx rm /data/challenges/http-01/phase-three-test

apply_https() {
    in_rustinx env "RUSTINX_CERTIFICATES_CONF=/fixtures/$1.toml" rustinx https
}

printf 'unknown_test_directive;\n' > "$FIXTURES/broken.inc"
chmod a+r "$FIXTURES/broken.inc"
sed 's|/etc/nginx/test-site.inc|/fixtures/broken.inc|' "$FIXTURES/https.toml" > "$FIXTURES/broken.toml"
chmod a+r "$FIXTURES/broken.toml"
before="$(in_rustinx sha256sum /run/rustinx/https.conf /data/state/https.conf)"
if apply_https broken > "$LOG_DIR/invalid-https.log" 2>&1; then
    echo "Expected Nginx to reject invalid HTTPS config" >&2
    exit 1
fi
grep -qF 'unknown directive' "$LOG_DIR/invalid-https.log"
test "$before" = "$(in_rustinx sha256sum /run/rustinx/https.conf /data/state/https.conf)"
test "$(https www.rustinx.example.test)" = 'upstream: https'

sed 's/other.rustinx.example.test/www.rustinx.example.test/g' "$FIXTURES/https.toml" > "$FIXTURES/duplicate.toml"
chmod a+r "$FIXTURES/duplicate.toml"
if apply_https duplicate > "$LOG_DIR/duplicate-https.log" 2>&1; then
    echo "Expected discovery to reject duplicate certificate domains" >&2
    exit 1
fi
grep -qF 'multiple certificates are configured' "$LOG_DIR/duplicate-https.log"
test "$before" = "$(in_rustinx sha256sum /run/rustinx/https.conf /data/state/https.conf)"

# Block the master from reloading while the old workers continue to serve traffic.
master="$(in_rustinx cat /run/rustinx/nginx.pid)"
# shellcheck disable=SC2016
in_rustinx sh -c 'kill -STOP "$1"' sh "$master"
reload_status=0
in_rustinx env RUSTINX_STARTUP_TIMEOUT_SECS=1 RUSTINX_CERTIFICATES_CONF=/fixtures/https.toml \
    rustinx https > "$LOG_DIR/reload-timeout.log" 2>&1 || reload_status=$?
# shellcheck disable=SC2016
in_rustinx sh -c 'kill -CONT "$1"' sh "$master"
test "$reload_status" -ne 0
grep -qF 'after reload' "$LOG_DIR/reload-timeout.log"
test "$before" = "$(in_rustinx sha256sum /run/rustinx/https.conf /data/state/https.conf)"
test "$(https www.rustinx.example.test)" = 'upstream: https'

"${compose[@]}" restart rustinx
"${compose[@]}" up -d --wait --wait-timeout 60 rustinx
test "$(https www.rustinx.example.test)" = 'upstream: https'
echo "Managed HTTPS, proxy, redirect, invalid config rejection, rollback and restart checks passed"
