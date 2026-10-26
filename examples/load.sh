#!/usr/bin/env bash
# End-to-end load test: the release binary in front of the example origin,
# driven by oha (https://github.com/hatoo/oha). Prints throughput and
# latency for cache hits, for misses, and for a burst of concurrent misses
# on one slow URL.
set -euo pipefail

cd "$(dirname "$0")/.."
DURATION=${DURATION:-10s}
CONNECTIONS=${CONNECTIONS:-64}
ORIGIN=127.0.0.1:9100
PROXY=127.0.0.1:8180
ADMIN=127.0.0.1:9180
WORK=$(mktemp -d)

command -v oha >/dev/null || { echo "oha is required: cargo install oha" >&2; exit 1; }

cleanup() {
    for pid in ${CACHET_PID:-} ${ORIGIN_PID:-}; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$WORK"
}
trap cleanup EXIT

wait_for() {
    for _ in $(seq 100); do
        curl -fsS "$1" >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    echo "timed out waiting for $1" >&2
    exit 1
}

cargo build --release --quiet --bin cachet --example origin

cat >"$WORK/cachet.toml" <<TOML
node_id = "edge-load"
listen = "$PROXY"
admin_listen = "$ADMIN"
[[origins]]
name = "origin"
url = "http://$ORIGIN"
[cache]
memory_bytes = 268435456
TOML

target/release/examples/origin "$ORIGIN" >/dev/null &
ORIGIN_PID=$!
wait_for "http://$ORIGIN/healthz"
# Per-request logs go through the JSON formatter; warn keeps them out of
# the measurement, the same as a production edge that samples its logs.
CACHET_LOG=warn target/release/cachet --config "$WORK/cachet.toml" &
CACHET_PID=$!
wait_for "http://$ADMIN/healthz"

summary() {
    python3 -c '
import json, sys
d = json.load(sys.stdin)
s, p = d["summary"], d["latencyPercentiles"]
ms = lambda q: p[q] * 1000
print("  requests/s %10.0f   p50 %6.3f ms   p99 %6.3f ms   p99.9 %6.3f ms   status %s"
      % (s["requestsPerSec"], ms("p50"), ms("p99"), ms("p99.9"), d.get("statusCodeDistribution", {})))
'
}

echo "machine: $(uname -sm), $(sysctl -n hw.ncpu 2>/dev/null || nproc) cpus"
echo
echo "origin directly, 4 KiB body ($DURATION, $CONNECTIONS connections)"
oha -z "$DURATION" -c "$CONNECTIONS" --no-tui --output-format json "http://$ORIGIN/static/a" | summary

curl -fsS -o /dev/null "http://$PROXY/static/a"
echo "cache hits through cachet, 4 KiB body"
oha -z "$DURATION" -c "$CONNECTIONS" --no-tui --output-format json "http://$PROXY/static/a" | summary

echo "cache misses through cachet (random URLs, each fetched and stored)"
oha -z "$DURATION" -c "$CONNECTIONS" --no-tui --output-format json --rand-regex-url "http://$PROXY/static/m[0-9]{9}" | summary

echo "500 requests from 250 concurrent clients to one cold URL that takes 300 ms"
oha -n 500 -c 250 --no-tui --output-format json "http://$PROXY/slow" | summary
echo "  origin requests for /slow: $(curl -fsS "http://$ORIGIN/stats" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("/slow", 0))')"
echo
curl -fsS "http://$ADMIN/metrics" | grep -E '^cachet_(requests_total|collapsed_requests_total|memory_objects|memory_evictions_total)'
