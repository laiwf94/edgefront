# pacer: roadmap, November 2026 to October 2027

## Vision

By October 2027 pacer is a self-hosted traffic limiter with the rate limiting feature set of Cloudflare, Akamai, Fastly, Kong Enterprise, NGINX Plus, HAProxy Enterprise and Envoy's rate limit service, packed into one static binary under 5 MB with no external store. It limits TCP, UDP, HTTP/1.1, HTTP/2, gRPC and WebSocket traffic by address, prefix, geography, TLS fingerprint, API key, JWT claim, path, header or response status, terminates TLS, shares limits across a cluster through built-in gossip instead of Redis, and serves decisions to other proxies over Envoy's rate limit API, the Redis protocol and HTTP. Operators get a web console with OIDC sign-in, roles and a tamper-evident audit log, versioned config with rollback, shadow mode, security analytics, OpenTelemetry export, a Kubernetes controller for Gateway API policies, and libpacer bindings and modules for NGINX and proxy-wasm. It wins where a small C project can: an image under 5 MB, start-up under 10 ms, the lowest memory per tracked client and per connection, kernel-level early drop with XDP, and decision throughput and p99 above NGINX, HAProxy and Envoy, all published from a reproducible benchmark re-measured every quarter and enforced as a CI regression gate.

## Benchmarked against

Cloudflare rate limiting rules, Akamai App and API Protector rate controls, Fastly Edge Rate Limiting, Kong Gateway Enterprise Rate Limiting Advanced, NGINX Plus limit_req with zone_sync, HAProxy Enterprise stick tables with the Global Profiling Engine, and Envoy with envoyproxy/ratelimit. The self-hosted ones (NGINX, HAProxy, Envoy with its rate limit service and Redis, Kong) are benchmarked on the same Linux host from week 13 with explicit targets: decisions per second and p99 at or above the best of them, resident memory per 1 million tracked clients at least 50% below, image under 5 MB and cold start under 10 ms. The numbers are re-measured in weeks 13, 31, 43 and 52, and a CI gate fails a change that regresses them by more than 5%.

Weeks 1 to 12 deliver the twelve v1 monthly themes, one per week; weeks 13 to 52 close the ranked gaps, and each week names the matrix rows it closes.

## 2026-11 · Limiter correctness, address policy, real addresses, observability and throughput

Due 2026-11-30. The first five v1 themes, one per week: reference-model proofs and quotas, CIDR policy and classes, PROXY protocol, Prometheus metrics with per-client lookups, and measured throughput gains.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 1 | 2026-11-02 to 2026-11-08 | Limiter correctness, long-run safety, quotas and config lints | Proves the library against reference models, removes the 4.5-year clock limit, adds daily quotas and fractional rates, and makes config mistakes visible before they reach production. Comparison matrix rows: A1, A2. |
| 2 | 2026-11-09 to 2026-11-15 | Address policy: CIDR lists, limit classes and prefix grouping | Decides who may connect at all and gives known address ranges their own limits, all reloadable in place, as every competitor allows. Comparison matrix rows: B1, B7. |
| 3 | 2026-11-16 to 2026-11-22 | Real client addresses with the PROXY protocol and upstream socket options | Carries and trusts the real client address through load balancers and tunes upstream sockets, so limits apply to clients and not to the balancer in front. Comparison matrix rows: B6. |
| 4 | 2026-11-23 to 2026-11-29 | Prometheus metrics, histograms, per-client lookup and top clients | An operator can see who is being limited and why: Prometheus metrics, latency histograms, live per-client state and the noisiest clients, all from the admin socket. Comparison matrix rows: E5, E6. |
| 5 | 2026-11-30 to 2026-12-06 | Throughput: splice, buffer pools, fewer epoll calls, batched accept and table scaling | Measured gains in bulk throughput, connection setup rate, round-trip latency and table scaling, each with published numbers. Comparison matrix rows: F3. |

## 2026-12 · Zero-downtime upgrades, upstream resilience, adaptive protection and libpacer

Due 2026-12-31. Socket and state handoff on upgrade, circuit breakers and health probes, bans and overload tightening, and an installable library with a stable ABI.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 6 | 2026-12-07 to 2026-12-13 | Zero-downtime upgrades with socket and limiter state handoff | Binary upgrades keep listening sockets and client limits, so an attacker cannot wait for an upgrade to reset buckets, plus systemd socket activation and readiness without libsystemd. Comparison matrix rows: D4. |
| 7 | 2026-12-14 to 2026-12-20 | Upstream resilience: caps, circuit breaker, health probes and retries | Protects a struggling upstream with a connection cap and wait queue, a circuit breaker, active probes and bounded retries, proven by a chaos suite. Comparison matrix rows: A5. |
| 8 | 2026-12-21 to 2026-12-27 | Adaptive protection: listener-wide rates, bans, overload mode and per-connection quotas | Stops botnets that stay under per-address limits, refuses repeat offenders for escalating periods, tightens limits under pressure, and caps long-lived connections, matching the penalty boxes of Cloudflare and Fastly. Comparison matrix rows: A8. |
| 9 | 2026-12-28 to 2027-01-03 | libpacer as a product: stable ABI, multi-limiter decisions, keyed quotas and hot keys | An installable library with a stable ABI, all-or-nothing decisions across limiters, keyed window quotas and a hot-key path that scales with cores. Comparison matrix rows: A4, G1. |

## 2027-01 · Portability, hardening, pacer 1.0 and the benchmark against the leaders

Due 2027-01-31. kqueue and Unix sockets, seccomp and privilege drop, the 1.0 release, then a reproducible benchmark against NGINX, HAProxy, Envoy and Kong that becomes a CI gate.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 10 | 2027-01-04 to 2027-01-10 | Portability and soak: kqueue, Unix sockets, multi-arch images and a 24-hour soak | pacerd runs on macOS through a kqueue backend, supports Unix socket endpoints, ships multi-arch images and proves flat resources over a 24-hour soak. Comparison matrix rows: F1. |
| 11 | 2027-01-11 to 2027-01-17 | Security hardening: seccomp, privilege drop, admin authorization and continuous fuzzing | pacerd confines itself with seccomp, drops privileges after bind, authorizes admin socket peers by credentials, and fuzzes every external input continuously against a written threat model. Comparison matrix rows: F8. |
| 12 | 2027-01-18 to 2027-01-24 | pacer 1.0: config diffs, man pages, reproducible artifacts and a compatibility promise | Config diffs before reload, man pages checked against the code, reproducible release artifacts and a 1.0 compatibility promise backed by tests. Comparison matrix rows: E8. |
| 13 | 2027-01-25 to 2027-01-31 | Reproducible benchmark against NGINX, HAProxy, Envoy and Kong with a CI regression gate | Claims about being lighter and faster than the leaders need numbers anyone can rerun. This sets the targets the rest of the year is measured against and makes regressions fail the build. Comparison matrix rows: F4, F5. |

## 2027-02 · HTTP-aware limiting and TLS

Due 2027-02-28. An HTTP/1.1 proxy mode, request rules keyed on any HTTP field and response status, TLS termination with fingerprints, and geo and ASN keys.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 14 | 2027-02-01 to 2027-02-07 | HTTP/1.1 proxy mode with request smuggling defenses | Every leader in the matrix limits HTTP requests, not just connections. An incremental, fuzzed HTTP/1.1 parser turns pacerd into a request-aware proxy with keep-alive and pipelining, refusing ambiguous messages that enable smuggling. Comparison matrix rows: C2. |
| 15 | 2027-02-08 to 2027-02-14 | HTTP rule engine: keys from any request field and counting on responses | Cloudflare, Kong, NGINX and Envoy limit by path, header, cookie or query and Cloudflare and HAProxy can count only failed responses. A compiled rule engine brings that to pacer, with standard RateLimit headers and custom 429 responses. Comparison matrix rows: B2, B8, C8. |
| 16 | 2027-02-15 to 2027-02-21 | TLS termination with mbedTLS and kernel TLS offload | All competitors terminate TLS. pacer adds it with mbedTLS (Apache-2.0, small, static-friendly and supporting TLS 1.3), the one new runtime dependency of the year, justified because writing TLS is not a reasonable option. kTLS moves record encryption into the kernel after the handshake. Comparison matrix rows: C5. |
| 17 | 2027-02-22 to 2027-02-28 | TLS fingerprints, SNI routing and geo and ASN keys | Cloudflare and Akamai key limits on JA3 and JA4 fingerprints and on country and ASN to catch bots that rotate addresses. pacer parses ClientHello itself, so this works with or without termination, and reads MaxMind DB files the operator supplies. Comparison matrix rows: B4, B5. |

## 2027-03 · HTTP/2, gRPC, WebSocket, UDP, shadow mode and identity

Due 2027-03-31. Modern protocols with their abuse defenses, a shadow mode with policy replay, and consumers, API keys and JWT identities with groups.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 18 | 2027-03-01 to 2027-03-07 | HTTP/2 proxying with HPACK and per-stream limits | HTTP/2 is the default between browsers and edges and between services. pacer parses and proxies it, applying request rules per stream. Comparison matrix rows: C3. |
| 19 | 2027-03-08 to 2027-03-14 | HTTP/2 abuse defenses and gRPC-aware limits | HTTP/2 brought new floods: rapid reset, CONTINUATION, SETTINGS and PING floods. Every leader patched them; pacer defends against all of them and limits gRPC per method with proper status codes. Comparison matrix rows: C4, C3. |
| 20 | 2027-03-15 to 2027-03-21 | WebSocket and UDP proxying with per-source limits | WebSocket connections and UDP services such as DNS, game servers and syslog need limits too. NGINX and Envoy proxy UDP; pacer adds both with message and packet rates. Comparison matrix rows: C6, C7. |
| 21 | 2027-03-22 to 2027-03-28 | Shadow mode and policy replay | Envoy, NGINX, Fastly and Cloudflare let operators log what a rule would do before enforcing it. pacer adds per-rule and global shadow mode and a replay tool that runs recorded traffic against a new policy. Comparison matrix rows: A9. |
| 22 | 2027-03-29 to 2027-04-04 | Consumers, API keys and JWT identities with groups | Kong limits per consumer and consumer group, and Envoy and NGINX Plus verify JWTs. pacer adds identities: API key stores, JWT verification and groups with their own limits. Comparison matrix rows: B3, D5. |

## 2027-04 · Rate limiting as a service

Due 2027-04-30. A pipelined decision protocol, Envoy's rate limit API, a Redis-protocol and HTTP front end, and client SDKs in four languages.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 23 | 2027-04-05 to 2027-04-11 | Decision service with a pipelined binary protocol | Many teams want limits as a service called from their own code. pacerd gains a service mode that answers decisions over TCP and Unix sockets with batching and pipelining at millions of decisions per second. Comparison matrix rows: D3. |
| 24 | 2027-04-12 to 2027-04-18 | Envoy rate limit service API compatibility | Envoy, Envoy Gateway and Istio call an external rate limit service over gRPC. Speaking that API with descriptor configs compatible with envoyproxy/ratelimit lets pacer replace it without Redis, including replaces and shadow_mode. Comparison matrix rows: D3, D2. |
| 25 | 2027-04-19 to 2027-04-25 | Redis-protocol and HTTP JSON decision front ends | redis-cell's CL.THROTTLE made rate limiting callable from any language with a Redis client. pacer serves the same command over RESP, plus an HTTP JSON API, so existing code moves without new libraries. Comparison matrix rows: D3. |
| 26 | 2027-04-26 to 2027-05-02 | Client SDKs for Go, Python, Node.js and Java | Native SDKs for the decision protocol, each standard library only, make pacer usable from the languages most services are written in. Comparison matrix rows: D3. |

## 2027-05 · Cluster-wide limits without Redis

Due 2027-05-31. Gossip membership, key ownership, global budgets, state replication with failover, and a multi-node fault harness.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 27 | 2027-05-03 to 2027-05-09 | Gossip cluster membership with encrypted UDP | NGINX Plus uses zone_sync, HAProxy Enterprise a central aggregation server and Envoy and Kong need Redis. pacer builds cluster membership into the binary with SWIM gossip, authenticated and encrypted, so a cluster needs no extra service. Comparison matrix rows: D1, D2. |
| 28 | 2027-05-10 to 2027-05-16 | Consistent-hash key ownership with forwarding and local fallback | For exact global limits, each key has an owner. Rendezvous hashing assigns owners, non-owners forward decisions, and a timeout falls back to a local share so a slow owner never stalls traffic. Comparison matrix rows: D1. |
| 29 | 2027-05-17 to 2027-05-23 | Global limits with budget leasing | Forwarding every decision costs a round trip. Budget leasing gives each node a slice of a key's tokens, refreshed by need, for near-exact global limits at local speed, compared with Envoy plus Redis on accuracy and latency. Comparison matrix rows: D1, D2. |
| 30 | 2027-05-24 to 2027-05-30 | State replication and failover for high availability | Limits must survive node loss. Owners replicate state to backups so a failed node's keys continue with their counts, and quotas never reset on failover. Comparison matrix rows: D4, D1. |
| 31 | 2027-05-31 to 2027-06-06 | Multi-node fault harness and the second quarterly benchmark | Distributed limits fail in partitions, packet loss and clock jumps. A harness runs five real pacerd processes under faults and checks limit accuracy, and the quarterly comparison is re-measured. Comparison matrix rows: D1, F4. |

## 2027-06 · Upstream traffic management and analytics

Due 2027-06-30. Upstream pools and load balancing, adaptive concurrency with priorities, outlier ejection with retry budgets, and security analytics.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 32 | 2027-06-07 to 2027-06-13 | Upstream pools with load balancing and service discovery | NGINX Plus and HAProxy balance across upstream pools. pacer adds pools with round robin, least connections, power of two choices and consistent hashing, fed by files or an HTTP discovery endpoint. Comparison matrix rows: A5. |
| 33 | 2027-06-14 to 2027-06-20 | Adaptive concurrency limits with priority classes | Envoy's adaptive concurrency filter finds the concurrency an upstream can take from latency. pacer implements gradient and Vegas-style limiters with priority classes so important traffic is served first under pressure. Comparison matrix rows: A7. |
| 34 | 2027-06-21 to 2027-06-27 | Outlier ejection, retry budgets and hedging | A single bad upstream host should not hurt everyone. Passive outlier ejection, retry budgets that cannot amplify outages, and optional hedged requests for idempotent calls. Comparison matrix rows: A5. |
| 35 | 2027-06-28 to 2027-07-04 | Security analytics: time series and top offenders | Cloudflare, Akamai and Fastly show who is being limited over time. pacer keeps a compact on-box time series of decisions by rule, key class and reason, with top offenders per window. Comparison matrix rows: E5. |

## 2027-07 · Admin plane: API, console, SSO, audit and telemetry

Due 2027-07-31. A versioned admin API, a web console, OIDC sign-in with roles and audit, and OpenTelemetry export with usage metering.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 36 | 2027-07-05 to 2027-07-11 | Versioned admin REST API with config history and rollback | Every commercial leader has an admin API with versioned config. pacer adds a REST API over HTTPS for listeners, rules, policies and bans, with atomic apply, history and rollback. Comparison matrix rows: E1, E8. |
| 37 | 2027-07-12 to 2027-07-18 | Web admin console | Cloudflare, Kong and NGINX Plus ship dashboards. pacer embeds a console in the binary: live traffic, rules, top offenders, bans and config history, built with plain HTML and JavaScript and no build toolchain. Comparison matrix rows: E2. |
| 38 | 2027-07-19 to 2027-07-25 | OIDC sign-in, roles and a tamper-evident audit log | Enterprise admin planes need single sign-on, roles and an audit trail. Operators sign in with any OIDC provider, groups map to roles, and every change is written to a hash-chained audit log. Comparison matrix rows: E3, E4. |
| 39 | 2027-07-26 to 2027-08-01 | OpenTelemetry export and per-tenant usage metering | Standard telemetry pipelines speak OTLP. pacer exports metrics, logs and traces over OTLP/HTTP and meters decisions per tenant and consumer for chargeback. Comparison matrix rows: E6, D5. |

## 2027-08 · Kernel-speed data path and programmable policy

Due 2027-08-31. io_uring, XDP early drop, a smaller memory footprint, load-aware worker steering and Wasm policy plugins.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 40 | 2027-08-02 to 2027-08-08 | io_uring data path | io_uring cuts system calls per proxied byte. pacer adds an io_uring backend with raw syscalls, no liburing, selectable at start-up, with measured gains over epoll and over NGINX and HAProxy. Comparison matrix rows: F3. |
| 41 | 2027-08-09 to 2027-08-15 | XDP early drop for banned and over-limit sources | Dropping floods before they reach the socket layer is how the big networks survive them. pacer loads a small XDP program with raw bpf syscalls that drops packets from banned prefixes at line rate, fed by the penalty box. Comparison matrix rows: F2. |
| 42 | 2027-08-16 to 2027-08-22 | Smaller memory footprint per client and per connection | Memory per tracked client and per connection is where pacer should beat everyone by a wide margin. Compact table entries, slab allocation and lazy buffers cut RSS, measured against the leaders. Comparison matrix rows: F5. |
| 43 | 2027-08-23 to 2027-08-29 | Load-aware worker steering and the third quarterly benchmark | SO_REUSEPORT spreads connections by hash regardless of load. A reuseport BPF program steers new connections to the least busy worker, and the quarterly comparison is re-measured. Comparison matrix rows: F3, F4. |
| 44 | 2027-08-30 to 2027-09-05 | Wasm policy plugins | Cloudflare Workers, Fastly Compute, Kong plugins and Envoy filters let users program policy. pacer runs small Wasm modules for custom keys and decisions, using the wasm3 interpreter (MIT, about 100 KB), justified because a Wasm runtime is not reasonable to write here. Comparison matrix rows: E10. |

## 2027-09 · Kubernetes and migration

Due 2027-09-30. Helm deployment, a Gateway API policy controller, Envoy Gateway and Istio integration, and importers for NGINX, HAProxy, Envoy and Kong configs.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 45 | 2027-09-06 to 2027-09-12 | Helm chart and Kubernetes deployment | Most deployments run on Kubernetes. A Helm chart deploys pacerd as a gateway, sidecar or cluster service with config from ConfigMaps, hot reload, probes and gossip discovery from the API. Comparison matrix rows: E9. |
| 46 | 2027-09-13 to 2027-09-19 | Kubernetes controller for Gateway API rate limit policies | Kong, NGINX and Envoy Gateway expose rate limits as Kubernetes policies. pacer-controller watches policy resources attached to Gateway API routes and programs pacerd through the admin API. Comparison matrix rows: E9. |
| 47 | 2027-09-20 to 2027-09-26 | Envoy Gateway and Istio integration | Service meshes and Envoy gateways are where many limits live. pacer acts as their rate limit service with tested manifests. Comparison matrix rows: E9, D3. |
| 48 | 2027-09-27 to 2027-10-03 | Config importers for NGINX, HAProxy, Envoy and Kong | Switching costs block adoption. Importers translate rate limiting configs from the four self-hosted leaders into pacer config, with a report of anything that cannot map. Comparison matrix rows: E7. |

## 2027-10 · Embedding everywhere, durable quotas and 2.0

Due 2027-10-31. Language bindings, an NGINX module and proxy-wasm filter, crash-safe persistent quotas, and pacer 2.0 with the final benchmark.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 49 | 2027-10-04 to 2027-10-10 | Language bindings for libpacer | Embedding the limiter directly is cheaper than a network call. Bindings for Python, Go and Rust wrap the stable ABI. Comparison matrix rows: G1. |
| 50 | 2027-10-11 to 2027-10-17 | NGINX module and proxy-wasm filter | Teams already running NGINX or Envoy can use pacer's limiter inside them: a dynamic NGINX module and a proxy-wasm filter built from libpacer. Comparison matrix rows: G2. |
| 51 | 2027-10-18 to 2027-10-24 | Crash-safe persistent quotas | Daily and monthly quotas must survive a crash, not only an upgrade. pacer snapshots table state and logs quota changes so a restart after SIGKILL restores counts. Comparison matrix rows: D4. |
| 52 | 2027-10-25 to 2027-10-31 | pacer 2.0 with the final benchmark | Close the year with a release that states what pacer guarantees, how it compares with every product in the matrix, and how to upgrade from 1.0. Comparison matrix rows: F4. |

## Next year

Gaps from the comparison matrix still open after October 2027:

- Bot management with ML scoring (matrix row F7): pacer ships fingerprint and behaviour signals but no trained model.
- A global anycast edge network for volumetric DDoS absorption (row F6): out of scope for a self-hosted binary; pacer's answer is XDP early drop on each host.
- HTTP/3 and QUIC limiting (row C3, partial): needs a QUIC stack on top of the mbedTLS integration.
- A managed hosted control plane (rows E2 and E3 as a service).
