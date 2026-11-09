# edgefront

A self-hosted application edge in one Rust binary. It brings three projects together and is being built into a single Cargo workspace:

| Part | Started as | Today |
|---|---|---|
| Cache | cachet | An HTTP caching reverse proxy in Rust, split into `edgefront-cache` and the root package and served by the `edgefront` binary |
| Proxy | steer | A Go edge proxy and load balancer, under [legacy/steer](legacy/steer), being ported to `edgefront-proxy`; the Rust port already proxies HTTP/1.1 with steer's configuration (see below) |
| Limiter | pacer | A C rate limiter, under [legacy/pacer](legacy/pacer), being ported to `edgefront-limiter`; its algorithms are ported and make the same decisions as the C (see below) |

All three keep their full history. The plan for the next year, three tracks in parallel, is in [ROADMAP.md](ROADMAP.md) and the issues labelled `roadmap`.

## Workspace

| Crate | What it holds |
|---|---|
| `crates/edgefront` | The `edgefront` binary: the cache node (cachet's command line, unchanged) and `edgefront proxy` |
| `crates/edgefront-core` | The listener, pooled HTTP/1.1 client, steer's config schema, metrics and admin plumbing |
| `crates/edgefront-proxy` | Routing, path cleaning, forwarding headers, request IDs and the HTTP/1.1 request path ported from steer |
| `crates/edgefront-cache` | cachet's caching policy, storage and purge |
| `crates/edgefront-limiter` | pacer's rate limiting algorithms |
| `crates/edgefront-difftest` | Differential tests against `legacy/steer` and `legacy/pacer` |
| `.` (`cachet`) | The cache node's runtime still to move into the crates, and the reference `cachet` binary |

Which crate depends on which is in [docs/DESIGN.md](docs/DESIGN.md#crate-map).

## Proxy (port of steer)

`edgefront proxy` runs the proxy from a steer configuration file, unchanged, in JSON or the same schema in TOML:

```sh
cargo build --release -p edgefront
target/release/edgefront proxy --check-config legacy/steer/examples/steer.json
target/release/edgefront proxy --config legacy/steer/examples/steer.json   # listens on 127.0.0.1:18080
```

The log is JSON on stdout (`EDGEFRONT_LOG` sets the filter, default `info`), one line per request with steer's fields, and SIGTERM drains in-flight requests before exiting. The request path behaves as steer's did: routing on host and longest prefix after path cleaning, `X-Forwarded-*` rewritten and trusted only from `trusted_proxies`, request IDs accepted or generated and returned, hop-by-hop headers removed both ways, each cluster's `max_requests` and `timeout`, and steer's JSON errors (`no_route`, `overloaded`, `bad_gateway`, `upstream_timeout`, `bad_request`, `method_not_allowed`). `crates/edgefront/tests/e2e_port_http1.rs` holds the binary to a transcript recorded from steer, header values byte for byte, and in CI it also runs steer itself on the same 30 requests.

Still to come from steer: the balancing policies and priority load (week 2; until then each cluster takes turns over its most preferred endpoints), health checks, outlier detection, retries, the admin API and metrics (week 3), and TLS termination, reload and drain (week 4). A configuration with a `tls` block is refused rather than served in plain text.

## Limiter (port of pacer)

`edgefront-limiter` holds libpacer's algorithms, ported to safe Rust, and pacerd's config. Its only dependency is serde, for the config:

- `Gcra`: an exact token bucket (the generic cell rate algorithm) whose arrival time is one `AtomicU64` updated by compare-and-swap. `take`, `take_upto`, `take_range`, `refund`, `remaining` and `reconfigure` never lock, and a denied take says exactly how many nanoseconds to wait before it would be admitted. The arrival time counts from a moving epoch, so unlike pacer's it keeps working past 4.5 years of uptime: any `u64` of nanoseconds is a valid time.
- `clock`: the epoch arithmetic and the clocks that give `now_ns`: `MonotonicClock`, and `ManualClock` for tests and simulations.
- `Table`: a fixed-memory map from keys (an API key, an address) to their own bucket, in power-of-two shards, keyed by SipHash with a random key. When it is full it reclaims idle entries, then denies new keys or evicts the coldest of eight, and it never drops a key that holds a concurrency slot (`hold`/`release`).
- `Window`: a sliding-window counter of up to 16 buckets, for quotas such as 1,000 requests an hour, that weights the oldest bucket by the part of it still inside the window.
- `config::LimiterConfig`: pacerd's listeners and limits. `parse_pacerd` reads an existing pacerd.conf with pacerd's own messages and line numbers, and the same settings load through serde as a TOML section, with each value written as in pacerd (`per_ip_conn_rate = "10/s burst 20"`).

Every call takes the time in nanoseconds from the caller, so the library reads no clock and its answers are exact:

```rust
use edgefront_limiter::{Rate, Table, TableConfig};

const SEC: u64 = 1_000_000_000;
// 100 requests a minute per API key, in bursts of up to 10.
let limits = Table::new(&TableConfig::new(100_000, Rate::new(100, 60 * SEC, 10)))?;
let d = limits.take(b"alice", now_ns, 1)?;
if !d.allowed {
    // d.retry_after_ns: 600,000,000 right after a full burst.
}
```

`edgefront-difftest` holds the port to libpacer through `legacy/pacer/tests/diffdrive.c`, a C driver that reads one operation per line and prints each decision with the full state after it. `make difftest-pacer` diffs 100 generated scripts of 10,000 operations against the Rust crate and prints the seed, step and replay command of the first difference. `crates/edgefront-difftest/tests/e2e_limiter_port.rs` runs one million of those operations in CI on every push, plus a worked example of per-key limits, retry times and an hourly quota that both sides must answer exactly. How the port was checked and where it deliberately differs from the C is in [docs/DESIGN.md](docs/DESIGN.md#porting-pacer-the-limiter-algorithms).

Still to come from pacer: the rest of week 2 (the config difftest, a clock that runs past 4.5 years, fractional rates and longer units, and lints), the L4 data path with connection limits and shaping (week 3), and `pacerctl` with the published C-versus-Rust benchmark (week 4), after which `legacy/pacer` is removed.

## Cache (formerly cachet)

cachet is an HTTP caching reverse proxy for the edge, written in Rust. It follows the HTTP caching rules (RFC 9111) so origins control it with ordinary headers, sends one origin request when many clients miss at once, keeps serving stale content while the origin is slow or down when the origin allows it, and removes tagged content from every node with one purge request.

It is a single binary: an S3-FIFO memory cache with a byte budget, an optional crash-safe disk tier, health-checked origins with failover, signed purge propagation between nodes, Prometheus metrics and JSON logs. The design, its trade-offs and the papers and RFCs behind it are in [docs/DESIGN.md](docs/DESIGN.md).

## Running it

You need Rust 1.88 or newer.

```sh
cargo build --release -p edgefront --bin edgefront
cargo build --release -p cachet --example origin

# A small origin with cacheable routes, on 127.0.0.1:9100.
target/release/examples/origin 127.0.0.1:9100 &

# The cache on :8080, admin API on 127.0.0.1:9090.
CACHET_ADMIN_TOKEN=change-me target/release/edgefront --config examples/cachet.toml
```

The `edgefront` binary takes cachet's flags, environment variables and config file unchanged and prints the same responses and log lines. The `cachet` binary (`--bin cachet`) still builds as the reference it is checked against.

`examples/cachet.toml` lists every setting with its default. Secrets are read only from the environment: `CACHET_ADMIN_TOKEN` enables `POST /purge`, and `CACHET_CLUSTER_SECRET` (at least 32 bytes) signs purges between peers. `edgefront --check-config <file>` validates a config without starting.

To run two nodes and the origin in containers:

```sh
cd examples/compose
CACHET_ADMIN_TOKEN=change-me CACHET_CLUSTER_SECRET=$(openssl rand -hex 32) docker compose up --build
```

## Reading a response

Every response says what the cache did in a `Cache-Status` header (RFC 9211):

```console
$ curl -s -o /dev/null -D - localhost:8080/static/logo | grep -i cache-status
cache-status: edge-1; fwd=miss; fwd-status=200; stored; ttl=300
$ curl -s -o /dev/null -D - localhost:8080/static/logo | grep -i cache-status
cache-status: edge-1; hit; ttl=300
```

The origin tags responses with `Surrogate-Key: static`. One request removes every object with that tag, on this node and, when peers are configured, on every peer (the response then also carries the purge id):

```console
$ curl -s -X POST -H "authorization: Bearer change-me" \
    -d '{"tags": ["static"]}' localhost:9090/purge
{"removed":1,"targets":1}
```

The admin listener also serves `/healthz` and `/metrics`.

`examples/demo.sh` runs the release `edgefront` binary (`DEMO_BIN=cachet` for the reference binary) as two nodes in front of the example origin and checks each behaviour as it goes: a stored miss and a hit, a 304 revalidation, 50 concurrent clients making one origin request, a stale copy served while the origin is stopped, and a purge on one node reaching the other. `crates/edgefront/tests/e2e_workspace.rs` runs the same five steps as a test and checks every `Cache-Status` byte for byte. Part of a run:

```
== 3. 50 concurrent requests for a cold URL make one origin request
   statuses: 50 200; responses marked collapsed: 49
   ok: origin saw 1 request for 50 clients

== 4. With the origin down, a stale object is served (stale-if-error)
   GET /news       -> 200  cache-status: edge-a; fwd=miss; fwd-status=200; stored; ttl=2
   origin stopped
   GET /news       -> 200  cache-status: edge-a; hit; ttl=-1; fwd=stale; detail-origin=connect; detail=stale-if-error
   ok: served 'headlines, edition 1' from the stale copy
   GET /static/never-cached -> 502  cache-status: edge-a; fwd=miss; detail=connect

== 5. A purge on edge-a reaches edge-b
   POST edge-a /purge {"tags": ["static"]} -> {"id":"0bc78fbc7543cd09f3c8c9abd9b4f02e","removed":2,"targets":1}
   GET /static/tagged -> 200  cache-status: edge-a; fwd=miss; fwd-status=200; stored; ttl=300
   GET /static/tagged -> 200  cache-status: edge-b; fwd=miss; fwd-status=200; stored; ttl=300
   ok: both nodes fetched the object again after one purge request
```

## When the origin, a node or a peer fails

When an origin refuses connections or times out, the request moves to the next configured origin, and after three consecutive failures the origin is skipped until health checks see it recover. If the origin fails or returns 500, 502, 503 or 504 and a stale copy is within its `stale-if-error` window, the stale copy is served; otherwise the client gets 502 or 504. Inside the `stale-while-revalidate` window, clients get the stale copy at once while one background request refreshes it.

When many clients miss on the same object, one request goes to the origin and the rest wait for it. If the first client disconnects, the fetch carries on for the others. Responses that are `private` or set cookies are never shared between waiting clients.

With the disk tier on, a crash leaves each record whole or absent: records are written to a temporary file and renamed, and carry checksums. On restart the node deletes partial and corrupt files and serves what remains, so it comes back warm. A test kills the binary with SIGKILL and checks this.

A purge is signed and sent to every peer. Peers check the signature before parsing, refuse old or replayed messages, and relay new ones once, so a purge reaches nodes the first node cannot reach directly. A peer that is down is retried with backoff for ten minutes. A fetch that started before a purge does not put the old response back.

Malformed requests get 400, oversized heads 431, long targets 414, oversized bodies 413, and request loops 508. Malformed origin responses become 502 and are never stored.

## How fast the hit path is

On an Apple Silicon laptop (16 cores) that was also running other work (load average around 70), so read these as lower bounds. Reproduce with `cargo bench` and `examples/load.sh`.

In-process, with criterion:

| Benchmark | Time |
|---|---|
| Parse a five-directive Cache-Control header | 126 ns |
| Freshness decision for a stored object | 20 ns |
| Memory store lookup among 10,000 objects | 48 ns |
| Full cache hit through the request handler, 4 KiB body | 1.28 µs |

End to end with [oha](https://github.com/hatoo/oha), 64 connections for 10 seconds, release build, oha, cachet and the origin on the same machine:

| Run | Requests/s | p50 | p99 |
|---|---|---|---|
| Origin directly, 4 KiB body | 138,517 | 0.31 ms | 2.97 ms |
| Cache hits through cachet, 4 KiB body | 148,923 | 0.33 ms | 2.17 ms |
| Cache misses through cachet, every URL new | 63,423 | 0.82 ms | 4.20 ms |

Hits through cachet run at about the rate of the in-memory origin serving the same bytes, which suggests the load generator and the loopback stack set the limit here rather than the cache. In the collapsing run, 250 clients requested a cold URL that takes 300 ms at the same moment; the origin saw one request, and `cachet_collapsed_requests_total` counted 249 waiters.

## Where it stops

HTTP/1.1 only; no TLS, HTTP/2 or HTTP/3, and plain HTTP to origins and peers. Objects are buffered in memory up to the per-object limit (8 MiB by default); larger responses stream through uncached. Only single byte ranges are served. Purge propagation costs O(N²) messages for N nodes, which suits tens of nodes, and a peer that is down for longer than the ten-minute window misses the purge. Queued purge deliveries are lost on shutdown. See [docs/DESIGN.md](docs/DESIGN.md#what-it-does-not-do-well) for the full list.

## Checking a change

`make check` is the gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and the tests under cargo-llvm-cov, where `scripts/coverage.sh` holds every crate to at least 90% line coverage on its own sources, and the workspace too. `make audit` runs cargo deny on the workspace and on each fuzz crate, cargo audit on every lockfile, and `scripts/direct-deps.sh`, which requires exact pins and allows at most 14 direct runtime dependencies across the workspace. `make fuzz` runs every cargo-fuzz target in `crates/edgefront-cache/fuzz` and `crates/edgefront-core/fuzz` (nightly; CI gives each target 10 minutes). `make tsan` runs each crate's unit tests and its concurrency tests under ThreadSanitizer, one crate at a time (`make tsan-<crate>` for one; nightly with rust-src). `scripts/verify-history.sh` runs `make check` on every commit of the branch and prints each one's lowest crate coverage. CI runs all of these, with one ThreadSanitizer job per crate, builds on Rust 1.88, runs the demo and builds the container image, which runs as a non-root user.

While `legacy/steer` exists, its own gate runs too (gofmt, vet, `go test -race ./...` and short fuzzing), plus three comparisons that need Go: `make difftest` diffs 10,000 generated configs and 100,000 generated requests between steer and the Rust crates, `make difftest-balancer` diffs the pick sequences of 1,000 seeded balancing scenarios of 10,000 requests each (health changes, ejections, priority spill and panic included) between steer's `balancer.LB` and `edgefront_proxy::balancer::Balancer`, and `make e2e-steer` runs the end-to-end HTTP/1.1 cases against a live steer as well as the edgefront binary.

While `legacy/pacer` exists, its gate runs as well (format, lint, the tests under ASan+UBSan and TSan with coverage, short fuzzing, its demo and its image), and the pacer differential test needs only a C compiler: `make test` already runs a million operations through it, and `make difftest-pacer` runs ten times more from a clock seed. pacerd's config reader is diffed the same way: `make test` puts every file under legacy/pacer's `tests`, `examples` and `fuzz/corpus`, every string literal in its tests and 3,000 seeded mutants through `diffdrive config` and the Rust parser, comparing accept or reject, the error message and line, and every parsed value, and `make difftest-pacer-config` adds 2,000 mutants from a clock seed (`DIFFTEST_ARGS="--mutants N"` for more).

## License

MIT. See [LICENSE](LICENSE).
