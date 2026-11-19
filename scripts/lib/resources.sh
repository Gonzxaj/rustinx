# shellcheck shell=bash
# Helpers to track and remove Docker resources owned by a single test run.
# A resource is removed only if it is listed in the run manifest AND carries the run label.

RUN_LABEL="rustinx.test-run"

new_run_id() {
    printf '%s-%s' "$(date -u +%Y%m%d%H%M%S)" "$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
}

# record_resource MANIFEST KIND ID
record_resource() {
    grep -qxF "$2 $3" "$1" 2>/dev/null || printf '%s %s\n' "$2" "$3" >> "$1"
}

# record_project_resources MANIFEST PROJECT RUN_ID
record_project_resources() {
    local manifest="$1" project="$2" run_id="$3" id
    local filters=(--filter "label=com.docker.compose.project=$project" --filter "label=$RUN_LABEL=$run_id")
    for id in $(docker ps -aq "${filters[@]}"); do record_resource "$manifest" container "$id"; done
    for id in $(docker network ls -q "${filters[@]}"); do record_resource "$manifest" network "$id"; done
    for id in $(docker volume ls -q "${filters[@]}"); do record_resource "$manifest" volume "$id"; done
}

# resource_label KIND ID: prints the run label, fails if the resource does not exist.
resource_label() {
    local format
    case "$1" in
        container | image) format="{{ index .Config.Labels \"$RUN_LABEL\" }}" ;;
        network | volume) format="{{ index .Labels \"$RUN_LABEL\" }}" ;;
        *) return 1 ;;
    esac
    docker "$1" inspect --format "$format" "$2" 2>/dev/null
}

# remaining_resources RUN_ID: lists resources still carrying the run label.
remaining_resources() {
    local filter=(--filter "label=$RUN_LABEL=$1")
    docker ps -aq "${filter[@]}" | sed 's/^/container /'
    docker network ls -q "${filter[@]}" | sed 's/^/network /'
    docker volume ls -q "${filter[@]}" | sed 's/^/volume /'
    docker image ls -q "${filter[@]}" | sed 's/^/image /'
}

# cleanup_manifest MANIFEST RUN_ID: removes owned resources in dependency order.
# Returns non-zero if any removal failed or labelled resources remain.
cleanup_manifest() {
    local manifest="$1" run_id="$2" failures=0 kind id label
    [ -f "$manifest" ] || { echo "cleanup: no manifest at $manifest"; return 0; }
    for kind in container network volume image; do
        while read -r entry_kind id; do
            [ "$entry_kind" = "$kind" ] || continue
            if ! label="$(resource_label "$kind" "$id")"; then
                echo "cleanup: $kind $id already absent"
                continue
            fi
            if [ "$label" != "$run_id" ]; then
                echo "cleanup: SKIP $kind $id (label '$label' does not match run $run_id)"
                failures=$((failures + 1))
                continue
            fi
            case "$kind" in
                container) docker rm -f "$id" >/dev/null ;;
                network) docker network rm "$id" >/dev/null ;;
                volume) docker volume rm "$id" >/dev/null ;;
                image) docker image rm "$id" >/dev/null ;;
            esac && echo "cleanup: removed $kind $id" || {
                echo "cleanup: FAILED to remove $kind $id"
                failures=$((failures + 1))
            }
        done < "$manifest"
    done
    local remaining
    remaining="$(remaining_resources "$run_id")"
    if [ -n "$remaining" ]; then
        echo "cleanup: resources still labelled $RUN_LABEL=$run_id:"
        echo "$remaining"
        failures=$((failures + 1))
    fi
    [ "$failures" -eq 0 ]
}
