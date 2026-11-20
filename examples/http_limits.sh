#!/usr/bin/env bash
# HTTP rate limiting rules on edgefront proxy, end to end: a login path
# limited on failed attempts only, and an API limited per API key header,
# with the RateLimit-Policy, RateLimit and Retry-After headers. Each step
# prints what happened and checks it; the script exits non-zero if any
# check fails.
#
#   1. a user who logs in correctly is never limited
#   2. a password guesser is refused after five wrong passwords
#   3. an API key over ten requests a second is refused, other keys are not
#   4. edgefront check reads the rules and lints ones that cannot match
#
# Runs on Linux and macOS with python3 and curl. EDGEFRONT_BIN names a
# built binary to use instead of building one;
# crates/edgefront/tests/e2e_http_limits.rs runs the script that way. The
# ports are 19100 to 19102 unless HTTP_DEMO_PORT names the first of three
# others.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ -z "${EDGEFRONT_BIN:-}" ]; then
    cargo build --release --locked -q -p edgefront --bin edgefront
    EDGEFRONT_BIN=target/release/edgefront
fi

work=$(mktemp -d /tmp/httpdemo.XXXXXX)
up_port=${HTTP_DEMO_PORT:-19100}
px_port=$((up_port + 1))
admin_port=$((up_port + 2))
failures=0

cleanup() {
    [ -n "${edge_pid:-}" ] && kill "$edge_pid" 2>/dev/null || true
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

# One request: prints the status, then the headers asked for.
req() {
    local method=$1 path=$2
    shift 2
    curl -s -o /dev/null -D "$work/head" -X "$method" "$@" "http://127.0.0.1:$px_port$path" \
        -w '%{http_code}'
}
# N requests for PATH at once, over N connections; prints each status.
burst() {
    local n=$1 path=$2
    shift 2
    local urls=()
    for _ in $(seq "$n"); do urls+=(-o /dev/null "http://127.0.0.1:$px_port$path"); done
    curl -s --parallel --parallel-immediate --parallel-max "$n" "$@" \
        -w '%{http_code}\n' "${urls[@]}"
}
head_of() {
    grep -i "^$1:" "$work/head" | cut -d' ' -f2- | tr -d '\r'
}

cat > "$work/proxy.json" <<CONF
{"listen":"127.0.0.1:$px_port","admin_listen":"127.0.0.1:$admin_port",
 "routes":[{"host":"*","path_prefix":"/","cluster":"app"}],
 "clusters":[{"name":"app","endpoints":[{"address":"127.0.0.1:$up_port"}]}]}
CONF

python3 examples/http_limits/upstream.py "$up_port" &
up_pid=$!
EDGEFRONT_LOG=info EDGEFRONT_DRAIN_PERIOD=0s "$EDGEFRONT_BIN" proxy --config "$work/proxy.json" \
    --limits examples/http_limits/limits.toml >"$work/edgefront.log" 2>&1 &
edge_pid=$!
for _ in $(seq 100); do
    grep -q '"listening"' "$work/edgefront.log" 2>/dev/null &&
        curl -s -o /dev/null "http://127.0.0.1:$up_port/" && break
    sleep 0.1
done
echo "$("$EDGEFRONT_BIN" --version) proxy on 127.0.0.1:$px_port, limits from examples/http_limits/limits.toml"
echo "login: 5 failed logins per account per minute (only 401 and 403 count)"
echo "api:   10 requests per second per API key under /api"

echo
echo "1. alice logs in correctly 20 times"
ok=0
for _ in $(seq 20); do
    s=$(req POST /login -H 'x-user: alice' -H 'x-password: right')
    [ "$s" = 200 ] && ok=$((ok + 1))
done
check "logins answered 200" "$ok" 20
check "RateLimit-Policy" "$(head_of ratelimit-policy)" '"login";q=5;w=60'
check "RateLimit after a success" "$(head_of ratelimit)" '"login";r=5'

echo
echo "2. mallory guesses alice's password... as mallory, 6 times"
statuses=""
for _ in $(seq 6); do
    statuses="$statuses $(req POST /login -H 'x-user: mallory' -H 'x-password: guess')"
done
check "statuses" "${statuses# }" "401 401 401 401 401 429"
check "Retry-After" "$(head_of retry-after)" 12
# The first failure's token is back 12 s after it was spent: a little
# under 12 s from now.
limit=$(head_of ratelimit)
check "RateLimit on the refusal" "${limit%=*}=" '"login";r=0;t=11;edgefront-ms='
ms=${limit##*=}
check "edgefront-ms within 12 s" "$([ "$ms" -gt 11000 ] && [ "$ms" -le 12000 ] && echo yes)" yes
check "the right password is refused too" "$(req POST /login -H 'x-user: mallory' -H 'x-password: right')" 429
check "alice is unaffected" "$(req POST /login -H 'x-user: alice' -H 'x-password: right')" 200

echo
echo "3. key k1 sends 15 API requests at once, then k2 sends one"
# One curl sends them together, so no token refills in between.
codes=$(burst 15 /api/items -H 'x-api-key: k1')
echo "   statuses: $(tr "\n" " " <<<"$codes")"
ok=$(grep -cx 200 <<<"$codes" || true)
limited=$(grep -cx 429 <<<"$codes" || true)
check "k1 admitted" "$ok" 10
check "k1 refused" "$limited" 5
check "k2 admitted" "$(req GET /api/items -H 'x-api-key: k2')" 200
check "no key: not limited" "$(req GET /api/items)" 200
check "outside /api: not limited" "$(req GET /home -H 'x-api-key: k1')" 200
# A refusal says when the next token is due: one every 100 ms at 10/s.
req GET /api/items -H 'x-api-key: k1' >/dev/null
ms=$(head_of ratelimit | sed 's/.*edgefront-ms=//')
check "next token due within 100 ms" "$([ "$ms" -ge 1 ] && [ "$ms" -le 100 ] && echo yes)" yes
echo "   waiting ${ms} ms"
sleep "$(awk "BEGIN { print $ms / 1000 }")"
check "k1 admitted once it is due" "$(req GET /api/other -H 'x-api-key: k1')" 200
echo
echo "4. edgefront check"
check "the demo's rules" "$("$EDGEFRONT_BIN" check examples/http_limits/limits.toml | tail -1)" \
    "examples/http_limits/limits.toml: ok"
set +e
"$EDGEFRONT_BIN" check examples/http_limits/lint.toml >"$work/lint.out"
code=$?
set -e
sed 's/^/   /' "$work/lint.out"
check "exit code" "$code" 1
check "findings" "$(grep -c 'warning\[' "$work/lint.out")" 2

echo
if [ "$failures" -ne 0 ]; then
    echo "$failures check(s) failed"
    exit 1
fi
echo "all checks passed"
