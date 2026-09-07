#!/usr/bin/env bash
set -uo pipefail

OUT="${RSS_LOG:-/tmp/kuluu-rss.tsv}"
CLIENT_LOG="${CLIENT_LOG:-/tmp/kuluu-client.log}"
INTERVAL="${RSS_INTERVAL:-5}"

echo "interval=${INTERVAL}s; rss=${OUT}; client=${CLIENT_LOG}" >&2

./launch.sh "${1:-vulkan}" >"$CLIENT_LOG" 2>&1 &
LAUNCH_PID=$!
echo -e "elapsed_s\trss_kb\trss_mb" > "$OUT"

while kill -0 "$LAUNCH_PID" 2>/dev/null; do
    KPID=$(pgrep -x kuluu | head -1)
    if [ -n "$KPID" ]; then
        RSS=$(ps -o rss= -p "$KPID" 2>/dev/null | tr -d ' ')
        if [ -n "$RSS" ]; then
            ELAPSED=$(ps -o etimes= -p "$LAUNCH_PID" 2>/dev/null | tr -d ' ')
            printf "%s\t%s\t%.1f\n" "$ELAPSED" "$RSS" "$(echo "scale=1; $RSS/1024" | bc)" >> "$OUT"
        fi
    fi
    sleep "$INTERVAL"
done

wait "$LAUNCH_PID"
echo "client exited; RSS log at ${OUT}, client log at ${CLIENT_LOG}" >&2