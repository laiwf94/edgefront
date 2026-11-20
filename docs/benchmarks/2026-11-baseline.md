# Baseline benchmark, November 2026

The first published comparison from the shared harness (#84), for the cache track's week 4 (#87): edgefront against Varnish, NGINX and Apache Traffic Server. It covers the cache-hit, cache-miss (`miss-heavy`, 50% hits), mixed 90% (`web-mix`) and large-object scenarios. Each run gives every target the same origin, cache size, CPUs and worker count; [bench/targets/README.md](../../bench/targets/README.md) lists every configuration difference from the products' defaults. The raw results are under [data/2026-11/](data/2026-11/), and `examples/bench-report.sh` regenerates this page from them.

Recorded on 20 Nov 2026 (07:04 to 07:34 UTC+8) on the benchmark machine, a 16-core arm64 Mac running Docker 28. The runs were sequential and the runtime lock held throughout, but other work on the Mac kept its one-minute load average between 4.4 and 37 (median 8.4). Read the sub-millisecond p99s and the p99.9s as upper bounds: the same `cache-hit` run on a quieter machine gave edgefront 124 µs and Varnish 180 µs at p99 ([the harness demo](2026-11-20-harness-demo.md)). The medians, hit ratios, memory and CPU numbers moved much less between runs.

What the numbers say:

- **Hits.** At 5,000 requests per second, every target serves the cache-hit scenario at a p50 of 49 to 73 µs. edgefront had the lowest p99 (151 µs) and p99.9 (727 µs) in this run.
- **Memory is edgefront's weak point.** With the same cache size as the others (the working set plus a quarter, 1.07 GiB for `miss-heavy`), edgefront's resident set peaked at 2.8 GiB in `miss-heavy` and 3.0 GiB in `web-mix`. NGINX keeps its cache in page cache and peaked at about 40 MiB of RSS. The memory store's byte budget does not bound its real footprint; that is the cache track's next memory item.
- **Hit ratios.** Every target is below the 50% target in `miss-heavy` (40.6 to 47.5%), because once-only objects compete for the same space. In `large-objects`, edgefront reached 86.7% against NGINX's 93.8%.
- **Varnish** failed 53 requests in `web-mix` (out of 600,000) and peaked at 4.4 GiB.

The proxy section is the proxy track's week 4 Go-against-Rust runs (#86): steer and `edgefront proxy`, over HTTP/1.1 and TLS, three repetitions each, read from [data/steer-go-vs-rust/](data/steer-go-vs-rust/). Each row is the median of the three (the `runs` column), errors summed; [steer-go-vs-rust.md](steer-go-vs-rust.md) has the full write-up. The limiter section is the limiter track's week 4 C-against-Rust runs (#88): libpacer and pacerd against edgefront-limiter and `edgefront limits`, read from `data/pacer-port-*.json`, the files `edgefront-bench limiter`, `l4` and `memory` wrote. They are decision rates, round trips and resident memory rather than load runs against a target, so they keep their own tables; [pacer-port.md](pacer-port.md) has the full write-up.

Machine: 16-core arm64, Docker 28, with 16 CPUs and 7.7 GiB for the container runtime (docker 28.0.4, kernel 6.10.14-linuxkit, aarch64). The target ran on CPUs 0-3, the origin on 4-9 and the load generator on 10-15.

Machine: Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB), with 16 CPUs and 7.7 GiB for the container runtime (docker 28.0.4, kernel 6.10.14-linuxkit, aarch64). The target ran on CPUs 0-3, the origin on 4-9 and the load generator on 10-15.

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

## Proxy

### steer-port-http1-1

cleartext HTTP/1.1 to the origin through the proxy over 1 connection, 500 requests per second of 1 KiB objects.

Load: 500 req/s over 1 HTTP/1.1 connections, 30 s measured after 5 s of warm-up. Hit ratio target 90%, working set 1.0 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes | runs |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy | 0.1.0 | 500 req/s | 143 µs | 2.98 ms | 9.99 ms | 0.0% | 0 | 0.8 MiB | 4.4 MiB | 1.8 s (0.06 cores) | 13.6 MiB | 8,198,480 | 3 |
| steer | 0.1.0 | 500 req/s | 145 µs | 352 µs | 3.50 ms | 0.0% | 0 | 11.1 MiB | 14.1 MiB | 2.3 s (0.08 cores) | 13.5 MiB | 7,602,336 | 3 |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/steer-port/http1-1.toml --scenario bench/scenarios/steer-port/http1-10000.toml --scenario bench/scenarios/steer-port/http1-64-ramp.toml --scenario bench/scenarios/steer-port/http1-64.toml --targets steer,edgefront-proxy --out target/bench/steer-port/rep1 --run steer-port-1 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB) --rebuild
```

### steer-port-http1-10000

cleartext HTTP/1.1 to the origin through the proxy over 10000 connections, 20,000 requests per second of 1 KiB objects.

Load: 20,000 req/s over 10000 HTTP/1.1 connections, 30 s measured after 5 s of warm-up. Hit ratio target 90%, working set 1.0 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes | runs |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy | 0.1.0 | 20,000 req/s | 131 µs | 1.21 ms | 4.24 ms | 0.0% | 0 | 220.9 MiB | 239.7 MiB | 46.4 s (1.54 cores) | 13.6 MiB | 8,198,480 | 3 |
| steer | 0.1.0 | 20,000 req/s | 128 µs | 1.52 ms | 5.17 ms | 0.0% | 0 | 337.5 MiB | 357.4 MiB | 39.1 s (1.30 cores) | 13.5 MiB | 7,602,336 | 3 |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/steer-port/http1-1.toml --scenario bench/scenarios/steer-port/http1-10000.toml --scenario bench/scenarios/steer-port/http1-64-ramp.toml --scenario bench/scenarios/steer-port/http1-64.toml --targets steer,edgefront-proxy --out target/bench/steer-port/rep1 --run steer-port-1 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB) --rebuild
```

### steer-port-http1-64

cleartext HTTP/1.1 to the origin through the proxy over 64 connections, 20,000 requests per second of 1 KiB objects.

Load: 20,000 req/s over 64 HTTP/1.1 connections, 30 s measured after 5 s of warm-up. Hit ratio target 90%, working set 1.0 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes | runs |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy | 0.1.0 | 20,000 req/s | 144 µs | 2.28 ms | 12.31 ms | 0.0% | 0 | 10.3 MiB | 13.3 MiB | 44.6 s (1.48 cores) | 13.6 MiB | 8,198,480 | 3 |
| steer | 0.1.0 | 20,000 req/s | 155 µs | 27.87 ms | 74.51 ms | 0.0% | 175,488 | 15.3 MiB | 22.4 MiB | 41.3 s (1.37 cores) | 13.5 MiB | 7,602,336 | 3 |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/steer-port/http1-1.toml --scenario bench/scenarios/steer-port/http1-10000.toml --scenario bench/scenarios/steer-port/http1-64-ramp.toml --scenario bench/scenarios/steer-port/http1-64.toml --targets steer,edgefront-proxy --out target/bench/steer-port/rep1 --run steer-port-1 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB) --rebuild
```

### steer-port-http1-64-ramp

cleartext HTTP/1.1 to the origin through the proxy over 64 connections, from 20,000 to 120,000 requests per second in six 10-second steps.

Load: 70,000 req/s over 64 HTTP/1.1 connections, 60 s measured after 5 s of warm-up. Hit ratio target 90%, working set 1.0 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes | runs |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy | 0.1.0 | 70,000 req/s | 379 µs | 1672 ms | 1684 ms | 0.0% | 3,534,080 | 11.5 MiB | 14.2 MiB | 165.3 s (2.68 cores) | 13.6 MiB | 8,198,480 | 3 |
| steer | 0.1.0 | 52,630 req/s | 93.78 ms | 4945 ms | 5004 ms | 0.0% | 2,518,626 | 284.8 MiB | 355.0 MiB | 172.3 s (2.65 cores) | 13.5 MiB | 7,602,336 | 3 |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/steer-port/http1-1.toml --scenario bench/scenarios/steer-port/http1-10000.toml --scenario bench/scenarios/steer-port/http1-64-ramp.toml --scenario bench/scenarios/steer-port/http1-64.toml --targets steer,edgefront-proxy --out target/bench/steer-port/rep1 --run steer-port-1 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB) --rebuild
```

### steer-port-tls-1

HTTP/1.1 over TLS 1.3 to the origin through the proxy over 1 connection, 500 requests per second of 1 KiB objects.

Load: 500 req/s over 1 HTTP/1.1 connections, 30 s measured after 5 s of warm-up. Hit ratio target 90%, working set 1.0 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes | runs |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy-tls | 0.1.0 | 500 req/s | 163 µs | 13.89 ms | 64.32 ms | 0.0% | 0 | 0.9 MiB | 4.9 MiB | 2.1 s (0.07 cores) | 13.6 MiB | 8,198,480 | 3 |
| steer-tls | 0.1.0 | 500 req/s | 190 µs | 97.78 ms | 177 ms | 0.0% | 0 | 9.7 MiB | 12.3 MiB | 2.9 s (0.10 cores) | 13.5 MiB | 7,602,336 | 3 |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/steer-port/tls-1.toml --scenario bench/scenarios/steer-port/tls-10000.toml --scenario bench/scenarios/steer-port/tls-64-ramp.toml --scenario bench/scenarios/steer-port/tls-64.toml --targets steer-tls,edgefront-proxy-tls --out target/bench/steer-port/rep1 --run steer-port-1 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB)
```

### steer-port-tls-10000

HTTP/1.1 over TLS 1.3 to the origin through the proxy over 10000 connections, 20,000 requests per second of 1 KiB objects.

Load: 20,000 req/s over 10000 HTTP/1.1 connections, 30 s measured after 5 s of warm-up. Hit ratio target 90%, working set 1.0 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes | runs |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy-tls | 0.1.0 | 20,000 req/s | 149 µs | 3.62 ms | 12.94 ms | 0.0% | 0 | 309.3 MiB | 329.6 MiB | 48.4 s (1.61 cores) | 13.6 MiB | 8,198,480 | 3 |
| steer-tls | 0.1.0 | 20,000 req/s | 153 µs | 6.49 ms | 72.88 ms | 0.0% | 0 | 495.3 MiB | 515.3 MiB | 43.4 s (1.44 cores) | 13.5 MiB | 7,602,336 | 3 |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/steer-port/tls-1.toml --scenario bench/scenarios/steer-port/tls-10000.toml --scenario bench/scenarios/steer-port/tls-64-ramp.toml --scenario bench/scenarios/steer-port/tls-64.toml --targets steer-tls,edgefront-proxy-tls --out target/bench/steer-port/rep1 --run steer-port-1 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB)
```

### steer-port-tls-64

HTTP/1.1 over TLS 1.3 to the origin through the proxy over 64 connections, 20,000 requests per second of 1 KiB objects.

Load: 20,000 req/s over 64 HTTP/1.1 connections, 30 s measured after 5 s of warm-up. Hit ratio target 90%, working set 1.0 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes | runs |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy-tls | 0.1.0 | 20,000 req/s | 126 µs | 438 µs | 9.34 ms | 0.0% | 0 | 9.4 MiB | 11.9 MiB | 43.4 s (1.45 cores) | 13.6 MiB | 8,198,480 | 3 |
| steer-tls | 0.1.0 | 20,000 req/s | 154 µs | 11.28 ms | 64.68 ms | 0.0% | 0 | 16.7 MiB | 24.6 MiB | 41.6 s (1.39 cores) | 13.5 MiB | 7,602,336 | 3 |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/steer-port/tls-1.toml --scenario bench/scenarios/steer-port/tls-10000.toml --scenario bench/scenarios/steer-port/tls-64-ramp.toml --scenario bench/scenarios/steer-port/tls-64.toml --targets steer-tls,edgefront-proxy-tls --out target/bench/steer-port/rep1 --run steer-port-1 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB)
```

### steer-port-tls-64-ramp

HTTP/1.1 over TLS 1.3 to the origin through the proxy over 64 connections, from 20,000 to 120,000 requests per second in six 10-second steps.

Load: 70,000 req/s over 64 HTTP/1.1 connections, 60 s measured after 5 s of warm-up. Hit ratio target 90%, working set 1.0 MiB.

| Target | Version | Throughput | p50 | p99 | p99.9 | Hit ratio | Errors | Peak RSS | Peak memory | CPU | Image | binary_bytes | runs |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| edgefront-proxy-tls | 0.1.0 | 70,000 req/s | 292 µs | 946 ms | 954 ms | 0.0% | 2,360,979 | 13.0 MiB | 16.7 MiB | 162.8 s (2.69 cores) | 13.6 MiB | 8,198,480 | 3 |
| steer-tls | 0.1.0 | 54,522 req/s | 322 ms | 4861 ms | 4987 ms | 0.0% | 2,905,683 | 31.1 MiB | 44.4 MiB | 163.6 s (2.52 cores) | 13.5 MiB | 7,602,336 | 3 |

Reproduce with:

```sh
edgefront-bench run --scenario bench/scenarios/steer-port/tls-1.toml --scenario bench/scenarios/steer-port/tls-10000.toml --scenario bench/scenarios/steer-port/tls-64-ramp.toml --scenario bench/scenarios/steer-port/tls-64.toml --targets steer-tls,edgefront-proxy-tls --out target/bench/steer-port/rep1 --run steer-port-1 --machine Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB)
```

## Limiter

libpacer and pacerd (C) against edgefront-limiter and `edgefront limits` (Rust), from the limiter track's week 4 runs (#88). They are not load runs against a target, so they have tables of their own rather than rows like the ones above; [pacer-port.md](pacer-port.md) gives the machine, the method and what each number means.

### limiter-decisions

Millions of decisions per second.

| workload | implementation | 1 thread | 2 threads | 4 threads | 8 threads | 16 threads |
|---|---|---:|---:|---:|---:|---:|
| shared-gcra | Rust | 439.8 | 238.9 | 135.1 | 108.4 | 187.6 |
| shared-gcra | C | 336.3 | 90.0 | 93.8 | 101.5 | 111.5 |
| shared-gcra | Rust / C | 1.31 | 2.65 | 1.44 | 1.07 | 1.68 |
| mutex-gcra | Rust | 557.7 | 128.4 | 93.0 | 72.1 | 84.2 |
| mutex-gcra | C | 192.4 | 58.1 | 49.5 | 34.1 | 36.5 |
| mutex-gcra | Rust / C | 2.90 | 2.21 | 1.88 | 2.11 | 2.31 |
| table-spread | Rust | 29.8 | 41.0 | 45.4 | 49.5 | 54.3 |
| table-spread | C | 21.3 | 22.7 | 26.3 | 34.4 | 41.7 |
| table-spread | Rust / C | 1.40 | 1.81 | 1.73 | 1.44 | 1.30 |
| table-hot | Rust | 102.4 | 33.8 | 30.3 | 20.0 | 18.0 |
| table-hot | C | 66.6 | 22.3 | 21.1 | 13.8 | 13.9 |
| table-hot | Rust / C | 1.54 | 1.51 | 1.43 | 1.45 | 1.29 |

### l4-proxy

The median of 3 sessions of 5 runs each.

| | direct | pacerd | edgefront |
|---|---:|---:|---:|
| bulk download, one connection (MiB/s) | 18345 | 5819 | 6026 |
| 64-byte round trip, p50 (µs) | 45.6 | 94.0 | 96.0 |
| 64-byte round trip, p99 (µs) | 108.5 | 194.5 | 173.1 |
| round trips/s, 32 connections | 293212 | 133032 | 126463 |
| proxy CPU per round trip (µs) | - | 21.8 | 22.2 |

### Memory per tracked client

| implementation | clients | RSS before (KiB) | table made (KiB) | filled (KiB) | bytes per client |
|---|---:|---:|---:|---:|---:|
| libpacer (C) | 1035213 | 896 | 3488 | 132512 | 130.2 |
| edgefront-limiter (Rust) | 1035213 | 2456 | 138044 | 138328 | 134.4 |

Reproduce with `scripts/bench-pacer-port.sh`, which runs all three. The C side needs legacy/pacer, so run it from 2965963a1d200e09c3dac5da49510087e660642e, the commit before legacy/pacer was deleted; the Rust side runs from any commit.

Latency is response time from each request's scheduled send time, so queueing behind a stall counts (no coordinated omission). Hit ratio is the share of measured requests the origin never saw. Peak RSS is the highest resident set (anon plus mapped file pages) summed over the target's containers, sampled every 100 ms; peak memory is the cgroup's `memory.peak`, page cache and tmpfs included. CPU is the target's CPU time over the measured phase. Image is the uncompressed size of the target's images.
