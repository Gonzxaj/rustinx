#!/usr/bin/env bash
# Removes leftover resources of test runs that did not finish their cleanup (e.g. killed runs).
# Only resources recorded in a run manifest AND labelled with that run id are removed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/resources.sh
source "$ROOT/scripts/lib/resources.sh"

RUNS_DIR="$ROOT/target/test-runs"
failures=0
pending=0

if [ -d "$RUNS_DIR" ]; then
    for run_dir in "$RUNS_DIR"/*/; do
        [ -d "$run_dir" ] || continue
        run_dir="${run_dir%/}"
        [ -e "$run_dir/cleanup.done" ] && continue
        pending=$((pending + 1))
        run_id="$(basename "$run_dir")"
        echo "test-clean: run $run_id"
        project="rustinx-test-$run_id"
        # Containers created after the last manifest update are still tied to the run.
        record_project_resources "$run_dir/resources.manifest" "$project" "$run_id"
        if cleanup_manifest "$run_dir/resources.manifest" "$run_id"; then
            touch "$run_dir/cleanup.done"
        else
            failures=$((failures + 1))
        fi
    done
fi

echo "test-clean: $pending run(s) needed cleanup, $failures failed"
[ "$failures" -eq 0 ]
