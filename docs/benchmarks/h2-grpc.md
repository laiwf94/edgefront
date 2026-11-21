# HTTP/2 and gRPC at 1,000 streams

Proxy week 6 (#92): edgefront proxy, Envoy 1.39.6 and HAProxy 3.2.26 with HTTP/2 on both sides, at 1,000 concurrent streams. Measured by the bench workflow's `h2-grpc` job on a GitHub-hosted runner (4-core x86_64, Docker 29, kernel 7.0) for commit d1f100b on 21 November 2026, each target on one CPU, the origin on one and the load generator on two. The raw results, in the harness's result schema, are under [data/h2-grpc](data/h2-grpc), with the report the run wrote.

## What runs

`crates/edgefront-bench/scenarios/h2_grpc.toml` holds two scenarios over the same load: 5,000 requests a second over 10 prior-knowledge HTTP/2 connections, 30 s measured after 5 s at 1,000 a second, and an origin that holds every request 200 ms. By Little's law that keeps about 1,000 streams open through the proxy, 100 on each client connection, and the same number upstream, where each proxy speaks HTTP/2 to the origin. The load generator counts the most requests on the wire at once (`measured.peak_in_flight`).

- `h2-1000-streams`: GETs of 4 KiB objects.
- `grpc-1000-streams`: unary gRPC calls, each asking for a 1 KiB object by its path and answered with it as one message and `grpc-status: 0` in the trailers. A call that ends any other way counts as an error.

The targets are `bench/targets/edgefront-proxy-h2`, `envoy-h2` and `haproxy-h2`. Each allows 1,000 streams a connection on both sides and sets no request limit below the load; `bench/targets/README.md` lists the settings. Reproduce with:

```sh
edgefront-bench run --scenario crates/edgefront-bench/scenarios/h2_grpc.toml \
  --targets edgefront-proxy-h2,envoy-h2,haproxy-h2 --target-cpus 1
```

## Results

All six runs answered all 150,000 measured requests with no error of any kind, gRPC included. Latency is the response time from each request's scheduled send, so it includes the origin's 200 ms.

| Scenario | Target | Peak streams | p50 | p99 | p99.9 | Peak RSS | CPU over 30 s | Image |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| h2-1000-streams | edgefront proxy | 1,039 | 201 ms | 202 ms | 206 ms | 21.8 MiB | 10.4 s (0.34 cores) | 16.4 MiB |
| h2-1000-streams | Envoy | 1,028 | 201 ms | 202 ms | 204 ms | 33.2 MiB | 13.7 s (0.45 cores) | 68.6 MiB |
| h2-1000-streams | HAProxy | 1,020 | 201 ms | 201 ms | 203 ms | 16.4 MiB | 5.9 s (0.20 cores) | 42.6 MiB |
| grpc-1000-streams | edgefront proxy | 1,039 | 201 ms | 202 ms | 206 ms | 22.4 MiB | 13.7 s (0.45 cores) | 16.4 MiB |
| grpc-1000-streams | Envoy | 1,031 | 201 ms | 202 ms | 204 ms | 35.7 MiB | 15.8 s (0.52 cores) | 68.6 MiB |
| grpc-1000-streams | HAProxy | 1,023 | 201 ms | 201 ms | 203 ms | 16.3 MiB | 7.2 s (0.24 cores) | 42.6 MiB |

## Reading it

- **Streams.** Every proxy carried about 1,000 concurrent streams on 10 client connections, so none of them serialised or refused streams at this depth; at the origin's 200 ms, any queueing would have shown in p99.
- **Memory.** edgefront held 1,000 open streams in 21.8 to 22.4 MiB of RSS, two thirds of Envoy's 33.2 to 35.7 MiB and a third more than HAProxy's 16.3 to 16.4 MiB.
- **CPU.** edgefront used 0.34 cores for GETs and 0.45 for gRPC, below Envoy's 0.45 and 0.52 but 1.7 to 1.9 times HAProxy's 0.20 and 0.24. gRPC costs edgefront 32% more CPU than GETs at the same rate, Envoy 15% and HAProxy 22%. HAProxy's CPU per stream is the gap to close.
- **Tail.** p99.9 is 206 ms for edgefront against 204 for Envoy and 203 for HAProxy: 3 ms more queueing at the far tail on one CPU.
- **Image.** edgefront's image is 16.4 MiB against 42.6 (HAProxy) and 68.6 MiB (Envoy).

edgefront's target raises the cluster's `max_requests`, a circuit breaker of 1,024 concurrent requests by default, as Envoy's breakers are raised: with the peak a little over 1,000 the default refused 7 and 29 of 25,000 gRPC calls as `overflow` in two short local runs.
