#!/usr/bin/env bash
# Polls Confluence until the application or setup wizard is reachable.
# Usage: ./wait-for-confluence.sh [host] [timeout_seconds]

set -euo pipefail

HOST="${1:-http://localhost:8090}"
TIMEOUT="${2:-900}"
INTERVAL=5
elapsed=0

echo "Waiting for Confluence at $HOST to be reachable (timeout: ${TIMEOUT}s)..."

while [ "$elapsed" -lt "$TIMEOUT" ]; do
    status_payload="$(curl -fsS "$HOST/status" 2>/dev/null || true)"
    state="$(printf '%s' "$status_payload" | grep -o '"state":"[^"]*"' | cut -d'"' -f4 || true)"
    if [ "$state" = "RUNNING" ]; then
        echo "Confluence is ready."
        exit 0
    fi

    http_code="$(curl -s -o /dev/null -w '%{http_code}' "$HOST/setup/setupstart.action" || true)"
    if [ "$http_code" = "200" ] || [ "$http_code" = "302" ] || [ "$http_code" = "303" ]; then
        echo "Confluence setup wizard is reachable."
        exit 0
    fi

    echo "  state=${state:-unknown} http=${http_code:-000} — waiting ${INTERVAL}s..."
    sleep "$INTERVAL"
    elapsed=$((elapsed + INTERVAL))
done

echo "Timed out waiting for Confluence after ${TIMEOUT}s."
exit 1
