#!/usr/bin/env bash
# The penalty box, bans and overload mode, end to end. Each step prints
# what happened and checks it; the script exits non-zero if any check
# fails.
#
#   1. a client over its limit two seconds running is boxed, refused
#      without its limit being looked at, released on time, and banned on
#      its second offence; connections close after their quota, and a
#      listener-wide connection rate caps the whole entry point
#   2. a ban set through the edgefront limits admin API refuses a client,
#      survives a reload, and expires on time
#   3. overload mode under synthetic CPU saturation keeps the good
#      clients' p99 low while shedding the abusive ones
#
# Runs on Linux and macOS with python3 and curl. EDGEFRONT_BIN names a
# built binary to use instead of building one, BENCH_BIN the same for
# edgefront-bench; crates/edgefront/tests/e2e_penalty_box.rs runs the
# script that way, with PENALTY_DEMO_NO_BENCH=1 to leave step 3 to the
# harness's own test (edgefront-bench's overload module). The ports are 19200 to 19204 unless PENALTY_DEMO_PORT
# names the first of five others.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ -z "${EDGEFRONT_BIN:-}" ]; then
    cargo build --release --locked -q -p edgefront --bin edgefront
    EDGEFRONT_BIN=target/release/edgefront
fi
if [ -z "${BENCH_BIN:-}" ] && [ -z "${PENALTY_DEMO_NO_BENCH:-}" ]; then
    cargo build --release --locked -q -p edgefront-bench --bin edgefront-bench
    BENCH_BIN=target/release/edgefront-bench
fi

work=$(mktemp -d /tmp/penaltydemo.XXXXXX)
base=${PENALTY_DEMO_PORT:-19200}
up=$base
px=$((base + 1))
adm=$((base + 2))
l4up=$((base + 3))
l4=$((base + 4))
failures=0

cleanup() {
    for p in ${edge_pid:-} ${up_pid:-} ${l4_pid:-} ${l4up_pid:-}; do kill "$p" 2>/dev/null || true; done
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

req() {
    curl -s -o /dev/null -D "$work/head" -H "x-api-key: $1" "http://127.0.0.1:$px/" -w '%{http_code}'
}
head_of() {
    grep -i "^$1:" "$work/head" | cut -d' ' -f2- | tr -d '\r'
}

cat > "$work/proxy.json" <<CONF
{"listen":"127.0.0.1:$px","admin_listen":"127.0.0.1:$adm",
 "routes":[{"host":"*","path_prefix":"/","cluster":"app"}],
 "clusters":[{"name":"app","endpoints":[{"address":"127.0.0.1:$up"}]}]}
CONF
python3 examples/http_limits/upstream.py "$up" &
up_pid=$!
EDGEFRONT_LOG=info EDGEFRONT_DRAIN_PERIOD=0s "$EDGEFRONT_BIN" proxy --config "$work/proxy.json" \
    --limits examples/penalty_box/limits.toml >"$work/proxy.log" 2>&1 &
edge_pid=$!
for _ in $(seq 100); do
    grep -q '"listening"' "$work/proxy.log" 2>/dev/null &&
        curl -s -o /dev/null "http://127.0.0.1:$up/" && break
    sleep 0.1
done

echo "1. key bad sends 8 requests a second for two seconds"
for s in 1 2; do
    codes=""
    for _ in $(seq 8); do codes="$codes $(req bad)"; done
    echo "   second $s:$codes"
    [ "$s" = 1 ] && sleep 1
done
check "boxed" "$(req bad)" 429
retry=$(head_of retry-after)
check "Retry-After is the box's time left, 3 s or less" "$([ "$retry" -ge 1 ] && [ "$retry" -le 3 ] && echo yes)" yes
check "another key is unaffected" "$(req good)" 200
echo "   waiting ${retry} s for the box to open"
sleep "$retry"
check "released" "$(req bad)" 200
for _ in $(seq 8); do req bad >/dev/null; done
sleep 1
for _ in $(seq 8); do req bad >/dev/null; done
check "second offence: banned" "$(req bad)" 429
check "banned for the hour" "$([ "$(head_of retry-after)" -gt 3500 ] && echo yes)" yes

echo
echo "   one keep-alive connection, five requests, quota 3:"
urls=()
for p in a b c d e; do urls+=(-o /dev/null "http://127.0.0.1:$px/$p"); done
curl -s -D "$work/ka" -H 'x-api-key: ka' -w '%{num_connects}\n' "${urls[@]}" >"$work/connects"
check "Connection: close on the third response" "$(grep -ci '^connection: close' "$work/ka")" 1
check "connections opened" "$(awk '{ n += $1 } END { print n }' "$work/connects")" 2

echo
echo "   a second proxy, capped at 3 new connections a second for the whole listener:"
kill "$edge_pid" 2>/dev/null || true
wait "$edge_pid" 2>/dev/null || true
EDGEFRONT_LOG=info EDGEFRONT_DRAIN_PERIOD=0s "$EDGEFRONT_BIN" proxy --config "$work/proxy.json" \
    --limits examples/penalty_box/listener.toml >"$work/proxy2.log" 2>&1 &
edge_pid=$!
for _ in $(seq 100); do grep -q '"listening"' "$work/proxy2.log" 2>/dev/null && break; sleep 0.1; done
codes=""
for _ in $(seq 6); do codes="$codes $(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$px/" || true)"; done
echo "   six fresh connections:$codes"
check "connections served" "$(echo "$codes" | tr ' ' '\n' | grep -cx 200)" 3

echo
echo "2. a ban through the edgefront limits admin API"
sock=$work/admin.sock
cat > "$work/pacerd.conf" <<CONF
admin_socket $sock
listener demo {
    listen 127.0.0.1:$l4
    upstream 127.0.0.1:$l4up
}
CONF
python3 examples/l4_limits/peers.py upstream "$l4up" &
l4up_pid=$!
EDGEFRONT_BANS_FILE="$work/bans.txt" "$EDGEFRONT_BIN" limits -c "$work/pacerd.conf" 2>"$work/limits.log" &
l4_pid=$!
for _ in $(seq 50); do grep -q msg=ready "$work/limits.log" 2>/dev/null && break; sleep 0.1; done
admin() { curl -s --unix-socket "$sock" -X "$1" "http://edgefront$2"; }
check "a client connects" "$(python3 examples/l4_limits/peers.py burst "$l4" 1)" 1
check "ban 127.0.0.1 for 2 s" "$(admin POST '/ban?addr=127.0.0.1&for=2')" "ok banned"
check "refused while banned" "$(python3 examples/l4_limits/peers.py burst "$l4" 1)" 0
check "listed" "$(admin GET /bans | cut -d' ' -f1,3)" "127.0.0.1 manual"
check "saved" "$(wc -l <"$work/bans.txt" | tr -d ' ')" 1
check "a reload" "$(admin POST /reload)" "ok reloaded"
check "still refused after the reload" "$(python3 examples/l4_limits/peers.py burst "$l4" 1)" 0
sleep 2.1
check "admitted once the ban expired" "$(python3 examples/l4_limits/peers.py burst "$l4" 1)" 1
check "the log names the refusal" "$(grep -c 'reason=banned' "$work/limits.log")" 2

if [ -n "${PENALTY_DEMO_NO_BENCH:-}" ]; then
    echo
    echo "3. overload mode: skipped (PENALTY_DEMO_NO_BENCH)"
    [ "$failures" -eq 0 ] && { echo; echo "all checks passed"; exit 0; }
    echo "$failures check(s) failed"
    exit 1
fi
echo
echo "3. overload mode under synthetic CPU saturation (examples/penalty_box/overload.toml)"
"$BENCH_BIN" overload --scenario examples/penalty_box/overload.toml --json "$work/ov.json" | sed 's/^/   /'
on_p99=$(python3 -c "import json; r=[x for x in json.load(open('$work/ov.json'))['runs'] if x['overload']][0]; print(r['good_p99_ms'])")
shed=$(python3 -c "import json; r=[x for x in json.load(open('$work/ov.json'))['runs'] if x['overload']][0]; print(int(r['abusive_shed_rps']))")
check "good clients' p99 under 5 ms with overload mode" "$(python3 -c "print('yes' if $on_p99 < 5 else 'no')")" yes
check "abusive requests shed" "$([ "$shed" -gt 0 ] && echo yes)" yes

echo
if [ "$failures" -ne 0 ]; then
    echo "$failures check(s) failed"
    exit 1
fi
echo "all checks passed"
