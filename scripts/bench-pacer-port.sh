#!/usr/bin/env bash
# The C-versus-Rust benchmark of the pacer port, as published in
# docs/benchmarks/pacer-port.md: limiter decisions at 1 to 16 threads
# (crates/edgefront-bench/scenarios/limiter_decisions.toml), the L4 proxy
# direct, through pacerd and through edgefront limits
# (scenarios/l4_proxy.toml), and memory per tracked client at 1M clients.
# Runs inside the Docker Linux VM, where pacer measured its numbers, in
# the image scripts/bench-limiter-vm.sh builds; pacerd needs Linux.
# Writes the JSON results to docs/benchmarks/data/pacer-port-*.json.
#
# Usage: scripts/bench-pacer-port.sh
set -euo pipefail
root=$(git rev-parse --show-toplevel)
tag=edgefront-limiter-bench:1
docker image inspect "$tag" >/dev/null 2>&1 || "$root/scripts/bench-limiter-vm.sh" 0 >/dev/null
docker info --format 'docker {{.ServerVersion}}: {{.OperatingSystem}} {{.Architecture}}, {{.NCPU}} cpus'
exec docker run --rm -v "$root:/src" -w /src \
    -v edgefront-bench-target:/target -e CARGO_TARGET_DIR=/target "$tag" sh -c '
  set -e
  make -s -C legacy/pacer VARIANT=release build/release/bench_limiter build/release/pacerd >/dev/null
  cargo build -q --release --locked -p edgefront-bench -p edgefront -p edgefront-difftest
  drive=$(ls -t /target/release/build/edgefront-difftest-*/out/pacer-diffdrive | head -1)
  out=docs/benchmarks/data
  echo "== limiter decisions"
  /target/release/edgefront-bench limiter --scenario crates/edgefront-bench/scenarios/limiter_decisions.toml \
      --c-bench legacy/pacer/build/release/bench_limiter --json $out/pacer-port-decisions.json
  echo "== L4 proxy"
  /target/release/edgefront-bench l4 --scenario crates/edgefront-bench/scenarios/l4_proxy.toml \
      --pacerd legacy/pacer/build/release/pacerd --edgefront /target/release/edgefront \
      --json $out/pacer-port-l4.json
  echo "== memory per tracked client"
  /target/release/edgefront-bench memory --keys 1048576 --diffdrive "$drive" \
      --difftest /target/release/edgefront-difftest --json $out/pacer-port-memory.json'
