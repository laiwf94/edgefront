# steer against edgefront proxy: the Go-vs-Rust benchmark

steer, the Go proxy, against `edgefront proxy`, its Rust port, on steer's configuration. Both ran in pinned containers on the shared harness (`crates/edgefront-bench`), in three repetitions of eight scenarios. The run was on 20 Nov 2026 on this repository's benchmark machine: an Apple Mac with 16 arm64 cores, running Docker Desktop 28 (Linux VM with 16 CPUs and 7.6 GiB, kernel 6.10.14-linuxkit). Each target ran on CPUs 0-3, the origin on 4-9 and the load generator on 10-15. edgefront was built at 4d32823 and steer from `legacy/steer` at the same commit; its last change was 8f0c2a5. The raw result JSON for every scenario, target and repetition is in [data/steer-go-vs-rust](data/steer-go-vs-rust/), with the harness's own report beside each repetition.

**Other work shared the machine.** Builds, test suites and another track's benchmarks ran during the run, and the harness's runtime lock only serialises benchmarks. Medians and throughput are stable across repetitions, and so are the memory numbers. The p99 columns vary by an order of magnitude between repetitions on both sides, so the tables give their ranges. Read a p99 difference as real only where the ranges do not overlap.

## Results

Each scenario sends 1 KiB objects through the proxy to the harness origin, open loop at a constant rate: 30 s measured after 5 s of warm-up. The rates are 500/s over one keep-alive connection, and 20,000/s over 64 and over 10,000. The TLS scenarios use HTTP/1.1 over TLS 1.3, with SNI `api.example.com` and one full handshake per connection. The proxies cache nothing, so every request reaches the origin.

Medians of 3 repetitions; p99 ranges in brackets.

| Scenario | Conns | Offered | Throughput steer / edgefront | p50 steer / edgefront | p99 steer | p99 edgefront | Peak RSS steer / edgefront | CPU s steer / edgefront |
|---|---|---|---|---|---|---|---|---|
| steer-port-http1-1 | 1 | 500/s | 500 / 500 | 0.145 / 0.143 ms | 0.352 ms (0.322 to 2.550) | 2.984 ms (0.334 to 6.160) | 11.1 MiB / 0.8 MiB | 2.3 / 1.8 |
| steer-port-http1-64 | 64 | 20,000/s | 20,000 / 20,000 | 0.155 / 0.144 ms | 27.869 ms (3.326 to 2086.666) | 2.275 ms (0.486 to 5.636) | 15.3 MiB / 10.3 MiB | 41.3 / 44.6 |
| steer-port-http1-10000 | 10,000 | 20,000/s | 20,000 / 20,000 | 0.128 / 0.131 ms | 1.525 ms (0.318 to 7.975) | 1.211 ms (0.219 to 6.668) | 337.5 MiB / 220.9 MiB | 39.1 / 46.4 |
| steer-port-tls-1 | 1 | 500/s | 500 / 500 | 0.190 / 0.163 ms | 97.780 ms (1.609 to 296.485) | 13.885 ms (4.764 to 912.785) | 9.7 MiB / 0.9 MiB | 2.9 / 2.1 |
| steer-port-tls-64 | 64 | 20,000/s | 20,000 / 20,000 | 0.154 / 0.126 ms | 11.280 ms (7.786 to 476.840) | 0.438 ms (0.275 to 2.525) | 16.7 MiB / 9.4 MiB | 41.6 / 43.4 |
| steer-port-tls-10000 | 10,000 | 20,000/s | 20,000 / 20,000 | 0.153 / 0.149 ms | 6.488 ms (1.266 to 12.960) | 3.617 ms (0.401 to 17.859) | 495.3 MiB / 309.3 MiB | 43.4 / 48.4 |


**Memory.** At 10,000 connections, edgefront's peak RSS is 220.9 MiB against steer's 337.5 MiB in cleartext (35% lower), and 309.3 MiB against 495.3 MiB over TLS (38% lower). That is Rust at or below Go, as this week's Done-when asks. edgefront is also lower in every other scenario.

Peak RSS is the cgroup's `anon` plus `file_mapped` pages, sampled every 100 ms. For edgefront over a single connection, almost everything else is shared file pages, which is why it reads under 1 MiB there.

**Throughput.** Both proxies carried every scheduled request, at every constant rate, with no errors.

**CPU.** At 20,000 requests per second, edgefront spends 4 to 19% more CPU time than steer (44.6 against 41.3 CPU s over 64 cleartext connections, 46.4 against 39.1 over 10,000). At 500 per second it spends less (1.8 against 2.3).

**Ramp.** Over 64 connections, each proxy was stepped from 20,000 to 120,000 requests per second in 10-second steps. The table gives the highest step held with every request answered and p99 under 10 ms:

| Ramp | steer: highest step held | edgefront: highest step held |
|---|---|---|
| steer-port-http1-64-ramp | 20,000/s (runs: 20,000, 60,000, 0) | 20,000/s (runs: 20,000, 40,000, 20,000) |
| steer-port-tls-64-ramp | 60,000/s (runs: 60,000, 20,000, 60,000) | 60,000/s (runs: 60,000, 60,000, 60,000) |

The steps where either side fails differ from one repetition to the next by more than the steps themselves. On this shared machine the ramp shows that both proxies hold 20,000/s in cleartext and 60,000/s over TLS. It does not separate them.

**Size.**

| Target | Binary | Image |
|---|---|---|
| edgefront-proxy | 7.8 MiB | 13.6 MiB |
| edgefront-proxy-tls | 7.8 MiB | 13.6 MiB |
| steer | 7.3 MiB | 13.5 MiB |
| steer-tls | 7.3 MiB | 13.5 MiB |

The binary is the file in the image. steer's is built as its own Dockerfile built it (`CGO_ENABLED=0 go build -trimpath -ldflags="-s -w"`, stripped). edgefront's is the release profile (thin LTO, one codegen unit), which keeps its symbol table. Both images are the binary on `gcr.io/distroless/cc-debian13:nonroot`.

## Reproducing

```sh
BENCH_REBUILD=1 BENCH_MACHINE="Apple Mac, 16-core arm64, Docker Desktop 28 (Linux VM, 16 CPUs, 7.6 GiB)" examples/bench-steer.sh
```

`examples/bench-steer.sh` builds the images (`bench/Dockerfile` stages `edgefront` and `steer`; steer's source is fetched from the repository at its last commit). It runs the cleartext scenarios in `bench/scenarios/steer-port/` against `bench/targets/steer` and `edgefront-proxy`, and the TLS ones against `steer-tls` and `edgefront-proxy-tls`, `BENCH_REPEAT` times (default 3). It validates every result against `bench/schema/result.schema.json` and prints these tables (`scripts/steer-bench-summary.py`). Each repetition's command is in its results' `command` field.

## The regression gate

`.github/workflows/bench.yml`'s `proxy-p99-gate` job fails a change that makes edgefront proxy's p99 more than 5% worse.
- On every push and pull request that touches the crates, it builds `edgefront proxy` at the commit and at its parent.
- It runs `bench/ci/steer-port-gate.toml` (cleartext HTTP/1.1, 64 connections, 5,000/s for 15 s, one target CPU) five times per side on the same runner, alternating which side goes first.
- `scripts/check-p99-gate.py` fails the job when this commit's median p99 is more than 5% above the parent's, or when any request went unanswered.
- The verdict and each run's p99 go to the job summary.

It compares two builds on one machine instead of against the numbers above, because a p99 measured on a shared CI runner is not comparable with one from this machine.
