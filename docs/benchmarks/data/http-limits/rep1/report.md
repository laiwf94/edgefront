# Benchmark run http-limits-1

Machine: Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB), with 16 CPUs and 7.7 GiB for the container runtime (docker 28.0.4, kernel 6.10.14-linuxkit, aarch64). The target ran on CPUs 0-3, the origin on 4-9 and the load generator on 10-15.

## Proxy

### http-limits

100,000 paths under a Zipf 1.0 law at a constant 50,000 requests per second over HTTP/1.1, limited per path and Host header.

Load: 50,000 req/s over 256 HTTP/1.1 connections, 30 s measured after 5 s of warm-up. Hit ratio target 94%, working set 97.7 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy | 0.1.0 | 50,000 req/s | 135 µs | 2.68 ms | 17.84 ms | 0.0% | 0 | 22.4 MiB | 26.9 MiB | 75.4 s (2.51 cores) | 14.4 MiB | 8,198,480 |

Reproduce with:

```sh
edgefront-bench run --scenario crates/edgefront-bench/scenarios/http_limits.toml --targets edgefront-proxy,edgefront-proxy-limits --out docs/benchmarks/data/http-limits/rep1 --run http-limits-1 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB) --rebuild
```

## Limiter

### http-limits

100,000 paths under a Zipf 1.0 law at a constant 50,000 requests per second over HTTP/1.1, limited per path and Host header.

Load: 50,000 req/s over 256 HTTP/1.1 connections, 30 s measured after 5 s of warm-up. Hit ratio target 94%, working set 97.7 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy-limits | 0.1.0 | 50,000 req/s | 295 µs | 4182 ms | 4230 ms | 40.7% | 0 | 165.2 MiB | 175.5 MiB | 65.2 s (2.17 cores) | 14.4 MiB | 8,198,480 |

Reproduce with:

```sh
edgefront-bench run --scenario crates/edgefront-bench/scenarios/http_limits.toml --targets edgefront-proxy,edgefront-proxy-limits --out docs/benchmarks/data/http-limits/rep1 --run http-limits-1 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB) --rebuild
```

Latency is response time from each request's scheduled send time, so queueing behind a stall counts (no coordinated omission). Hit ratio is the share of measured requests the origin never saw. Peak RSS is the highest resident set (anon plus mapped file pages) summed over the target's containers, sampled every 100 ms; peak memory is the cgroup's `memory.peak`, page cache and tmpfs included. CPU is the target's CPU time over the measured phase. Image is the uncompressed size of the target's images.
