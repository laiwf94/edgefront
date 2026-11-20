# Shadow mode and policy replay

Limiter week 9 (#103), measured on this Mac (Apple M4 Max, 16-core arm64, macOS, release build) on 21 November 2026, at load averages of 4 to 6.

## Replaying an hour of traffic

`cargo bench -p edgefront-limiter --bench replay` builds the recording first, as the proxy writes one: 10,000,000 decisions spread over 3,600 s, from 100,000 keys with a skew, under a limit of 5 a second with a burst of 10. That is 305 MiB in two files. The timed part reads the files back and replays them against a limit of 2 a second with a burst of 5, as `edgefront ctl limits replay` does. Three runs:

| | run 1 | run 2 | run 3 |
|---|---:|---:|---:|
| read, s | 0.05 | 0.06 | 0.06 |
| read and replayed, s | 0.63 | 1.05 | 0.78 |

Each run gave the same answer: 13,681 refusals before and 37,615 after, with all 40 keys whose outcome changed listed. That is well under the 60 s target.

## What recording costs

`cargo bench -p edgefront-limiter --bench http_layer` has a third route with the same limit and every decision recorded, flushed to files every 65,536 requests inside the timed loop. Over 1,000,000 requests and 100,000 keys, three runs:

| ns per request, mean | run 1 | run 2 | run 3 |
|---|---:|---:|---:|
| the limiter | 612 | 590 | 620 |
| the limiter, recording | 650 | 641 | 689 |
| recording adds | 38 | 51 | 68 |

Nothing was dropped. A request through `edgefront proxy` with limits costs about 43 µs of CPU in the week 6 benchmark: 65.0 to 65.6 CPU seconds for 1,500,000 requests at 50,000 a second ([http-limits.md](http-limits.md)). Against that, recording adds at most 68 ns, or 0.16%, well under the 3% target.
