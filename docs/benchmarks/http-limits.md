# HTTP limits on edgefront proxy

Limiter week 6 (#94): what HTTP limits cost per request and how exactly they enforce, measured on this Mac (Apple M4 Max, 16-core arm64; Docker Desktop 28, Linux VM with 16 CPUs and 7.6 GiB) on 20 November 2026, with the other tracks' work running beside it (load averages 13 to 27).

## Per request, in the proxy pipeline

`cargo bench -p edgefront-limiter --bench http_layer`: 1,000,000 requests (after 200,000 of warm-up) through `edgefront_core::filter::run`, once on a route with a limit keyed on path and `x-api-key` and once without, alternately, over 100,000 distinct keys in a 262,144-key table, each request timed on its own. Three runs:

| ns per request | run 1 | run 2 | run 3 |
|---|---:|---:|---:|
| without the limiter, p50 / p99 | 83 / 125 | 83 / 125 | 83 / 125 |
| with the limiter, p50 / p99 | 625 / 834 | 625 / 875 | 625 / 875 |
| paired difference, p50 / p99 / p99.9 | 542 / 791 / 1,208 | 542 / 792 / 1,626 | 583 / 792 / 1,666 |
| added at p99 (p99 with less p99 without) | 709 | 750 | 750 |

The limiter's work per request is the key (cleaned path and header on the stack), one table take, the boxed settlement and the two RateLimit header values: under 0.8 µs at p99, against the 2 µs target.

## End to end at 50,000 requests per second

`scripts/bench-http-limits.sh` (`OUT=docs/benchmarks/data/http-limits`): `crates/edgefront-bench/scenarios/http_limits.toml`, 100,000 paths under a Zipf 1.0 law at 50,000 requests per second over 256 HTTP/1.1 connections for 30 s after 5 s of warm-up, 104,973 distinct paths in all. `edgefront-proxy` has no limits; `edgefront-proxy-limits` limits every request to 20 a second with a burst of 20 per path and `Host` header (`bench/targets/edgefront-proxy-limits/limits.toml`). Each target on 4 CPUs, three repetitions with the order alternating; the raw results, in the harness's result schema, are under [data/http-limits](data/http-limits).

| run | edgefront-proxy p50 / p99 / p99.9 | edgefront-proxy-limits p50 / p99 / p99.9 | peak RSS, without / with |
|---|---|---|---|
| 1 | 0.135 / 2.679 / 17.842 ms | 0.295 / 4,181.721 / 4,229.956 ms | 22.4 / 165.2 MiB |
| 2 | 0.126 / 0.248 / 2.619 ms | 0.110 / 5.333 / 13.230 ms | 23.7 / 155.2 MiB |
| 3 | 0.127 / 0.256 / 0.984 ms | 0.098 / 0.266 / 5.022 ms | 17.4 / 154.8 MiB |

Run 3 is the quiet one: 0.266 ms p99 against 0.256. In run 1 the limited target fell about four seconds behind (p99 4.18 s, from the scheduled send times) while the Mac's load average stood at 24 to 27; run 2's 5.3 ms p99 is the same kind of stall, shorter. The memory is the table: 1,048,576 keys of 64-byte slots, sized for every path a run sends.

**Accuracy.** `edgefront-bench http-limits` replays the scenario's requests at their scheduled times through the same rules, on a manual clock, and says what the limit should do with the 1,500,000 measured requests: admit 900,472 and refuse 599,528. Against each run's counts:

| | admitted (2xx) | refused (429) | admitted off by | refused off by |
|---|---:|---:|---:|---:|
| expected | 900,472 | 599,528 | | |
| run 1 | 889,741 | 610,259 | 1.192% | 1.790% |
| run 2 | 900,475 | 599,525 | 0.000% | 0.001% |
| run 3 | 900,475 | 599,525 | 0.000% | 0.001% |

Runs 2 and 3 differ from the replay by three requests: paths near the limit whose requests arrived a little off schedule. Run 1's stall delayed requests by up to four seconds and then sent them in a rush, so some paths saw their requests bunched into fewer windows than scheduled, and 10,731 more were refused: the limit enforced what it was sent, which was no longer what the schedule said. The accuracy target (within 1% at 50,000 requests per second) holds in the two runs whose requests arrived on time.
