# Benchmark scenarios

Each file is one scenario for `edgefront_bench::scenario::Scenario`. The module documentation in `crates/edgefront-bench/src/scenario.rs` gives the format. A scenario sets the arrival schedule (warm-up, then a constant or stepped rate), the protocol and connections, the popular objects (count and Zipf exponent), the object size mix and the hit ratio target.

Requests are `GET /obj/<size>/<name>`. `<size>` is the body length the origin serves. `<name>` is `k<rank>` for a popular object or `u<seq>` for an object asked for only once. Warm-up asks only for popular objects. After warm-up, a share of requests (the "once-only" column) goes to once-only objects, chosen so that the expected hit ratio meets the target against a cache that keeps everything it fetched. A target the popular objects cannot reach is refused when the file is read. Every request depends only on the scenario and its sequence number, so repeated runs send the same requests.

| Scenario | Objects | Sizes | Load | Hit target | Once-only | Working set | Length |
|---|---|---|---|---|---|---|---|
| `cache-hit` | 1,000, Zipf 1.0 | 1 to 16 KiB | 5,000/s, HTTP/1.1 | 98% | 2.0% | 3.6 MiB | 70 s |
| `web-mix` | 10,000, Zipf 0.9 | 1 KiB to 2 MiB | 5,000/s, HTTP/1.1 | 90% | 10.0% | 783 MiB | 180 s |
| `large-objects` | 200, Zipf 0.8 | 1 to 16 MiB | 200/s, HTTP/1.1 | 95% | 5.0% | 725 MiB | 90 s |
| `miss-heavy` | 50,000, Zipf 0.8 | 4 to 64 KiB | 3,000/s, HTTP/1.1 | 50% | 41.8% | 879 MiB | 80 s |
| `ramp` | 10,000, Zipf 1.0 | 2 to 8 KiB | 5,000 to 40,000/s in 8 steps, HTTP/2 | 95% | 5.0% | 34 MiB | 140 s |

`crates/edgefront-bench/tests/scenarios.rs` reads every file here. It checks that each scenario reaches its target, that each working set fits in 1 GiB, and that `cache-hit` can run against two targets in under four minutes.

`steer-port/` holds the proxy track's Go-vs-Rust scenarios, which `examples/bench-steer.sh` runs against steer and edgefront proxy (`docs/benchmarks/steer-go-vs-rust.md`). Each sends 1 KiB objects through the proxy to the origin: cleartext HTTP/1.1 (`http1-*`) and HTTP/1.1 over TLS (`tls-*`, with `[tls] server_name`), over 1 connection at 500 requests per second, and over 64 and 10,000 connections at 20,000, for 30 seconds after 5 of warm-up; and a ramp over 64 connections from 20,000 to 120,000 requests per second in six 10-second steps. The proxies cache nothing, so the hit ratio target only spreads requests over paths.
