#!/usr/bin/env bash
# Docker integration tests: the real image, with Nginx supervised by rustinx.
#
# Isolation: unique run id, dedicated Compose project, labelled resources recorded in
# target/test-runs/<run-id>/resources.manifest. Logs are collected before cleanup, and cleanup
# removes only the recorded resources that carry the run label. Build caches are preserved.
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
REPORT="$RUN_DIR/report.txt"
WAIT_SECS=60

mkdir -p "$LOG_DIR"
: > "$MANIFEST"
cp -R "$ROOT/config/nginx" "$RUN_DIR/user_conf"
mkdir -p "$RUN_DIR/bad_conf"
printf 'server {\n    listen 80;\n    this_is_not_a_directive on;\n}\n' > "$RUN_DIR/bad_conf/broken.conf"
chmod -R a+rX "$RUN_DIR/user_conf" "$RUN_DIR/bad_conf"

export RUSTINX_TEST_RUN_ID="$RUN_ID"
export RUSTINX_TEST_IMAGE="$IMAGE"
export RUSTINX_TEST_USER_CONF="$RUN_DIR/user_conf"
export RUSTINX_TEST_BAD_CONF="$RUN_DIR/bad_conf"
compose=(docker compose -p "$PROJECT" -f "$ROOT/tests/docker/compose.yaml" --profile badconf)

PASSED=0
FAILED=0
RESULTS=()
BUILD_RESULT="not run"
CLEANUP_RESULT="not run"

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
pass() { PASSED=$((PASSED + 1)); RESULTS+=("PASS  $1"); log "PASS $1"; }
fail() { FAILED=$((FAILED + 1)); RESULTS+=("FAIL  $1: $2"); log "FAIL $1: $2"; }

record() { record_project_resources "$MANIFEST" "$PROJECT" "$RUN_ID"; }
cid() { "${compose[@]}" ps -aq "$1"; }
inspect() { docker inspect --format "$1" "$2"; }
health() { inspect '{{if .State.Health}}{{.State.Health.Status}}{{end}}' "$1"; }
in_container() { "${compose[@]}" exec -T rustinx "$@"; }
start_count() { in_container cat /data/state/runtime.json | jq -r .start_count; }

wait_health() {
    local id="$1" want="$2" deadline=$((SECONDS + WAIT_SECS))
    until [ "$(health "$id")" = "$want" ]; do
        [ "$SECONDS" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}

wait_exited() {
    local id="$1" deadline=$((SECONDS + WAIT_SECS))
    until [ "$(inspect '{{.State.Status}}' "$id")" = "exited" ]; do
        [ "$SECONDS" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}

expect() {
    # expect NAME ACTUAL EXPECTED
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1" "expected '$3', got '$2'"; fi
}

expect_log() {
    # expect_log NAME CONTAINER NEEDLE
    # Capture first: with pipefail, `grep -q` exiting early would SIGPIPE `docker logs`.
    local logs
    logs="$(docker logs "$2" 2>&1)"
    if grep -qF -- "$3" <<< "$logs"; then pass "$1"; else fail "$1" "log does not contain '$3'"; fi
}

save_container_logs() {
    local id
    while read -r kind id; do
        [ "$kind" = container ] || continue
        docker inspect "$id" > /dev/null 2>&1 || continue
        docker logs --timestamps "$id" > "$LOG_DIR/container-$id.log" 2>&1 || true
        docker inspect "$id" > "$LOG_DIR/container-$id.inspect.json" 2>/dev/null || true
    done < "$MANIFEST"
}

write_report() {
    local overall="PASSED"
    { [ "$FAILED" -eq 0 ] && [ "$CLEANUP_RESULT" = "success" ] && [ "$BUILD_RESULT" = "success" ]; } || overall="FAILED"
    {
        echo "Test run: $RUN_ID"
        echo
        echo "Environment:"
        echo "- Local Docker ($(docker version --format '{{.Server.Version}}'))"
        echo
        echo "Build:"
        echo "- $BUILD_RESULT ($IMAGE)"
        echo
        echo "Docker integration tests:"
        echo "- Total: $((PASSED + FAILED))"
        echo "- Passed: $PASSED"
        echo "- Failed: $FAILED"
        echo "- Skipped: 0"
        printf '  %s\n' "${RESULTS[@]}"
        echo
        echo "ACME:"
        echo "- Not applicable (Phase 1)"
        echo
        echo "Docker:"
        echo "- Compose project: $PROJECT"
        echo "- Resources created:"
        sed 's/^/  /' "$MANIFEST"
        echo "- Resource cleanup result: $CLEANUP_RESULT"
        echo
        echo "Logs:"
        echo "- $LOG_DIR"
        echo
        echo "Overall result:"
        echo "- $overall"
    } > "$REPORT"
    cat "$REPORT"
    [ "$overall" = "PASSED" ]
}

finish() {
    local status=$?
    trap - EXIT INT TERM
    set +e
    record
    log "collecting logs into $LOG_DIR"
    "${compose[@]}" logs --no-color --timestamps > "$LOG_DIR/compose.log" 2>&1
    save_container_logs
    log "cleaning up resources of run $RUN_ID"
    if cleanup_manifest "$MANIFEST" "$RUN_ID" | tee "$RUN_DIR/cleanup.log"; [ "${PIPESTATUS[0]}" -eq 0 ]; then
        CLEANUP_RESULT="success"
        touch "$RUN_DIR/cleanup.done"
    else
        CLEANUP_RESULT="FAILED (see $RUN_DIR/cleanup.log)"
    fi
    if [ "$status" -ne 0 ] && [ "$FAILED" -eq 0 ]; then fail "script" "aborted with status $status"; fi
    write_report
    exit $?
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

log "run $RUN_ID: building $IMAGE"
record_resource "$MANIFEST" image "$IMAGE"
if docker build --label "$RUN_LABEL=$RUN_ID" -t "$IMAGE" "$ROOT" > "$LOG_DIR/build.log" 2>&1; then
    BUILD_RESULT="success"
else
    BUILD_RESULT="FAILED (see $LOG_DIR/build.log)"
    exit 1
fi
user_conf_sum="$(cd "$RUN_DIR/user_conf" && find . -type f -exec sha256sum {} + | sort)"

# --- Startup -------------------------------------------------------------------------------
log "scenario: startup"
"${compose[@]}" up -d --wait --wait-timeout "$WAIT_SECS" rustinx > "$LOG_DIR/up-1.log" 2>&1 || true
record
app="$(cid rustinx)"
expect "startup: container healthy" "$(health "$app")" "healthy"
expect_log "startup: rustinx validated nginx config" "$app" "nginx configuration is valid"
expect_log "startup: rustinx reports nginx healthy" "$app" "nginx is healthy"
addr="$("${compose[@]}" port rustinx 80)"
expect "nginx: serves test page" "$(curl -fsS --max-time 5 "http://$addr/" | grep -c 'rustinx is running')" "1"
expect "nginx: health endpoint not published" "$(inspect '{{json .NetworkSettings.Ports}}' "$app" | grep -c 8081 || true)" "0"
processes="$(docker top "$app" -eo pid,ppid,user,args)"
printf '%s\n' "$processes" > "$LOG_DIR/processes.txt"
expect "process tree: tini is PID 1" "$(printf '%s\n' "$processes" | grep -c '/usr/bin/tini -- /usr/local/bin/rustinx run')" "1"
expect "process tree: single nginx master" "$(printf '%s\n' "$processes" | grep -c 'nginx: master process')" "1"
expect "process tree: nginx workers running" "$(printf '%s\n' "$processes" | grep -c 'nginx: worker process' | sed 's/^[1-9][0-9]*$/ok/')" "ok"
expect "security: runs as uid 101" "$(in_container id -u)" "101"
expect "security: read-only rootfs" "$(inspect '{{.HostConfig.ReadonlyRootfs}}' "$app")" "true"
expect "security: capabilities dropped" "$(inspect '{{json .HostConfig.CapDrop}}' "$app")" '["ALL"]'
expect "persistence: /data is a named volume" \
    "$(inspect '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Type}} {{.Name}}{{end}}{{end}}' "$app")" \
    "volume ${PROJECT}_data"
expect "persistence: layout created" "$(in_container ls /data | tr '\n' ' ')" "accounts certificates challenges state "
expect "persistence: first start recorded" "$(start_count)" "1"
expect "healthcheck: command reports healthy" "$(in_container rustinx healthcheck)" "healthy"

# --- SIGTERM and container restart -----------------------------------------------------------
log "scenario: SIGTERM (docker stop)"
started=$SECONDS
"${compose[@]}" stop rustinx > /dev/null 2>&1
elapsed=$((SECONDS - started))
expect "sigterm: exit code 0" "$(inspect '{{.State.ExitCode}}' "$app")" "0"
expect "sigterm: stopped before docker would SIGKILL" "$([ "$elapsed" -lt 15 ] && echo yes || echo "no (${elapsed}s)")" "yes"
expect_log "sigterm: graceful nginx shutdown" "$app" "graceful nginx shutdown requested (SIGQUIT)"
expect_log "sigterm: nginx exited before rustinx" "$app" "nginx stopped"
expect_log "sigterm: rustinx stopped cleanly" "$app" "rustinx stopped"

log "scenario: container restart"
"${compose[@]}" start rustinx > /dev/null 2>&1 || true
if wait_health "$app" healthy; then pass "restart: healthy again"; else fail "restart: healthy again" "$(health "$app")"; fi
expect "restart: state persisted (start count 2)" "$(start_count)" "2"

# --- SIGINT ----------------------------------------------------------------------------------
log "scenario: SIGINT"
"${compose[@]}" kill -s SIGINT rustinx > /dev/null 2>&1
if wait_exited "$app"; then pass "sigint: container exited"; else fail "sigint: container exited" "still running"; fi
expect "sigint: exit code 0" "$(inspect '{{.State.ExitCode}}' "$app")" "0"
expect_log "sigint: signal logged" "$app" 'signal="SIGINT"'
"${compose[@]}" start rustinx > /dev/null 2>&1 || true
if wait_health "$app" healthy; then pass "sigint: restarted healthy"; else fail "sigint: restarted healthy" "$(health "$app")"; fi
expect "sigint: state persisted (start count 3)" "$(start_count)" "3"

# --- Health check reflects a stalled nginx -----------------------------------------------------
log "scenario: stalled nginx makes the container unhealthy"
master="$(in_container cat /run/rustinx/nginx.pid)"
in_container sh -c 'kill -s STOP -- "-$1"' sh "$master"
if wait_health "$app" unhealthy; then pass "health: stalled nginx reported unhealthy"; else fail "health: stalled nginx reported unhealthy" "$(health "$app")"; fi
in_container sh -c 'kill -s CONT -- "-$1"' sh "$master"
if wait_health "$app" healthy; then pass "health: recovered nginx reported healthy"; else fail "health: recovered nginx reported healthy" "$(health "$app")"; fi

# --- Unexpected nginx termination ------------------------------------------------------------
log "scenario: nginx master killed"
in_container sh -c 'kill -s KILL "$1"' sh "$master" || true
if wait_exited "$app"; then pass "crash: container exited"; else fail "crash: container exited" "still running"; fi
expect "crash: exit code 3" "$(inspect '{{.State.ExitCode}}' "$app")" "3"
expect_log "crash: unexpected exit detected" "$app" "nginx exited unexpectedly"
expect_log "crash: orphaned nginx workers killed" "$app" "killed processes left in the nginx process group"

# --- Container recreation keeps the volume -----------------------------------------------------
log "scenario: container recreated"
save_container_logs
"${compose[@]}" rm -f rustinx > /dev/null 2>&1
"${compose[@]}" up -d --wait --wait-timeout "$WAIT_SECS" rustinx > "$LOG_DIR/up-2.log" 2>&1 || true
record
app="$(cid rustinx)"
expect "recreate: healthy" "$(health "$app")" "healthy"
expect "recreate: state persisted (start count 4)" "$(start_count)" "4"

# --- Invalid nginx configuration -------------------------------------------------------------
log "scenario: invalid nginx configuration"
"${compose[@]}" up -d badconf > "$LOG_DIR/up-badconf.log" 2>&1 || true
record
bad="$(cid badconf)"
if wait_exited "$bad"; then pass "badconf: container exited"; else fail "badconf: container exited" "still running"; fi
expect "badconf: exit code 2" "$(inspect '{{.State.ExitCode}}' "$bad")" "2"
expect_log "badconf: nginx -t error logged" "$bad" 'unknown directive "this_is_not_a_directive"'
expect "badconf: never healthy" "$([ "$(health "$bad")" != healthy ] && echo yes || echo no)" "yes"

# --- User configuration untouched -------------------------------------------------------------
expect "user config: unchanged" "$(cd "$RUN_DIR/user_conf" && find . -type f -exec sha256sum {} + | sort)" "$user_conf_sum"

"${compose[@]}" stop > /dev/null 2>&1 || true
[ "$FAILED" -eq 0 ]
