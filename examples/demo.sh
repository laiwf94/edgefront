#!/usr/bin/env bash
# A live run of the release binary: two cachet nodes in front of the
# example origin. Each step prints what happened and fails the script if
# the cache did not behave as described.
set -euo pipefail

cd "$(dirname "$0")/.."
ORIGIN=127.0.0.1:9200
A=127.0.0.1:8201 A_ADMIN=127.0.0.1:9201
B=127.0.0.1:8202 B_ADMIN=127.0.0.1:9202
WORK=$(mktemp -d)
export CACHET_ADMIN_TOKEN=demo-admin-token
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

# get <proxy> <path>: prints the status, Cache-Status and body on one line each.
get() {
    curl -sS -D "$WORK/h" -o "$WORK/b" "http://$1$2"
    STATUS=$(head -1 "$WORK/h" | cut -d' ' -f2)
    CACHE_STATUS=$(grep -i '^cache-status:' "$WORK/h" | cut -d' ' -f2- | tr -d '\r')
    BODY=$(cat "$WORK/b")
    printf '   GET %-11s -> %s  cache-status: %s\n' "$2" "$STATUS" "$CACHE_STATUS"
}

origin_count() {
    curl -fsS "http://$ORIGIN/stats" | python3 -c "import json,sys; print(json.load(sys.stdin).get('$1', 0))"
}

start_origin() {
    target/release/examples/origin "$ORIGIN" >/dev/null &
    ORIGIN_PID=$!
    wait_for "http://$ORIGIN/healthz"
}

node() {
    local name=$1 listen=$2 admin=$3 peer=$4 peer_admin=$5
    cat >"$WORK/$name.toml" <<TOML
node_id = "$name"
listen = "$listen"
admin_listen = "$admin"
[[origins]]
name = "origin"
url = "http://$ORIGIN"
[health]
interval_ms = 500
[[peers]]
name = "$peer"
url = "http://$peer_admin"
TOML
    target/release/cachet --config "$WORK/$name.toml" &
    PIDS+=($!)
    wait_for "http://$admin/healthz"
}

cargo build --release --quiet --bin cachet --example origin
start_origin
node edge-a "$A" "$A_ADMIN" edge-b "$B_ADMIN"
node edge-b "$B" "$B_ADMIN" edge-a "$A_ADMIN"
echo "origin on $ORIGIN, edge-a on $A (admin $A_ADMIN), edge-b on $B (admin $B_ADMIN)"

step "1. A miss is fetched and stored; the next request is a hit"
get "$A" /static/demo
[[ $CACHE_STATUS == "edge-a; fwd=miss; fwd-status=200; stored"* ]] || fail "first request should be a stored miss"
get "$A" /static/demo
[[ $CACHE_STATUS == "edge-a; hit"* ]] || fail "second request should be a hit"
[[ $(origin_count /static/demo) == 1 ]] || fail "origin should have seen one request"
ok "origin saw 1 request for 2 client requests"

step "2. A stale object is revalidated with its entity tag"
get "$A" /versioned
sleep 2.5
get "$A" /versioned
[[ $CACHE_STATUS == *"fwd=stale; fwd-status=304"* ]] || fail "expected a 304 revalidation"
[[ $BODY == "version 0" ]] || fail "body should be unchanged after a 304"
ok "origin answered 304; the stored body was reused: '$BODY'"
curl -fsS -X POST "http://$ORIGIN/bump"
sleep 2.5
get "$A" /versioned
[[ $CACHE_STATUS == *"fwd=stale; fwd-status=200"* && $BODY == "version 1" ]] || fail "changed object should replace the stale one"
ok "after the origin changed, the new version replaced it: '$BODY'"

step "3. 50 concurrent requests for a cold URL make one origin request"
for i in $(seq 50); do
    curl -sS -D "$WORK/slow-$i" -o /dev/null "http://$A/slow" &
done
wait_jobs() { for job in $(jobs -p); do [[ " ${PIDS[*]} $ORIGIN_PID " == *" $job "* ]] || wait "$job"; done; }
wait_jobs
collapsed=$(grep -il '^cache-status:.*collapsed' "$WORK"/slow-* | wc -l | tr -d ' ')
statuses=$(grep -h '^HTTP' "$WORK"/slow-* | cut -d' ' -f2 | sort | uniq -c | tr -s ' ')
echo "   statuses:$statuses; responses marked collapsed: $collapsed"
[[ $(origin_count /slow) == 1 ]] || fail "origin should have seen exactly one /slow request, saw $(origin_count /slow)"
[[ $collapsed == 49 ]] || fail "49 requests should have waited on the first one"
ok "origin saw 1 request for 50 clients"

step "4. With the origin down, a stale object is served (stale-if-error)"
get "$A" /news
kill "$ORIGIN_PID"; wait "$ORIGIN_PID" 2>/dev/null || true; ORIGIN_PID=
echo "   origin stopped"
sleep 2.5
get "$A" /news
[[ $STATUS == 200 && $CACHE_STATUS == *"detail=stale-if-error"* ]] || fail "expected the stale copy while the origin is down"
ok "served '$BODY' from the stale copy"
get "$A" /static/never-cached
[[ $STATUS == 502 ]] || fail "an uncached URL should fail with 502 while the origin is down"
ok "an object that was never cached gets 502"
start_origin
echo "   origin restarted"

step "5. A purge on edge-a reaches edge-b"
get "$A" /static/tagged
get "$B" /static/tagged
get "$B" /static/tagged
[[ $CACHE_STATUS == "edge-b; hit"* ]] || fail "edge-b should have it cached"
result=$(curl -fsS -X POST -H "authorization: Bearer $CACHET_ADMIN_TOKEN" \
    -d '{"tags": ["static"]}' "http://$A_ADMIN/purge")
echo "   POST edge-a /purge {\"tags\": [\"static\"]} -> $result"
for _ in $(seq 50); do
    curl -fsS "http://$B_ADMIN/metrics" | grep -q '^cachet_purges_peer_total 1$' && break
    sleep 0.1
done
curl -fsS "http://$B_ADMIN/metrics" | grep -q '^cachet_purges_peer_total 1$' || fail "edge-b never applied the purge"
get "$A" /static/tagged
[[ $CACHE_STATUS == *"fwd=miss"* ]] || fail "edge-a should have dropped the object"
get "$B" /static/tagged
[[ $CACHE_STATUS == *"fwd=miss"* ]] || fail "edge-b should have dropped the object"
ok "both nodes fetched the object again after one purge request"

printf '\nAll five steps behaved as described.\n'
