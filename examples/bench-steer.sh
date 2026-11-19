#!/usr/bin/env bash
# The Go-vs-Rust proxy benchmark: steer and edgefront proxy on steer's
# configuration, through every scenario in bench/scenarios/steer-port/
# (cleartext HTTP/1.1 and HTTP/1.1 over TLS at 1, 64 and 10,000
# connections, and a ramp over 64 connections), each target in its pinned
# container on the same CPUs, with the shared harness.
#
# Runs the whole set BENCH_REPEAT times (default 3), each repetition into
# BENCH_OUT/rep<N> (BENCH_OUT defaults to target/bench/steer-port), with the
# harness's Markdown report beside each, then prints the Go-vs-Rust tables
# of docs/benchmarks/steer-go-vs-rust.md (scripts/steer-bench-summary.py),
# each number the median of the repetitions. Needs Docker with at least 16
# CPUs: the targets get 4, the origin 6 and the load generator the rest.
# BENCH_REBUILD=1 rebuilds the images built from this repository (steer from
# legacy/steer's last commit, edgefront at this commit).
set -euo pipefail

cd "$(dirname "$0")/.."
OUT=${BENCH_OUT:-target/bench/steer-port}
RUN=${BENCH_RUN:-steer-port}
REPEAT=${BENCH_REPEAT:-3}
MACHINE=${BENCH_MACHINE:-$(uname -sm)}
REBUILD=()
[[ ${BENCH_REBUILD:-0} == 1 ]] && REBUILD=(--rebuild)

fail() { echo "FAIL: $*" >&2; exit 1; }
command -v docker >/dev/null || fail "docker is not installed"
docker info >/dev/null 2>&1 || fail "docker is not running"

cargo build --release --locked -p edgefront-bench --bin edgefront-bench
BENCH=target/release/edgefront-bench
rm -rf "$OUT"
mkdir -p "$OUT"

# Cleartext scenarios against the cleartext targets, TLS scenarios against
# the TLS ones; one invocation each per repetition.
plain=() tls=()
for s in bench/scenarios/steer-port/*.toml; do
    case $(basename "$s") in
        tls-*) tls+=(--scenario "$s") ;;
        *) plain+=(--scenario "$s") ;;
    esac
done
dirs=()
for i in $(seq 1 "$REPEAT"); do
    dir=$OUT/rep$i
    "$BENCH" run "${plain[@]}" --targets steer,edgefront-proxy --out "$dir" --run "$RUN-$i" \
        --machine "$MACHINE" ${REBUILD[@]+"${REBUILD[@]}"}
    REBUILD=()
    "$BENCH" run "${tls[@]}" --targets steer-tls,edgefront-proxy-tls --out "$dir" --run "$RUN-$i" \
        --machine "$MACHINE"
    files=("$dir"/*/*.json)
    "$BENCH" validate "${files[@]}" || fail "a result breaks bench/schema/result.schema.json"
    "$BENCH" report --results "$dir" --out "$dir/report.md" --title "steer against edgefront proxy, repetition $i"
    dirs+=("$dir")
done
scripts/steer-bench-summary.py "${dirs[@]}" | tee "$OUT/summary.md"
echo "wrote $REPEAT repetitions under $OUT and $OUT/summary.md"
