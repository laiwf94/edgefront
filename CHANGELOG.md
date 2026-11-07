# Changelog

Changes to edgefront, newest first. Each week's entries name the roadmap issue they close or advance.

## Unreleased

### Cache

- cachet is split into the edgefront workspace with no change a client can see (#78):
  - `edgefront-core` holds the accept loop, config loading (`config::loader`), the metrics writers, the admin router with bearer tokens, request ids and the response body type.
  - `edgefront-cache` holds the caching policy, Cache-Control and range parsing, objects, request collapsing, purge, cluster gossip (through a `PurgeHost` trait, so the crate never depends on cachet), the memory and disk stores, and the cache's config sections.
  - `crates/edgefront` is the binary. It takes cachet's flags, `CACHET_*` variables, exit codes and config file unchanged, and logs the same JSON lines under the same targets.
  - The root package keeps the node runtime still to move and the reference `cachet` binary.
- `crates/edgefront/tests/golden.rs` and `tests/golden_cli.rs` hold both binaries to the same command-line transcripts, log lines included. (#78)
- `crates/edgefront/tests/e2e_workspace.rs` runs the demo's five steps against two edgefront nodes and compares every status, `Cache-Status` and body with a fixture byte for byte; `tests/e2e_workspace.rs` runs the cachet binary against the same fixture. `examples/demo.sh` runs the edgefront binary by default and the reference one with `DEMO_BIN=cachet`, and CI runs both. (#78)
- The fuzz targets live in `crates/edgefront-cache/fuzz` and `crates/edgefront-core/fuzz`, and CI fuzzes each of the nine for 10 minutes. The three new core targets (`request_id`, `base_url`, `bearer_token`) found that `http://:80` and `http://origin:8o8o` were accepted as origin URLs; both are now rejected. (#78, #240)
- `make cov` holds every crate to 90% line coverage on its own sources, `make tsan` runs ThreadSanitizer one crate at a time (one CI job each), and `make audit` runs cargo deny on every manifest, cargo audit on every lockfile and a budget of 14 exactly pinned direct dependencies. (#78)

### Proxy

- `edgefront_core::clock`, ported from steer's `internal/clock`: a `Clock` trait over monotonic time and one-shot timers, `MonotonicClock` on tokio's timer, and `ManualClock`, which moves only when advanced and then fires due timers earliest deadline first, ties in the order they were made. Balancing, health checks and outlier detection take it so tests and the difftest can drive time. (#80)
- `edgefront proxy --config <file>` runs the HTTP/1.1 proxy ported from steer, from steer's configuration file unchanged (JSON, or the same schema in TOML). `--check-config` validates a file. The request path matches steer:
  - Go's server checks and steer's JSON errors.
  - Routing on the cleaned path.
  - Forwarding headers trusted only from `trusted_proxies`.
  - Request IDs.
  - Hop-by-hop removal.
  - Each cluster's `max_requests` and `timeout`.
  - One access log line per request. (#77)
- `crates/edgefront/tests/e2e_port_http1.rs` holds the binary to a transcript of 30 requests recorded from steer, covering responses and what the upstream received. `make e2e-steer`, run in CI, also replays the cases against a live steer. (#77)
- `edgefront-difftest` diffs 10,000 generated configs and 100,000 generated requests between `legacy/steer/cmd/diffdrive` and the Rust crates on every push (`make difftest`). (#77)
- Ported from steer: the config loader (`edgefront_core::config`), host and prefix routing (`edgefront_proxy::route`), path cleaning and request targets (`edgefront_proxy::path`), forwarding headers (`edgefront_proxy::forwarded`), request IDs (`edgefront_proxy::reqid`), and the HTTP/1.1 listener and pooled client (`edgefront_core::listener`, `edgefront_core::http1`). (#77)
- Config errors quote values the way Go's `%q` does. (#237)

### Limiter

- `edgefront_limiter::config`: pacerd's config in Rust. `parse_pacerd` accepts and rejects pacerd.conf files as pacerd does, with the same message and line number. `LimiterConfig` also loads through serde as a TOML section that uses pacerd's directive names and value syntax, and writes back out in either syntax. serde becomes the crate's one dependency. (#82)
- New crate `edgefront-limiter`, libpacer's algorithms in safe Rust with no dependencies (#79):
  - `Gcra`: an exact, lock-free token bucket whose arrival time is one `AtomicU64`, with `take`, `take_upto`, `take_range`, `refund`, `remaining` and `reconfigure`, and retry-after times exact to the nanosecond.
  - `Table`: per-key buckets in fixed memory, sharded, with idle reclaim, deny or evict when full, holds, sweeps and stats. Keys are hashed with SipHash-1-3 by default, or SipHash-2-4 to place keys as libpacer does.
  - `Window`: a sliding-window counter for quotas.
- The keyed table takes about 60% more decisions per second than libpacer's with spread keys and 25% more with one hot key on the same machine, where it took 15 to 30% fewer before: the SipHash tail is built in registers instead of reloaded from a byte buffer, which stalled every hash on store forwarding, and table entries are aligned to cache lines. Numbers are in docs/DESIGN.md. (#79)
- Rates whose burst times interval overflows are refused with `InvalidRate`. libpacer wrapped them to a zero tolerance and then divided by zero. (#79)
- `legacy/pacer` holds pacer with its full history. Its own gate (sanitizers, coverage, fuzzing, demo and image) runs in CI while it exists. (#79)
- `legacy/pacer/tests/diffdrive.c` and `edgefront-difftest pacer` diff generated operation scripts between libpacer and edgefront-limiter, comparing every decision, retry time and state. The first difference is reported with its seed, step and a replay command; `make difftest-pacer` runs 100 scripts of 10,000 operations from a clock seed. (#79)
- `crates/edgefront-difftest/tests/e2e_limiter_port.rs` runs one million differential operations on every push, checks that a divergence replays from the command it prints, and walks through per-key limits, retry times and an hourly quota with the answers written out. (#79)
- `crates/edgefront-limiter/tests/model_gcra.rs` checks `Gcra` against a token bucket in exact rational arithmetic, from seeds and from a shrinking proptest tape. (#79)
