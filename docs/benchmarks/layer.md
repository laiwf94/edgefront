# The cache as a route layer: what it costs

Week 2 moved the cache into a layer that runs in the proxy's filter phases (`edgefront_core::filter`, `edgefront_cache::layer::CacheLayer`). cachet's node now serves through that same layer. Two questions follow:

1. What does a route that does not use the cache pay for the layer existing?
2. Is a cache hit through the layer as fast as cachet 0.1's own hit path?

Measured on 13 Nov 2026 on a Linux x86_64 VM with 4 CPUs, Rust 1.100, release builds. Load generator, node and origin share the machine, as in the README's table. cachet 0.1 is commit `ed77c32`, the build `make difftest-cache` compares against.

## In-process: `benches/layer_overhead.rs`

`cargo bench --bench layer_overhead`. Every variant builds the same request and drains the same 4 KiB body. The upstream answers at once from memory, so the differences are the pipeline's own cost.

| Benchmark | What runs | Time |
|---|---|---|
| `layer/bypass/direct` | The upstream, called directly | 256 ns |
| `layer/bypass/route_filters` | The proxy's bypass: look up the route's filter, find none, call the upstream | 248 ns |
| `layer/bypass/run` | The same through `filter::run(None, …)` | 300 ns |
| `layer/hit_4k/layer` | A 4 KiB hit answered by the layer's request phase | 1.12 µs |
| `layer/hit_4k/node` | The same hit through cachet's node: its checks, the layer, request ID, metrics and access log | 1.71 µs |

**Bypass.** A route without a cache block costs the proxy one lookup in its `RouteFilters` table and a branch, which is within the run-to-run noise of a direct call (−8 ns here). `filter::run` adds 44 ns. Both are well under the 200 ns the week allows.

**Hit, against cachet 0.1.** `benches/hit_path.rs`'s `proxy/hit_4k` is the same benchmark in both trees: one 4 KiB hit through the node's request handler. Runs alternated between the trees, 10 s each:

| Build | Run 1 | Run 2 | Run 3 |
|---|---|---|---|
| cachet 0.1 (`ed77c32`) | 1.77 µs | 1.74 µs | 1.66 µs |
| this tree, through `CacheLayer` | 1.44 µs | 1.44 µs | 1.46 µs |

The layered hit is about 15% faster. Where the time went was not profiled; the point for this week is that moving the cache into the layer cost the hit path nothing.

## End to end: `examples/load.sh`

`CACHET_BIN=<binary> examples/load.sh`, 64 connections for 10 s per scenario, oha 1.16.0. The hit scenario is a 4 KiB body. Hit throughput, three alternating runs:

| Build | Run 1 | Run 2 | Run 3 | Mean |
|---|---|---|---|---|
| `edgefront` (through `CacheLayer`) | 158,226 | 143,875 | 149,919 | 150,673 req/s |
| cachet 0.1 | 149,812 | 140,124 | 139,356 | 143,097 req/s |

The layered binary is 5% above cachet 0.1, inside the week's "within 3%" bound. One full run of each:

| Scenario | edgefront | cachet 0.1 |
|---|---|---|
| Origin directly, 4 KiB | 181,451 req/s, p99 1.03 ms | 181,996 req/s, p99 1.03 ms |
| Hits, 4 KiB | 147,002 req/s, p50 0.38 ms, p99 1.31 ms | 140,063 req/s, p50 0.41 ms, p99 1.26 ms |
| Misses, every URL new | 35,707 req/s, p99 3.37 ms | 31,566 req/s, p99 3.73 ms |
| 500 requests, 250 at once, one cold 300 ms URL | 1 origin request | 1 origin request |

On a 4-CPU machine where oha takes much of the CPU, the gap between the two builds is near the run-to-run spread. The fair reading is "no slower". The in-process numbers above isolate the hit path itself.
