# Benchmark run h2-grpc

Machine: 4-core x86_64, Docker 29, with 4 CPUs and 15.6 GiB for the container runtime (docker 29.4.2, kernel 7.0.0-1019-azure, x86_64). The target ran on CPUs 0, the origin on 1 and the load generator on 2-3.

## Proxy

### h2-1000-streams

HTTP/2 through the proxy to the origin over 10 connections, 5,000 requests a second each held 200 ms by the origin: 1,000 concurrent streams.

Load: 5,000 req/s over 10 HTTP/2 connections, 30 s measured after 5 s of warm-up. Hit ratio target 90%, working set 3.9 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy-h2 | 0.1.0 | 5,000 req/s | 201 ms | 202 ms | 206 ms | 0.0% | 0 | 21.8 MiB | 23.3 MiB | 10.4 s (0.34 cores) | 16.4 MiB | 17,317,592 |
| envoy-h2 | 1.39.6 | 5,000 req/s | 201 ms | 202 ms | 204 ms | 0.0% | 0 | 33.2 MiB | 35.0 MiB | 13.7 s (0.45 cores) | 68.6 MiB | |
| haproxy-h2 | 3.2.26 | 5,000 req/s | 201 ms | 201 ms | 203 ms | 0.0% | 0 | 16.4 MiB | 18.4 MiB | 5.9 s (0.20 cores) | 42.6 MiB | |

Reproduce with:

```sh
edgefront-bench run --scenario crates/edgefront-bench/scenarios/h2_grpc.toml --targets edgefront-proxy-h2,envoy-h2,haproxy-h2 --target-cpus 1 --out target/bench/h2-grpc --run h2-grpc
```

### grpc-1000-streams

Unary gRPC through the proxy to the origin over 10 connections, 5,000 calls a second each held 200 ms by the origin: 1,000 concurrent streams.

Load: 5,000 req/s over 10 HTTP/2 connections, 30 s measured after 5 s of warm-up. Hit ratio target 90%, working set 1.0 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy-h2 | 0.1.0 | 5,000 req/s | 201 ms | 202 ms | 206 ms | 0.0% | 0 | 22.4 MiB | 23.9 MiB | 13.7 s (0.45 cores) | 16.4 MiB | 17,317,592 |
| envoy-h2 | 1.39.6 | 5,000 req/s | 201 ms | 202 ms | 204 ms | 0.0% | 0 | 35.7 MiB | 37.5 MiB | 15.8 s (0.52 cores) | 68.6 MiB | |
| haproxy-h2 | 3.2.26 | 5,000 req/s | 201 ms | 201 ms | 203 ms | 0.0% | 0 | 16.3 MiB | 18.3 MiB | 7.2 s (0.24 cores) | 42.6 MiB | |

Reproduce with:

```sh
edgefront-bench run --scenario crates/edgefront-bench/scenarios/h2_grpc.toml --targets edgefront-proxy-h2,envoy-h2,haproxy-h2 --target-cpus 1 --out target/bench/h2-grpc --run h2-grpc
```

Latency is response time from each request's scheduled send time, so queueing behind a stall counts (no coordinated omission). Hit ratio is the share of measured requests the origin never saw. Peak RSS is the highest resident set (anon plus mapped file pages) summed over the target's containers, sampled every 100 ms; peak memory is the cgroup's `memory.peak`, page cache and tmpfs included. CPU is the target's CPU time over the measured phase. Image is the uncompressed size of the target's images.
