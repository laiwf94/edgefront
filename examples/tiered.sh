#!/usr/bin/env bash
# Tiered caching live: five edge nodes and one shield of the release binary,
# joined by SWIM, in front of the example origin. Each step prints the
# origin's request counts and fails the script if the tiers did not behave
# as described. tests/tier.rs runs the same behaviour with ten nodes.
set -euo pipefail

cd "$(dirname "$0")/.."
ORIGIN=127.0.0.1:9300
# One site, whichever edge a client reaches: the cache key is the Host and
# the path, so every edge must be asked for the same Host.
SITE=demo.example
WORK=$(mktemp -d)
export CACHET_CLUSTER_SECRET=demo-cluster-secret-at-least-32-bytes
export CACHET_LOG=${CACHET_LOG:-warn}

PIDS=()
cleanup() {
    for pid in "${PIDS[@]}" ${ORIGIN_PID:-}; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }
ok() { echo "   ok: $*"; }

wait_for() {
    for _ in $(seq 100); do
        curl -fsS "$1" >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    fail "timed out waiting for $1"
}

origin_count() {
    curl -fsS "http://$ORIGIN/stats" | python3 -c "import json,sys; print(json.load(sys.stdin).get('$1', 0))"
}

# The nodes: edge-1 to edge-5 on 8301-8305, the shield on 8306, admin on
# 9301-9306 and SWIM on 7301-7306, every one joining through edge-1.
NAMES=(edge-1 edge-2 edge-3 edge-4 edge-5 shield)
node() {
    local i=$1 name=${NAMES[$1]}
    cat >"$WORK/$name.toml" <<TOML
node_id = "$name"
listen = "127.0.0.1:830$((i + 1))"
admin_listen = "127.0.0.1:930$((i + 1))"
[[origins]]
name = "origin"
url = "http://$ORIGIN"
[health]
interval_ms = 500
[tier]
gossip_listen = "127.0.0.1:730$((i + 1))"
seeds = ["127.0.0.1:7301"]
[[tier.shields]]
path_prefix = "/"
nodes = ["shield"]
TOML
    target/release/edgefront --config "$WORK/$name.toml" &
    PIDS+=($!)
    wait_for "http://127.0.0.1:930$((i + 1))/healthz"
}

members() {
    curl -fsS "http://127.0.0.1:930$(($1 + 1))/metrics" | awk '$1 == "cachet_tier_members" { print $2 }'
}

metric() {
    local total=0 v
    for i in 0 1 2 3 4 5; do
        v=$(curl -fsS "http://127.0.0.1:930$((i + 1))/metrics" | awk -v m="$1" '$1 == m { print $2 }')
        total=$((total + ${v:-0}))
    done
    echo "$total"
}

cargo build --release --quiet -p edgefront --bin edgefront
cargo build --release --quiet -p cachet --example origin
target/release/examples/origin "$ORIGIN" >/dev/null &
ORIGIN_PID=$!
wait_for "http://$ORIGIN/healthz"
for i in 0 1 2 3 4 5; do node "$i"; done

step "1. The six nodes find each other through one seed"
for _ in $(seq 100); do
    settled=1
    for i in 0 1 2 3 4 5; do [[ $(members "$i") == 6 ]] || settled=0; done
    [[ $settled == 1 ]] && break
    sleep 0.1
done
[[ $settled == 1 ]] || fail "membership did not settle on 6"
ok "every node sees 6 members"

step "2. The same cold URL on all five edges at once: one origin request"
for round in 1 2 3 4; do
    for port in 8301 8302 8303 8304 8305; do
        curl -fsS -H "Host: $SITE" -o /dev/null "http://127.0.0.1:$port/slow" &
    done
done
for job in $(jobs -p); do [[ " ${PIDS[*]} $ORIGIN_PID " == *" $job "* ]] || wait "$job"; done
echo "   origin requests for /slow: $(origin_count /slow) (20 client requests over 5 edges)"
echo "   misses sent to owners: $(metric cachet_tier_owner_fetches_total), to the shield: $(metric cachet_tier_shield_fetches_total)"
[[ $(origin_count /slow) == 1 ]] || fail "the origin should have seen one /slow request"
ok "the owner asked the shield once, and the shield the origin once"

step "3. 200 objects requested on every edge: each reaches the origin once"
for n in $(seq 200); do
    for port in 8301 8302 8303 8304 8305; do
        curl -fsS -H "Host: $SITE" -o /dev/null "http://127.0.0.1:$port/static/obj-$n" &
    done
    (( n % 20 == 0 )) && for job in $(jobs -p); do [[ " ${PIDS[*]} $ORIGIN_PID " == *" $job "* ]] || wait "$job"; done
done
for job in $(jobs -p); do [[ " ${PIDS[*]} $ORIGIN_PID " == *" $job "* ]] || wait "$job"; done
seen=0
for n in $(seq 200); do seen=$((seen + $(origin_count "/static/obj-$n"))); done
echo "   origin requests: $seen for 1,000 client requests ($(( (1000 - seen) / 10 ))% offload)"
[[ $seen == 200 ]] || fail "expected 200 origin requests, saw $seen"
ok "one origin request per object"

step "4. An edge is killed: the others notice, and its keys still work"
kill -9 "${PIDS[2]}"; wait "${PIDS[2]}" 2>/dev/null || true
start=$(python3 -c "import time; print(time.time())")
for _ in $(seq 60); do
    settled=1
    for i in 0 1 3 4 5; do [[ $(members "$i") == 5 ]] || settled=0; done
    [[ $settled == 1 ]] && break
    sleep 0.1
done
[[ $settled == 1 ]] || fail "the killed node was not noticed in 6 s"
echo "   noticed by every node after $(python3 -c "import time; print(f'{time.time() - $start:.1f}')") s"
for n in $(seq 200); do curl -fsS -H "Host: $SITE" -o /dev/null "http://127.0.0.1:8301/static/obj-$n"; done
seen=0
for n in $(seq 200); do seen=$((seen + $(origin_count "/static/obj-$n"))); done
echo "   origin requests after the kill: $seen (the killed node's share fetched again from the shield, which still had them)"
[[ $seen == 200 ]] || fail "the shield should have answered for the killed edge's keys"
ok "no extra origin requests"
