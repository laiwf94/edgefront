# edgefront design notes

The first two sections cover the workspace and the steer port. The rest are cachet's design notes, which still describe the cache.

## The workspace

edgefront is one Cargo workspace. Every crate lives under `crates/` and takes its version, edition, lints and dependency pins from the root `Cargo.toml`, and the whole workspace shares one `Cargo.lock`. `crates/edgefront` is the binary. `edgefront-core` holds what every product needs: the HTTP/1.1 listener with steer's connection limits and graceful drain, the pooled client, steer's config schema, metrics and admin plumbing. `edgefront-proxy` is steer's routing and request path, `edgefront-cache` is cachet's engine, `edgefront-limiter` is pacer's rate limiting algorithms, and `edgefront-difftest` diffs the ports against the legacy programs. The root package is cachet as it was before the split, kept until its sources have all moved into the member crates. `legacy/steer` (Go) and `legacy/pacer` (C) keep building and testing in CI, with their full history, until their ports pass their differential suites. Each is then deleted, and its history stays reachable through the merge commit.

### Crate map

Each crate and the workspace crates it depends on. There are no cycles: `edgefront-cache` reaches its node through the `cluster::PurgeHost` trait rather than depending on cachet, and `edgefront-limiter` depends on no workspace crate and on serde alone, for its config.

| Crate | Holds | Workspace dependencies |
|---|---|---|
| `crates/edgefront` | The binary: cachet's command line (`--config`, `--check-config`, `--version`, the `CACHET_*` variables, exit codes and JSON log) and `edgefront proxy` | cachet, edgefront-core, edgefront-proxy |
| `.` (`cachet`) | The node runtime still to move: the cache's request path (`proxy`), origins and health checks (`origin`), node start-up (`node`), metric families and the admin routes; and the reference `cachet` binary | edgefront-cache, edgefront-core |
| `crates/edgefront-cache` | Caching policy, Cache-Control and range parsing, objects, request collapsing, purge, cluster gossip, the memory and disk stores, and the cache's config sections | edgefront-core |
| `crates/edgefront-core` | The HTTP/1.1 listener the proxy and the cache both serve on, the pooled client, the clock, request ids, config loading and steer's schema, metrics writers and the admin router | none |
| `crates/edgefront-proxy` | Routing, path cleaning, forwarding headers, request ids, the balancers and endpoint state, and the request path ported from steer | edgefront-core |
| `crates/edgefront-limiter` | pacer's GCRA, keyed table and sliding window, and pacerd's config | none |
| `crates/edgefront-difftest` | Differential drivers against `legacy/steer`, `legacy/pacer` and cachet 0.1 | edgefront-core, edgefront-proxy, edgefront-limiter |

Two tests hold the edgefront binary to cachet's behaviour while both exist. `crates/edgefront/tests/golden.rs` compares the command line's transcripts (usage, config checks, a served request, a purge, shutdown and start failure, log lines included) with `tests/golden/`. `crates/edgefront/tests/e2e_workspace.rs` runs `examples/demo.sh`'s five steps against two edgefront nodes and an origin served by the test, and compares every response's status, `Cache-Status` and body with `tests/fixtures/e2e_workspace.txt` byte for byte. The origin sends no `Date` header and makes objects stale on arrival with `Age`, so no value in the transcript depends on timing and nothing is blanked. The root package's `tests/golden_cli.rs` and `tests/e2e_workspace.rs` run the reference cachet binary against the same files, and CI runs the demo script against both binaries.

## Porting steer: the request path and the differential tests

The port keeps steer's behaviour exactly, byte for byte where bytes are visible, so every decision can be diffed against the Go code before the Go code goes. That means porting the parts of Go's standard library steer leaned on, not only steer's own code. The config reader follows `encoding/json`'s rules (case-insensitive keys, duplicate keys merging, invalid UTF-8 becoming U+FFFD). Durations, addresses, `strconv.Quote` and `path.Clean` follow Go. Request targets are decoded and re-encoded with `net/url`'s rules, and Go's server checks (`Host` validity, `OPTIONS *`, `CONNECT`) come first.

The HTTP/1.1 request path is `edgefront_proxy::server::Proxy`, a handler for the core listener:

1. Go's server checks.
2. Route on the host and the cleaned path.
3. Take a place under the cluster's `max_requests`.
4. Remove hop-by-hop headers and the headers `Connection` names, rewrite the forwarding headers, and set the request ID.
5. Pick the endpoint with the cluster's balancer (below). No pick is `503 no_healthy_upstream`.
6. Send the upstream target: the client's own encoding when cleaning changed nothing, Go's encoding of the cleaned path otherwise.

The body keeps the client's framing, because Go's transport copied `TransferEncoding`: a chunked body stays chunked even after steer buffered it for replay. The cluster's `timeout` covers the whole exchange, so it answers `504` before the response head and cuts the body after it. The place under `max_requests` is held until the response body is dropped, which is when steer's handler returned.

Three layers check the port:

- **Unit and property tests** use Go's own test vectors and, for the trickier functions, fixtures recorded by running steer's Go code.
- **`edgefront-difftest`** (`make difftest`, in CI on every push) feeds 10,000 generated configs and 100,000 generated requests to `legacy/steer/cmd/diffdrive` and to the Rust crates. It fails on any difference in acceptance, error text, route choice or cleaned path.
- **`edgefront-difftest balancer`** (`make difftest-balancer`, in CI on every push) sends 1,000 seeded scenarios to diffdrive's `picks` query and to `edgefront_proxy::balancer::Balancer`. Each scenario has a policy, a panic threshold, endpoints over up to eight priority levels and 10,000 requests, mixed with health flips, ejections, clock moves, in-flight changes and latency samples. Both sides use SplitMix64 from the scenario's seed and a manual clock, and answer one line per operation: the picks of each request, retries included, and the priority state. The first differing line is reported with the scenario's seed, the step, the eight operations before it and a replay command.
- **`crates/edgefront/tests/e2e_port_balancer.rs`** sends 20,000 HTTP requests per policy through a proxy built with a manual clock and a seeded generator, to nine origins over three priority levels, while a seeded script walks the cluster through ten phases: spill from level 0 to 1 to 2, panic, ejections that run out, and recovery, with random health flips, latency samples and requests held in flight on top. Each event and request becomes a line of a diffdrive `picks` script. The origins' counts must equal what the responses name, and the per-endpoint counts and a digest of all 60,000 picks must equal a fixture recorded from steer. `make e2e-steer` also runs the script through steer live and reports the first request where the picks differ.
- **`crates/edgefront/tests/e2e_port_http1.rs`** starts the binary with `legacy/steer/examples/steer.json` and three recording fake upstreams. It sends 30 raw requests: the README's traffic, path cleaning, spoofed forwarding headers, request IDs, hop-by-hop headers both ways, body framing, upstream errors and the requests Go refuses. It then compares the response and the upstream's view of each request with a transcript recorded from steer. `make e2e-steer`, also in CI, runs steer live on the same cases and requires it to still produce that transcript.

The HTTP/1.1 transcript only records which priority level answered, so it does not depend on which of the two primaries `least_loaded` picks; `e2e_port_balancer.rs` checks the picks themselves.

### Balancing

Each cluster has one `edgefront_proxy::balancer::Balancer` (steer's `balancer.LB`) over its endpoints, and a pick takes two steps, as in Envoy:

1. **A priority level.** The balancer counts the available endpoints (healthy and not ejected, at the clock's time) per level, up to eight levels. `priority::priority_load` turns those counts into a load per level: each level's health percentage times the overprovisioning factor of 1.4, capped at 100, with what is left spilling to the next level, in integers exactly as steer computes them. If the normalised total health is below the cluster's `panic_threshold_percent`, the pick is in panic and every endpoint, available or not, is a candidate. A roll in `0..100` chooses the level by its load: drawn from the generator, or for Maglev with a key, taken from the key's high half so a key keeps its level. When every endpoint of the chosen level has been tried, the other levels are tried in priority order.
2. **An endpoint in the level**, by the cluster's policy:
   - `round_robin` (`rr.rs`): nginx's smooth weighted round robin. While first attempts keep the same candidate set, one full cycle is recorded and replayed in O(1); any other pick scans. About 27 ns over 100 endpoints.
   - `least_loaded` (`ewma.rs`): power of two choices on `max(latency, 1 µs) × (in flight + 1)`, with steer's peak-EWMA latency estimate. It walks to its two candidates instead of copying the untried ones, so it never allocates. About 115 ns over 100 endpoints, a third of it the one clock read.
   - `maglev` (`maglev.rs`): Maglev consistent hashing on the first value of `hash_header` (steer's FNV-1a and SplitMix64 finaliser of the value), with weighted turns and a cache of the last 8 lookup tables, one per candidate set. A retry walks forward to the next untried owner; a request without the header starts from a random slot. About 40 ns per pick, and a 65,537-slot table over 100 endpoints builds in 2.1 ms.

Endpoint state (`endpoint.rs`, steer's `internal/endpoint`) is what the pick reads, so everything it reads is lock-free: the in-flight count, the health verdict, the end of the current ejection and the latency estimate. The probe and failure runs, interval counters and ejection multiplier change together under one mutex that only health checks, outlier detection and snapshots take. In the request path the endpoint counts the request in flight from the pick until the response body is dropped, and observes the time to the response head on the balancer's clock, at least one second when the attempt failed or answered 5xx so that a failing endpoint never looks fastest. Active health checks (`health.rs`, steer's `internal/health`) drive the verdict: each endpoint is probed at once and then every interval plus a random fraction of the jitter on the injected clock, over a new connection each time, and passes on a 2xx within the timeout; the verdict flips after `unhealthy_threshold` failures or `healthy_threshold` passes in a row, and a probe cut short by shutdown is not recorded. Outlier detection (`outlier.rs`, steer's `internal/outlier`) drives the ejections. Each attempt's outcome is reported to the cluster's detector: `consecutive_gateway_failures` connect errors, resets, timeouts or 502/503/504 in a row eject the endpoint at once. Every `interval` the detector first decays multipliers, then judges the interval that just ended: if at least `success_rate_minimum_hosts` un-ejected endpoints served `success_rate_request_volume` attempts, any whose success rate is below `mean - success_rate_stdev_factor * stdev` is ejected, worst first, unless every rate is equal. An ejection lasts the base time times the endpoint's ejection count, capped at the maximum, and the count drops by one for each full base period the endpoint then stays un-ejected. At most `max_ejection_percent` of the cluster is ejected at once, but always at least one when the percent is above zero, and one mutex serialises the decisions so the cap holds under concurrent reports. An endpoint with no attempts in an interval never counts towards the success rate; steer divides by zero there, but config validation keeps the volume at 1 or more, so the two never differ. `Proxy::endpoints` exposes the same state to tests. Retries (`retry.rs`, steer's `internal/retry` and the retry loop in its `transport.go`) are decided in a fixed order. An attempt may be retried only below `max_retries` and with a replayable body (no body, or one buffered whole because its length is unknown or at most `max_body_bytes`). A connect error is always retriable, since the request never reached the upstream; a timeout, a reset or a `5xx` in `retry_on` only when the request is idempotent (GET, HEAD, OPTIONS, TRACE, PUT, DELETE, or any request with an `Idempotency-Key`). The balancer is then asked for an endpoint not yet tried, and only if it has one is the cluster's budget withdrawn from, so a retry with nowhere to go costs nothing. The budget counts requests and retries in one-second buckets over a window of whole seconds and allows a retry while `retries < min_per_second × window + ratio × requests`, compared in `f64` as steer does; one mutex covers the ring and the clock is read under it, so concurrent withdrawals never overdraw and a stale reading never recycles a newer bucket. The wait before retry `n` is a uniform draw below `min(backoff_max, backoff_base × 2^n)`, with steer's handling of a doubling that overflows, and each attempt is cut off by `per_try_timeout` on the injected clock, the attempt winning a tie.

Every random draw (level rolls, the two choices, keyless Maglev starts) comes from one `rng::Rng`, SplitMix64 in production seeded from entropy, and time comes from `edgefront_core::clock::Clock`. `Proxy::with_sources` takes both, so a test or the difftest can build a proxy whose picks are repeatable and draw in exactly steer's order, which is what lets them be diffed pick for pick.

## Porting pacer: the limiter algorithms

`edgefront-limiter` is libpacer's `gcra.c`, `gcra_math.h`, `siphash.c`, `table.c` and `window.c` in safe Rust, with no dependencies. (The crate's one dependency, serde, serves the config described below.) The port keeps the C's arithmetic and memory layout, not only its behaviour, so that a table holding the same keys holds them in the same slots and evicts the same ones.

- **GCRA.** A limiter's changing state is its theoretical arrival time (TAT), one `AtomicU64` in ticks of 1/64 ns, so high byte rates keep their fractional intervals; the interval and tolerance sit in two more words that only `reconfigure` writes. A decision is a pure function from the TAT, the interval, the tolerance (burst times interval) and the time to a new TAT and a `Decision` (allowed, granted, remaining, retry-after in nanoseconds, rounded up). `Gcra` applies it with a relaxed compare-and-swap loop, and `Table` applies the same function under its shard lock. libpacer clamped times past 2^57 ns, about four and a half years of uptime; `Gcra` and `Table` store the TAT from a moving epoch instead (below). `reconfigure` changes the rate while takes race with it and keeps debt as time: a client that had to wait 2 s still waits 2 s. A take at the same moment may mix the old interval with the new tolerance, and is then bounded by the larger of the two rates.
- **Keyed table.** All memory is allocated in `Table::new`: power-of-two shards picked by the top hash bits, each a 64-byte-aligned mutex over a linear-probing array kept at most three quarters full, 64-byte entries aligned to cache lines and an intrusive LRU list. Deletion shifts the probe chain back instead of leaving tombstones. When a shard is full it first reclaims an idle entry (one whose bucket has refilled and that holds nothing), then denies the new key or evicts the coldest of the eight least recent entries that hold nothing. Keys are hashed with SipHash-1-3 under a random key by default; SipHash-2-4, libpacer's choice, is kept so the differential tests can place keys exactly as the C does.
- **Window.** A fixed 168-byte value owned by one connection: up to 16 buckets plus the bucket being filled, with the oldest bucket weighted by the part of it still inside the window. Times that go backwards count at the newest time seen, and a jump of a whole window clears it.

Two inputs make the C wrong, and the Rust refuses them rather than copying the bug:

- `pacer_rate_ticks` multiplied interval by burst in wrapping 128-bit arithmetic, so a rate such as one token per 2^60 ns with a burst of 2^62 wrapped to a tolerance of 0 and the next take divided by zero. `Rate::ticks` returns `InvalidRate` on any overflow.
- `window.c` tested `now < head_start + slot_ns`, which wraps near the top of the clock. The Rust compares the difference, which agrees whenever the C does not wrap.

The generator keeps clear of both (`wraps_in_c`, and a clock at or below 2^60), and everything else is compared exactly. Four layers check the port:

- **Unit tests** port every case of `test_gcra.c`, `test_table.c`, `test_siphash.c` and `test_window.c`, plus threaded tests that also run under ThreadSanitizer.
- **`tests/model_gcra.rs`** holds `Gcra` to a token bucket kept as an exact reduced rational, through seeded scripts and a proptest tape that shrinks to a minimal failing script. A failure prints the seed and a replay command.
- **`legacy/pacer/tests/diffdrive.c`** reads one operation per line (clock, `gcra`, `table` and `window` operations) and prints the decision and the full state after it: TAT, interval and tolerance, table stats and invariant checks, window start and total. `edgefront-difftest` answers the same protocol from the Rust crate, byte for byte, and generates scripts that mix four limiters, three tables and three windows under one clock that jumps, goes backwards and passes the clamp. Each script ends by dumping every live object. The first differing line is reported with its seed, its step, the eight steps before it and a replay command.
- **`crates/edgefront-difftest/tests/e2e_limiter_port.rs`** drives the `edgefront-difftest` binary: one million generated operations against the C driver on every push, a divergence that must replay from the command it prints, and a worked example (per-key limits with exact retry times, then an hourly window quota) whose answers are written out in the test.

### The moving epoch

libpacer's TAT was absolute ticks in 64 bits. With up to 2^62 ticks of tolerance on top of the clock, that runs out at 2^57 ns, about 4.57 years of uptime, so pacer clamped every later timestamp and its limiters stopped refilling. `edgefront_limiter::clock` splits time into epochs of 2^56 ns (about 2.28 years, exactly the largest tolerance), and `Gcra` stores its TAT as an offset from an epoch:

- **Two words.** `base`, the limiter's base epoch, only grows. The TAT word's top bit is the parity of the epoch its 63-bit offset counts from, which is always `base` or `base + 1`. An offset holds just under two epochs, so the pair covers three: every arrival time a caller in either epoch can produce, with the full tolerance on top.
- **Lowest epoch first.** A write stores the new TAT from the lowest epoch that holds it and never below the base. Within one epoch the decision runs on 64-bit offsets, as before; across epochs it runs on 128-bit absolute ticks, then stores the result the same way. Any `u64` timestamp (584 years) is valid.
- **Rebasing.** When an arrival time would need an epoch past `base + 1`, the limiter first moves the TAT word up to `base + 1` (one CAS), then the base behind it (another). Each is a single CAS that another thread can finish just as well, so the limiter stays lock-free. A limiter idle for a century catches up in about 90 such steps, once.
- **Readers** on the slow path load `base`, the TAT word and `base` again, and retry if the base moved, so a word is never decoded against the wrong epoch. The fast path loads the base once: the base cannot move past the timestamp's epoch while every caller is within one epoch of it.
- **Contract.** Callers sharing a limiter must use timestamps within one epoch of each other, which any monotonic clock gives. Only a refund can move the TAT back. A refund dated before the base epoch's start stores that start, not the earlier time.

The fast path, when the TAT word and the timestamp share an epoch, checks one parity bit against a tag computed from the timestamp and runs the same 64-bit step as before. The slow paths are out of line. The cost is one more word per limiter (32 bytes) and one load of the base. Single-thread `gcra take`, alternating the old and new builds three times on the same 4-vCPU machine, gave 154 to 187M decisions per second before and 163 to 165M after, within the run-to-run spread. One trap on the way: closures that captured the rate by reference sent its address into the out-of-line slow path. The compiler then reloaded the interval from memory, lost the fact that it is non-zero, and kept a division the caller never used. That halved throughput to 89M. The closures now capture by value.

**The keyed table** has a lock per shard already, so it does not need the parity word. Each shard keeps a base epoch, and its entries keep their TATs as 64-bit offsets from the start of it. The base is one epoch behind the newest timestamp the shard has seen, or 0. Under the shard lock, the timestamp is turned into an offset first, and one two or more epochs past the base moves the base up to the epoch before it, in one pass over the shard's slots that shifts every entry down. That pass runs at most once every 2.28 years per shard, and an idle century costs one pass, not 44. The timestamp's offset is then below two epochs, so an entry's TAT, with the deepest tolerance on top, stays below 2^64 and the decision runs on the same 64-bit step as before. A TAT that would fall before the new base's start becomes that start: it is more than an epoch older than any timestamp the shard accepts, so the entry was a full bucket and stays one, and no decision changes. `Table::get` reports the TAT as 128-bit ticks since the clock's start. The table's contract is the `Gcra`'s: callers within one epoch of each other, and a timestamp before the base epoch's start counts as that start. The hot path gains one compare and one subtraction under the lock. Alternating the old and new builds twice on the same 4-vCPU machine, `table take` gave 26.3 and 26.8M decisions per second before and 26.5 and 26.1M after with 10k keys, and 53.4 and 54.3M before and 48.6 and 52.7M after with one hot key, within the run-to-run spread.

Under libpacer's horizon the decisions are unchanged: below 2^57 ns no base ever moves (every arrival time fits in epochs 0 and 1, and a table's base moves only at the start of epoch 2), and the difftest interpreter applies libpacer's clamp before calling `Gcra` or `Table`, so the C and Rust still agree line for line. Without the clamp on tables the difftest diverges in its first script. `tests/model_gcra.rs` runs its exact reference model at 4.5, 10 and 100 years of synthetic uptime, from just before an epoch boundary, and with idle gaps of up to three epochs over 24. The table's reference model, a map and a recency queue with 128-bit absolute TATs, does the same in `table.rs`: 12 seeds of 3,000 operations for each uptime, starting there, starting 10 s before an epoch boundary with keys in debt, and with idle jumps of up to three epochs. Putting libpacer's clamp back fails each of the three. `clock::ManualClock` and `clock::MonotonicClock` implement the `Clock` trait that callers read `now_ns` from.

### pacerd's config

`edgefront_limiter::config` is pacerd's `config.c`. `parse_pacerd` takes the file's bytes and gives a typed `LimiterConfig` (workers, admin socket, drain timeout, log level and up to 16 `ListenerConfig`s), or a `ConfigError` with pacerd's line number and message, word for word: `line 12: per_ip_conn_rate: expected a rate like 10/s, 600/m or 1m/s, got "10/d"`. To keep acceptance identical, it reproduces config.c's limits as well as its grammar:

- The file is at most 1 MiB and each line at most 1,024 bytes. A NUL byte is an error. A line has at most eight words, separated by spaces, tabs and carriage returns only, and `#` starts a comment even in the middle of a word.
- A number in a size or duration has at most 23 digits, leading zeros included. Sizes and rate counts are at most 2^50, and durations at most 24 hours before each directive's own range applies.
- Addresses are numeric `a.b.c.d:port` or `[v6]:port`. Leading zeros in an IPv4 octet are refused, as glibc's `inet_pton` refuses them. A port is 1 to 65535 and may have leading zeros.
- A directive may be set once per scope. Top-level directives keep their scope across listener blocks, so `workers` after a block still counts as a repeat.
- Tokens quoted in messages show printable ASCII except `"`, replace every other byte with `?`, and are cut at 44 bytes with `...`.
- Errors found at the end of the file name its last line, and a file over the limit names line 0.

The same directive table serves the serde form, which is how the limiter appears in an edgefront config. Field names are the directive names, and listeners are a `listeners` array with a `name` each. A value is the directive's text as a string (`"10/s burst 20"`, `"512 per 10s"`), or an integer where pacerd takes a bare number or size. Each value goes through the same parser, so a TOML error is the pacerd message with the field's path in place of the line: `listeners[0].max_conns: must be between 1 and 1000000`. The admin socket path is taken whole, so in TOML it may contain spaces.

The serde form also says two things pacerd.conf cannot, both from `rate::RateSpec`:

- **Rates in `RateSpec` syntax.** `per_ip_conn_rate`, `per_ip_bandwidth` and `bandwidth` are first read by pacerd's parser, so every pacerd rate keeps its meaning and its `Rate` value, and the config difftest stays at 0 divergences. Text pacerd refuses is then read as a `RateSpec`, whose refusal is the one reported. The count over the period is kept in lowest terms with the period in whole seconds (`2.5/10s` is 5 per 20 s), not in `RateSpec`'s own lowest terms, which may need a sub-second period (1 per 400 ms) that no text can write back. A rate whose exact period would overflow 64 bits that way is refused as too precise. A quota-shaped rate (a period of an hour or more with no burst) is refused in these fields, which are token buckets.
- **`per_ip_conn_quota`**, a `ConnQuota { limit, window }`: at most `limit` new connections per client in any `window`, from an hour to 366 days, which is `RateSpec`'s quota plan. A rate-shaped value is refused. Its line is recorded in the `SourceMap` like a directive's, and it counts as per-client admission for `table-below-max-conns`, `unused-table` and the IPv6 prefix lints.

Writing back, a rate pacerd can hold is written in pacerd's text, and any other as `RateSpec` text with the burst always written (`1/2s burst 30`, `10/day burst 10`), so it cannot read back as a quota. `to_pacerd` refuses a config with a quota or with such a rate, naming the field. Both writers leave out every value equal to pacerd's default, so `edgefront check --print`, which writes any limiter config as TOML, gives a file that says only what it changes and lints exactly as its source does.

`LimiterConfig::to_pacerd` and `Serialize` write a config back out, choosing the largest unit that is exact (`2m`, `16k`). `validate` checks a config built in code by writing it and reading it back. A value neither syntax can express is an error naming the field: a duration that is not a whole number of milliseconds, a rate period other than a second, minute or hour, or an IPv6 scope id. Property tests round-trip generated configs through both syntaxes.

The reader is fuzzed by `crates/edgefront-limiter/fuzz/fuzz_targets/limiter_config.rs`, which replaces pacer's `fuzz/fuzz_config.c`. It starts from pacer's three corpus files and its dictionary, kept in the crate because `legacy/pacer` goes in week 4; the dictionary adds the log levels and a few values pacer's left out, and `tests/fuzz_seeds.rs` fails if it loses a directive name or if a seed stops parsing. Beyond pacer's checks (an accepted file has 1 to 16 listeners, 1 to 64 workers, valid names, buffers of at least 1 KiB, tables of at least 16 keys and rates the limiter takes; an error is one printable line starting `line N:`), each input must also:

- name a line of the file in its error, line 0 only for the empty file or one over 1 MiB, and get the same error from `parse_pacerd` again;
- if accepted, pass `validate`, have no two listeners with the same name, and write out as pacerd.conf and as TOML, each reading back into an equal config;
- lint without a panic, each finding on a line of the file, about one of its listeners, in printable text.

CI runs it for 30 minutes on every push with `-timeout=10`, so a parse that loops or goes quadratic fails like a crash. A local 30-minute run made 20.4 million executions at about 11,300 a second, reached 2,848 coverage edges and a corpus of 781 inputs, and found nothing. Two bugs put in by hand were each found within three minutes: letting control bytes into quoted tokens, and writing sizes of 2 GiB and up one MiB too large.

### Config lints

A config can parse and still not do what it says. `edgefront_limiter::lint` reads a valid `LimiterConfig` and reports what pacerd would actually do with it, and `edgefront check [--allow CLASS]... FILE` prints each finding as `FILE:LINE: warning[CLASS]: listener NAME: MESSAGE`, then a count, exiting 1 when there are findings, 0 with `FILE: ok` when there are none and 2 when the file does not parse. A `.toml` file is the limiter section in the serde form; anything else is a pacerd.conf. The line comes from a `SourceMap`: `parse_pacerd_mapped` records the line of each directive and each `listener NAME {`, and for TOML the binary reads toml's spans for each key and `[[listeners]]` header. A setting left at its default is reported on the line that opens its listener.

Each class is a fact about pacerd's data path, and the unit tests check the facts the messages state against the limiter itself:

| class | reported when | why it matters |
|---|---|---|
| `burst-below-read` | `bandwidth` or `per_ip_bandwidth` has a burst below the smaller of 4 KiB and `buffer` | `reserve` asks a byte limiter for at least that much, and the limiter clamps the minimum to the burst. Reads are cut to the burst, not refused, and a busy connection wakes once per burst (a test drives `take_range` the way `reserve` does) |
| `listen-overlap` | a listener's address and an earlier one share a family and port, and exactly one is a wildcard | pacerd binds every listener with `SO_REUSEPORT`, so both bind and the kernel gives each connection to the most specific socket: the wildcard listener's limits never see that address's clients. IPv6 sockets are `V6ONLY`, so `[::]` and `0.0.0.0` do not overlap |
| `table-below-max-conns` | `per_ip_max_conns` or `per_ip_conn_rate` is set and `table_size` is below `max_conns` | each connected client holds a table entry, and a held entry is never reclaimed or evicted, so client `table_size + 1` is refused as `table_full` under either policy while the listener has room (tested on a `Table` under both policies) |
| `per-ip-cap-unreachable` | `per_ip_max_conns` is at least `max_conns` | one client reaches the listener's own cap first |
| `per-ip-bandwidth-unreachable` | `per_ip_bandwidth` is at least `bandwidth` in rate (compared exactly) and in burst | one client's bytes are a share of the listener's, so the listener limit binds first |
| `rate-rounding` | a rate is enforced more than 1% slower than written | a bucket's interval rounds up to a whole tick of 1/64 ns. pacerd's byte rates are commonly a little over 0.1% slow (`100m/s` by 0.106%), which is noise for a bandwidth cap; at 16 GiB a second it is 6.9%, and above 64 billion a second the exact interval is below one tick, so the rate enforced is 64 billion a second whatever is written |
| `unused-table` | `table_size` or `table_full` is written but no per-client limit is set | the client table is never consulted |
| `ipv6-prefix-unused` | `ipv6_prefix` is written on an IPv4 listener or on one without per-client limits | no client is ever grouped by it |
| `wide-ipv6-prefix` | per-client limits group an IPv6 listener's clients by a prefix shorter than /48 | /48 is one end site, so a shorter prefix makes several sites, or a provider, share one client's limit |
| `upstream-loop` | the upstream is a listener's address, or a loopback or unspecified address on a wildcard listener's port | every connection comes back into pacerd. Another address on a wildcard listener's port may be a different host, so it is not reported |
| `quota-unreachable` | `per_ip_conn_quota`'s limit is at least `per_ip_conn_rate`'s burst plus what the bucket refills in the quota's window, compared exactly | the bucket can never admit that many in one window, and its rounding only makes it slower, so the quota never refuses anyone. TOML only, since pacerd.conf has no quotas |

A config built in code has no source map; its findings carry line 0, and the two classes that depend on what was written out (`unused-table` and `ipv6-prefix-unused`) are skipped. pacer's `examples/pacerd.conf` lints clean, and every config pacer's tests and corpus accept lints without a finding outside the file.

### Rates, units and quotas

pacerd took whole counts per second, minute or hour (`10/s`, `600/m`), so half a request a second or a daily quota could not be written. `edgefront_limiter::rate::RateSpec` reads `COUNT/PERIOD [burst N]`: a count with up to nine decimal places and pacer's `k`, `m` and `g` multipliers, and a period of an optional whole number of units, `s`, `min`, `h` or `day` (with the usual spellings and plurals, and pacer's `m` for minutes). `0.5/s`, `90/min`, `2.5/10s burst 5` and `10000/day` all parse. `parse_pacerd` is unchanged and still refuses `10/d`, so pacerd files read exactly as before; `RateSpec` is the syntax for new configs.

- **Exact.** The count is kept as a decimal, `mantissa / 10^scale` per period, and never passes through a float. A token bucket gets the reduced fraction as its `Rate`: `0.5/s` is one per 2 s and `90/min` three per 2 s, so the limiter's interval is the exact quotient rounded up to a tick. `Display` writes a canonical form (`0.50/sec` becomes `0.5/s`) that parses back to an equal value.
- **Long windows become quotas.** A period of an hour or more without `burst` means "at most N in any window", the usual reading of an hourly or daily quota. A token bucket cannot say that: with a burst of N it grants N at once and then refills N over the window, up to 2N in one window. So such a rate is a `Plan::Quota`, enforced by `rate::Quota` on the sliding `Window` in 16 buckets, which holds each window to the count within the weighting of one bucket. A fractional count takes the shortest whole window: `1.5/h` is 3 in any 2 hours, `0.5/day` is 1 in any 2 days. Shorter periods, and any rate that names a burst, are `Plan::Bucket`s on `Gcra` or a `Table`. The default burst is the count rounded up, as pacer made it the count.
- **Exact waits for quotas.** A denied `Quota::take` says how long to wait, like `Gcra`. `Window::until_at_most(now, n)` finds when the count first drops to `n`: the count never rises while nothing is added, and within each bucket step the weighted oldest bucket `b` counts at most `room` exactly when `b * (slot - into) < (room + 1) * slot`, so the first nanosecond comes out in closed form, one pass over the ring. A test holds it to brute force on 300 random windows: the count at `now + wait` fits and one nanosecond earlier it does not.
- **Within 0.1%, or refused.** A bucket's time per token rounds up to a whole tick of 1/64 ns, so it never grants faster than written and is slower by less than one part in its interval. A rate where that exceeds 0.1%, faster than about 64 million a second, is refused with the slowdown it would have (`2g/s: too fast to enforce within 0.1% (it would run 0.66% slow)`) rather than silently slowed, and so is anything slower than one per 366 days, a period over 366 days, a count over 2^50 or with more than nine decimal places, a rate and burst whose tolerance passes 2^62 ticks, and a fraction whose exact interval does not fit 64 bits. Every check runs in `parse`, so `RateSpec::plan` cannot fail.

`tests/rate.rs` checks this from the text with plain integer arithmetic. Over 2,000 generated rates (counts of up to seven digits and nine decimals, every unit spelling, periods of up to 400 units, with and without burst), each accepted rate is held as exactly the written fraction, each bucket's interval is never shorter than written and at most 0.1% longer, each quota's limit over its window equals the count over the period with the shortest whole window, and each refusal is one of the reasons above for a reason that holds. Two simulations run a greedy client, which takes one at a time and comes back exactly when told to:

- 64 random bucket rates over a simulated day grant their burst plus the day's share of the rate, never more and within 0.1% or one request.
- 64 random hourly and daily quotas grant exactly their limit in the first window and within 0.1% of it in each of the next three, and no window that starts at a grant holds more than the limit plus a sixteenth.

The rates the roadmap names, from an unaligned start: `0.5/s` grants 43,200 in a day against a target of 43,200, `90/min` grants 129,689 (a burst of 90 plus 129,599 of 129,600), and `10000/day` grants exactly 10,000 on each of eight days. The bench gives a quota 150M decisions per second while it has room and 12.5M when spent, each denial computing its exact wait, against 128M for `gcra take` in the same run on a 4-vCPU machine.

### Speed against libpacer

`crates/edgefront-limiter/benches/decisions.rs` runs the single-thread workloads of `bench/bench_limiter.c`: a rate of 10^7 per second with a burst of 1000, time advancing 50 ns per decision, and a 200,000-key table under the evict policy, taking either 10,000 spread keys or one hot key. On one 4-vCPU Intel Xeon at 2.1 GHz, two runs each, in millions of decisions per second:

| workload | Rust, SipHash-1-3 | Rust, SipHash-2-4 | C |
|---|---:|---:|---:|
| GCRA take | 174 to 185 | | 111 to 120 |
| table take, 10k keys | 20.1 to 20.3 | 18.7 to 20.5 | 12.0 to 12.3 |
| table take, one hot key | 46.2 to 48.3 | 47.1 to 47.7 | 37.4 to 38.3 |

Before two changes the table was 15 to 30% slower than the C on the same machine (10.3 and 9.2 million with 10k keys, 31.9 and 27.1 with the hot key). Callgrind showed the Rust running fewer instructions per decision than the C (about 350 against 440), so the gap was stalls, not work:

- **SipHash tail.** The bytes after the last whole block were copied into a zeroed buffer and loaded back as one 64-bit word. That load spans several narrower stores, which the processor cannot forward, so every hash waited for the stores to retire, on the path from key to shard to slot. The tail is now assembled in registers from at most a 4-, a 2- and a 1-byte load. libpacer's byte-at-a-time load never had the stall. This alone doubled the 10k-key rate.
- **Entry alignment.** `Entry` is now `#[repr(align(64))]`, so each of its 64 bytes sits on one cache line and a probe or an LRU relink touches one line per entry. A heap slice of 64-byte values had only 16-byte alignment, and libpacer's `calloc` returns a large table's slots 16 bytes into a line, so three entries in four straddled two lines. This added about 8% with 10k keys and no memory.

The absolute targets of 300 million GCRA and 18 million keyed-table decisions per second come from pacer's own numbers (313 and 19) on its reference machine, the Docker Desktop Linux VM on an Apple M4 Max; that machine still has to run these benchmarks. On the Xeon above the table reaches 18 million and the GCRA does not reach 300 million, and the C is further from both. To compare on any machine, run `cargo bench -p edgefront-limiter --bench decisions` and `make -C legacy/pacer bench-build && legacy/pacer/build/release/bench_limiter`, and read the 1-thread column.

`legacy/pacer` stays until its L4 data path and `pacerctl` are ported too (week 4).

## What cachet is for

cachet is a shared HTTP cache that sits in front of one or more origins, close to clients. It should follow the HTTP caching rules closely enough that an origin can control it entirely through standard headers, keep the origin's load flat when popular objects expire, keep serving when the origin is slow or down if the origin allows it, and let an operator remove content from every node with one request.

It is a single binary with no external services. A node keeps its cache in memory with an optional disk tier, and nodes coordinate only to spread purges.

It does not terminate TLS, speak HTTP/2 or HTTP/3, run edge code, rewrite cache keys, or fetch from other nodes before the origin. Each of these is real work that a production edge needs, and each would hide the parts this project is about.

## Cache settings per route

The cache is becoming a layer that each proxy route turns on, so its settings are per route too. The `[cache]` section is the global block: besides the store's sizes it holds `max_object_bytes`, `max_variants`, `heuristic_max_secs` and an optional `max_ttl_secs` (a cap on any stored response's freshness lifetime), and every route inherits them. A `[[cache.routes]]` entry names one proxy route by `host` and `path_prefix` and may override any of those four; `enabled = false` keeps the entry but bypasses the cache. A route with no entry bypasses the cache.

`CacheConfig::resolve` ties the entries to the proxy's routes and returns each route's effective `RoutePolicy`, or every problem it found (up to the proxy config's 20), each with its field path in the proxy config's style: `cache.routes[2].max_variants: must be between 1 and 8, got 9`. An override may only tighten what the store allows (`max_object_bytes` and `max_variants` are bounded by the global values the store is built with), lifetimes are capped at a year, hosts compare without case and path prefixes exactly, two entries for one route are refused, and an entry that matches no route is an error, so a typo cannot quietly leave a route uncached. The heuristic cap is held under the route's lifetime cap, so a guessed lifetime never outlasts an explicit one.

## Request phases: the cache as a route layer

`edgefront_core::filter` defines the phases a layer runs in on a proxy route. It lives in edgefront-core so that neither edgefront-cache nor edgefront-proxy depends on the other. A `Filter` is attached per route in a `RouteFilters` table. A route without one skips the phases, so bypass costs one branch. For a route with a filter, a request goes through three phases:

1. **Request**, after routing and before the upstream pick. `Filter::request` sees the route index, method, URI and headers, and may change the headers that go upstream. It returns `Decision::Respond` (the answer, final) or `Decision::Forward`, optionally with an `Exchange` that holds the request's state.
2. **Upstream response**, when the upstream's head arrives and before any of it reaches the client. `Exchange::upstream_response` returns the response to send on.
3. **Deliver**, once, on the response the client gets. `Exchange::deliver` also sees responses the proxy wrote itself (`Delivery::Generated` with a reason such as `connect` or `timeout`), so the cache can answer an upstream failure with a stale copy.

`filter::run` drives the phases around an upstream call. Tests use it, and so can a server that has nothing of its own between the phases.

`edgefront_cache::layer::CacheLayer` is the cache as such a filter. Each route has its own `RoutePolicy`, and `CacheLayer::route_filters` attaches the layer only to cached routes. The phases do what cachet's `proxy.rs` does for one request:

- **Request phase.** Lookup, then the four freshness outcomes. A fill strips the client's conditionals and `Range` and adds the stored copy's validator. `only-if-cached` gets `504`. `no-store`, `HEAD` misses and other methods pass through.
- **Upstream response phase.**
  - A matching `304` refreshes the copy. For a `304` about another representation, the layer returns `Upstream::Resend` with its validators removed. The server sends the request once more, without a body, and gives the new response back to the same exchange. A second `Resend` is answered with a `502` the server writes (`resend_limit`).
  - A storable body is read within the route's `max_object_bytes` and `body_timeout`, then stored under the purge epoch taken in the request phase.
  - A larger body streams through unstored.
  - A successful unsafe method invalidates its URL and its same-host `Location` and `Content-Location`.
- **Deliver phase.** `stale-if-error` replacement, and `Cache-Status` on everything.

A route's `max_ttl` caps every stored lifetime, its heuristic cap applies, and its `max_variants` limits variants per URL through `Cache::store_limited`.

**Collapsing.** Concurrent fills of one URL share one upstream request, through `Collapser::lead_or_follow`.

- The first request leads. It goes upstream as usual, and its exchange publishes the result once it has one: a stored object, a response only it could use, or a failure.
- The others follow. They wait in their request phase, then answer from the published object (`collapsed` in `Cache-Status`) or share the failure, served stale where `stale-if-error` allows.
- A follower goes upstream on its own when the result was not storable, or is a variant its request would not select.
- In cachet the shared fetch ran on a task of its own. Here it is part of the leader's request, so a leader whose client goes away drops the flight, and a follower leads instead.
- An error the proxy wrote about the leader's own request, such as a `400` for its body, is not shared, and its followers go upstream themselves.

**Background refresh.** A copy inside its `stale-while-revalidate` window is answered with `Decision::Refresh`: the client gets the stale copy at once, and the request goes upstream in the background, leading the URL's flight, so there is one refresh per URL at a time.

**In the proxy.** `Proxy::with_filters` attaches a `RouteFilters`; the router maps each request to its route index and its cluster. The phases sit in `Proxy::handle` as follows:

- The request phase runs right after routing, before `max_requests`. A hit takes no place under the limit and never touches an upstream.
- The upstream-response phase runs before the proxy adds the request ID, so a stored response never carries another request's ID.
- The deliver phase runs on everything, including the proxy's own errors. These are named by their steer code (`bad_gateway`, `upstream_timeout`, `no_healthy_upstream`, `overloaded`, `bad_request`), and the cache treats only the 5xx ones as upstream failures.
- A refresh runs on its own task. It takes a cluster permit and the cluster timeout like any request, carries the client request's ID, and has no body.

`Via` and the forwarding headers stay with the proxy.

## Checking the cache against cachet 0.1

Moving the cache onto the shared runtime must not change anything a client sees, so `edgefront-difftest cache` (`make difftest-cache`, in CI on every push) replays a request log against cachet 0.1 and the edgefront binary and diffs every answer. cachet 0.1 is commit `ed77c32`, the last one before the workspace split: the repository has no release tags, so the Makefile pins the commit, exports it under `target/` and builds its `cachet` binary there.

The log is JSON lines, one request each, with a logical time in seconds, a target (a node's proxy or admin listener, or an origin event) and the method, path, headers and body. `cache-record` writes the log a seed generates. The 50,000 generated requests of a run cover about six logical hours across 16 route families, chosen by a skewed popularity:

- Fresh and stale hits, ETag and Last-Modified revalidation, and heuristic freshness.
- `Vary` on two headers, `no-store`, `private`, `no-cache`, `s-maxage`, `Expires` and `Set-Cookie`.
- Objects over the size limit, redirects and errors.
- Byte ranges, valid and not; client conditionals; client `Cache-Control` and `Pragma`.
- `Authorization` and cookies; HEAD and unsafe methods; three spellings of the host.
- Purges by tag and URL, refused purges, origin content changes, and origin outages that `stale-if-error` covers.

Each node runs the same config against its own copy of a deterministic origin, whose answers depend only on the request, the log's clock and the events so far. For each request the runner compares:

- the status;
- every header except `Date`, with the values of a repeated header in order (a request id the node made up is compared only as "generated");
- the body's SHA-256;
- how many requests the node sent its origin to answer it, which catches a revalidation or collapsing change that the response alone would hide.

Two things make a divergence a behaviour change and never noise:

- **Time.** Ages, lifetimes and `ttl=` are whole seconds measured on the wall clock, so two nodes a few microseconds apart could disagree near a second boundary. Each node runs with libfaketime preloaded, reading a frozen wall-clock time from a file that the runner rewrites when the log's clock moves. Monotonic time is left real, so timeouts and health checks behave normally.
- **Eviction.** The memory store picks shards and eviction candidates with a per-process hash seed. The nodes get a budget whose single shard holds the whole generated working set, so nothing is ever evicted.

Requests go one at a time, and a store completes before its response is sent, so the order is fixed too. `crates/edgefront/tests/difftest_cache.rs` runs the harness against the binary itself and must find nothing. It also runs it against a wrapper that sets `max_variants = 1` and a one-minute heuristic cap, and must catch that change, naming the lines and differences. A node that is not on the logical clock is refused before the run starts.

## Following one request

A request is handled in `proxy.rs`, which since W02 is a front over `CacheLayer`. The node answers some requests itself before the cache sees them:

- a request that already passed through this node (`508`);
- a target over `max_uri_bytes` (`414`);
- a request without a host (`400`);
- a body over `max_request_body` on a request that passes through (`413`).

Everything else goes through the layer's phases (`filter::run`) on a single route whose policy is the `[cache]` settings. The upstream is the origin pool. Requests get hop-by-hop headers removed, plus `Via`, `X-Forwarded-For` and the request ID. A failing origin is reported to the layer by name (`connect`, `timeout`, `protocol`, `body-too-large`).

The layer marks each response with its `Outcome` for the requests counter and the access log. It reports the other counters (not stored, streamed uncached, collapsed, background refreshes) through `Observer`, which cachet's `Metrics` implements. It adds the node's `Via` to responses that pass through unstored.

The cluster tests (`tests/cluster.rs`), `tests/revalidation.rs`, `tests/proxy.rs` and `tests/failure.rs` run against this layered node unchanged. `make difftest-cache` holds it to cachet 0.1's answers, request for request.

GET and HEAD go through the cache; every other method passes through to the origin. A request with `Cache-Control: no-store` also passes through.

For GET, the node computes the primary key (lower-cased host plus the request target), looks up the stored variants for that key, and picks the newest one whose Vary selecting headers match the request. The freshness rules in `policy.rs` then decide one of four things: serve it (fresh), serve it because the client accepts staleness (`max-stale`), serve it and refresh in the background (`stale-while-revalidate`), or validate it with the origin first.

Misses and validations go through the collapser. The origin request strips the client's own conditionals and Range, because its result may be shared; the node answers those itself from the stored copy afterwards. When a stored copy exists, the request carries its validator (`If-None-Match` from the entity tag, otherwise `If-Modified-Since`). A 304 whose entity tag matches refreshes the stored headers and freshness; a 304 for some other representation triggers a plain fetch.

Every response carries `Cache-Status` (RFC 9211), for example `edge-a; fwd=stale; fwd-status=304; stored; ttl=60`, and an `x-request-id`. A caller's request id is kept only if it is short and plain, so clients cannot put arbitrary text into the logs.

## Which HTTP caching rules apply, and where cachet is stricter

`policy.rs` implements the shared-cache parts of RFC 9111. A response is stored only if the method is GET, the status is one the cache understands, neither side sent `no-store` (unless `must-understand` applies), it is not `private`, `Vary` is not `*`, and it has explicit freshness, `public`, or a heuristically cacheable status. A request with `Authorization` is stored only with `public`, `s-maxage` or `must-revalidate` (section 3.5).

Freshness comes from `s-maxage`, then `max-age`, then `Expires` minus `Date`, then ten percent of the time since `Last-Modified`, capped at a day by default (sections 4.2.1 and 4.2.2). Age follows section 4.2.3 exactly, including the response delay correction, and time running backwards is clamped rather than underflowing. Cache-Control parsing accepts quoted arguments, takes the first of duplicated directives, clamps overflowing delta-seconds to 2^31 (section 1.2.2), and treats an unparseable `max-age` as zero, which makes the response stale rather than fresh forever.

Some choices go beyond or narrow the RFC, on purpose:

Responses with `Set-Cookie` are not stored. The RFC allows it, but sharing one client's cookie with everyone is almost never what an origin meant.

Responses that can never be reused (no freshness, no validator, no stale allowance) are not stored, since they would only take space.

Qualified `no-cache="field"` and `private="field"` are treated as their unqualified forms. That is the conservative reading.

Only single byte ranges are served. A request for several ranges gets the full object with 200, which RFC 9110 section 14.2 permits and which avoids multipart amplification. Partial responses (206) from the origin are never stored; the node always fetches the whole object and answers ranges from it.

HEAD misses pass through to the origin without being stored. HEAD hits are answered from the stored GET response.

Unsafe methods that succeed invalidate the target URL and same-host `Location` and `Content-Location` URLs (section 4.4). Only the local node is invalidated; each node sees its own writes.

## Many clients, one origin request

`collapse.rs` is a single-flight map from cache key to an in-flight fetch. The first caller starts the fetch on its own task and every caller, including the first, waits on a watch channel for the result. Running the fetch on a separate task matters: if the first client disconnects, the fetch is not cancelled, the followers still get an answer, and the response still reaches the cache. A panic in the fetch drops the channel, which releases every waiter with an error, and a drop guard removes the key. The result is published before the key is removed, so a request arriving in that gap reads the result instead of starting a second fetch.

Sharing is limited by what the response allows. A storable response is buffered and every waiter may use it. A response that may not be stored (for example `private` or carrying `Set-Cookie`) was produced for the first client's request, so only that client receives it; each follower then makes its own request. A follower whose Vary selecting headers do not match the shared response also fetches on its own. A test sends ten concurrent requests with different cookies to a `private` URL and checks that each client gets its own body.

## Serving stale content (RFC 5861)

Inside the `stale-while-revalidate` window the stale copy is served at once and one collapsed background refresh starts; further requests during the refresh are served stale without starting more refreshes.

When revalidation fails with a connection error, a timeout, a broken response, or a 500, 502, 503 or 504, the stale copy is served if the response's or the request's `stale-if-error` window covers its staleness. Other statuses, such as 404, are answers and are passed on. `must-revalidate`, `proxy-revalidate`, `s-maxage` and `no-cache` turn off every form of stale serving, as RFC 9111 section 4.2.4 requires.

## Keeping the right objects in memory: S3-FIFO with a byte budget

The memory tier (`store/memory.rs`) uses S3-FIFO from Yang et al. (SOSP 2023). New objects enter a small FIFO queue. When the small queue is over its share (ten percent of the bytes), its oldest object either moves to the main queue, if it was read since it was inserted, or is evicted and its key hash recorded in a ghost queue. An object whose key is in the ghost queue skips the small queue on its next insert. The main queue evicts its oldest object, giving it another round for each recorded read, with the count capped at three.

I chose it over LRU and over TinyLFU (Einziger, Friedman and Manes, 2017) for three reasons. A hit only bumps a two-bit counter, so lookups take a shared lock and never reorder a list, which is what an edge cache does most. One-hit objects, which dominate CDN traces, leave through the small queue without pushing popular objects out; a test inserts a thousand one-hit keys and checks that a frequently read key survives. And it is a few hundred lines with no frequency sketch to size and age.

The paper counts objects; cachet counts bytes. Each object is charged its body, its headers and a fixed overhead of 256 bytes, so many tiny objects still count. The ghost queue holds as many key hashes as the main queue holds objects, with a floor of 64. The paper's promotion threshold is a parameter; cachet promotes on a single read in the small queue.

The store is split into 16 shards by key hash, each with its own lock and an equal slice of the budget. An object larger than the per-object limit or than a shard is refused. Removal leaves stale queue entries behind, which eviction skips; a shard compacts its queues when stale entries outnumber live ones.

## Surviving a restart: the disk tier

The disk tier (`store/disk.rs`) is optional and write-through: every stored entry is queued for disk, and memory holds the hot subset. A memory eviction then costs nothing, and a restarted node comes back warm instead of sending every request to the origin at once. Writes go through a bounded queue (1,024 entries by default) to one thread. When the queue is full the write is skipped and counted, because the entry is still in memory and a cache may forget.

Each URL is one file named by a hash of its key, so keys never become paths. A record holds a magic number, a header with the key, statuses, freshness and headers for every variant, a SHA-256 of the header, the bodies, and a SHA-256 of the bodies. Files are written to a temporary name, synced, and renamed into place, so a crash leaves the old record or the new one and never a torn one. At startup the node deletes leftover temporary files, reads only the headers of the records (oldest first) to rebuild the index and the surrogate-key index, and deletes anything that fails a check: wrong magic, bad checksum, truncated, or a valid record stored under the wrong name. Body checksums are verified on every read; a record that fails is deleted and treated as a miss. Eviction on disk is plain FIFO by insertion within its own byte budget.

A test kills the real binary with SIGKILL, leaves a torn temporary file behind, stops the origin, restarts the binary and gets the object back as a hit from disk.

## Removing content: surrogate keys and the purge log

Origins tag responses with a `Surrogate-Key` header, a space-separated list of keys (a convention several CDNs use; there is no RFC). Each memory shard and the disk tier keep an index from tag to keys, updated on insert, replace, eviction and removal. `POST /purge` on the admin listener takes `{"tags": [...], "urls": [...]}` with up to 1,000 targets and a 64 KiB body, behind a bearer token. The token is compared through SHA-256 digests with no early exit, which also hides its length.

A fetch that started before a purge can finish after it. Without care, it would put the purged response back. Each fetch records the purge log's epoch when it starts. After storing, the cache asks the log whether a purge since that epoch matches the key or any of its tags, and removes its own insert if so. A purge records itself in the log before it removes anything, so either the purge removes the insert or the check sees the purge. The log keeps the last 4,096 targets; if it no longer reaches back to a fetch's epoch the answer is a conservative yes. The disk writer applies the same check before and after each write.

## Getting a purge to every node

`cluster.rs` spreads a purge to every node. The node that accepts a purge turns it into a message with a unique id, its node id, the time, and the targets, serialises it to JSON and signs those exact bytes with HMAC-SHA256 (RFC 2104) under a key shared by the cluster. The key is never sent. A receiver checks the signature over the raw body before parsing anything, then refuses messages older than the window (600 seconds by default) or more than 60 seconds in the future, and remembers ids for the window, so a captured message cannot be replayed. A new message is applied and then relayed, as the same signed bytes, to the receiver's peers other than the origin and the sender. This is the flooding form of the epidemic broadcast in Demers et al. (1987): each node forwards a message once, ids stop loops, and a purge still reaches a node that the first node cannot reach directly. A test with three nodes in a line checks that.

Each peer has its own queue (1,024 messages) and one delivery task, so a slow or dead peer delays only its own messages. A failed delivery is retried with exponential backoff from 100 ms up to 5 seconds until the message would be too old for the peer to accept. A peer that answers with a 4xx other than 429 has refused the message (usually a different key) and is not retried. A test stops a peer, purges, starts the peer again on the same disk directory and watches the purge arrive and remove the object that came back from disk.

With N nodes a purge costs O(N²) messages. That is fine for tens of nodes and wrong for thousands, where a tree or a fan-out service would be needed.

## Choosing an origin

`origin.rs` keeps an ordered list of origins. Requests go to the first healthy one. Requests without a body move to the next origin on a connection error, a timeout before response headers, or a broken response. A broken response on a pooled connection is retried once on the same origin first, since that is how a connection the origin already closed fails. If every origin is marked down, they are still tried in order: health data lags behind recovery, and failing closed would turn a brief blip into a full outage.

Health comes from active probes (`GET /healthz` every 2 seconds by default) and from connection failures and timeouts in real traffic, which share the same counters. An origin is marked down after three consecutive failures and back up after two consecutive successes. A broken response does not count against health.

## Input limits and secrets

Both of a node's listeners are edgefront-core's `listener`, the one the proxy track serves on. Clients get 10 seconds (`timeouts.client_header_ms`) to send a request head, timed from the accept for the first request and from its first byte for later ones; the head may be at most 64 KiB and 100 fields, and a larger one gets 431. A keep-alive connection may then sit silent for `timeouts.idle_ms`, which defaults to the header timeout because cachet's own accept loop closed silent connections after that long. A request counts as open until its response is fully written, so neither timer cuts a slow origin or a long download. Request targets over 8 KiB get 414. Request bodies over 10 MiB get 413, checked against Content-Length before reading and enforced while streaming otherwise. A request whose `Via` names this node gets 508. Origin bodies are buffered only up to the per-object limit (8 MiB); the declared length is checked before reading, and a body that turns out larger streams through to the one client that asked, uncached, without buffering the rest. Only the first 64 surrogate keys of a response are kept, keys over 128 bytes are dropped, and a URL keeps at most eight Vary variants, so an origin varying on something like User-Agent cannot fill the cache with one URL.

The admin listener binds to localhost by default. Without `CACHET_ADMIN_TOKEN` the purge API is off. Without `CACHET_CLUSTER_SECRET` (at least 32 bytes) the peer endpoint is off, and a config with peers refuses to start. Secrets come only from the environment and are redacted from `Debug` output.

## What happens when things fail

| Situation | What happens |
|---|---|
| Origin refuses connections | Next origin is tried; after three failures it is marked down and skipped. Stale copies inside `stale-if-error` are served; otherwise 502. |
| Origin hangs | 504 after the response timeout (10 s), or the next origin. A stalled body while caching is a timeout after 30 s. |
| Origin returns 5xx | Passed on, or the stale copy inside `stale-if-error`. |
| Origin sends malformed HTTP | 502, never stored. |
| Popular object expires | One origin request; every concurrent client waits for it. |
| Client disconnects mid-fetch | The fetch continues for other waiters and the cache. |
| Node crashes | Disk records are whole or absent. Restart deletes temporary files and corrupt records, and serves from disk. |
| Disk write queue full or disk fails | Write skipped and counted; memory still serves. |
| Peer unreachable during a purge | Retried with backoff for the message window, then dropped and counted. |
| Peer down for longer than the window | It misses the purge. Objects it still holds expire normally. |
| Forged, replayed or stale purge message | Refused before or after parsing, counted. |
| Purge races a fetch | The fetched response is not kept. |
| Shutdown | Stops accepting and closes idle connections at once. Open requests get up to 10 s (`shutdown_ms`) to finish, each answered with `Connection: close`; connections still open after that are closed. Then the disk queue is flushed. Queued purge deliveries are lost. |

## What it does not do well

HTTP/1.1 only, plain HTTP to origins and peers. Peer messages are signed, not encrypted; purge targets are not secret, but deployments that care should put the admin listener on a private network.

Objects are buffered whole in memory up to the per-object limit before they are served to collapsed waiters. Large media should use a lower limit and rely on streaming, which bypasses the cache.

Two concurrent stores of different variants of one URL can race, and one of them may be lost. The next request for it fetches again.

Conditional requests from clients are answered for `If-None-Match` and `If-Modified-Since`; `If-Match` and `If-Unmodified-Since` are ignored on hits.

Age depends on the origin's `Date`, so clock skew between origin and cache shifts apparent age, as the RFC acknowledges. A `Date` in the future counts as zero apparent age.

The startup scan reads every record header, which takes time proportional to the number of objects on disk.

## Why these dependencies

tokio, hyper and hyper-util provide the runtime, HTTP/1.1 server and pooled client; http, http-body-util and bytes are their shared types. httpdate parses the three HTTP date formats. sha2 and hmac provide the record checksums and message signatures; writing either by hand would be a mistake. serde, serde_json and toml read the config and the purge and peer messages. tracing and tracing-subscriber give JSON logs. criterion is a dev-dependency for benchmarks. Metrics, the Prometheus text format, request ids, the disk record format, request collapsing and the S3-FIFO store are written here because each is small.

Direct dependencies are pinned to exact versions published at least two weeks before they were added, and the lockfiles pin transitive crates to releases at least as old. One exception: cargo audit reported RUSTSEC-2026-0385 against ctutils 0.4.3, a transitive dependency of sha2 and hmac, so both lockfiles use ctutils 0.4.4, the first fixed release, published four days before it was taken.

## Sources

RFC 9110, HTTP Semantics. R. Fielding, M. Nottingham, J. Reschke, 2022.

RFC 9111, HTTP Caching. R. Fielding, M. Nottingham, J. Reschke, 2022.

RFC 5861, HTTP Cache-Control Extensions for Stale Content. M. Nottingham, 2010.

RFC 9211, The Cache-Status HTTP Response Header Field. M. Nottingham, 2022.

RFC 2104, HMAC: Keyed-Hashing for Message Authentication. H. Krawczyk, M. Bellare, R. Canetti, 1997. RFC 4231 provides the HMAC-SHA256 test vector used in the tests.

J. Yang, Y. Zhang, Z. Qiu, Y. Yue, R. Vinayak. FIFO Queues are All You Need for Cache Eviction. SOSP 2023.

G. Einziger, R. Friedman, B. Manes. TinyLFU: A Highly Efficient Cache Admission Policy. ACM Transactions on Storage, 2017.

A. Demers, D. Greene, C. Hauser, W. Irish, J. Larson, S. Shenker, H. Sturgis, D. Swinehart, D. Terry. Epidemic Algorithms for Replicated Database Maintenance. PODC 1987.
