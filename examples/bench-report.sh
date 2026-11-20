#!/usr/bin/env bash
# Regenerates the November 2026 baseline report from its raw results and
# checks it matches the committed one: every number in
# docs/benchmarks/2026-11-baseline.md comes from a file under
# docs/benchmarks/data/2026-11/ (the cache track) or
# docs/benchmarks/data/steer-go-vs-rust/ (the proxy track's week 4 runs,
# three repetitions, shown as medians) or docs/benchmarks/data/pacer-port-*.json
# (the limiter track's week 4 C-against-Rust runs, in the formats
# `edgefront-bench limiter`, `l4` and `memory` write), and a result that
# breaks the schema stops the script.
#
# REPORT_WRITE=1 writes the report instead of checking it, after new raw
# results are added (by any track: the report has a section per track).
set -euo pipefail

cd "$(dirname "$0")/.."
DATA=docs/benchmarks/data/2026-11
PROXY=docs/benchmarks/data/steer-go-vs-rust
LIMITER=docs/benchmarks/data
REPORT=docs/benchmarks/2026-11-baseline.md

fail() { echo "FAIL: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }
ok() { echo "   ok: $*"; }

step "Build the harness"
cargo build --release --locked -p edgefront-bench --bin edgefront-bench
BENCH=target/release/edgefront-bench

step "Validate the raw results"
files=()
while IFS= read -r f; do files+=("$f"); done < <(find "$DATA" "$PROXY" -name '*.json' | sort)
[[ ${#files[@]} -gt 0 ]] || fail "no results under $DATA"
"$BENCH" validate "${files[@]}" || fail "a result breaks bench/schema/result.schema.json"
ok "${#files[@]} results match the schema"

step "Render"
out=$(mktemp)
trap 'rm -f "$out"' EXIT
"$BENCH" report --results "$DATA" --results "$PROXY" --out "$out" \
    --title "Baseline benchmark, November 2026" --intro "$DATA/intro.md" --limiter "$LIMITER"
if [[ ${REPORT_WRITE:-0} == 1 ]]; then
    cp "$out" "$REPORT"
    ok "wrote $REPORT"
else
    diff -u "$REPORT" "$out" || fail "$REPORT differs from what its raw results render; run with REPORT_WRITE=1"
    ok "$REPORT matches its ${#files[@]} raw results"
fi
