# Penalty box and overload mode

Limiter week 8 (#100), measured on this Mac (Apple M4 Max, 16-core arm64, macOS, release build) on 21 November 2026, at load averages of 3 to 6.

## A boxed client's refusal

`cargo bench -p edgefront-limiter --bench penalty`: 100,000 keys boxed in a `PenaltyBox` of 262,144 slots, then 2,000,000 checks of random boxed keys, timed in batches of 32. About 99.9% of the keys stayed boxed: a key whose slots are full replaces the one released first. Three runs:

| ns per check | p50 | p99 | p99.9 |
|---|---:|---:|---:|
| run 1 | 23.4 | 56.0 | 268.2 |
| run 2 | 15.6 | 23.4 | 207.0 |
| run 3 | 16.9 | 27.3 | 49.5 |

The check comes before the limit's table, so a boxed client costs one hash, one shard lock and a probe: under the 200 ns target at p99 in every run.

## Overload mode under CPU saturation

`edgefront-bench overload --scenario crates/edgefront-bench/scenarios/overload.toml`, in process. Four workers each burn 200 µs of CPU per request, a capacity of 20,000 a second. 100 good clients send 20 a second each and 200 abusive ones 90 a second each, 20,000 a second together, all within the per-client limit of 100 a second. Response times run from each request's scheduled send time, after 5 s of warm-up, over 15 s. Raw results in [data/overload](data/overload):

| run | overload mode | good p50 | good p99 | abusive offered / served / shed (req/s) | final level |
|---|---|---:|---:|---:|---|
| 1 | off | 13.50 ms | 20.57 ms | 18,000 / 18,000 / 0 | normal |
| 1 | on | 0.22 ms | 0.23 ms | 18,000 / 10,000 / 8,000 | elevated |
| 2 | off | 56.31 ms | 116.50 ms | 18,000 / 18,000 / 0 | normal |
| 2 | on | 0.22 ms | 0.24 ms | 18,000 / 10,000 / 8,000 | elevated |
| 3 | off | 217.98 ms | 225.24 ms | 18,000 / 18,000 / 0 | normal |
| 3 | on | 0.22 ms | 0.25 ms | 18,000 / 10,000 / 8,000 | elevated |

Without overload mode the pool runs at its whole capacity, so the queue only grows, and how far it gets depends on the run. With it, the workers' busy time read every 100 ms passes 85% within the first moments, the limits drop to a quarter, and the pool's load falls. After the 2 s dwell they step back up to a half, where the load holds at about 60%, under the 75% that would allow another step. The good clients, at a fifth of their limit, lose nothing; the abusive ones are held to 50 a second each. The good clients' p99 stays under 5 ms in every run, against 20 to 225 ms without overload mode.

The sampler here is the in-process `BusySampler`. On Linux, `edgefront proxy --limits` reads pressure stall information instead (`LinuxSampler`); macOS has none, so this Mac cannot run that path.
