# edgefront roadmap, November 2026 to October 2027

edgefront merges three projects into one Rust edge platform: cachet (Rust HTTP caching reverse proxy) becomes the edgefront repository and Cargo workspace, and steer (Go load balancer and edge proxy) and pacer (C rate-limiting proxy) join it with their full git history under legacy/steer and legacy/pacer, are ported to Rust, and are deleted once the Rust crates pass differential tests against them.

## Vision

By October 2027 edgefront is one Rust binary that runs a complete self-hosted application edge, built the way Cloudflare's Pingora is: a proxy (TLS with ACME, HTTP/1.1, HTTP/2, HTTP/3, gRPC, TCP and UDP, balancing, health, a rules engine, edge authentication, Wasm filters and edge functions, DNS steering, smart routing, BGP, origin tunnels, mesh mode and an API gateway), a cache (cachet's engine grown into a multi-disk, encrypted, tiered cache with global purge under 150 ms, image and video delivery and edge storage) and a limiter (pacer's lock-free algorithms as a library and as an Envoy RLS and Redis-protocol decision service, cluster limits without Redis, WAF, bot and DDoS defences and XDP early drop), all run by a Raft control plane with tenants, SSO, RBAC, audit, a console, an API, Kubernetes and Terraform. It targets parity on the rows that matter with Cloudflare's application edge, Fastly, Akamai, Envoy, HAProxy Enterprise, NGINX Plus, F5 BIG-IP, Varnish Enterprise and Kong, and leads where a small project can: a static image under 20 MB (under 5 MB for the limiter-only build), cold start under 50 ms, cache-hit p99 under 1 ms at 300,000 requests per second on 16 cores, proxy p99 within 10% of HAProxy using under a third of Envoy's memory at 10,000 connections, limiter memory per tracked client 50% below the best self-hosted leader, and global purge p99 under 150 ms across 50 nodes, all published from one reproducible harness, re-measured every quarter and enforced as a CI regression gate.

Benchmark products: Cloudflare's application edge (Load Balancing, Spectrum, Argo, SSL/TLS, WAF, DDoS, Bot Management, rate limiting rules, Rules, Workers, Cache, Access, Analytics, Logpush), Fastly, Akamai, Envoy, HAProxy Enterprise, NGINX Plus, F5 BIG-IP, Varnish Enterprise and Kong.

## Crate layout

| Crate | Purpose | Track that owns it |
|---|---|---|
| `edgefront-core` | Runtime and data-plane foundation: tokio event loop with per-core workers, listeners and connection lifecycle, rustls TLS, ACME, HTTP/1.1 (hyper), HTTP/2 (h2), HTTP/3 (quinn and h3), the base L4 relay, typed config with validation and hot reload, binary upgrade with socket handoff, telemetry (metrics, OTLP traces, access logs), SWIM membership for data-plane peers. | proxy (SWIM module: cache; base L4 relay: limiter port) |
| `edgefront-control` | Control plane: Raft-replicated config and event log (openraft), staged rollouts, versioned admin API, tenants, operator SSO, RBAC, hash-chained audit log, envelope-encrypted secrets, backups. | proxy |
| `edgefront-proxy` | Routing, load balancing, health checks, outlier detection, retries, discovery, the shared rules engine, edge authentication and authorization, L4 edge (TCP, UDP, TLS passthrough, PROXY protocol), global traffic (DNS steering, smart routing, BGP), origin tunnels, mesh mode, API gateway. | proxy |
| `edgefront-wasm` | Programmable edge on wasmtime: native filter trait, Proxy-Wasm ABI, WASI HTTP edge functions, actor state and queues. | proxy |
| `edgefront-cache` | cachet's engine: RFC 9111 policy, request collapsing, slices, memory and segment stores, multi-disk engine, encryption at rest, purge log, tiered caching, ESI, VCL translator, image and video delivery, edge KV and object storage. | cache |
| `edgefront-limiter` | pacer's algorithms in Rust: lock-free GCRA, fixed-memory keyed table, window counters, quotas, descriptors, adaptive concurrency, fairness queues, cluster limits with budget leasing, and the decision service (Envoy RLS gRPC, Redis protocol, HTTP JSON). | limiter |
| `edgefront-shield` | Abuse controls on the limiter: WAF (SecLang subset, OWASP CRS), L7 DDoS, bot management, custom security rules and lists, waiting room, API abuse, leaked credentials, DLP, content scanning, AI crawler control. | limiter |
| `edgefront-xdp` | One of two crates allowed unsafe: an aya eBPF/XDP program and loader for early drop, SYN and UDP flood defence, bounded by the kernel verifier and fuzzed through its map protocol. | limiter |
| `edgefront-limiter-ffi` | The other crate allowed unsafe: a stable C ABI over edgefront-limiter (cbindgen header, Python and Go bindings, NGINX module), Miri-tested and fuzzed. | limiter |
| `edgefront-bench` | Shared benchmark harness: open-loop load generator, competitor containers, result schema, published reports and the CI regression gate. | cache |
| `edgefront-difftest` | Differential test runner: feeds the same inputs to legacy/steer and legacy/pacer driver programs over stdin/stdout and to the Rust crates, and diffs the decisions. No FFI, no unsafe. | proxy and limiter (weeks 1 to 4) |
| `edgefront` | The one binary: serve, check, ctl, bench subcommands; cargo features build slim variants (proxy-only, cache-only, limiter-only). | all three |

Rules for every crate: `unsafe_code = "forbid"` at the workspace level, lifted only for `edgefront-xdp` (eBPF needs it; bounded by the kernel verifier and fuzzed through its map protocol) and `edgefront-limiter-ffi` (the C ABI, Miri-tested and fuzzed). Runtime dependencies stay few and each is named in the week that adds it: tokio, hyper, rustls, h2, quinn and h3, openraft and redb, wasmtime, aya, tonic and prost, kube-rs, hickory, plus smaller ones justified in their week (rcgen, cryptoki, seccompiler, landlock, tokio-uring, lettre, zstd, quick-xml, maxminddb, regex, aho-corasick, image, ravif, lol_html, aes-gcm, cbindgen, loom). The Terraform provider is the one non-Rust artifact, in Go, because Terraform loads providers as Go plugins. Other directories: `legacy/` (until each port is deleted), `console/` (TypeScript and Preact, embedded in the binary), `sdk/`, `integrations/`, `deploy/`, `docs/`.

## How the weeks run

Every week has three tracks that run in parallel, each with one cloud run per day (seven half-day tasks, the last one an end-to-end demo plus docs):

- **proxy**: the platform. Core runtime, TLS, HTTP/1 to 3, L4, routing, balancing, control plane, tenancy, identity, console, API, Wasm, Kubernetes, global traffic and operations.
- **cache**: caching, purge, storage and media, plus the shared benchmark harness and its quarterly reports.
- **limiter**: all limiting, quotas and abuse controls: the decision service, cluster limits, WAF, bots, DDoS, waiting room and XDP early drop.

A capability the three old plans shared appears once, in the track that owns it; the other tracks build on it by naming that week. The ownership table is at the end.

## Weeks 1 to 4: porting the legacy code

- **proxy** ports steer's working Go features into `edgefront-core` and `edgefront-proxy`: config and routing (w1), the balancers with priority levels, panic threshold, round robin, peak EWMA and Maglev (w2), health checks, outlier detection, retry budgets, admin API and metrics (w3), and the TLS edge, reload and drain (w4). `edgefront-difftest` drives a small Go program in `legacy/steer/cmd/diffdrive` and the Rust crates with the same configs, request sequences, seeds and simulated clock, and fails on any different decision. Week 4 publishes the Go-vs-Rust benchmark (throughput, p99, RSS at 10,000 connections, binary size) and deletes `legacy/steer` only after the differential suite is green on main; history stays reachable through the merge commit. steer came in as a subtree merge (c57a076), not through `git filter-repo`, so its 23 commits record their paths at the root: `git log c57a076^2 -- internal/route/route.go` follows a file through them, while `git log --follow legacy/steer/...` stops at the merge. Rewriting main to change that is ruled out.
- **limiter** ports pacer's C: the lock-free GCRA, keyed table, SipHash and window counter (w1), the config language with fixes for fractional rates and the 4.5-year clock limit (w2), and pacerd's L4 data path with connection limits, shaping, slowloris cut-off and drain (w3). Differential tests feed operations to a C driver `legacy/pacer/tests/diffdrive.c` over stdin and stdout, so no FFI or unsafe code is needed. Week 4 publishes the C-vs-Rust benchmark (decisions per second, proxy cost, memory per tracked client) and deletes `legacy/pacer` after its suite passes.
- **cache** splits cachet into workspace crates with no behaviour change (w1), moves the cache onto the shared runtime as a route-level layer with a differential test against the pre-split cachet binary (w2), builds the shared benchmark harness `edgefront-bench` (w3), and publishes the first report with a CI regression gate (w4).

## Milestones

### 2026-11 · Ports land and the workspace stands

Weeks 1 to 5: steer's balancer and pacer's limiter ported to Rust with differential suites green and legacy code deleted, cachet split into workspace crates, shared benchmark harness and first report, then ACME and mutual TLS, streaming sliced storage, and address policy.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 1 | Nov 2 to Nov 8 | Port steer's HTTP/1.1 core: config, routing and the differential harness | Restructure cachet into the edgefront Cargo workspace | Port libpacer's algorithms: lock-free GCRA, keyed table and window counter |
| 2 | Nov 9 to Nov 15 | Port steer's balancers: priority levels, panic threshold, round robin, peak EWMA and Maglev | Cache on the shared runtime: the cache as a route-level layer | Port pacer's config and fix its limits: quotas, fractional rates, epoch rebasing and lints |
| 3 | Nov 16 to Nov 22 | Port steer's resilience: health checks, outlier detection, retry budgets, admin API and metrics | Shared benchmark harness in edgefront-bench | Port pacerd's L4 data path: connection limits, shaping, slowloris cut-off and drain |
| 4 | Nov 23 to Nov 29 | Port steer's TLS edge and lifecycle, publish the Go-vs-Rust benchmark, delete legacy/steer | Baseline benchmark report and CI regression gate | Publish the C-vs-Rust benchmark, port pacerctl, delete legacy/pacer |
| 5 | Nov 30 to Dec 6 | ACME, OCSP stapling and mutual TLS on both sides | RFC conformance, streaming fill, sliced large objects and a segment store | Address policy: CIDR lists, limit classes, IPv6 prefix grouping and trusted proxies |

### 2026-12 · HTTP/2, discovery, tiered cache and HTTP limits

Weeks 6 to 9: HTTP/2 and gRPC, service discovery, routing and observability; tiered caching with SWIM, compression, eviction research and images; HTTP rate limiting rules, descriptors, penalty box and shadow mode.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 6 | Dec 7 to Dec 13 | HTTP/2 on both sides and gRPC proxying | Tiered caching and SWIM membership: peer fetch, origin shield and hot keys | HTTP rate limiting rules: keys from any request field and counting on responses |
| 7 | Dec 14 to Dec 20 | Service discovery from DNS, SRV, files, Kubernetes EndpointSlices and a delta feed | Compression variants, dictionary compression and Early Hints | Multi-limit decisions, descriptors, cost-based limits and hot keys |
| 8 | Dec 21 to Dec 27 | Routing and traffic shaping: matchers, weighted splits, canaries, mirroring and per-route policy | Trace-driven eviction: simulator, SIEVE, W-TinyLFU and disk admission | Penalty box, bans and overload mode |
| 9 | Dec 28 to Jan 3 | Platform observability: OTLP traces, access logs, structured logs and per-route metrics | Image optimisation: resize, format negotiation and quality | Shadow mode and policy replay |

### 2027-01 · HTTP/3, L4 edge, storage scale and the decision service

Weeks 10 to 13: HTTP/3, balancing, L4 edge and signed releases; multi-disk storage, 100M objects, ESI and the Q1 benchmark; Envoy RLS and Redis-protocol decision service and cluster limits without Redis.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 10 | Jan 4 to Jan 10 | HTTP/3 over QUIC towards clients | Multi-disk storage engine | Decision service: Envoy rate limit service API |
| 11 | Jan 11 to Jan 17 | Smarter balancing: zones, slow start, bounded-load hashing, affinity, hedging and deadlines | A hundred million objects per node | Redis-protocol and HTTP JSON decision front ends |
| 12 | Jan 18 to Jan 24 | L4 edge: TCP, UDP, TLS passthrough, SNI routing and PROXY protocol | ESI, signed URLs, token auth and cache key rules | Cluster limits: key ownership, forwarding and local fallback |
| 13 | Jan 25 to Jan 31 | Release engineering: reproducible builds, signed artifacts, SBOM and a compatibility policy | Q1 head-to-head benchmark across proxy, cache and limiter | Global limits with budget leasing and replicated state |

### 2027-02 · Rules engine, edge auth, Wasm filters and adaptive protection

Weeks 14 to 17: shared rules engine, JWT and OIDC, Proxy-Wasm, binary upgrades; encryption at rest, cache rules, VCL translator, bans; adaptive concurrency, fairness queues, fingerprints and HTTP/2 flood defences.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 14 | Feb 1 to Feb 7 | Shared rules engine: expressions, transform, redirect, origin and config rules | Encryption at rest for cached objects | Adaptive concurrency and priority load shedding |
| 15 | Feb 8 to Feb 14 | Edge authentication and authorization: JWT, OIDC sign-on and external authorization | Cache rules on the shared rules engine | Origin fairness and request queueing |
| 16 | Feb 15 to Feb 21 | Filter API and Proxy-Wasm filters | VCL translator for migrating from Varnish | Fingerprint, geography and ASN keys |
| 17 | Feb 22 to Feb 28 | Operability: offline config tools, zero-downtime binary upgrades and runtime drain | Bans, regex, prefix and soft purge at scale | HTTP/2 abuse defences and gRPC-aware limits |

### 2027-03 · Control plane, tenancy, WAF and video

Weeks 18 to 22: Raft control plane, staged rollouts, tenants, SSO, RBAC, audit, admin API; video and LL-HLS, edge KV, memory and disk I/O; consumer quotas, WAF, custom rules and L7 DDoS.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 18 | Mar 1 to Mar 7 | Raft control plane in edgefront-control | Video delivery: segment prefetch, HLS and DASH, preload and early refresh | Consumer quotas and plan tiers |
| 19 | Mar 8 to Mar 14 | Control plane HA: replicated config, delta subscriptions and staged fleet rollouts | Low-latency HLS and CMAF chunked delivery | WAF engine: SecLang subset and OWASP Core Rule Set |
| 20 | Mar 15 to Mar 21 | Multi-tenancy: tenants, verified hostnames, resource budgets and isolation | Edge KV store | WAF body inspection, exceptions and tuning |
| 21 | Mar 22 to Mar 28 | Operator SSO, RBAC, hash-chained audit log and encrypted secrets | Memory footprint: compact objects and zero-copy reads | Custom security rules, managed IP lists and threat feeds |
| 22 | Mar 29 to Apr 4 | Versioned admin API, edgefront ctl and SDKs | Disk I/O path: direct reads, read-ahead and per-disk schedulers | L7 DDoS mitigation |

### 2027-04 · Console, edge functions, Kubernetes and instant purge

Weeks 23 to 26: console, WASI edge functions, Kubernetes, ext_proc; purge over the Raft log under 150 ms and the Q2 benchmark; bot management, waiting room, API abuse and leaked credentials.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 23 | Apr 5 to Apr 11 | Web admin console | Purge over the replicated control plane log | Bot management: fingerprints, heuristics, scores and challenges |
| 24 | Apr 12 to Apr 18 | Edge functions on WASI HTTP components with KV bindings | Instant purge under 150 ms p99 across 50 nodes | Waiting room |
| 25 | Apr 19 to Apr 25 | Kubernetes: Gateway API controller, operator and Helm chart | Q2 benchmark, static binary cold start and warm restart | API abuse detection: volumetric and sequence |
| 26 | Apr 26 to May 2 | External processing, gRPC-JSON transcoding, gRPC-Web and WebSocket over HTTP/2 | Cache transfer between nodes and preload | Leaked credential detection and account takeover protection |

### 2027-05 · Analytics, global traffic and kernel-speed drop

Weeks 27 to 31: analytics, log push, GSLB DNS, smart routing and BGP; tenant cache partitions, image transformations, JIT packaging, object-storage tier, poisoning defences; XDP early drop and flood defence, security analytics, durable quotas.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 27 | May 3 to May 9 | Built-in edge analytics | Multi-tenant cache: partitions, storage budgets and tenant keys | XDP early drop for banned and over-limit sources |
| 28 | May 10 to May 16 | Log push to customer destinations | Image transformations API | XDP SYN and L4 flood defence |
| 29 | May 17 to May 23 | Global load balancing: authoritative DNS with health, geo and latency steering | Just-in-time packaging from MP4 to HLS and DASH | UDP and QUIC flood protection |
| 30 | May 24 to May 30 | Smart routing between edgefront sites | Persistent object-storage cache tier | Security analytics and top offenders |
| 31 | May 31 to Jun 6 | Anycast announcement with a minimal BGP speaker | Cache poisoning and cache deception defences | Durable quotas and limiter state handoff |

### 2027-06 · Origins, certificates at scale, mesh and embedding

Weeks 32 to 35: origin tunnels, keyless and FIPS, custom hostnames at scale, mesh; HTML rewriter, caching conformance, POST and GraphQL caching, always online; compact limiter table, Wasm policy plugins, C ABI and NGINX module.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 32 | Jun 7 to Jun 13 | Origin tunnels and authenticated origin pulls | Streaming HTML rewriter and minification | Memory per tracked client: compact keyed table |
| 33 | Jun 14 to Jun 20 | Keyless signing, HSM-backed keys and a FIPS build | Full HTTP caching conformance and targeted cache control | Programmable limits: Wasm policy plugins with a limiter host API |
| 34 | Jun 21 to Jun 27 | Custom hostnames and certificates at SaaS scale | POST, QUERY and GraphQL caching | Embeddable limiter: C ABI and language bindings |
| 35 | Jun 28 to Jul 4 | Sidecar mesh mode with workload certificates | Always online: stale snapshots when the origin is down | NGINX module and proxy-wasm filter builds of the limiter |

### 2027-07 · API gateway, edge storage and AI-era defences

Weeks 36 to 39: Redis and PostgreSQL proxies, API gateway, zero-allocation HTTP/1.1, deterministic simulation; Q3 benchmark, edge object storage, Client Hints, speculative prefetch; decision SDKs, AI crawler control, upload scanning, bot scores v2.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 36 | Jul 5 to Jul 11 | Protocol-aware Redis and PostgreSQL proxies | Q3 benchmark and hit-path tail latency | Decision client libraries for Go, Python, Node.js and Java |
| 37 | Jul 12 to Jul 18 | API gateway: consumers, API keys, schema validation and transformation | Object storage at the edge: bucket API and multipart uploads | AI crawler control |
| 38 | Jul 19 to Jul 25 | Zero-allocation HTTP/1.1 path and memory budgets | Client Hints, device detection and Vary normalisation | Upload content scanning |
| 39 | Jul 26 to Aug 1 | Deterministic simulation and fault injection | Speculative prefetch: Speculation Rules and predictive prefetch | Bot management v2: behaviour and session scores |

### 2027-08 · Upgrades, importers, automation and verification

Weeks 40 to 44: backups and rolling upgrades, config importers, Terraform, metering and SLOs, hardening; storage migration, large objects, private content, hotlink protection, personalisation; limiter verification, tenant isolation, DLP, script monitoring, firewall for AI.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 40 | Aug 2 to Aug 8 | Backups, disaster recovery and rolling mixed-version upgrades | Storage format versioning and online migration | Limiter verification: loom, TLA+ and partition tests |
| 41 | Aug 9 to Aug 15 | Config importers from NGINX, HAProxy, Envoy and Kong | Large object delivery: resumable downloads and parallel origin fetch | Tenant limit isolation and noisy neighbours |
| 42 | Aug 16 to Aug 22 | Terraform provider | Caching private and authenticated content | Sensitive data detection in responses |
| 43 | Aug 23 to Aug 29 | Usage metering, SLOs and notifications | Content protection: hotlink protection and signed media tokens | Client-side script monitoring |
| 44 | Aug 30 to Sep 5 | Security hardening programme | Edge personalisation: variant buckets and A/B caching | Firewall for AI: prompt inspection for LLM endpoints |

### 2027-09 · Kernel data path, edge state and protocol hardening

Weeks 45 to 48: io_uring and kTLS, actors and queues, conformance and smuggling, ECH and post-quantum; latency-chosen tiers, zero-copy hits, purge soak, data residency; rule update channel, attack lab, sub-microsecond decisions, stream limits.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 45 | Sep 6 to Sep 12 | Kernel-assisted data path: io_uring, kTLS, splice and per-core scaling | Tiered cache v2: topology chosen by measured latency | Managed ruleset update channel |
| 46 | Sep 13 to Sep 19 | Edge functions v2: actors, queues and cron triggers | Zero-copy hit serving with sendfile and kTLS | Attack simulation lab |
| 47 | Sep 20 to Sep 26 | Protocol conformance and request smuggling hardening | Cache cluster soak with purge linearizability checks | Decision throughput: per-core counters and sub-microsecond p99 |
| 48 | Sep 27 to Oct 3 | Encrypted Client Hello and post-quantum key exchange | Data residency: region-pinned storage and tier topology | Message-level limits for WebSocket, gRPC streams and server-sent events |

### 2027-10 · Final benchmark, drills and edgefront 1.0

Weeks 49 to 52: HTTP/3 to origins, docs site, multi-region soak and 1.0; final benchmark, crash consistency, purge API compatibility, reference CDN deployment; limit recommendations, parser hardening, fleet penalty sharing and the attack day drill.

| Week | Dates | proxy | cache | limiter |
|---|---|---|---|---|
| 49 | Oct 4 to Oct 10 | HTTP/3 to origins | Final benchmark report | Limit recommendations from observed traffic |
| 50 | Oct 11 to Oct 17 | Documentation site and migration guides | Storage crash consistency and fuzzing | Shield parser hardening and differential checks against ModSecurity |
| 51 | Oct 18 to Oct 24 | Multi-region reference deployment, week-long soak and chaos drill | Purge API compatibility for migrations | Fleet-wide penalty sharing in under one second |
| 52 | Oct 25 to Oct 31 | edgefront 1.0: frozen config and API, final release | Reference CDN deployment: three tiers, 50 nodes, offload SLO report | Attack day drill: combined L3, L4 and L7 attack on the reference deployment |

## Benchmark targets

One harness (`edgefront-bench`, cache weeks 3 and 4) measures all three tracks against the leaders; reports land in `docs/benchmarks/` in weeks 4, 13, 25, 36 and 49, and the CI gate fails a pull request that regresses a target.

| Target | Compared with | First measured | Gate from |
|---|---|---|---|
| Static image under 20 MB, limiter-only build under 5 MB | Envoy, HAProxy, NGINX, Varnish images | w4 | proxy w13 |
| Cold start under 50 ms; warm restart with 10M disk objects under 2 s | Envoy, Varnish | w4 | cache w25 |
| Cache-hit p99 under 1 ms at 300,000 req/s on 16 cores | Varnish, NGINX, Apache Traffic Server | w4 | cache w4 |
| Under 80 bytes of index memory per cached object | Varnish, Apache Traffic Server | cache w11 | cache w11 |
| Proxy p99 within 10% of HAProxy, RSS under a third of Envoy's at 10,000 connections | HAProxy, Envoy, NGINX | proxy w4 | proxy w38 |
| Limiter memory per tracked client 50% below the best | NGINX limit_req, HAProxy stick tables, envoyproxy/ratelimit | limiter w4 | limiter w32 |
| Decision service throughput and p99 above envoyproxy/ratelimit with Redis | envoyproxy/ratelimit | limiter w10 | limiter w47 |
| Global purge p99 under 150 ms across 50 nodes | Fastly and Cloudflare published figures | cache w24 | cache w24 |
| XDP drop rate per core in Mpps | kernel stack drop | limiter w27 | limiter w28 |

## Ownership of shared capabilities

| Capability | Where the old plans had it | Owner in edgefront | edgefront week |
|---|---|---|---|
| TLS termination, ACME, OCSP, mutual TLS | steer w1, cachet wk 2, pacer wk 16 | proxy | w4, w5 |
| HTTP/2 and gRPC | steer w2, cachet wk 2, pacer wk 18 | proxy | w6 |
| HTTP/3 and QUIC | steer w13, cachet wk 3 | proxy | w10, w49 |
| L4 proxy and PROXY protocol | steer w14, pacer wk 3, wk 20 | proxy (base TCP relay ported by limiter w3) | w12 |
| Raft control plane and config rollout | steer w27, w28, cachet wk 5, wk 38 | proxy | w18, w19 |
| Multi-tenancy | steer w29, cachet wk 14, pacer wk 22, wk 39 | proxy (cache w27 and limiter w41 add enforcement) | w20 |
| Operator SSO, RBAC, audit, secrets | steer w30, cachet wk 15, wk 16, pacer wk 38 | proxy | w21 |
| Admin API, CLI and SDKs | steer w32, cachet wk 42, pacer wk 36 | proxy (limiter w36 = decision-service clients, a different API) | w22 |
| Web console | steer w31, cachet wk 10, wk 40, pacer wk 37 | proxy | w23 |
| Wasm runtime, filters and edge functions | steer w23, w24, cachet wk 6, wk 27, pacer wk 44 | proxy (limiter w33 adds a limiter host API) | w16, w24, w46 |
| Rules engine | steer w15, cachet wk 6 | proxy (cache w15 cache actions, limiter w21 security actions) | w14 |
| Edge KV and object storage | steer w24 (KV) | cache | w20, w37 |
| WAF | steer w18, cachet wk 33 | limiter | w19, w20 |
| L7 DDoS mitigation | steer w19, cachet wk 35, pacer wk 8 | limiter | w22 |
| Bot management | steer w20, cachet wk 34 | limiter | w23, w39 |
| Rate limiting rules | steer w5, w21, cachet wk 7, wk 36 | limiter | w6, w7 |
| Adaptive concurrency and load shedding | steer w5, cachet wk 7, pacer wk 33 | limiter | w14 |
| Origin fairness and request queueing | cachet wk 47 | limiter | w15 |
| Waiting room | steer w44 | limiter | w24 |
| Consumers and API keys | steer w50, cachet wk 36, pacer wk 22 | proxy identity (w37), limiter quotas (w18) | proxy w37, limiter w18 |
| Kubernetes Gateway API, operator, Helm | steer w33, cachet wk 22, wk 23, pacer wk 45 to 47 | proxy | w25 |
| Terraform provider | cachet wk 41 | proxy | w42 |
| Analytics store | steer w35, cachet wk 28, pacer wk 35 | proxy (limiter w30 security views) | w27 |
| Log push and streaming | steer w36, cachet wk 29 | proxy | w28 |
| Tracing, metrics, access logs | steer w6, pacer wk 4, wk 39 | proxy | w9 |
| DNS traffic steering | steer w37, cachet wk 31 | proxy | w29 |
| Origin tunnels | steer w42, cachet wk 32 | proxy | w32 |
| Certificates at scale, keyless, FIPS | steer w40, w41, cachet wk 44, wk 46 | proxy (cache w48 keeps data residency) | w33, w34 |
| Origin load balancing, outlier detection, hedging | steer w7, cachet wk 11, pacer wk 32, wk 34 | proxy | w2, w3, w11 |
| Zero-downtime binary upgrade | steer w11, cachet wk 10, pacer wk 6 | proxy (limiter w31 hands over limiter state) | w17 |
| Backups and rolling upgrades | steer w47, cachet wk 43, wk 48 | proxy (cache w40 storage format migration) | w40 |
| Config importers | steer w51, pacer wk 48 | proxy (cache w16 VCL translator is a separate language) | w41 |
| Release engineering | steer w12, cachet wk 12, pacer wk 12 | proxy | w13 |
| Security hardening (seccomp, privilege drop, fuzzing) | steer w48, pacer wk 11 | proxy (limiter w50 hardens its own parsers) | w44 |
| io_uring, kTLS, splice | steer w46, cachet wk 21, pacer wk 5, wk 40 | proxy (cache w46 zero-copy hits on it) | w45 |
| XDP early drop | pacer wk 41 | limiter | w27 to w29 |
| SWIM membership for data-plane peers | cachet wk 4, pacer wk 27 | cache (limiter w12 uses it) | w6 |
| Benchmark harness and quarterly reports | steer w9, w22, w35, w48, cachet wk 13, wk 26, wk 39, wk 50, pacer wk 13, wk 31, wk 43, wk 52 | cache (proxy w4 and limiter w4 publish their port benchmarks on it) | w3, w4, w13, w25, w36, w49 |
| Documentation site and migration guides | cachet wk 51 | proxy | w50 |
| Simulation, soak, chaos | steer w10, cachet wk 49, pacer wk 31 | proxy for the platform (w39, w51); cache w47 purge linearizability; limiter w40 limiter models | w39, w51 |

## Next year

- Volumetric DDoS scrubbing and a managed anycast network with peering. These need a global network, which is a service, not software.
- Bot and threat scores trained on cross-customer traffic; edgefront scores from fingerprints, behaviour and local data only.
- Video transcoding and encoding (edgefront packages and caches, it does not encode).
- JavaScript edge functions; edgefront runs Wasm and WASI components this year.
- PCI DSS and SOC 2 attestations for a hosted offering, and a managed control plane.
- Upstream QUIC is in w49; multipath QUIC and MASQUE (HTTP/3 CONNECT-UDP proxying) follow next year.
- Machine-learned DDoS signatures shared across a fleet of independent operators.
