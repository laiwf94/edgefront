# steer: roadmap, November 2026 to October 2027

## Vision

By October 2027 steer is a self-hostable application edge in the spirit of Cloudflare's: one Go binary that terminates TLS (ACME, mutual TLS, keyless signing, custom hostnames), speaks HTTP/1.1, HTTP/2, HTTP/3, gRPC, TCP and UDP, balances and steers traffic globally with health-aware DNS and steer-to-steer smart routing, runs a rules engine, WAF, bot and L7 DDoS defences, Wasm edge functions, origin tunnels and a waiting room, and is run by a Raft-replicated control plane (steerd) with tenants, SSO, RBAC, audit, analytics, log push, a web console, an API, a Kubernetes Gateway API controller and a mesh mode. It reaches feature parity on the rows that matter against Cloudflare's application edge, Envoy, HAProxy Enterprise, NGINX Plus, F5 BIG-IP, Traefik Enterprise and Kong, integrates with its siblings instead of copying them (cachet for caching, pacer for rate limiting, wicket for sign-on), and wins where a small project can: an image under 15 MB, cold start under 50 ms, under a third of Envoy's memory at 10,000 connections and p99 within 10% of HAProxy, published in a reproducible benchmark, re-measured every quarter and held by a CI regression gate.

## Benchmarked against

Feature parity is measured against Cloudflare's application edge (Load Balancing, Spectrum, Argo Smart Routing, SSL/TLS, WAF, DDoS protection, Bot Management, rate limiting, Rules, Workers, Cache, Zero Trust Access, Analytics and Logpush), Envoy, HAProxy Enterprise, NGINX Plus, F5 BIG-IP, Traefik Enterprise and Kong Gateway Enterprise.Performance is measured against Envoy, HAProxy and NGINX in bench/, first in week 9 and then every quarter (weeks 22, 35, 48 and 52), with a CI gate on steer's own numbers.

Weeks 1 to 12 each deliver one whole monthly theme of the earlier draft; weeks 13 to 52 add the enterprise capabilities that close the gaps ranked in the comparison.

## 2026-11 · TLS, HTTP/2, gRPC, discovery, routing and overload

Weeks 1 to 5 deliver v1's first five monthly themes, one per week: full TLS on both sides, HTTP/2 and gRPC, dynamic discovery, rich routing with canaries and mirroring, and overload protection.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 1 | 2026-11-02 to 2026-11-08 | TLS termination, ACME and upstream TLS on both sides | v1 month 1 in one week: steer cannot face clients without terminating TLS, and every leader has SNI, client certificates, automatic certificates and verified upstream TLS (matrix: TLS > ACME, SNI, mutual TLS, OCSP stapling). |
| 2 | 2026-11-09 to 2026-11-15 | HTTP/2 on both sides and gRPC proxying | v1 month 2 in one week: browsers and service clients expect HTTP/2, and gRPC behind a proxy is Envoy's main draw (matrix: Protocols > HTTP/2, gRPC). |
| 3 | 2026-11-16 to 2026-11-22 | Service discovery from DNS, SRV, files, Kubernetes and a delta feed | v1 month 3 in one week: static files do not survive autoscaling, and the reload path already carries endpoint state (matrix: Control plane > service discovery, dynamic config API). |
| 4 | 2026-11-23 to 2026-11-29 | Routing and traffic shaping: matching, canaries, mirroring, per-route policy | v1 month 4 in one week: real gateways route on far more than host and prefix and use the proxy for releases (matrix: Traffic management > header and regex routing, rewrites, weighted splits, mirroring). |
| 5 | 2026-11-30 to 2026-12-06 | Overload protection: local limits, adaptive concurrency, hedging and deadlines | v1 month 5 in one week: a proxy that only fails over still collapses under overload (matrix: Traffic management > circuit breaking, retries; Security > rate limiting). Fleet-wide rate limiting rules come in week 21 through pacer, so this week stays local. |

## 2026-12 · Observability, balancing, fleet and first benchmark

Weeks 6 to 9: the year's one observability week, every balancing policy in a simulator, a minimal fleet control plane, and the hot-path work with the first published benchmark against Envoy, HAProxy and NGINX.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 6 | 2026-12-07 to 2026-12-13 | Observability: OTLP traces, access log formats, event stream and per-route metrics | v1 month 6 in one week, and the year's only observability week: operators need traces through the proxy, logs in their own shape and live decisions (matrix: Observability > metrics, tracing, logs). |
| 7 | 2026-12-14 to 2026-12-20 | Smarter balancing: zones, slow start, bounded loads, affinity and a simulator | v1 month 7 in one week: the leaders all ship zone awareness, slow start, ring hash and session affinity (matrix: Load balancing > consistent hashing, session affinity, slow start, locality). |
| 8 | 2026-12-21 to 2026-12-27 | Running a fleet: config history, rollback, shared outlier evidence, minimal control plane | v1 month 8 in one week: outlier detection weakens as instances multiply, and a fleet needs one place to push and roll back config (matrix: HA and clustering > state sharing; Control plane > config API). |
| 9 | 2026-12-28 to 2027-01-03 | Performance: allocation-free hot path, PGO, and the first benchmark against Envoy, HAProxy and NGINX | v1 month 9 plus v1's comparison benchmark, moved here so the numbers come early: low footprint and top p99 are the reasons to pick a small proxy (matrix: Footprint and performance). |

## 2027-01 · Verification, operability, releases and HTTP/3

Weeks 10 to 13: deterministic simulation and soak, zero-downtime upgrades and offline config tools, signed releases with a versioned config schema, then HTTP/3 over QUIC.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 10 | 2027-01-04 to 2027-01-10 | Verification and resilience: deterministic simulation, slow clients, fault injection, soak | v1 month 10 in one week: rare interleavings and slow attacks only show up under simulation and long runs (matrix: Security > slow-client protection; Footprint and performance > stability). |
| 11 | 2027-01-11 to 2027-01-17 | Operability: offline config tools, zero-downtime binary upgrades, runtime drain | v1 month 11 in one week: HAProxy and Envoy upgrade without dropping connections, and operators need to check configs before they ship (matrix: Operations > zero-downtime reload and upgrade). |
| 12 | 2027-01-18 to 2027-01-24 | Versioned config schema, signed releases and a compatibility policy | v1 month 12 in one week, and the year's only release-engineering week: enterprises adopt software with a stable config contract and verifiable binaries (matrix: Operations > packaging, upgrades). |
| 13 | 2027-01-25 to 2027-01-31 | HTTP/3 over QUIC towards clients | Cloudflare, NGINX Plus, HAProxy Enterprise and Envoy all serve HTTP/3; it cuts tail latency on lossy mobile networks (matrix: Protocols > HTTP/3 and QUIC). |

## 2027-02 · L4 edge, rules engine and edge access

Spectrum-style TCP and UDP proxying, a Cloudflare-style rules engine for transform, redirect, origin and config rules, sign-on at the edge through wicket, and external authorization with RBAC.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 14 | 2027-02-01 to 2027-02-07 | Spectrum-style L4 proxy: TCP, UDP, TLS passthrough and PROXY protocol | Cloudflare Spectrum, HAProxy, NGINX stream and F5 front non-HTTP services; steer has none (matrix: Protocols > TCP, UDP, PROXY protocol). |
| 15 | 2027-02-08 to 2027-02-14 | A Cloudflare-style rules engine: transform, redirect, origin and config rules | Cloudflare Rules, NGINX njs and HAProxy ACLs let operators change behaviour per request without code; WAF, bot and rate limiting rules in later weeks reuse this engine (matrix: Traffic management > rules engine). |
| 16 | 2027-02-15 to 2027-02-21 | Edge sign-on through wicket and local JWT validation | Cloudflare Access, NGINX Plus and Kong authenticate at the edge; wicket already does OIDC and SAML sign-on, so steer fronts it instead of copying it (matrix: Security > edge authentication; Zero Trust Access, integration with wicket). |
| 17 | 2027-02-22 to 2027-02-28 | External authorization and an RBAC policy engine | Envoy ext_authz, Kong and F5 let central services decide; enterprises keep policy outside the proxy (matrix: Security > external authorization). |

## 2027-03 · Edge security and sibling integrations

A WAF with managed rulesets, adaptive L7 DDoS mitigation, bot management, rate limiting rules on pacer's algorithm, and a cache tier through cachet, closed by the first quarterly re-benchmark.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 18 | 2027-03-01 to 2027-03-07 | WAF with managed rulesets and anomaly scoring | Cloudflare WAF, F5 Advanced WAF, HAProxy Enterprise WAF and NGINX App Protect are why buyers pay; steer has none (matrix: Security > WAF, managed rules). |
| 19 | 2027-03-08 to 2027-03-14 | Adaptive L7 DDoS mitigation | Cloudflare's HTTP DDoS protection and F5's behavioural DoS detect floods from baselines and mitigate automatically (matrix: Security > DDoS L7). |
| 20 | 2027-03-15 to 2027-03-21 | Bot management: fingerprints, scores and challenges | Cloudflare Bot Management, F5 and Kong plugins score clients and challenge suspicious ones (matrix: Security > bot management). |
| 21 | 2027-03-22 to 2027-03-28 | Rate limiting rules on pacer's algorithm | Cloudflare rate limiting rules, Kong and Envoy's global rate limit service count across a fleet; pacer owns the limiter, so steer adopts its algorithm and protocol instead of inventing one (matrix: Security > rate limiting, integration with pacer). |
| 22 | 2027-03-29 to 2027-04-04 | Cache tier through cachet, and the Q1 re-benchmark | Cloudflare Cache, NGINX Plus and Varnish cache at the edge; cachet already implements RFC 9111 with purge by tag, so steer routes to it rather than duplicating it (matrix: Caching, integration with cachet). |

## 2027-04 · Programmable edge

Proxy-Wasm filters, Workers-style edge functions with a KV store, a Go filter API with external processing, and gRPC-JSON transcoding with WebSocket over HTTP/2.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 23 | 2027-04-05 to 2027-04-11 | Proxy-Wasm filters | Envoy and others run Proxy-Wasm filters; it is the portable plugin format of the category (matrix: Programmability > Wasm). |
| 24 | 2027-04-12 to 2027-04-18 | Workers-style edge functions with a KV store | Cloudflare Workers is the programmable edge; steer can run Wasm functions that answer requests themselves (matrix: Programmability > edge functions). |
| 25 | 2027-04-19 to 2027-04-25 | Go filter API and external processing | Envoy's filter chain and ext_proc let teams extend without forking (matrix: Programmability > plugins and external processing). |
| 26 | 2027-04-26 to 2027-05-02 | gRPC-JSON transcoding and WebSocket over HTTP/2 | Envoy and Kong expose gRPC as REST, and RFC 8441 WebSockets over HTTP/2 are expected by modern clients (matrix: Protocols > gRPC transcoding, WebSocket). |

## 2027-05 · Enterprise control plane

steerd on a hand-written Raft: replicated config with staged fleet rollouts, tenants with verified hostnames, admin SSO, RBAC, audit and encrypted secrets, and a web console.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 27 | 2027-05-03 to 2027-05-09 | internal/raft: a tested Raft implementation for steerd | The week 8 control plane is a single process; Cloudflare, F5 and Kong control planes survive node loss (matrix: HA and clustering > control plane HA). |
| 28 | 2027-05-10 to 2027-05-16 | steerd HA: replicated config and staged fleet rollouts | Fleets of hundreds of proxies need a replicated source of truth and safe rollouts (matrix: Control plane > staged rollout; HA and clustering). |
| 29 | 2027-05-17 to 2027-05-23 | Tenants with verified hostnames, quotas and isolation | Cloudflare accounts and zones, Kong workspaces and F5 partitions let one platform serve many teams (matrix: Management > multi-tenancy). |
| 30 | 2027-05-24 to 2027-05-30 | Admin SSO, RBAC, audit and encrypted secrets | Every commercial edge sells SSO, roles and audit logs for the people who change routing (matrix: Management > admin RBAC and SSO, audit logs; TLS > keys at rest). |
| 31 | 2027-05-31 to 2027-06-06 | Web admin console | Cloudflare's dashboard, NGINX Plus's dashboard, Kong Manager and F5's GUI are how most operators work (matrix: Management > admin UI). |

## 2027-06 · API, Kubernetes, mesh and analytics

A versioned admin API with CLI and SDK, a Gateway API controller with Helm, a sidecar mesh mode with workload certificates, and built-in edge analytics, plus the Q2 re-benchmark.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 32 | 2027-06-07 to 2027-06-13 | Versioned admin API, steerctl and a Go SDK | Cloudflare, Kong and F5 are automated through APIs and Terraform; steer needs a stable API and GitOps tooling (matrix: Control plane > admin API). |
| 33 | 2027-06-14 to 2027-06-20 | Kubernetes Gateway API controller and Helm chart | Kubernetes is where most proxies run; Envoy Gateway, NGINX Gateway Fabric, Traefik and Kong implement the Gateway API (matrix: Control plane > Kubernetes Gateway API). |
| 34 | 2027-06-21 to 2027-06-27 | Sidecar mesh mode with workload certificates | Envoy in Istio, Kong Mesh and Traefik Mesh secure service-to-service traffic; steer has mTLS and a control plane already (matrix: Control plane > service mesh). |
| 35 | 2027-06-28 to 2027-07-04 | Built-in edge analytics, and the Q2 re-benchmark | Cloudflare Analytics, NGINX Plus and Kong Vitals answer traffic questions without an external stack (matrix: Observability > analytics). |

## 2027-07 · Global traffic

Log push to customer destinations, health-aware DNS load balancing with geo and latency steering, Argo-style smart routing between steer sites, and anycast announcement through a minimal BGP speaker.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 36 | 2027-07-05 to 2027-07-11 | Log push to customer destinations | Cloudflare Logpush, Kong and F5 ship logs to customer systems with field selection (matrix: Observability > log push). |
| 37 | 2027-07-12 to 2027-07-18 | Global load balancing: health-aware DNS with geo and latency steering | Cloudflare Load Balancing, F5 DNS and NGINX Plus steer clients between regions with pools and monitors (matrix: Global traffic > DNS GSLB, geo and latency steering, failover pools). |
| 38 | 2027-07-19 to 2027-07-25 | Smart routing between steer sites | Cloudflare Argo Smart Routing sends traffic over the fastest path between sites (matrix: Global traffic > smart routing). |
| 39 | 2027-07-26 to 2027-08-01 | Anycast announcement with a minimal BGP speaker | Cloudflare and other edges use anycast; self-hosters need health-driven route announcement (matrix: Global traffic > anycast). |

## 2027-08 · Certificates, origins and protocols

Keyless signing with a FIPS build, custom hostnames at SaaS scale, outbound origin tunnels and authenticated origin pulls, Redis and PostgreSQL proxies, and a waiting room.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 40 | 2027-08-02 to 2027-08-08 | Keyless signing, HSM-backed keys and a FIPS build | Cloudflare Keyless SSL, F5's HSM support and FIPS builds from HAProxy, NGINX and Kong are entry tickets for regulated buyers (matrix: TLS > keyless and HSM, FIPS). |
| 41 | 2027-08-09 to 2027-08-15 | Custom hostnames at SaaS scale | Cloudflare for SaaS lets platforms serve thousands of customer domains with automatic certificates; steer's tenants need the same (matrix: TLS > custom hostnames). |
| 42 | 2027-08-16 to 2027-08-22 | Origin tunnels and authenticated origin pulls | Cloudflare Tunnel and Authenticated Origin Pulls let origins accept traffic without open ports or with proof it came from the edge (matrix: Security > origin protection). |
| 43 | 2027-08-23 to 2027-08-29 | Protocol-aware Redis and PostgreSQL proxies | Envoy ships Redis and PostgreSQL filters and HAProxy fronts databases; protocol awareness gives sharding, failover and per-command metrics (matrix: Protocols > L7 database protocols). |
| 44 | 2027-08-30 to 2027-09-05 | Waiting room, custom error pages and maintenance mode | Cloudflare Waiting Room and custom error pages keep launches and incidents orderly instead of failing (matrix: Traffic management > waiting room). |

## 2027-09 · Performance leadership and hardening

A zero-allocation HTTP/1.1 path with memory budgets, per-core scaling with splice and kTLS, backup and rolling upgrades of steerd, and a chaos, soak and fuzzing programme with the Q3 re-benchmark.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 45 | 2027-09-06 to 2027-09-12 | Zero-allocation HTTP/1.1 path and memory budgets | Low memory and top p99 are the stated advantages over Envoy and HAProxy; net/http allocations and idle buffers decide both (matrix: Footprint and performance). |
| 46 | 2027-09-13 to 2027-09-19 | Per-core scaling, splice and kTLS | Throughput must scale with cores, and HAProxy moves L4 bytes with splice (matrix: Footprint and performance). |
| 47 | 2027-09-20 to 2027-09-26 | steerd backup, disaster recovery and rolling upgrades | Enterprise control planes need backups, point-in-time restore and mixed-version upgrades (matrix: Operations > backup and restore, upgrades). |
| 48 | 2027-09-27 to 2027-10-03 | Security hardening programme and the Q3 re-benchmark | Edge proxies are attacked first; HTTP parsing differences between proxies cause smuggling, and buyers ask for a threat model (matrix: Security > protocol hardening). |

## 2027-10 · Gateway features and 1.0

Usage metering with notifications, API gateway consumer management and schema validation, config importers from NGINX, HAProxy and Envoy, and a frozen 1.0 with the final benchmark.

| Week | Dates | Deliverable | Why |
|---:|---|---|---|
| 49 | 2027-10-04 to 2027-10-10 | Usage metering, SLOs and notifications | Cloudflare notifications and Kong Vitals tell tenants about usage, certificate expiry and incidents (matrix: Management > notifications and usage). |
| 50 | 2027-10-11 to 2027-10-17 | API gateway: consumers, keys, quotas and schema validation | Kong and Cloudflare API Shield manage API consumers and validate requests against schemas (matrix: Security > API schema validation; Management > API consumers). |
| 51 | 2027-10-18 to 2027-10-24 | Config importers from NGINX, HAProxy and Envoy | Migration cost is the main barrier to switching proxies (matrix: Operations > migration tooling). |
| 52 | 2027-10-25 to 2027-10-31 | 1.0: frozen config and API, final benchmark and a multi-region reference drill | Enterprises adopt a 1.0 with guarantees and evidence (matrix: every row marked Oct 2027). |

## Next year

Gaps from the comparison still open after October 2027:

- Volumetric L3/L4 DDoS scrubbing at network scale (needs a global network; steer limits itself to L7 mitigation and per-host connection defences, with XDP filtering a candidate for 2028).
- A global anycast network and managed points of presence, which are the service Cloudflare sells rather than software.
- Upstream HTTP/3 to origins (Envoy has it stable; steer keeps HTTP/1.1 and HTTP/2 upstream this year).
- Machine-learned bot detection trained on cross-customer traffic, which needs data a self-hosted product does not see.
- JavaScript edge functions (Workers runs JS; steer runs Wasm and WASI guests, with a JS engine compiled to Wasm left for next year).
