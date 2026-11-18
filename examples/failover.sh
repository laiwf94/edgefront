#!/usr/bin/env bash
# Failover drill against edgefront, ported from legacy/steer/examples/failover.sh.
# Two priority-0 upstreams and one priority-1 backup sit behind `edgefront
# proxy` while load runs at a constant rate. Each phase adds a fault:
#
#   1. baseline              all three upstreams healthy
#   2. kill primary-a        kill -9 one primary
#   3. primary-b 50% errors  the other primary fails half its requests
#   4. blackhole primary-b   it accepts requests and never answers
#
# The drill runs twice: with examples/failover/resilient.json (health checks,
# outlier detection, retries, priority failover) and with
# examples/failover/plain.json (the same endpoints, none of those features).
# Both are steer's examples/steer.json and steer-plain.json unchanged. It
# prints the client-side success rate and latency per phase, in the table
# steer's README publishes. The upstreams and the load generator are
# `edgefront-difftest upstream` and `edgefront-difftest loadgen`, ports of
# steer's cmd/upstream and cmd/loadgen, so no Go is needed.
#
#   RATE           requests per second (default 200)
#   PHASE_SECONDS  length of each phase (default 10)
#   CHECK=1        exit 1 unless every phase with the features on succeeds
#                  100.00% and every fault without them shows (scripts/check-failover.sh)
set -euo pipefail

RATE="${RATE:-200}"
PHASE_SECONDS="${PHASE_SECONDS:-10}"
DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/.." && pwd)"
RUN="$DIR/failover/.run"
PROXY=http://127.0.0.1:18080
ADMIN=http://127.0.0.1:19901
pids=()

cleanup() {
  for pid in "${pids[@]:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT

if [ "${CHECK:-0}" = 1 ]; then
  out="$(mktemp)"
  CHECK=0 "$0" | tee "$out"
  "$ROOT/scripts/check-failover.sh" "$out"
  exit
fi

mkdir -p "$RUN"
(cd "$ROOT" && cargo build --release --locked --quiet -p edgefront -p edgefront-difftest)
EDGEFRONT="$ROOT/target/release/edgefront"
TOOLS="$ROOT/target/release/edgefront-difftest"

wait_for() { # url
  for _ in $(seq 1 100); do
    curl -fsS -o /dev/null "$1" 2>/dev/null && return 0
    sleep 0.05
  done
  echo "timed out waiting for $1" >&2
  exit 1
}

control() { # port json
  curl -fsS -o /dev/null -X PUT "http://127.0.0.1:$1/state" -d "$2"
}

drill() { # config title
  local config="$1" title="$2"
  pids=()
  "$TOOLS" upstream -name primary-a -addr 127.0.0.1:19101 -control 127.0.0.1:19201 >"$RUN/primary-a.log" 2>&1 &
  local primary_a=$!
  pids+=("$primary_a")
  "$TOOLS" upstream -name primary-b -addr 127.0.0.1:19102 -control 127.0.0.1:19202 >"$RUN/primary-b.log" 2>&1 &
  pids+=($!)
  "$TOOLS" upstream -name backup -addr 127.0.0.1:19103 -control 127.0.0.1:19203 >"$RUN/backup.log" 2>&1 &
  pids+=($!)
  for port in 19201 19202 19203; do wait_for "http://127.0.0.1:$port/state"; done
  EDGEFRONT_DRAIN_PERIOD=0s "$EDGEFRONT" proxy --config "$config" >"$RUN/edgefront-$(basename "$config" .json).log" 2>&1 &
  local proxy=$!
  pids+=("$proxy")
  wait_for "$ADMIN/readyz"
  sleep 2 # let the first health checks settle

  echo
  echo "### $title"
  echo
  echo "| Phase | Requests | Success | p50 | p99 | p99.9 |"
  echo "|---|---|---|---|---|---|"
  local phase
  for phase in "1. baseline" "2. kill primary-a" "3. primary-b 50% errors" "4. blackhole primary-b"; do
    "$TOOLS" loadgen -url "$PROXY/" -rate "$RATE" -duration "${PHASE_SECONDS}s" -timeout 2s -format row -label "$phase" &
    local load=$!
    sleep 1 # the fault lands while load is running
    case "$phase" in
      2.*) kill -9 "$primary_a"; wait "$primary_a" 2>/dev/null || true ;;
      3.*) control 19202 '{"error_rate":0.5}' ;;
      4.*) control 19202 '{"blackhole":true}' ;;
    esac
    wait "$load"
  done
  echo
  echo "End state (GET $ADMIN/clusters):"
  curl -fsS "$ADMIN/clusters" | python3 -c '
import json, sys
c = json.load(sys.stdin)["clusters"][0]
print("  priority load %s, panic %s" % (c["balancer"]["priority_load_percent"], c["balancer"]["panic"]))
for e in c["endpoints"]:
    print("  %-16s priority %d  healthy %-5s  ejected %-5s  times ejected %d" % (
        e["address"], e["priority"], e["healthy"], e["ejected"], e["times_ejected"]))'
  cleanup
  pids=()
}

drill "$DIR/failover/resilient.json" "Resilience on (examples/failover/resilient.json)"
drill "$DIR/failover/plain.json" "Resilience off (examples/failover/plain.json)"
