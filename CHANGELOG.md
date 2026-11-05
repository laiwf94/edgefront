# Changelog

Changes to edgefront, newest first. Each week's entries name the roadmap issue they close or advance.

## Unreleased

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

- New crate `edgefront-limiter`, libpacer's algorithms in safe Rust with no dependencies (#79):
  - `Gcra`: an exact, lock-free token bucket whose arrival time is one `AtomicU64`, with `take`, `take_upto`, `take_range`, `refund`, `remaining` and `reconfigure`, and retry-after times exact to the nanosecond.
  - `Table`: per-key buckets in fixed memory, sharded, with idle reclaim, deny or evict when full, holds, sweeps and stats. Keys are hashed with SipHash-1-3 by default, or SipHash-2-4 to place keys as libpacer does.
  - `Window`: a sliding-window counter for quotas.
- Rates whose burst times interval overflows are refused with `InvalidRate`. libpacer wrapped them to a zero tolerance and then divided by zero. (#79)
- `legacy/pacer` holds pacer with its full history. Its own gate (sanitizers, coverage, fuzzing, demo and image) runs in CI while it exists. (#79)
- `legacy/pacer/tests/diffdrive.c` and `edgefront-difftest pacer` diff generated operation scripts between libpacer and edgefront-limiter, comparing every decision, retry time and state. The first difference is reported with its seed, step and a replay command; `make difftest-pacer` runs 100 scripts of 10,000 operations from a clock seed. (#79)
- `crates/edgefront-difftest/tests/e2e_limiter_port.rs` runs one million differential operations on every push, checks that a divergence replays from the command it prints, and walks through per-key limits, retry times and an hourly quota with the answers written out. (#79)
- `crates/edgefront-limiter/tests/model_gcra.rs` checks `Gcra` against a token bucket in exact rational arithmetic, from seeds and from a shrinking proptest tape. (#79)
