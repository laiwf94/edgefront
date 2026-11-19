# Benchmark targets

Each directory is one target for `edgefront-bench run`: a `target.toml` naming a container image pinned by digest (or a stage of `bench/Dockerfile` for images built from this repository), the port it serves on, its command, and its config files. `crates/edgefront-bench/src/targets.rs` documents the format and the template values (`{{origin}}`, `{{cpus}}`, `{{cache_bytes}}`, `{{max_object_bytes}}` and the rest) that the runner fills in for each scenario.

Every target gets the same things, so that a difference in the results is a difference between the products:

- **The same origin.** `edgefront-bench origin` serves `GET /obj/<size>/<name>` with `Cache-Control: public, max-age=3600`, a strong `ETag`, a fixed `Last-Modified` and `Content-Length`. No target overrides the origin's TTL, so every hit ratio is the scenario's.
- **The same cache size.** The scenario's working set plus a quarter, at least 64 MiB (`{{cache_bytes}}`), and the scenario's largest object as the largest cacheable one (`{{max_object_bytes}}`).
- **The same CPUs.** The target and its sidecars are pinned to one CPU set (4 CPUs by default), and each product runs one worker per CPU (`{{cpus}}`). The origin and the load generator run on other CPUs.
- **No access logs** and no per-request debug output, and the rest of each product's settings at its documented default.

| Target | Product and image | Cache configuration | Departures from the defaults |
|---|---|---|---|
| `edgefront` | edgefront 0.1.0, `bench/Dockerfile` stage `edgefront` (distroless, the binary alone) | `[cache] memory_bytes` and `max_object_bytes` | Health checks use the origin's `/__origin/healthz`; the log is at `warn`. |
| `varnish` | Varnish Cache 9.0.4, `varnish:9.0.4` | `-s malloc,<cache>` with the built-in VCL, which caches what `Cache-Control` allows and collapses concurrent misses | `thread_pools` set to the CPU count (default 2). |
| `nginx` | NGINX 1.30.7, `nginx:1.30.7-alpine` | `proxy_cache` on a tmpfs, `max_size` the cache size, `proxy_cache_lock on` to collapse misses | `worker_processes` the CPU count, keep-alive to the origin, `access_log off`. |
| `haproxy` | HAProxy 3.2.26 (LTS), `haproxy:3.2.26` | `cache` section: `total-max-size` the cache size, `max-object-size` the largest object, `max-age 3600` | `nbthread` the CPU count, `http-reuse always`, `maxconn 30000` (within the runner's 65,536 descriptors). HAProxy caps its cache at 4,095 MB and an object at half the cache. |
| `envoy` | Envoy 1.39.6, `envoyproxy/envoy:v1.39.6` | HTTP cache filter with `simple_http_cache` | `--concurrency` the CPU count, circuit breakers raised past any scenario. `simple_http_cache` has no size limit, so Envoy is the one cache not held to the shared size. |
| `ats` | Apache Traffic Server 10.1.2, `trafficserver/trafficserver:10.1.2` from Docker Hub | RAM cache of the cache size holding objects up to the largest one, over a cache volume of the same size on a tmpfs | `exec_thread.limit` the CPU count, no Via headers, logging off, `map / http://origin:9000/`. |
| `steer`, `steer-tls` | steer 0.1.0, `bench/Dockerfile` stage `steer`: legacy/steer built as its own Dockerfile builds it (static, `-trimpath -ldflags="-s -w"`), on edgefront's distroless base | none: the proxy track's Go baseline, one route to the origin | steer's configuration with every setting at its default; the TLS one terminates TLS with a test certificate for `api.example.com`; `STEER_LOG_LEVEL=warn`. |
| `edgefront-proxy`, `edgefront-proxy-tls` | edgefront 0.1.0, `bench/Dockerfile` stage `edgefront`, run as `edgefront proxy` | none: the Rust port, on the same configuration as `steer` and `steer-tls` | `EDGEFRONT_LOG=warn`. |
| `envoy-ratelimit` | Envoy 1.39.6 with envoyproxy/ratelimit `9b8d6515` and Redis 8.8.4 | none: a proxy whose every request asks the rate limit service, with a limit no scenario reaches | `failure_mode_deny: true`, so a decision that fails shows as a 5xx. For the limiter track; it counts as one target of three containers. |

The images are pinned by digest of their multi-architecture index, so the same reference runs on arm64 and x86_64. To update one, change the tag and digest together (`docker buildx imagetools inspect <image>:<tag>` prints the digest) and the version in `target.toml`.

A new target is a new directory: the proxy and limiter tracks add theirs (steer, pacerd and the Rust ports) the same way. A target may name its binary's path in the image (`binary = "/steer"`), and the result then records its size as `extra.binary_bytes`, with `track = "proxy"` or `track = "limiter"` so the report puts them in their own section.
