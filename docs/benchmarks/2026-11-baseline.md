# Baseline benchmark, November 2026

The first published comparison from the shared harness (#84), for the cache track's week 4 (#87): edgefront against Varnish, NGINX and Apache Traffic Server. It covers the cache-hit, cache-miss (`miss-heavy`, 50% hits), mixed 90% (`web-mix`) and large-object scenarios. Each run gives every target the same origin, cache size, CPUs and worker count; [bench/targets/README.md](../../bench/targets/README.md) lists every configuration difference from the products' defaults. The raw results are under [data/2026-11/](data/2026-11/), and `examples/bench-report.sh` regenerates this page from them.

Recorded on 20 Nov 2026 (07:04 to 07:34 UTC+8) on the benchmark machine, a 16-core arm64 Mac running Docker 28. The runs were sequential and the runtime lock held throughout, but other work on the Mac kept its one-minute load average between 4.4 and 37 (median 8.4). Read the sub-millisecond p99s and the p99.9s as upper bounds: the same `cache-hit` run on a quieter machine gave edgefront 124 µs and Varnish 180 µs at p99 ([the harness demo](2026-11-20-harness-demo.md)). The medians, hit ratios, memory and CPU numbers moved much less between runs.

What the numbers say:

- **Hits.** At 5,000 requests per second, every target serves the cache-hit scenario at a p50 of 49 to 73 µs. edgefront had the lowest p99 (151 µs) and p99.9 (727 µs) in this run.
- **Memory is edgefront's weak point.** With the same cache size as the others (the working set plus a quarter, 1.07 GiB for `miss-heavy`), edgefront's resident set peaked at 2.8 GiB in `miss-heavy` and 3.0 GiB in `web-mix`. NGINX keeps its cache in page cache and peaked at about 40 MiB of RSS. The memory store's byte budget does not bound its real footprint; that is the cache track's next memory item.
- **Hit ratios.** Every target is below the 50% target in `miss-heavy` (40.6 to 47.5%), because once-only objects compete for the same space. In `large-objects`, edgefront reached 86.7% against NGINX's 93.8%.
- **Varnish** failed 53 requests in `web-mix` (out of 600,000) and peaked at 4.4 GiB.

The proxy (Go against Rust) and limiter (C against Rust) week 4 tables join this page as those tracks publish their raw results under `data/2026-11/`. Each result's `track` field files it into its own section.

Machine: 16-core arm64, Docker 28, with 16 CPUs and 7.7 GiB for the container runtime (docker 28.0.4, kernel 6.10.14-linuxkit, aarch64). The target ran on CPUs 0-3, the origin on 4-9 and the load generator on 10-15.

## Cache

### cache-hit

1,000 small objects under a Zipf 1.0 law, 98% hits at a constant 5,000 requests per second.

Load: 5,000 req/s over 64 HTTP/1.1 connections, 60 s measured after 10 s of warm-up. Hit ratio target 98%, working set 3.6 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ats | 10.1.2 | 5,000 req/s | 65 µs | 881 µs | 5.21 ms | 98.0% | 0 | 92.4 MiB | 142.9 MiB | 11.1 s (0.19 cores) | 210.9 MiB |
| edgefront | 0.1.0 | 5,000 req/s | 49 µs | 151 µs | 727 µs | 98.0% | 0 | 82.8 MiB | 85.0 MiB | 11.4 s (0.19 cores) | 13.4 MiB |
| nginx | 1.30.7 | 5,000 req/s | 73 µs | 3.38 ms | 17.19 ms | 98.0% | 0 | 31.6 MiB | 93.9 MiB | 13.4 s (0.22 cores) | 24.8 MiB |
| varnish | 9.0.4 | 5,000 req/s | 57 µs | 2.19 ms | 11.24 ms | 98.0% | 0 | 129.8 MiB | 166.7 MiB | 10.2 s (0.17 cores) | 119.8 MiB |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/cache-hit.toml --scenario bench/scenarios/miss-heavy.toml --scenario bench/scenarios/web-mix.toml --scenario bench/scenarios/large-objects.toml --targets edgefront,varnish,nginx,ats --out docs/benchmarks/data/2026-11/cache --run 2026-11-baseline --rebuild
```

### large-objects

200 objects of 1 to 16 MiB under a Zipf 0.8 law, 95% hits at 200 requests per second.

Load: 200 req/s over 32 HTTP/1.1 connections, 60 s measured after 30 s of warm-up. Hit ratio target 95%, working set 725.0 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ats | 10.1.2 | 200 req/s | 1.51 ms | 10.34 ms | 21.02 ms | 85.7% | 0 | 894.5 MiB | 1819.8 MiB | 17.8 s (0.30 cores) | 210.9 MiB |
| edgefront | 0.1.0 | 200 req/s | 1.38 ms | 10.29 ms | 14.79 ms | 86.7% | 0 | 1614.4 MiB | 1626.1 MiB | 17.6 s (0.29 cores) | 13.4 MiB |
| nginx | 1.30.7 | 200 req/s | 1.80 ms | 9.52 ms | 13.53 ms | 93.8% | 0 | 30.8 MiB | 1471.5 MiB | 19.2 s (0.32 cores) | 24.8 MiB |
| varnish | 9.0.4 | 200 req/s | 2.07 ms | 9.03 ms | 13.18 ms | 92.2% | 0 | 1505.4 MiB | 1525.1 MiB | 18.5 s (0.31 cores) | 119.8 MiB |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/cache-hit.toml --scenario bench/scenarios/miss-heavy.toml --scenario bench/scenarios/web-mix.toml --scenario bench/scenarios/large-objects.toml --targets edgefront,varnish,nginx,ats --out docs/benchmarks/data/2026-11/cache --run 2026-11-baseline --rebuild
```

### miss-heavy

50,000 objects of 4 to 64 KiB under a Zipf 0.8 law, 50% hits at 3,000 requests per second.

Load: 3,000 req/s over 128 HTTP/1.1 connections, 60 s measured after 20 s of warm-up. Hit ratio target 50%, working set 878.8 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ats | 10.1.2 | 3,000 req/s | 103 µs | 1.25 ms | 8.50 ms | 40.6% | 0 | 344.2 MiB | 1451.0 MiB | 10.3 s (0.17 cores) | 210.9 MiB |
| edgefront | 0.1.0 | 3,000 req/s | 104 µs | 515 µs | 8.75 ms | 47.5% | 0 | 2800.5 MiB | 2810.6 MiB | 12.8 s (0.21 cores) | 13.4 MiB |
| nginx | 1.30.7 | 3,000 req/s | 119 µs | 1.70 ms | 6.09 ms | 45.5% | 0 | 39.4 MiB | 1607.4 MiB | 13.3 s (0.22 cores) | 24.8 MiB |
| varnish | 9.0.4 | 3,000 req/s | 225 µs | 22.27 ms | 55.74 ms | 46.1% | 0 | 2128.4 MiB | 2150.9 MiB | 25.5 s (0.43 cores) | 119.8 MiB |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/cache-hit.toml --scenario bench/scenarios/miss-heavy.toml --scenario bench/scenarios/web-mix.toml --scenario bench/scenarios/large-objects.toml --targets edgefront,varnish,nginx,ats --out docs/benchmarks/data/2026-11/cache --run 2026-11-baseline --rebuild
```

### web-mix

10,000 objects from 1 KiB to 2 MiB under a Zipf 0.9 law, 90% hits at 5,000 requests per second.

Load: 5,000 req/s over 128 HTTP/1.1 connections, 120 s measured after 60 s of warm-up. Hit ratio target 90%, working set 783.0 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ats | 10.1.2 | 5,000 req/s | 80 µs | 2.63 ms | 18.61 ms | 78.3% | 0 | 1018.9 MiB | 2010.9 MiB | 35.5 s (0.30 cores) | 210.9 MiB |
| edgefront | 0.1.0 | 5,000 req/s | 72 µs | 3.15 ms | 18.07 ms | 89.4% | 0 | 2964.4 MiB | 2974.3 MiB | 38.9 s (0.32 cores) | 13.4 MiB |
| nginx | 1.30.7 | 5,000 req/s | 72 µs | 556 µs | 3.47 ms | 86.6% | 0 | 33.1 MiB | 1660.8 MiB | 36.5 s (0.30 cores) | 24.8 MiB |
| varnish | 9.0.4 | 5,000 req/s | 79 µs | 5.90 ms | 30.54 ms | 84.4% | 53 | 4360.1 MiB | 4382.9 MiB | 43.9 s (0.37 cores) | 119.8 MiB |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/cache-hit.toml --scenario bench/scenarios/miss-heavy.toml --scenario bench/scenarios/web-mix.toml --scenario bench/scenarios/large-objects.toml --targets edgefront,varnish,nginx,ats --out docs/benchmarks/data/2026-11/cache --run 2026-11-baseline --rebuild
```

Latency is response time from each request's scheduled send time, so queueing behind a stall counts (no coordinated omission). Hit ratio is the share of measured requests the origin never saw. Peak RSS is the highest resident set (anon plus mapped file pages) summed over the target's containers, sampled every 100 ms; peak memory is the cgroup's `memory.peak`, page cache and tmpfs included. CPU is the target's CPU time over the measured phase. Image is the uncompressed size of the target's images.
