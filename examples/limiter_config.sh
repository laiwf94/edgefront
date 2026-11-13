#!/usr/bin/env bash
# The limiter's config, end to end, through `edgefront check`: an old
# pacerd.conf loads unchanged and converts to the TOML limiter section,
# a new TOML config with fractional rates and daily quotas loads, and the
# mistakes each syntax can hold are reported with their line. Each step
# prints what happened and fails the script if the binary did not behave
# as described.
#
# EDGEFRONT_BIN names a built binary to use instead of building one;
# crates/edgefront/tests/e2e_limiter_config.rs runs the script that way.
set -euo pipefail

cd "$(dirname "$0")/.."
if [ -z "${EDGEFRONT_BIN:-}" ]; then
    cargo build --release --locked -q -p edgefront --bin edgefront
    EDGEFRONT_BIN=target/release/edgefront
fi
OLD=examples/limiter/pacerd.conf
NEW=examples/limiter/limiter.toml
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }
ok() { echo "   ok: $*"; }

# run <args...>: runs edgefront, keeping stdout in $OUT, stderr in
# $ERR and the exit code in $CODE.
run() {
    CODE=0
    "$EDGEFRONT_BIN" "$@" >"$WORK/out" 2>"$WORK/err" || CODE=$?
    OUT=$(cat "$WORK/out")
    ERR=$(cat "$WORK/err")
}

# line_of <file> <text>: the line number of the first line holding text.
line_of() { grep -n -F -m1 -- "$2" "$1" | cut -d: -f1; }

step "an old pacerd.conf loads unchanged"
run check "$OLD"
[ "$CODE" = 0 ] && [ "$OUT" = "$OLD: ok" ] || fail "check $OLD: exit $CODE: $OUT $ERR"
ok "$OUT"

step "it converts to the TOML limiter section, which reads back the same"
run check --print "$OLD"
[ "$CODE" = 0 ] || fail "print $OLD: exit $CODE: $ERR"
printf '%s\n' "$OUT" >"$WORK/converted.toml"
sed 's/^/   | /' "$WORK/converted.toml"
run check --print "$WORK/converted.toml"
[ "$OUT" = "$(cat "$WORK/converted.toml")" ] || fail "the converted config prints differently"
run check "$WORK/converted.toml"
[ "$CODE" = 0 ] || fail "the converted config lints: $OUT"
ok "converted.toml prints back byte for byte and lints clean"

step "the new config, with fractional rates and daily quotas, loads"
run check "$NEW"
[ "$CODE" = 0 ] && [ "$OUT" = "$NEW: ok" ] || fail "check $NEW: exit $CODE: $OUT $ERR"
ok "$OUT"
run check --print "$NEW"
printf '%s\n' "$OUT" >"$WORK/new.toml"
for want in \
    'per_ip_conn_rate = "1/2s burst 30"' \
    'per_ip_conn_quota = "10000/day"' \
    'per_ip_conn_quota = "1500/6h"' \
    'per_ip_conn_rate = "90/m"'; do
    grep -q -F -x -- "$want" "$WORK/new.toml" || fail "missing $want in: $OUT"
    ok "$want"
done
# Its web listener is pacer's example listener, value for value.
web() { awk '/^\[\[listeners\]\]/{n++} n==1' "$1"; }
[ "$(web "$WORK/new.toml")" = "$(web "$WORK/converted.toml")" ] ||
    fail "listener web differs from the converted pacerd.conf"
ok "listener web is the same as in the converted pacerd.conf"

step "a quota the rate never reaches is reported on its line"
sed 's|per_ip_conn_quota = "10000/day"|per_ip_conn_quota = "50000/day"|' "$NEW" >"$WORK/unreachable.toml"
run check "$WORK/unreachable.toml"
LINE=$(line_of "$WORK/unreachable.toml" 'per_ip_conn_quota = "50000/day"')
[ "$CODE" = 1 ] || fail "expected exit 1, got $CODE: $OUT $ERR"
grep -q -F "$WORK/unreachable.toml:$LINE: warning[quota-unreachable]: listener api: per_ip_conn_quota 50000/day is never reached: per_ip_conn_rate 1/2s burst 30 admits at most 43230 new connections per client in any day" <<<"$OUT" ||
    fail "expected quota-unreachable on line $LINE: $OUT"
ok "$(head -1 <<<"$OUT" | cut -c1-120)..."

step "a rate written as a quota, and a quota written as a rate, are refused"
sed 's|per_ip_conn_quota = "1500/6h"|per_ip_conn_quota = "25/min"|' "$NEW" >"$WORK/rate.toml"
run check "$WORK/rate.toml"
[ "$CODE" = 2 ] || fail "expected exit 2, got $CODE: $OUT"
grep -q -F 'listeners[2].per_ip_conn_quota: "25/min" is a rate, not a quota' <<<"$ERR" || fail "$ERR"
ok "$(head -1 <<<"$ERR")"
sed 's|per_ip_conn_rate = "90/min"|per_ip_conn_rate = "1500/6h"|' "$NEW" >"$WORK/quota.toml"
run check "$WORK/quota.toml"
[ "$CODE" = 2 ] || fail "expected exit 2, got $CODE: $OUT"
grep -q -F 'listeners[2].per_ip_conn_rate: "1500/6h" is a quota' <<<"$ERR" || fail "$ERR"
ok "$(head -1 <<<"$ERR")"

step "pacerd.conf keeps pacerd's grammar: no daily rates there"
sed 's|per_ip_conn_rate 10/s burst 20|per_ip_conn_rate 10000/d|' "$OLD" >"$WORK/daily.conf"
LINE=$(line_of "$WORK/daily.conf" 'per_ip_conn_rate 10000/d')
run check "$WORK/daily.conf"
[ "$CODE" = 2 ] || fail "expected exit 2, got $CODE: $OUT"
[ "$ERR" = "edgefront: $WORK/daily.conf: line $LINE: per_ip_conn_rate: expected a rate like 10/s, 600/m or 1m/s, got \"10000/d\"" ] ||
    fail "$ERR"
ok "line $LINE: refused as pacerd refuses it"

step "a pacerd.conf mistake is linted on its line"
sed 's|    table_size 65536|    table_size 64|' "$OLD" >"$WORK/small.conf"
LINE=$(line_of "$WORK/small.conf" 'table_size 64')
run check "$WORK/small.conf"
[ "$CODE" = 1 ] || fail "expected exit 1, got $CODE: $OUT"
grep -q -F "$WORK/small.conf:$LINE: warning[table-below-max-conns]: listener web: table_size 64 is below max_conns 10000" <<<"$OUT" ||
    fail "$OUT"
ok "line $LINE: table-below-max-conns"

printf '\nall steps passed\n'
