#!/usr/bin/env bash
# Shadow mode and policy replay, end to end. Each step prints what
# happened and checks it; the script exits non-zero if any check fails.
#
#   1. a shadow limit lets every request through while recording what it
#      would have refused
#   2. edgefront ctl limits replay runs the recording against the same
#      policy (no change) and a tighter one (the keys it would refuse)
#   3. the limit is promoted to enforce by editing one word of the file,
#      and enforces at once with the counts it already had
#
# Runs on Linux and macOS with python3 and curl. EDGEFRONT_BIN names a
# built binary to use instead of building one;
# crates/edgefront/tests/e2e_shadow_replay.rs runs the script that way.
# The ports are 19300 to 19302 unless SHADOW_DEMO_PORT names the first of
# three others.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ -z "${EDGEFRONT_BIN:-}" ]; then
    cargo build --release --locked -q -p edgefront --bin edgefront
    EDGEFRONT_BIN=target/release/edgefront
fi
work=$(mktemp -d /tmp/shadowdemo.XXXXXX)
up=${SHADOW_DEMO_PORT:-19300}
px=$((up + 1))
adm=$((up + 2))
failures=0
cleanup() {
    for p in ${edge_pid:-} ${up_pid:-}; do kill "$p" 2>/dev/null || true; done
    rm -rf "$work"
}
trap cleanup EXIT INT TERM
check() {
    if [ "$2" = "$3" ]; then
        echo "  ok    $1: $2"
    else
        echo "  FAIL  $1: got $2, want $3"
        failures=$((failures + 1))
    fi
}
burst() {
    local urls=()
    for _ in $(seq "$2"); do urls+=(-o /dev/null "http://127.0.0.1:$px/"); done
    curl -s -H "x-api-key: $1" -w '%{http_code}\n' "${urls[@]}"
}

cp examples/shadow_replay/limits.toml "$work/limits.toml"
cat > "$work/proxy.json" <<CONF
{"listen":"127.0.0.1:$px","admin_listen":"127.0.0.1:$adm",
 "routes":[{"host":"*","path_prefix":"/","cluster":"app"}],
 "clusters":[{"name":"app","endpoints":[{"address":"127.0.0.1:$up"}]}]}
CONF
python3 examples/http_limits/upstream.py "$up" &
up_pid=$!
EDGEFRONT_LOG=info EDGEFRONT_DRAIN_PERIOD=0s "$EDGEFRONT_BIN" proxy --config "$work/proxy.json" \
    --limits "$work/limits.toml" >"$work/proxy.log" 2>&1 &
edge_pid=$!
for _ in $(seq 100); do
    grep -q '"listening"' "$work/proxy.log" 2>/dev/null &&
        curl -s -o /dev/null "http://127.0.0.1:$up/" && break
    sleep 0.1
done

echo "1. key k1 sends 20 requests, k2 sends 3, under a shadow limit of 5 a minute"
check "k1 answered 200" "$(burst k1 20 | grep -cx 200)" 20
check "k2 answered 200" "$(burst k2 3 | grep -cx 200)" 3
sleep 1.5
check "recorded" "$(ls "$work/rec" | grep -c '\.rec$')" 1

echo
echo "2. replay"
"$EDGEFRONT_BIN" ctl limits replay --recording "$work/rec" --policy "$work/limits.toml" \
    --json "$work/same.json" >"$work/same.txt"
sed 's/^/   /' "$work/same.txt" | grep -v '^   $'
check "the same policy: refused before (the would-blocks)" \
    "$(python3 -c "import json; print(json.load(open('$work/same.json'))['rules'][0]['refused_before'])")" 15
check "the same policy: keys changed" \
    "$(python3 -c "import json; print(json.load(open('$work/same.json'))['rules'][0]['keys_changed'])")" 0
"$EDGEFRONT_BIN" ctl limits replay --recording "$work/rec" --policy examples/shadow_replay/tighter.toml \
    --json "$work/tight.json" >"$work/tight.txt"
sed 's/^/   /' "$work/tight.txt" | grep -v '^   $'
check "two a minute: refused after" \
    "$(python3 -c "import json; print(json.load(open('$work/tight.json'))['rules'][0]['refused_after'])")" 19
check "two a minute: keys changed" \
    "$(python3 -c "import json; print(json.load(open('$work/tight.json'))['rules'][0]['keys_changed'])")" 2

echo
echo "3. promote: mode = \"shadow\" becomes \"enforce\""
sed -i.bak 's/mode = "shadow"/mode = "enforce"/' "$work/limits.toml"
for _ in $(seq 50); do grep -q 'http limit mode changed' "$work/proxy.log" && break; sleep 0.1; done
check "the proxy applied it" "$(grep -c '"mode":"enforce"' "$work/proxy.log")" 1
check "k1, spent while shadowed, is refused at once" "$(burst k1 1)" 429
check "k2 has the 2 it had left" "$(burst k2 3 | tr '\n' ' ')" "200 200 429 "
check "k3, new, gets its 5" "$(burst k3 6 | grep -cx 200)" 5

echo
if [ "$failures" -ne 0 ]; then
    echo "$failures check(s) failed"
    exit 1
fi
echo "all checks passed"
