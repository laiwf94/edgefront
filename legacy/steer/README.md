# steer

An HTTP load balancer and reverse proxy for the edge of a service. It keeps
the success rate that clients see high while the servers behind it crash,
return errors or stop answering.

Written in Go with the standard library only (no third-party dependencies).
The design follows [Envoy](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/upstream/load_balancing/overview)'s
priority levels, panic threshold and outlier detection,
[Finagle](https://twitter.github.io/finagle/guide/Clients.html)'s retry budgets
and least-loaded balancing, and Google's
[Maglev](https://research.google/pubs/maglev-a-fast-and-reliable-software-network-load-balancer/)
consistent hashing.

```json
{
  "listen": ":8080",
  "routes": [
    {"host": "*", "path_prefix": "/api/", "cluster": "api"}
  ],
  "clusters": [{
    "name": "api",
    "policy": "least_loaded",
    "endpoints": [
      {"address": "10.0.0.1:9000"},
      {"address": "10.0.0.2:9000"},
      {"address": "10.0.1.1:9000", "priority": 1}
    ],
    "health_check":      {"path": "/healthz", "interval": "1s"},
    "outlier_detection": {"consecutive_gateway_failures": 5},
    "retry":             {"max_retries": 2, "per_try_timeout": "250ms"}
  }]
}
```

## Failover drill

`examples/failover.sh` puts two primary upstreams and one backup behind steer,
sends 200 requests per second, and adds one fault per 10-second phase while
the load runs. The same drill runs with the resilience features on
(`examples/steer.json`) and off (`examples/steer-plain.json`: the same
endpoints and timeouts, no health checks, outlier detection or retries).
Measured on an Apple M4 Max, all processes on one machine:

With health checks, outlier detection and retries on:

| Phase | Requests | Success | p50 | p99 | p99.9 |
|---|---|---|---|---|---|
| 1. baseline | 2000 | 100.00% | 1.3 ms | 16.4 ms | 31.9 ms |
| 2. kill -9 primary-a | 2000 | 100.00% | 1.1 ms | 2.4 ms | 5.7 ms |
| 3. primary-b fails 50% of requests | 2000 | 100.00% | 1.2 ms | 5.6 ms | 11.0 ms |
| 4. primary-b blackholed | 2000 | 100.00% | 1.2 ms | 256.4 ms | 261.2 ms |

With them off:

| Phase | Requests | Success | p50 | p99 | p99.9 |
|---|---|---|---|---|---|
| 1. baseline | 2000 | 100.00% | 1.2 ms | 2.2 ms | 4.1 ms |
| 2. kill -9 primary-a | 2000 | 55.00% | 1.2 ms | 2.8 ms | 5.8 ms |
| 3. primary-b fails 50% of requests | 2000 | 26.15% | 1.4 ms | 2.4 ms | 4.0 ms |
| 4. primary-b blackholed | 2000 | 2.45% | 1.9 ms | 1003.3 ms | 1005.1 ms |

Here is what steer's log shows for the run with the features on. When
primary-a is killed, requests that land on it fail to connect and go straight
to another endpoint; within two seconds two failed health checks take it out,
and with half of priority 0 left, Envoy's priority math splits traffic 70/30
between the surviving primary and the backup.

The 50% errors in phase 3 are invisible to health checks, because the fake
upstream's `/healthz` stays green the way a real service's often does when a
dependency is failing. Its 503s are retried on the backup, and within a second
outlier detection ejects it for five gateway failures in a row.

Phase 4 is where the p99 comes from. The 10 s ejection expires mid-phase, the
now blackholed primary takes requests again, and each of those waits out the
250 ms per-try timeout before being retried on the backup. Five timeouts later
it is ejected again, this time for 20 s, and its health checks fail a second
after that.

Without the features, round robin keeps sending traffic to the dead and
failing hosts and never uses the backup.

## What steer handles

Routing picks a host (exact, `*.suffix` or `*`) and then the longest path
prefix, with Envoy's virtual-host semantics. Paths are cleaned before they are
matched and forwarded, so `/public/../admin` can't slip past a route.

Each cluster balances with one of three policies: nginx's smooth weighted
round robin, power of two choices on in-flight requests x peak-EWMA latency
(as in Finagle and Linkerd), or Maglev consistent hashing on a request header,
with weights. Endpoints have priorities; traffic fails over between them with
Envoy's priority load (overprovisioning factor 1.4), and below a panic
threshold steer spreads traffic over every host instead of crushing the few
that still look healthy.

Bad hosts are found two ways. Active health checks probe an HTTP path on an
interval with jitter and flip after a run of passes or failures. Outlier
detection watches real responses and ejects on consecutive gateway failures or
on a success rate below mean - k x stdev; ejections get longer each time, the
multiplier decays while the host behaves, and only a capped share of the
cluster can be out at once.

Retries go to a different endpoint every time, with full-jitter backoff. They
apply to idempotent methods, to requests with an `Idempotency-Key`, and to
connect failures for any method, since nothing was sent. The per-try timeout
only covers time to headers, so long streams aren't cut. Small bodies are
buffered for replay; larger ones go once. A Finagle-style retry budget caps
retries at a fraction of recent traffic so they can't multiply an outage, and a
per-cluster concurrency limit sheds excess load with an immediate 503 and
`X-Steer-Overflow: cluster_max_requests`.

Proxying is `httputil.ReverseProxy` with a custom `RoundTripper`, which keeps
hop-by-hop handling, flushing, trailers and WebSocket upgrades correct.
Client-supplied `X-Forwarded-*` headers are dropped unless the peer is a
configured trusted proxy, and request IDs are accepted when safe, generated
otherwise, and passed both ways.

With a `tls` block steer terminates TLS 1.2 and 1.3 on the proxy listener,
choosing the certificate by SNI (exact names, then one-label wildcards, then
the first certificate, or a refusal with `strict_sni`), with a cipher policy,
a handshake timeout and a cap on concurrent handshakes. A reload re-reads the
certificates; connections that are already open keep theirs.

Configuration reloads on SIGHUP or `POST /reload`: the file is validated, a new
runtime is built and swapped in atomically, and endpoints that keep their
address keep their health and ejection state. A bad file is rejected and the
old configuration keeps serving. The admin API listens on loopback unless a
token is set; logs are JSON, metrics are Prometheus text, and SIGTERM drains
before shutting down.

## Running it

```console
$ go build -o bin/ ./cmd/...
$ bin/upstream -name a -addr 127.0.0.1:19101 -control 127.0.0.1:19201 &
$ bin/upstream -name b -addr 127.0.0.1:19102 -control 127.0.0.1:19202 &
$ bin/upstream -name c -addr 127.0.0.1:19103 -control 127.0.0.1:19203 &
$ STEER_CONFIG=examples/steer.json bin/steer &
$ curl -s localhost:18080/hello
{"method":"GET","path":"/hello","upstream":"a"}
$ curl -s -X PUT localhost:19202/state -d '{"error_rate":0.5}'   # break b
$ bin/loadgen -url http://localhost:18080/ -rate 500 -duration 5s
requests 2500 in 5.0s  success 100.00%  (2500 ok; failures: none)
latency  p50 0.6 ms  p99 1.7 ms  p999 5.3 ms  max 6.6 ms
$ curl -s localhost:19901/clusters                                # health, ejections, latency estimates
$ ./examples/failover.sh                                          # the drill above
```

Or with Docker (the image's default configuration points at upstreams on the
host and exposes the admin API, so it needs a token):

```console
$ docker build -t steer .
$ docker run -p 8080:8080 -p 9901:9901 -e STEER_ADMIN_TOKEN=change-me-0123456789 steer
```

`docker stop` sends SIGTERM and waits 10 s by default, longer than the 5 s
drain period; raise both together.

| Variable | Meaning |
|---|---|
| `STEER_CONFIG` | Path to the JSON configuration (required). |
| `STEER_ADMIN_TOKEN` | Bearer token for the admin API, at least 16 characters. Required if `admin_listen` is not loopback. |
| `STEER_DRAIN_PERIOD` | How long `/readyz` reports draining before shutdown (default 5s). |
| `STEER_SHUTDOWN_TIMEOUT` | Time in-flight requests get after that (default 30s). |
| `STEER_LOG_LEVEL` | `debug`, `info`, `warn` or `error`. |

Every configuration field, its default and its bounds are listed in
[docs/DESIGN.md](docs/DESIGN.md#configuration).

## Admin API

Served on `admin_listen` (default `127.0.0.1:9901`). With `STEER_ADMIN_TOKEN`
set, everything except `/healthz` and `/readyz` needs `Authorization: Bearer <token>`.

| Endpoint | Purpose |
|---|---|
| `GET /healthz` | Process is up. |
| `GET /readyz` | 200 while serving, 503 while draining. Point the outer load balancer here. |
| `GET /clusters` | Per cluster: in-flight count, priority load, panic state, retry budget; per endpoint: health, last probe error, ejection state and multiplier, in-flight, latency estimate. |
| `POST /reload` | Re-read the configuration file. 400 with every validation error if it is invalid. |
| `GET /metrics` | Prometheus text format. |

Errors from the proxy and the admin API have one shape,
`{"error":{"code":"...","message":"...","request_id":"..."}}`, with stable codes:
`no_route` 404, `method_not_allowed` 405, `overloaded` 503,
`no_healthy_upstream` 503, `bad_gateway` 502, `upstream_timeout` 504,
`bad_request` 400, `unauthenticated` 401, `not_found` 404 (admin), `invalid_config` 400.
An upstream's own error response is passed through untouched.

### Metrics

| Metric | Labels |
|---|---|
| `steer_requests_total` | `cluster`, `code_class` |
| `steer_request_duration_seconds` (histogram) | `cluster` |
| `steer_upstream_attempts_total` | `cluster`, `endpoint`, `result` (`success`, `http_5xx`, `connect_error`, `timeout`, `reset`) |
| `steer_retries_total`, `steer_retry_budget_exhausted_total` | `cluster` |
| `steer_outlier_ejections_total` | `cluster`, `reason` |
| `steer_health_transitions_total` | `cluster`, `to` |
| `steer_overflow_total` | `cluster` |
| `steer_no_route_total`, `steer_config_reloads_total{result}`, `steer_config_version` | |
| `steer_tls_handshakes_total` | `result` (`ok`, `timeout`, `error`) |
| `steer_tls_handshake_duration_seconds` (histogram), `steer_tls_handshakes_in_progress` | |

## Tests and benchmarks

| Area | What the tests check |
|---|---|
| Chaos | Five upstreams, one blackholed and one failing half its requests, 4,000 requests at 32-way concurrency through a real listener: the test requires at least 99.5% success (it gets 100%), and requires the same faults with the features off to fail more than 10% (they fail about 30%). |
| Outlier detection | Random sequences of results, clock jumps and interval ticks over random configurations are checked, ejection by ejection, against a separate plain re-statement of the rules. Five deliberately broken versions (no ejection floor, no multiplier, decay every tick, counters never reset, ejected hosts in the statistics) all fail it. |
| Load balancing | Envoy's documented priority-load examples; randomised priority-load checks against a floating-point reference; smooth round robin against the exact weight counts over every window; Maglev's spread by weight and its disruption when one of ten endpoints leaves (0.2% of slots move between the survivors, against about 90% of keys for mod-N hashing). |
| Routing | Random route tables and requests against a brute-force matcher; fuzzing of matching and path cleaning. |
| Retry budget | Random request, retry and clock sequences against a model that keeps every event. |
| Proxy behaviour | Over real HTTP: forwarded headers and trust, request IDs, hop-by-hop removal, path cleaning, retries per method, body replay limits, per-try and overall timeouts, streams outliving the per-try timeout, budget exhaustion, never retrying the same endpoint, overflow, resets, client cancellation, WebSocket-style upgrades, server-sent events, reload carrying state, failed upgrades not leaking. |
| Parsers | Go fuzzing of the configuration, forwarded-for chains, request IDs, the fake upstream's control body and the load generator's header flag. |
| Process | `cmd/steer` end to end: SIGHUP and admin reload, a broken file rejected while the old configuration serves, drain, refusal to expose the admin API without a token. |

The whole suite runs under the race detector; coverage is 96% of statements.

```console
$ go test -race ./...
$ go test -run=^$ -bench=. ./internal/proxy ./internal/balancer
```

The benchmarks below ran on an Apple M4 Max with Go 1.27, over loopback with
keep-alive and a 12-byte upstream response. The client, steer and the upstream
share the machine, which was also busy with other work, so the table shows the
best of five runs.

| Benchmark | Direct | Through steer | With least-loaded, retries, outlier detection |
|---|---|---|---|
| One request at a time (latency) | 29.4 µs | 68.2 µs | 79.7 µs |
| 16 parallel clients (time per request) | 10.6 µs | 45.6 µs | 40.7 µs |

steer adds roughly 40 µs per request on loopback: a second HTTP parse and
loopback hop, plus routing, balancing, header rewriting and a JSON access log
line. Turning on the resilience features adds no cost beyond run-to-run noise.

| Operation (median of three) | Time |
|---|---|
| Route match, 100 prefixes | 186 ns |
| Pick, 10 endpoints: round robin / least loaded / Maglev | 276 / 83 / 80 ns |
| Pick, 100 endpoints: round robin / least loaded / Maglev | 0.97 / 0.47 / 0.51 µs |
| Build a Maglev table (65,537 slots, 100 endpoints) | 1.11 ms |

## Source tree

The reasoning behind the code is in [docs/DESIGN.md](docs/DESIGN.md): the
request path, each algorithm and the trade-offs taken, failure modes, limits,
security and what would change to run many steer instances.

```text
cmd/steer            proxy entry point, signals, drain
cmd/upstream         fake upstream with run-time faults
cmd/loadgen          open-loop load generator
internal/config      configuration parsing and validation
internal/route       host and path-prefix routing
internal/balancer    priority load, round robin, least loaded, Maglev
internal/endpoint    per-endpoint state that survives reloads
internal/health      active health checks
internal/outlier     outlier detection
internal/retry       retry budget and backoff
internal/proxy       request path, retries, forwarded headers, reload
internal/tlsedge     TLS termination: SNI certificate selection, handshakes, reload
internal/admin       admin API
internal/fakeupstream  the upstream behind cmd/upstream and the tests
internal/clock, reqid, metrics   time, request IDs, Prometheus exposition
examples/            configurations and the failover drill
```

## License

MIT
