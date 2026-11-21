#!/usr/bin/env bash
# Compression live: the release binary with [cache.compression] in front of
# the example origin. A browser-like client loads a JavaScript bundle, then
# the next release of it twice: once with plain brotli, once holding the
# first release as a shared dictionary (RFC 9842). Each step prints the
# bytes on the wire and fails the script if the cache did not behave as
# described. tests/compression.rs runs the same behaviour as tests.
set -euo pipefail

cd "$(dirname "$0")/.."
ORIGIN=127.0.0.1:9400
NODE=127.0.0.1:8401
ADMIN=127.0.0.1:9401
WORK=$(mktemp -d)
export CACHET_LOG=${CACHET_LOG:-warn}

cleanup() {
    for pid in ${NODE_PID:-} ${ORIGIN_PID:-}; do
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

# fetch <name> <path> [header...]: saves the body to $WORK/<name> and its
# headers to $WORK/<name>.h, and prints the coding and size.
fetch() {
    local name=$1 path=$2
    shift 2
    local args=()
    for h in "$@"; do args+=(-H "$h"); done
    curl -sS "${args[@]}" -D "$WORK/$name.h" -o "$WORK/$name" "http://$NODE$path"
    local enc
    enc=$(header "$name" content-encoding)
    local label=${1:-no headers}
    (($# > 1)) && label="$label, with the dictionary"
    printf '   GET %-14s %-62s -> %-8s %7d bytes\n' "$path" "($label)" "${enc:-identity}" "$(size "$name")"
}
# header <name> <field>: the field's first value, empty when it is absent.
header() { { grep -i "^$2:" "$WORK/$1.h" || true; } | head -1 | cut -d' ' -f2- | tr -d '\r'; }
size() { wc -c <"$WORK/$1" | tr -d ' '; }

cat >"$WORK/node.toml" <<TOML
node_id = "edge"
listen = "$NODE"
admin_listen = "$ADMIN"
[[origins]]
name = "origin"
url = "http://$ORIGIN"
[cache.compression]
enabled = true
TOML

cargo build --release --quiet -p edgefront --bin edgefront
cargo build --release --quiet -p cachet --example origin
target/release/examples/origin "$ORIGIN" >/dev/null &
ORIGIN_PID=$!
wait_for "http://$ORIGIN/healthz"
target/release/edgefront --config "$WORK/node.toml" &
NODE_PID=$!
wait_for "http://$ADMIN/healthz"

step "1. The first visit loads release 1, which offers itself as a dictionary"
fetch v1 /js/app.v1.js "Accept-Encoding: identity"
[[ $(header v1 use-as-dictionary) == *'app.*.js'* ]] || fail "no Use-As-Dictionary on release 1"
ok "Use-As-Dictionary: $(header v1 use-as-dictionary)"
HASH=$(python3 -c "import base64,hashlib,sys; print(base64.b64encode(hashlib.sha256(open(sys.argv[1],'rb').read()).digest()).decode())" "$WORK/v1")
ok "the client keeps it under sha-256 :$HASH:"

step "2. Release 2 for a client without the dictionary: stored once per coding"
fetch v2 /js/app.v2.js "Accept-Encoding: identity"
for _ in $(seq 100); do
    fetch br /js/app.v2.js "Accept-Encoding: gzip, deflate, br, zstd" >/dev/null
    [[ $(header br content-encoding) == br ]] && break
    sleep 0.1
done
fetch br /js/app.v2.js "Accept-Encoding: gzip, deflate, br, zstd"
fetch gzip /js/app.v2.js "Accept-Encoding: gzip"
[[ $(header br content-encoding) == br && $(header gzip content-encoding) == gzip ]] ||
    fail "release 2 should come as br and gzip"
[[ $(header br vary) == accept-encoding ]] || fail "Vary should be accept-encoding, got $(header br vary)"
[[ $(curl -fsS "http://$ORIGIN/stats" | python3 -c "import json,sys; print(json.load(sys.stdin)['/js/app.v2.js'])") == 1 ]] ||
    fail "release 2 should have reached the origin once"
ok "one origin request; br and gzip served from the cache"

step "3. Release 2 for the client holding release 1: dictionary-compressed"
DICT=("Available-Dictionary: :$HASH:" "Dictionary-ID: \"app\"" "Sec-Fetch-Dest: script")
fetch dcb /js/app.v2.js "Accept-Encoding: gzip, deflate, br, zstd, dcb, dcz" "${DICT[@]}"
fetch dcz /js/app.v2.js "Accept-Encoding: zstd, dcz" "${DICT[@]}"
[[ $(header dcb content-encoding) == dcb && $(header dcz content-encoding) == dcz ]] ||
    fail "release 2 should come as dcb and dcz"
[[ $(header dcb vary) == "accept-encoding, available-dictionary" ]] || fail "Vary was $(header dcb vary)"
# Each body starts with its coding's magic and the dictionary's hash.
python3 - "$WORK" "$HASH" <<'PY' || fail "a dictionary-compressed body names the wrong dictionary"
import base64, sys
work, h = sys.argv[1], base64.b64decode(sys.argv[2])
for name, magic in (("dcb", b"\xffDCB"), ("dcz", b"\x5e\x2a\x4d\x18\x20\x00\x00\x00")):
    body = open(f"{work}/{name}", "rb").read()
    assert body.startswith(magic + h), name
PY
ok "both bodies carry their magic and release 1's hash"
if command -v zstd >/dev/null; then
    tail -c +41 "$WORK/dcz" | zstd -q -d -D "$WORK/v1" -o "$WORK/dcz.out"
    cmp -s "$WORK/dcz.out" "$WORK/v2" || fail "dcz did not decode to release 2"
    ok "the zstd command line decodes dcz with release 1 to release 2, byte for byte"
else
    echo "   (no zstd command here, so dcz is not decoded; tests/compression.rs decodes both)"
fi

step "4. Bytes saved"
python3 - "$(size v2)" "$(size br)" "$(size gzip)" "$(size dcb)" "$(size dcz)" <<'PY'
import sys
identity, br, gz, dcb, dcz = map(int, sys.argv[1:])
print(f"   release 2 as identity {identity:>7} bytes")
print(f"             gzip     {gz:>7} bytes  ({100 * (1 - gz / identity):.1f}% saved)")
print(f"             br       {br:>7} bytes  ({100 * (1 - br / identity):.1f}% saved)")
for name, n in (("dcb", dcb), ("dcz", dcz)):
    print(f"             {name}      {n:>7} bytes  ({100 * (1 - n / br):.1f}% under br)")
    if n > 0.4 * br:
        sys.exit(f"{name} should be at least 60% under br")
PY
ok "a repeat download with the dictionary is at least 60% under plain brotli"

step "5. What the compression cost (/metrics)"
curl -fsS "http://$ADMIN/metrics" |
    grep -E '^cachet_compression_(cpu_microseconds_per_mib|skipped_total)' | sed 's/^/   /'
