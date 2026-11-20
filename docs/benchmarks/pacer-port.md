# The pacer port against the C it replaced

Week 4 closes the port of pacer: libpacer's algorithms became `edgefront-limiter` (week 1), pacerd's config reader its `config` (week 2), and pacerd's TCP data path `edgefront limits` (week 3). Before legacy/pacer was deleted, both were measured side by side with pacer's own two benchmarks, `bench/bench_limiter.c` and `bench/bench_proxy.c`, run as edgefront-bench scenarios, plus the memory a tracked client costs.

**Machine.** pacer's reference machine: the Docker Desktop Linux VM (aarch64, 16 vCPUs, 8 GiB) on an Apple M4 Max, Docker 28.0.4. Every number below was measured inside that VM in one Ubuntu 24.04 container, Rust 1.99.1 with `lto = "thin"` and `codegen-units = 1`, clang 18 at pacer's release flags (`-O2`), on 20 Nov 2026. The Mac was shared with other jobs (load averages are given with each run), so single runs move; every point is a median of interleaved runs.

**Reproduce.** `scripts/bench-pacer-port.sh` ran all three and wrote `docs/benchmarks/data/pacer-port-*.json`. The C side needs legacy/pacer, so run it from the commit before the deletion; the Rust side runs from any commit:

```sh
cargo run --release -p edgefront-bench -- limiter --scenario crates/edgefront-bench/scenarios/limiter_decisions.toml
cargo run --release -p edgefront-bench -- l4 --scenario crates/edgefront-bench/scenarios/l4_proxy.toml --edgefront target/release/edgefront
cargo run --release -p edgefront-bench -- memory --keys 1048576 --difftest target/release/edgefront-difftest
```

## Limiter decisions per second

`crates/edgefront-bench/scenarios/limiter_decisions.toml`, the workloads of `bench_limiter.c`: a rate of 10^7 per second with a burst of 1000, each thread's clock moving 50 ns per decision, 4,000,000 decisions per thread per run, median of three runs per point (load average 7 to 12). `shared-gcra` is one lock-free GCRA every thread takes from; `mutex-gcra` is the same algorithm under a mutex, the baseline pacer kept; `table-spread` is a 200,000-key table under the evict policy with 10,000 keys per thread; `table-hot` is the same table with one key. Millions of decisions per second, all threads together:

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

Rust is ahead at every point, by 7% to 190%. The single-thread reasons are in docs/DESIGN.md ("Speed against libpacer"): the SipHash tail built in registers and cache-line-aligned table entries. One shared limiter is the worst case for any design, since every decision writes the same cache line; both sides fall from one thread to two, and the lock-free version stays ahead of the mutex at every count. `mutex-gcra`'s large lead at one thread is std's uncontended `Mutex`, a single compare-and-swap, against glibc's `pthread_mutex_lock`.

## The L4 proxy

`crates/edgefront-bench/scenarios/l4_proxy.toml`, the workloads of `bench_proxy.c`, against the upstream directly, through pacerd and through `edgefront limits`, both proxies on pacer's benchmark config (64 KiB buffers, no limits; one worker for the bulk download, four for the rest). The three targets take turns run by run. Three sessions of five runs, each session's medians, then the median of the three:

| | direct | pacerd | edgefront | edgefront / pacerd |
|---|---:|---:|---:|---:|
| bulk download, one connection (MiB/s) | 18,345 | 5,819 | 6,026 | 1.04 |
| 64-byte round trip, p50 (µs) | 45.6 | 94.0 | 96.0 | 1.02 |
| 64-byte round trip, p99 (µs) | 108.5 | 194.5 | 173.1 | 0.89 |
| round trips/s, 32 connections | 293,212 | 133,032 | 126,463 | 0.95 |
| proxy CPU per round trip (µs) | - | 21.8 | 22.2 | 1.02 |

Session by session (pacerd against edgefront; load average 11 to 13):

| session | p50 (µs) | p99 (µs) | round trips/s, 32 connections | bulk (MiB/s) |
|---|---|---|---|---|
| 1 | 93.5 / 96.0 | 143.4 / 143.6 | 117,552 / 137,735 | 6,412 / 6,245 |
| 2 | 95.2 / 100.3 | 221.4 / 207.5 | 133,749 / 119,611 | 5,819 / 6,026 |
| 3 | 94.0 / 95.4 | 194.5 / 173.1 | 133,032 / 126,463 | 5,716 / 6,023 |

On p99 edgefront is level with pacerd in one session and lower in two; its p50 is 1.5 to 5% higher in each; round trips per second over 32 connections and bulk throughput go either way from session to session, by more than the difference between the medians. The machine was shared, which moves these numbers far more than it moves the decisions above: the direct path's p99 alone ranged from 83 to 111 µs.

Three changes made in this week brought `edgefront limits` to this point; before them its p99 was 2 to 7% above pacerd's and its 32-connection rate up to 10% below. Each is what pacerd already did:

- **One runtime per worker.** Connections ran on tokio's multi-threaded runtime, where a connection's task can move between threads and a wake can cross them. Now each of `workers` threads has a single-threaded runtime and its own `SO_REUSEPORT` socket per listener, so the kernel spreads connections over the workers and a connection stays on one thread, like pacerd's worker loops.
- **No shared lock on the data path.** Every relay polled one `watch` channel for the drain timeout on every wake, which takes the channel's lock, shared by every connection on every thread. Each connection now polls a flag of its own (one atomic load), and the drain raises all of them.
- **Striped counters.** Every read added its bytes to the listener's one set of counters, a cache line all workers wrote. The counters are now 16 cache-line-aligned stripes; a thread adds to its own and a read sums them, as pacerd summed its per-worker counters.

## Memory per tracked client

`edgefront-bench memory --keys 1048576`: each table, libpacer's through its diffdrive and edgefront-limiter's through `edgefront-difftest pacer-rust-driver`, is made for 1,048,576 clients under the evict policy in a process of its own and offered that many clients, each taking a token of a rate of one per second so none is idle; both hold 1,035,213 (a shard that fills first evicts). Resident memory in KiB:

| implementation | clients held | before | table made | filled | bytes per client |
|---|---:|---:|---:|---:|---:|
| libpacer (C) | 1,035,213 | 896 | 3,488 | 132,512 | 130.2 |
| edgefront-limiter (Rust) | 1,035,213 | 2,456 | 138,044 | 138,328 | 134.4 |

Both tables take 64 bytes a slot, keep their slots at most three-quarters full and round each shard's slots up to a power of two, which for a million clients comes to about two slots a client: 128 bytes. edgefront-limiter writes every slot when it makes the table, so its memory is resident from the start; libpacer's `calloc` gets zeroed pages from the kernel that become resident only as clients land in them. A connection costs its two buffers (`buffer`, 16 KiB each by default) beside that.

## Against the targets

- **Within 10% of C on every decision row:** every Rust point is ahead of C, by 7% or more.
- **No worse than C on proxy p99:** the median of three sessions is 173 µs against 195, and no session has edgefront above pacerd by more than 0.2 µs.
- The raw results are in `docs/benchmarks/data/pacer-port-decisions.json`, `pacer-port-l4.json` (all three sessions) and `pacer-port-memory.json`.
