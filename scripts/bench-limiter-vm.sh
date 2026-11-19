#!/usr/bin/env bash
# Runs libpacer's limiter benchmark (legacy/pacer/bench/bench_limiter.c) and
# edgefront-limiter's (crates/edgefront-limiter/benches/decisions.rs) in a
# Linux container, the way pacer measured its published numbers: inside the
# Docker Desktop Linux VM. Builds the toolchain image on first use (Ubuntu
# 24.04, clang 18, Rust 1.99.1), then runs each benchmark RUNS times.
#
# Usage: scripts/bench-limiter-vm.sh [runs]     (default 4)
# Needs Docker. The cargo build cache lives in the docker volume
# edgefront-bench-target, so later runs do not rebuild from scratch.
set -euo pipefail

runs=${1:-4}
root=$(git rev-parse --show-toplevel)
tag=edgefront-limiter-bench:1
if ! docker image inspect "$tag" >/dev/null 2>&1; then
    docker build -q -t "$tag" - >/dev/null <<'DOCKERFILE'
FROM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      clang-18 llvm-18 make git ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && ln -sf /usr/bin/clang-18 /usr/local/bin/clang && ln -sf /usr/bin/clang-18 /usr/local/bin/cc
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.99.1
ENV PATH=/root/.cargo/bin:$PATH
DOCKERFILE
fi
docker info --format 'docker {{.ServerVersion}}: {{.OperatingSystem}} {{.Architecture}}, {{.NCPU}} cpus'
exec docker run --rm -v "$root:/src" -w /src \
    -v edgefront-bench-target:/target -e CARGO_TARGET_DIR=/target "$tag" sh -c "
  set -e
  make -s -C legacy/pacer bench-build >/dev/null
  cargo bench --locked -p edgefront-limiter --bench decisions --no-run 2>/dev/null
  for i in \$(seq $runs); do legacy/pacer/build/release/bench_limiter; done
  for i in \$(seq $runs); do
    cargo bench -q --locked -p edgefront-limiter --bench decisions 2>&1 | grep -E '^decisions/|Melem/s'
  done"
