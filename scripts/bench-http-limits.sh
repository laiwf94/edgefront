#!/usr/bin/env bash
# Week 6 of the limiter track: HTTP limits at 50,000 requests per second
# over 100,000 paths (crates/edgefront-bench/scenarios/http_limits.toml),
# through edgefront proxy without limits and with them
# (bench/targets/edgefront-proxy-limits), BENCH_REPEAT times (default 3),
# each repetition into $OUT/rep<N>, the two targets taking turns.
# Afterwards `edgefront-bench http-limits` replays the scenario through the
# same rules and holds every limited run's 2xx and 4xx to it, and
# `edgefront-bench report` renders the results.
#
#   OUT=docs/benchmarks/data/http-limits scripts/bench-http-limits.sh
#
# BENCH_REBUILD=1 rebuilds the edgefront image from this checkout.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=${OUT:-target/bench/http-limits}
REPEAT=${BENCH_REPEAT:-3}
MACHINE=${BENCH_MACHINE:-"Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB)"}
SCENARIO=crates/edgefront-bench/scenarios/http_limits.toml
LIMITS=bench/targets/edgefront-proxy-limits/limits.toml
REBUILD=()
[[ ${BENCH_REBUILD:-0} == 1 ]] && REBUILD=(--rebuild)

cargo build --release --locked -q -p edgefront-bench --bin edgefront-bench
BENCH=target/release/edgefront-bench
results=()
for i in $(seq "$REPEAT"); do
    if (( i % 2 )); then targets=edgefront-proxy,edgefront-proxy-limits; else targets=edgefront-proxy-limits,edgefront-proxy; fi
    "$BENCH" run --scenario "$SCENARIO" --targets "$targets" --out "$OUT/rep$i" --run "http-limits-$i" \
        --machine "$MACHINE" ${REBUILD[@]+"${REBUILD[@]}"}
    REBUILD=()
    results+=(--result "$OUT/rep$i/http-limits/edgefront-proxy-limits.json")
done
"$BENCH" validate $(find "$OUT" -name '*.json' -path '*rep*' | sort)
"$BENCH" http-limits --scenario "$SCENARIO" --limits "$LIMITS" "${results[@]}" --json "$OUT/accuracy.json" \
    | tee "$OUT/accuracy.md"
dirs=()
for i in $(seq "$REPEAT"); do dirs+=(--results "$OUT/rep$i"); done
"$BENCH" report "${dirs[@]}" --out "$OUT/report.md" --title "HTTP limits on edgefront proxy"
