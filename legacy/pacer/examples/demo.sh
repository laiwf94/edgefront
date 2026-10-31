#!/bin/sh
# Live demo against the real pacerd binary. Each step prints what happened
# and checks it; the script exits non-zero if any check fails.
#
#   1. a client opening connections too fast is limited, then recovers
#   2. a slowloris client is cut off while a steady client is not
#   3. a drain on SIGTERM lets a download in flight finish byte-for-byte
#
# Linux only. On macOS: scripts/dev.sh examples/demo.sh
set -eu
cd "$(dirname "$0")/.."
make -s VARIANT=release build/release/pacerd build/release/pacerctl >/dev/null

work=$(mktemp -d)
peers="python3 examples/demo_peers.py"
up_port=19000
px_port=19001
sock=$work/admin.sock
failures=0

cleanup() {
    [ -n "${pacerd_pid:-}" ] && kill "$pacerd_pid" 2>/dev/null || true
    [ -n "${up_pid:-}" ] && kill "$up_pid" 2>/dev/null || true
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

stat_of() {
    build/release/pacerctl -s "$sock" stats | tr ' ' '\n' | grep "^$1=" | cut -d= -f2
}

cat > "$work/pacerd.conf" <<CONF
workers 2
admin_socket $sock
drain_timeout 20s
listener demo {
    listen 127.0.0.1:$px_port
    upstream 127.0.0.1:$up_port
    per_ip_conn_rate 5/s burst 5
    per_ip_bandwidth 4m/s burst 256k
    min_rate 64 per 2s
}
CONF

$peers upstream $up_port &
up_pid=$!
build/release/pacerd -c "$work/pacerd.conf" 2>"$work/pacerd.log" &
pacerd_pid=$!
for _ in $(seq 50); do
    grep -q msg=ready "$work/pacerd.log" 2>/dev/null && break
    sleep 0.1
done
echo "pacerd $(build/release/pacerd -v | cut -d' ' -f2) listening on 127.0.0.1:$px_port"
echo "limits: 5 new connections/s per address (burst 5), 4 MiB/s per address,"
echo "        at least 64 bytes per 2 s per connection"

echo
echo "1. A client opens 12 connections at once"
admitted=$($peers burst $px_port 12)
check "connections admitted" "$admitted" 5
check "refused for connection rate" "$(stat_of rejected_per_ip_rate)" 7
echo "   waiting one second for the bucket to refill"
sleep 1.1
admitted=$($peers burst $px_port 5)
check "connections admitted after recovery" "$admitted" 5

echo
echo "2. A slowloris client and a steady client, side by side"
sleep 1.1
$peers steady $px_port 5 >"$work/steady" &
steady_pid=$!
slow=$($peers slowloris $px_port 8)
wait $steady_pid
echo "   slowloris: $slow"
check "slowloris cut off" "$(echo "$slow" | cut -d' ' -f1)" cut
check "steady client" "$(cat "$work/steady")" survived
check "closed as slow clients" "$(stat_of closed_slow)" 1

echo
echo "3. Graceful drain with a download in flight"
size=$((12 * 1024 * 1024))
sleep 1.1
$peers download $px_port $size "$work/got" &
dl_pid=$!
sleep 1
echo "   sending SIGTERM to pacerd one second into a ~3 s download"
kill -TERM "$pacerd_pid"
sleep 0.3
if $peers burst $px_port 1 | grep -q '^0$'; then refused=yes; else refused=no; fi
check "new connections refused while draining" "$refused" yes
wait $dl_pid
check "download complete and intact" "$(cat "$work/got")" "$($peers expected $size)"
code=0
wait "$pacerd_pid" || code=$?
pacerd_pid=
check "pacerd exit code" "$code" 0
check "drained without forcing connections closed" \
    "$(grep -c 'drain timeout' "$work/pacerd.log" || true)" 0

echo
echo "pacerd log, last lines:"
tail -n 4 "$work/pacerd.log" | sed 's/^/  /'
echo
if [ "$failures" -ne 0 ]; then
    echo "demo: $failures check(s) failed"
    exit 1
fi
echo "demo: all checks passed"
