#!/bin/sh
# Test double for nginx, used only by tests/supervisor.rs. Never shipped in the image.
# Behaviour is selected with FAKE_NGINX_MODE; observations are written to FAKE_NGINX_DIR.
set -u
dir="${FAKE_NGINX_DIR:?FAKE_NGINX_DIR is required}"
mode="${FAKE_NGINX_MODE:-normal}"

if [ "${1:-}" = "-t" ]; then
    echo "test $*" >> "$dir/invocations"
    if [ "$mode" = "bad-config" ]; then
        echo "nginx: [emerg] fake configuration error" >&2
        exit 1
    fi
    echo "nginx: configuration file test is successful" >&2
    exit 0
fi

echo "run $*" >> "$dir/invocations"
echo "$$" > "$dir/pid"

case "$mode" in
    exit-on-start) exit 1 ;;
    crash-after-start) sleep 1; exit 7 ;;
esac

if [ "$mode" = "ignore-quit" ]; then
    trap 'echo quit-ignored > "$dir/signal"' QUIT
else
    trap 'echo quit > "$dir/signal"; exit 0' QUIT
fi
trap 'echo term > "$dir/signal"; exit 0' TERM
trap 'echo int > "$dir/signal"; exit 0' INT

while :; do
    sleep 1 &
    wait $!
done
