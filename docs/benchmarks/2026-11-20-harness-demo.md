# Harness demo: cache-hit against edgefront and Varnish

The first run of `examples/bench.sh` (#84), on 20 Nov 2026 on this repository's benchmark machine, a 16-core arm64 Mac running Docker 28. Other builds shared the machine. The raw results are in [data/2026-11-20-harness-demo](data/2026-11-20-harness-demo/). This is a demonstration of the harness, not the baseline: the baseline report with every target and scenario comes with #87.

Machine: 16-core arm64, Docker 28, with 16 CPUs and 7.7 GiB for the container runtime (docker 28.0.4, kernel 6.10.14-linuxkit, aarch64). The target ran on CPUs 0-3, the origin on 4-9 and the load generator on 10-15.

## Cache

### cache-hit

1,000 small objects under a Zipf 1.0 law, 98% hits at a constant 5,000 requests per second.

Load: 5,000 req/s over 64 HTTP/1.1 connections, 60 s measured after 10 s of warm-up. Hit ratio target 98%, working set 3.6 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront | 0.1.0 | 5,000 req/s | 46 µs | 124 µs | 196 µs | 98.0% | 0 | 82.9 MiB | 85.3 MiB | 11.0 s (0.18 cores) | 12.7 MiB |
| varnish | 9.0.4 | 5,000 req/s | 46 µs | 180 µs | 593 µs | 98.0% | 0 | 129.1 MiB | 141.4 MiB | 8.4 s (0.14 cores) | 119.8 MiB |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/cache-hit.toml --targets edgefront,varnish --out target/bench/demo --run demo
```

Latency is response time from each request's scheduled send time, so queueing behind a stall counts (no coordinated omission). Hit ratio is the share of measured requests the origin never saw. Peak RSS is the highest resident set (anon plus mapped file pages) summed over the target's containers, sampled every 100 ms; peak memory is the cgroup's `memory.peak`, page cache and tmpfs included. CPU is the target's CPU time over the measured phase. Image is the uncompressed size of the target's images.
