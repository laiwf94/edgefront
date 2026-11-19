#!/usr/bin/env bash
# The benchmark harness end to end: the cache-hit scenario against edgefront
# and Varnish, each in its pinned container, then the Markdown report.
#
# Needs Docker (or a docker-compatible runtime) with at least 3 CPUs. The
# images built from this repository are built first if they are missing;
# BENCH_REBUILD=1 rebuilds them. BENCH_TARGETS and BENCH_SCENARIO pick other
# targets and scenarios; BENCH_OUT is where results go (default
# target/bench/demo). Each step fails the script if it did not do what it
# says.
set -euo pipefail

cd "$(dirname "$0")/.."
SCENARIO=${BENCH_SCENARIO:-bench/scenarios/cache-hit.toml}
TARGETS=${BENCH_TARGETS:-edgefront,varnish}
OUT=${BENCH_OUT:-target/bench/demo}
REBUILD=()
[[ ${BENCH_REBUILD:-0} == 1 ]] && REBUILD=(--rebuild)

fail() { echo "FAIL: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }
ok() { echo "   ok: $*"; }

command -v docker >/dev/null || fail "docker is not installed"
docker info >/dev/null 2>&1 || fail "docker is not running"
start=$(date +%s)

step "Build the harness"
cargo build --release --locked -p edgefront-bench --bin edgefront-bench
BENCH=target/release/edgefront-bench
ok "$BENCH"

step "Run $(basename "$SCENARIO" .toml) against $TARGETS"
rm -rf "$OUT"
"$BENCH" run --scenario "$SCENARIO" --targets "$TARGETS" --out "$OUT" --run demo ${REBUILD[@]+"${REBUILD[@]}"}

step "Check the results"
name=$(basename "$SCENARIO" .toml)
IFS=, read -ra names <<<"$TARGETS"
files=()
for t in "${names[@]}"; do
    file=$OUT/$name/$t.json
    [[ -f $file ]] || fail "no result for $t"
    files+=("$file")
done
"$BENCH" validate "${files[@]}" || fail "a result breaks bench/schema/result.schema.json"
ok "${#files[@]} results match the schema"
for file in "${files[@]}"; do
    # Every request answered, none failed, and the target cached: the
    # origin saw at most the misses the scenario plans for, plus a point.
    python3 - "$file" <<'EOF' || fail "$file"
import json, sys
r = json.load(open(sys.argv[1]))
m, s = r["measured"], r["scenario"]
name = r["target"]["name"]
errors = sum(m["errors"].values())
assert m["completed"] == m["scheduled"], f"{name}: {m['completed']} of {m['scheduled']} answered"
assert errors == 0, f"{name}: {errors} errors"
assert m["status_classes"][1] == m["completed"], f"{name}: not every response was 2xx"
if r["target"]["kind"] == "cache":
    assert m["hit_ratio"] >= s["hit_ratio_target"] - 0.01, f"{name}: hit ratio {m['hit_ratio']:.4f}"
assert r["resources"]["cpu_seconds"] > 0 and r["resources"]["peak_rss_bytes"] > 0, f"{name}: nothing measured"
print(f"   ok: {name}: {m['throughput_rps']:,.0f} req/s, p99 {m['latency']['p99_ms']:.3f} ms, "
      f"hit ratio {m['hit_ratio']:.1%}, peak RSS {r['resources']['peak_rss_bytes'] / 2**20:.1f} MiB")
EOF
done

step "Report"
[[ -s $OUT/report.md ]] || fail "no report"
cat "$OUT/report.md"
elapsed=$(($(date +%s) - start))
ok "wrote $OUT/report.md in ${elapsed} s"
