# steer design

steer exists so that clients keep getting successful responses while
upstreams crash, return errors or hang. Some of those failures show up in
health checks and some only in real traffic, so it watches both. Just as
important, it must never make an outage worse: retries are capped by a budget,
nothing unsafe is repeated, and a cluster that is mostly down spreads its load
rather than piling it onto the last few hosts. A reload shouldn't throw away
what steer has learned about each endpoint, and every size, count and time is
bounded; the limits are all in [one table](#limits).

This version terminates TLS on the proxy listener (see
[TLS termination](#tls-termination)) but leaves out HTTP/2 to upstreams,
service discovery other than the configuration file, and sharing state
between steer instances. The last two are discussed under
[Running more than one instance](#running-more-than-one-instance).

## The request path

```text
client ─▶ ServeHTTP ─▶ route ─▶ concurrency limit ─▶ ReverseProxy ─▶ RoundTripper ─┐
                       (404)    (503 overflow)        rewrite headers   pick endpoint │
                                                                        attempt ◀─────┘
                                                                        classify, report
                                                                        retry? budget? backoff
```

The cleaned path and the Host header pick a route, which names a cluster; no
match is a 404. Each cluster counts its requests in flight, and once that count
reaches `max_requests` new requests get an immediate 503 with
`X-Steer-Overflow: cluster_max_requests`. This is Envoy's circuit breaker.
Refusing quickly keeps the queue out of the proxy, where it would only add
latency to requests that are going to time out anyway.

`ReverseProxy`, in `Rewrite` mode, then drops hop-by-hop headers and any
`Forwarded` or `X-Forwarded-*` the client sent. steer sets its own (see
[Security](#security)), keeps the Host header and forwards the request ID.
Finally the custom `RoundTripper` picks an endpoint, sends the request,
classifies what came back, feeds that to outlier detection and the latency
estimate, and decides whether another endpoint should get a try.

Using `httputil.ReverseProxy` with a custom `RoundTripper`, rather than a
hand-written proxy, keeps the parts that are easy to get subtly wrong:
hop-by-hop header removal including the ones named in `Connection`, `Te:
trailers`, 1xx responses, trailers, immediate flushing for streams with no
length and for `text/event-stream`, and `101 Switching Protocols` handling.

## What a runtime holds

A runtime is one loaded configuration: the router, and per cluster the
endpoints, the load balancer, the health checker, the outlier detector, the
retry budget, the concurrency counter and the upstream connection pool. It is
immutable once built and published through an atomic pointer. A request loads
the pointer once and uses that runtime to the end, so a reload never changes
the rules under a request.

An endpoint is `(address, weight, priority)` from the configuration plus a
pointer to its state:

| State | Written by | Read by |
|---|---|---|
| in-flight count, latency estimate | every attempt | least-loaded picks |
| active health verdict and probe counters | health checker | every pick |
| ejection deadline, multiplier, failure counters | outlier detector | every pick |

Fields read on every pick are atomics; the rest sit behind a per-endpoint
mutex. Availability is `healthy && now < ejected_until`, computed from the
clock rather than flipped by a timer, so an ejection ends exactly on time
without any background work.

## Reloading without losing state

On SIGHUP or `POST /reload`, steer reads the file and validates all of it,
reporting every error with its JSON path, then builds a new runtime. An
endpoint whose cluster and address are unchanged gets the old state pointer,
so its health, ejection history, latency estimate and in-flight count carry
over; new addresses start healthy and are probed straight away. The
concurrency counter carries over by cluster name, so requests still running on
the old runtime release the counter they took. The connection pool is reused
if the connect timeout didn't change, and the retry budget if its parameters
didn't: reloading in the middle of an incident shouldn't hand out a fresh
retry allowance. Turning health checks off marks the endpoints healthy, since
nothing would ever probe them back up, and turning outlier detection off
clears ejections.

Then the runtime pointer is swapped and the old health checkers and outlier
loops are cancelled. A probe that was in flight when its loop was cancelled
is ignored rather than counted. If anything fails, the old runtime keeps
serving and the error is returned. `listen` and `admin_listen` cannot change
on reload; that needs a restart.

## How each decision is made

### Routing

Envoy's virtual hosts: the Host header (lower-cased, port and trailing dot
removed) chooses one virtual host, an exact name beating the longest
`*.suffix` beating `*`; then the longest `path_prefix` within that host wins.
There is no fall-through from a specific host to `*`. Adding a route for one
host therefore never changes where another host's requests go.

The request path is cleaned (`.` and `..` resolved, repeated slashes
collapsed, a trailing slash kept) and the cleaned path is both matched and
forwarded. Matching a cleaned path but forwarding the raw one would let
`/public/../admin` match the `/public` route while the upstream serves
`/admin`. Prefixes are validated to be clean themselves.

Routes are grouped by host and sorted by prefix length, so a match is a map
lookup (or a scan of the wildcard suffixes, longest first) and a scan of that
host's prefixes. With 100 prefixes it takes 186 ns.

### Priority levels and the panic threshold

Endpoints have a priority from 0 (preferred) to 7. Following Envoy, each
level's health is `min(100, 1.4 x 100 x healthy / total)` in integer percent,
the factor 1.4 being Envoy's default overprovisioning. Levels take load in
order, as much as their health allows; if the levels together are below 100,
the loads are scaled up so they add to 100:

| P0 healthy | P1 healthy | P0 load | P1 load |
|---|---|---|---|
| 100% | 100% | 100% | 0% |
| 72% | 100% | 100% | 0% |
| 50% | 100% | 70% | 30% |
| 25% | 100% | 35% | 65% |
| 25% | 25% | 50% | 50% |
| 0% | 100% | 0% | 100% |

The factor means a level that has lost a few hosts keeps all its traffic (the
remaining hosts are assumed to have headroom), and failover is gradual rather
than all at once.

If the normalised health of the whole cluster is below
`panic_threshold_percent` (default 50), steer stops trusting health and
spreads requests over every endpoint. When most hosts look down, the cause is
often the health signal itself (a bad deploy of the health endpoint, a
network partition between steer and the hosts); and if it is real, sending
all traffic to the last two hosts would take them down too. In panic steer
picks from the union of all endpoints, so each level gets traffic in
proportion to its size.

The level is chosen at random in proportion to the loads, except for Maglev,
where it is chosen from the request's key so a key stays on one endpoint
while the health picture is stable.

### Load-balancing policies

`round_robin` is nginx's smooth weighted round robin. Each pick adds every
candidate's weight to its running score, picks the highest and
subtracts the total from the winner. Over any window of `sum(weights)` picks
each endpoint is chosen exactly `weight` times, and the choices interleave
(weights 5, 1, 1 give `a a b a c a a`, not five `a`s in a row). It runs over
whichever endpoints are available at the moment, so it needs no rebuild when
health changes. Cost: O(n) under a per-cluster mutex.

`least_loaded` is power of two choices: pick two candidates at
random and keep the one with the lower `latency x (in-flight + 1)`. Mitzenmacher
showed that two random choices bring the maximum load from
`O(log n / log log n)` down to `O(log log n)`; it also avoids the herding of
"always the least loaded", where every proxy with the same stale view sends
the next burst to the same host. The latency term is Finagle's and Linkerd's
peak EWMA: a slower sample replaces the estimate at once, faster samples
pull it down with a 10-second time constant, so the estimate reacts to a
slowdown in one request but needs sustained good behaviour to recover.

Between samples the estimate decays towards zero, as Linkerd's does. Without
that, an endpoint whose estimate is high is never picked again and so never
re-measured: one slow response would exile it for good. With it, a penalised
endpoint is tried again after about a minute at light load, and sooner under
load, as its peers' in-flight counts raise their cost.

A failed attempt is recorded as at least 1 second of latency. Without that
penalty, an endpoint that refuses connections or returns 503 instantly looks
like the fastest endpoint, and least-loaded sends it more traffic: a
regression test pins this.

`maglev` is the consistent hashing from Google's Maglev paper (Eisenbud et
al., NSDI). Each endpoint derives a permutation of the 65,537 table slots from
two hashes of its address (`offset + j x skip mod M`). Endpoints take turns claiming their next
free slot until the table is full; a key goes to `table[hash(key) mod M]`.
Weights are honoured by giving an endpoint a turn in round `r` only when
`floor(r x w / max_w)` grows, so it claims slots in proportion to its weight.

Removing one endpoint out of ten moves its 10% of slots and only 0.2% of the
others' (measured in the tests); a mod-N hash would move about 90% of keys.
Because tables depend only on addresses and weights, every steer instance with
the same configuration maps a key to the same endpoint.

The table is built for the set of endpoints currently available in the chosen
level, so it changes when health changes. Building takes about 1.1 ms for 100
endpoints; up to eight tables are cached, keyed by the set, so flapping
between two health pictures does not rebuild. A retry walks forward from the
key's slot to the next slot owned by an endpoint not yet tried, which is
deterministic too. A request without the hash header gets a random endpoint.

Retries never repeat an endpoint. Every policy takes the set already tried
by this request and skips it. If the chosen level has no untried endpoint, the
other levels are tried in priority order. When nothing untried is available,
the request stops retrying and returns the last answer.

### Active health checks

One loop per endpoint: probe at once, then every `interval` plus a uniform
random `jitter`, so many proxies' probes do not arrive in lockstep. A probe is
`GET path` on a fresh connection (keep-alive off, so it proves a new
connection can be made, which is what a dead process breaks), passing on any
2xx within `timeout`; redirects are not followed and count as failures. The
verdict flips after `unhealthy_threshold` failures in a row, or
`healthy_threshold` passes in a row, so a single lost probe does not flap an
endpoint.

New endpoints start healthy. Envoy instead waits for the first round of checks
before a cluster takes traffic; steer accepts a few failed attempts on a dead
new endpoint, which the retry policy absorbs, in exchange for not delaying
startup and reload by a probe timeout.

### Outlier detection

Active checks only see what the health endpoint reports. A host whose
`/healthz` is green while half its real requests fail is caught only by
watching the real requests. Following Envoy:

Connect errors, resets, timeouts and 502/503/504 count as gateway failures.
After `consecutive_gateway_failures` of them in a row (default 5) the endpoint
is ejected immediately; any other response resets the run.

Every `interval`, the success-rate detector looks at endpoints that served at
least `success_rate_request_volume` attempts in that interval. If at least
`success_rate_minimum_hosts` qualify, any whose success rate is below
`mean - success_rate_stdev_factor x stdev` is ejected, worst first. When all
endpoints fail equally the standard deviation is zero and nothing is ejected,
which is what we want: a shared dependency failing is not an outlier.

An ejection lasts `base_ejection_time x times_ejected`, capped at
`max_ejection_time`. The multiplier drops by one for every
`base_ejection_time` the endpoint then stays un-ejected, so a host that
recovered starts from the base again while a flapping one is kept out longer
each time. No more than `max_ejection_percent` of the cluster can be ejected at
once (one endpoint is always allowed if the percent is above zero). Without
that cap, a fault in steer itself or in the network could eject every host.

An ejected endpoint is skipped by the load balancer exactly like an unhealthy
one, so priority failover and the panic threshold apply to both.

### Retries

| Outcome | Retried for |
|---|---|
| Connect failure | Any method: the request never reached the upstream. |
| Reset or per-try timeout | Idempotent methods (GET, HEAD, OPTIONS, TRACE, PUT, DELETE) and requests with an `Idempotency-Key` header. |
| A status in `retry_on` (default 502, 503, 504) | The same. |
| Anything else | Never. |

A request is retried only if its body can be replayed: bodies up to
`max_body_bytes` (default 64 KiB) are buffered; a larger body streams to the
first endpoint and is not retried. The size check happens before buffering
(`Content-Length`) and while buffering (chunked bodies), so memory per request
is bounded by the limit.

Waits between attempts are exponential backoff with full jitter: uniform
in `[0, min(backoff_max, backoff_base x 2^attempt))`. Of the variants
compared in Brooker's "Exponential Backoff And Jitter", full jitter spreads
synchronised clients out best.

The per-try timeout covers the time to response headers only. Once headers
arrive the timer is stopped and the body may stream for as long as the
cluster `timeout` allows, so a long download or event stream is not cut at
250 ms. The cluster `timeout` bounds the whole request, retries included.

The retry budget (Finagle's `RetryBudget`) counts requests and retries in
one-second buckets over a sliding window (default 10 s) and allows a retry
while

    retries in window < min_per_second x window + ratio x requests in window

With the defaults (ratio 0.2, 10 per second) retries can add at most 20% to
upstream load plus a small floor for quiet services. Without a budget, a
cluster-wide failure turns every request into `1 + max_retries` requests
exactly when the upstreams can least afford it. A per-request retry limit
alone does not prevent that; only a budget shared by all requests does.

### Draining

On SIGTERM, `/readyz` starts returning 503 and keep-alive is switched off
(idle connections are closed and responses carry `Connection: close`). The
outer load balancer sees the failing readiness check and stops sending new
connections, while requests that do arrive are still served. After
`STEER_DRAIN_PERIOD` the listeners close and in-flight requests have
`STEER_SHUTDOWN_TIMEOUT` to finish; connections still open then are cut.

## TLS termination

With a `tls` block, the proxy listener terminates TLS 1.2 and 1.3
(`internal/tlsedge`). The certificates and policy are loaded into an
immutable snapshot published through an atomic pointer, the same pattern as
the runtime. Every handshake reads the snapshot once, through
`tls.Config.GetConfigForClient`, so a reload changes what new handshakes get
and nothing else: connections that already finished their handshake keep
the certificate and keys they negotiated and carry on serving. A reload
re-reads every certificate and key file; if one is missing, unreadable or
does not match its key, the whole reload is refused and the old certificates
keep serving. TLS cannot be switched on or off by a reload, since that
changes what the listener is.

Certificates are chosen by SNI. Each certificate is selected for the DNS
names in its SANs, or for `server_names` when given. An exact name beats a
wildcard, and `*.example.com` covers exactly one label, `a.example.com` but
neither `a.b.example.com` nor `example.com`, the rule RFC 6125 sets for
clients; the server should not offer a certificate the client will reject.
Names are compared lower-case with one trailing dot dropped. When several
certificates carry a name, say an ECDSA and an RSA one, the first that the
client's signature algorithms and cipher suites can use is chosen, as with
NGINX's and HAProxy's dual certificates. A name that matches nothing, or no
SNI at all, gets the first certificate; with `strict_sni` the handshake is
refused instead, which keeps scanners that connect by IP address from
learning which names the listener serves. Selection is a map lookup: 42 ns
and no allocations with 400 names loaded.

The `modern` cipher policy (the default) allows only ECDHE key exchange with
AES-GCM or ChaCha20-Poly1305 in TLS 1.2; `compatible` adds the ECDHE AES-CBC
suites for older clients. `cipher_suites` names the suites explicitly
instead, and only suites that Go's TLS package considers secure are
accepted. TLS 1.3 suites are not configurable. ALPN offers only `http/1.1`.

Handshakes run in their own goroutines, not in the accept loop and not in
the HTTP server's connection goroutine, so a client that connects and then
stalls holds up no one. Each handshake has `handshake_timeout` as a hard
deadline on the socket, cleared once it succeeds, after which the HTTP
server's header and idle timeouts take over. At most
`max_concurrent_handshakes` run at once; beyond that the accept loop waits
and new connections queue in the kernel's listen backlog, not in steer's
memory. A full ECDSA P-256 handshake over loopback costs about 1.3 ms of CPU
for client and server together.

## What clients see when things break

| Failure | What steer does | What the client sees |
|---|---|---|
| Upstream process dies | Connect errors are retried on another endpoint; health checks remove it after `unhealthy_threshold` probes. | Success; a little latency on the requests that hit it. |
| Upstream returns errors, health check green | 502/503/504 retried (safe requests); outlier detection ejects it. | Success for retried requests; non-idempotent requests that hit it before ejection get its error. |
| Upstream accepts but never answers | Per-try timeout, then retry; health checks (which time out too) remove it. | Success, with latency up to the per-try timeout until it is removed. |
| Upstream slow but correct | Least-loaded shifts traffic away by latency x load. | Mostly fast responses. |
| Whole priority level down | Traffic moves to the next level by Envoy's priority load. | Success if the backup level has capacity. |
| Most of the cluster looks down | Panic: all endpoints get traffic regardless of health. | Partial success rather than a total outage caused by overload or a bad health signal. |
| Everything is down | Retries stop at the budget; nothing is amplified. | 502 / 503 / 504 with a JSON error and request ID. |
| Overload at steer | The per-cluster concurrency limit sheds excess at once. | Immediate 503 with `X-Steer-Overflow`. |
| Client goes away | The attempt is cancelled and not blamed on the endpoint. | (Logged as 499.) |
| Bad configuration pushed | Rejected with every error listed; the old configuration keeps serving. | Nothing. |
| steer restart | Drain: readiness fails first, in-flight requests finish. | Nothing, if the outer load balancer honours readiness. |

## Limits

| Limit | Value | Why |
|---|---|---|
| Configuration file | 1 MiB | Read before parsing. |
| Validation errors reported | 20 (then a count) | Keeps the reload response readable. |
| Routes / clusters / endpoints per cluster | 1,024 / 256 / 1,024 | Bounds memory, metric series and Maglev table quality (65,537 slots is at least 64 per endpoint). |
| Priorities / weight | 0-7 / 1-1,000 | Envoy uses at most a handful of levels. |
| Retries | 0-5 per request | Plus the budget. |
| Replay buffer | 64 KiB default, 16 MiB max per request | Bounds memory per request. |
| Trusted proxies / forwarded hops kept | 64 / 32 | Bounds header growth. |
| Request header size | 64 KiB | `MaxHeaderBytes`. |
| Header read timeout / idle timeout | 10 s / 120 s | Slow-header clients cannot hold connections. |
| Request timeout | 15 s default per cluster, 1 h max, or none | Bodies stream, so there is no server write timeout. |
| Concurrency per cluster | 1,024 default | Envoy's default `max_requests`. |
| Upstream response headers | 64 KiB | Transport limit. |
| Request ID | 128 characters of `A-Z a-z 0-9 . _ : -` | Safe to log and forward. |
| Admin token | at least 16 characters | |
| TLS certificates / names per certificate | 256 / 100 | Bounds memory and reload time. |
| TLS handshake timeout | 5 s default, 100 ms-1 m | A stalled client holds a goroutine and a socket only this long. |
| Concurrent TLS handshakes | 1,024 default, up to 100,000 | Bounds handshake CPU and memory under a connection flood. |

Request bodies are streamed, not limited: a proxy in front of an upload
service must not cap uploads. Put a limit in the upstream or a dedicated
layer if one is needed.

Metric series are created per cluster and per endpoint address and are never
deleted, so the series count is bounded by the addresses configured over the
life of the process (at most a few thousand series for the limits above).

## Configuration

All durations are strings in Go syntax (`"250ms"`, `"5s"`). Unknown fields
are errors. Missing fields take the default shown.

| Field | Default | Bounds |
|---|---|---|
| `listen` | required | `host:port`, host an IP, `localhost` or empty |
| `admin_listen` | `127.0.0.1:9901` | as above; non-loopback needs `STEER_ADMIN_TOKEN` |
| `trusted_proxies` | none | up to 64 IPs or CIDRs |
| `tls.certificates[]` | | `cert_file` and `key_file` (PEM; relative paths are relative to the configuration file), optional `server_names` (names or `*.suffix`) |
| `tls.min_version` | `1.2` | `1.2` or `1.3` |
| `tls.cipher_policy` | `modern` | `modern` or `compatible`; not with `cipher_suites` or TLS 1.3 only |
| `tls.cipher_suites` | | Go names of secure TLS 1.2 suites, in preference order |
| `tls.handshake_timeout` | 5s | 100ms-1m |
| `tls.max_concurrent_handshakes` | 1024 | 1-100,000 |
| `tls.strict_sni` | false | refuse handshakes whose name matches no certificate |
| `routes[].host` | `*` | `*`, `name`, `*.suffix` |
| `routes[].path_prefix` | `/` | clean absolute path, up to 1,024 characters |
| `routes[].cluster` | required | an existing cluster |
| `clusters[].name` | required | 1-64 of `A-Z a-z 0-9 . _ -` |
| `clusters[].policy` | `round_robin` | `round_robin`, `least_loaded`, `maglev` |
| `clusters[].hash_header` | | required for and only for `maglev` |
| `clusters[].endpoints[]` | | `address` (`host:port`), `weight` 1 (1-1000), `priority` 0 (0-7) |
| `clusters[].connect_timeout` | 1s | 1ms-1m |
| `clusters[].timeout` | 15s | 0 (none) to 1h |
| `clusters[].max_requests` | 1024 | 1-1,000,000 |
| `clusters[].panic_threshold_percent` | 50 | 0 (off) to 100 |
| `health_check.path` | required | absolute path |
| `health_check.host` | endpoint address | Host header for probes |
| `health_check.interval` / `jitter` / `timeout` | 5s / 0 / min(1s, interval) | jitter and timeout at most the interval |
| `health_check.healthy_threshold` / `unhealthy_threshold` | 2 / 2 | 1-100 |
| `outlier_detection.consecutive_gateway_failures` | 5 | 0 (off) to 1,000 |
| `outlier_detection.interval` | 10s | 10ms-1h |
| `outlier_detection.base_ejection_time` / `max_ejection_time` | 30s / 300s | max at least base |
| `outlier_detection.max_ejection_percent` | 10 | 0-100 |
| `outlier_detection.success_rate_minimum_hosts` | 5 | 0 (off) to 1,024 |
| `outlier_detection.success_rate_request_volume` | 100 | 1-1,000,000 |
| `outlier_detection.success_rate_stdev_factor` | 1.9 | (0, 10] |
| `retry.max_retries` | 2 | 0-5 |
| `retry.retry_on` | `[502, 503, 504]` | 5xx statuses |
| `retry.per_try_timeout` | none | at most the cluster timeout |
| `retry.backoff_base` / `backoff_max` | 25ms / 250ms | max at least base |
| `retry.max_body_bytes` | 65536 | 0-16 MiB |
| `retry.budget.ratio` / `min_per_second` / `window` | 0.2 / 10 / 10s | 0-1 / 0-100,000 / whole seconds, 1s-1m |

Omitting `health_check`, `outlier_detection` or `retry` turns that feature off
for the cluster. Omitting `tls` serves plain HTTP.

## Security

The admin API binds to loopback by default, and steer refuses to start or
reload with a non-loopback `admin_listen` unless `STEER_ADMIN_TOKEN` is set.
With a token, everything except `/healthz` and `/readyz` requires it. Tokens
are compared as SHA-256 digests with `subtle.ConstantTimeCompare`, so the
timing reveals neither length nor prefix, and the token comes from the
environment and is never logged. `POST /reload` only re-reads the configured
file; configuration never arrives over the network.

Client-supplied `X-Forwarded-*` and `Forwarded` headers are dropped. The
inbound `X-Forwarded-For` chain is kept only when the connection comes from a
`trusted_proxies` address, and even then entries that aren't IP addresses are
dropped and at most 32 hops survive; `X-Forwarded-Proto` must be `http` or
`https` and `X-Forwarded-Host` must be well formed. The client IP in the access
log is the rightmost address in the chain that isn't a trusted proxy, because
anything to the left of it could have been written by anyone.

Inbound request IDs are used only if they match `[A-Za-z0-9._:-]{1,128}`, so
they can't inject anything into logs or headers; otherwise steer generates 96
random bits. Paths are cleaned before routing and forwarding. The upstream
transport ignores proxy settings in the environment, adds no compression and
caps response header size. CONNECT is refused, since steer is a reverse proxy
and not a forward one. Errors sent to clients are generic and carry the
request ID; which endpoints were tried and how they failed goes to the access
log. The container runs as distroless `nonroot`, with no shell.

## Running more than one instance

steer is one process reading a static file of endpoints. Running a fleet of
them at the edge would change a few things.

The file would become a feed: Envoy's EDS, DNS SRV, or a registry's watch API.
The reload path already does the hard part (build a runtime, carry state over
by address, swap), so this is mostly plumbing.

The fleet would sit behind an L4 balancer or anycast. That layer is what Maglev
was designed for, and because steer's tables depend only on configuration, its
L7 hashing already agrees across instances.

Outlier detection gets weaker as the fleet grows. Each instance sees only its
share of an upstream's traffic and needs longer to gather evidence. Instances
could publish per-endpoint success rates to a control plane that aggregates
them, while keeping the local fast path for the obvious cases. The same
in-flight counts and latency estimates are what a global balancer needs to
weight endpoints or regions, and adding a locality to endpoints would let
steer prefer its own zone and spill over with the same health-weighted math
the priority levels use.

Finally, HTTP/2 or HTTP/3 towards clients, with pooled
HTTP/2 connections to upstreams. Retries, budgets and outlier detection work
per request, so none of that changes.

## Sources

- Envoy proxy documentation: load balancing, priority levels, panic threshold,
  outlier detection, circuit breaking, retry policy.
- D. Eisenbud et al. "Maglev: A Fast and Reliable Software Network Load
  Balancer." USENIX NSDI.
- Finagle documentation: retry budgets, `RetryBudget`, and the
  power-of-two-choices balancers with peak EWMA; Linkerd's P2C with peak EWMA.
- M. Mitzenmacher. "The Power of Two Choices in Randomized Load Balancing."
  IEEE Transactions on Parallel and Distributed Systems.
- nginx: the smooth weighted round-robin algorithm (`ngx_http_upstream_round_robin`).
- M. Brooker. "Exponential Backoff And Jitter."
- G. Tene: coordinated omission, the reason `cmd/loadgen` is open-loop.
