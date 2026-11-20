# One hot key across 16 cores

Limiter week 7 (#97): decisions per second for one hot key among 10,000 cold ones, nine decisions in ten for the hot key, on this Mac (Apple M4 Max, 16-core arm64, macOS, release build) on 21 November 2026, with the other tracks' builds and benchmarks running beside it (load averages 24 to 109 during the runs).

`edgefront-bench limiter --scenario crates/edgefront-bench/scenarios/hot_key.toml --json FILE`, three times. Each run times 4,000,000 decisions per thread at each thread count and keeps the median of three; the rate is 10^7 a second with a burst of 1000 and each thread's clock moves 50 ns a decision, as in `limiter_decisions.toml`. `hot-key-table` puts every key in one `Table`, where all threads meet on the hot key's shard lock; `hot-key-split` puts the same traffic through a `HotTable`, which samples the keys, finds the hot one and gives it a sub-bucket per thread. Millions of decisions per second, raw results in [data/hot-key](data/hot-key):

| workload | run | 1 thread | 2 threads | 4 threads | 8 threads | 16 threads |
|---|---|---:|---:|---:|---:|---:|
| hot-key-table | 1 | 66.9 | 47.9 | 20.2 | 16.9 | 19.2 |
| hot-key-table | 2 | 45.3 | 26.9 | 18.1 | 18.0 | 22.3 |
| hot-key-table | 3 | 68.8 | 53.3 | 23.3 | 17.1 | 17.1 |
| hot-key-split | 1 | 87.1 | 70.5 | 137.8 | 190.1 | 263.3 |
| hot-key-split | 2 | 85.4 | 90.7 | 171.2 | 197.2 | 279.0 |
| hot-key-split | 3 | 84.8 | 38.2 | 59.0 | 78.7 | 115.0 |

With one bucket the hot key does not scale: 16 threads make fewer decisions than one. Split, 16 threads made 115 to 279 million decisions a second, against the target of 50 million; run 3 ran while the load average was near 30 on 16 cores, so its threads were sharing them. The split's accuracy is checked in `hotkey::tests`: over 10 s of simulated traffic offered at twice the rate from 16 threads with uneven loads, it admitted within 1% of an exact bucket.
