# Benchmark run http-limits-2

Machine: Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB), with 16 CPUs and 7.7 GiB for the container runtime (docker 28.0.4, kernel 6.10.14-linuxkit, aarch64). The target ran on CPUs 0-3, the origin on 4-9 and the load generator on 10-15.

## Proxy

### http-limits

100,000 paths under a Zipf 1.0 law at a constant 50,000 requests per second over HTTP/1.1, limited per path and Host header.

Load: 50,000 req/s over 256 HTTP/1.1 connections, 30 s measured after 5 s of warm-up. Hit ratio target 94%, working set 97.7 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy | 0.1.0 | 50,000 req/s | 126 µs | 248 µs | 2.62 ms | 0.0% | 0 | 23.7 MiB | 32.5 MiB | 74.1 s (2.47 cores) | 14.4 MiB | 8,198,480 |

Reproduce with:

```sh
edgefront-bench run --scenario crates/edgefront-bench/scenarios/http_limits.toml --targets edgefront-proxy-limits,edgefront-proxy --out docs/benchmarks/data/http-limits/rep2 --run http-limits-2 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB)
```

## Limiter

### http-limits

100,000 paths under a Zipf 1.0 law at a constant 50,000 requests per second over HTTP/1.1, limited per path and Host header.

Load: 50,000 req/s over 256 HTTP/1.1 connections, 30 s measured after 5 s of warm-up. Hit ratio target 94%, working set 97.7 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy-limits | 0.1.0 | 50,000 req/s | 110 µs | 5.33 ms | 13.23 ms | 40.0% | 0 | 155.2 MiB | 159.8 MiB | 65.6 s (2.19 cores) | 14.4 MiB | 8,198,480 |

Reproduce with:

```sh
edgefront-bench run --scenario crates/edgefront-bench/scenarios/http_limits.toml --targets edgefront-proxy-limits,edgefront-proxy --out docs/benchmarks/data/http-limits/rep2 --run http-limits-2 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB)
```

Latency is response time from each request's scheduled send time, so queueing behind a stall counts (no coordinated omission). Hit ratio is the share of measured requests the origin never saw. Peak RSS is the highest resident set (anon plus mapped file pages) summed over the target's containers, sampled every 100 ms; peak memory is the cgroup's `memory.peak`, page cache and tmpfs included. CPU is the target's CPU time over the measured phase. Image is the uncompressed size of the target's images.
