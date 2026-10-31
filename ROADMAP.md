# cachet: one-year roadmap (November 2026 to October 2027)

## Vision

By October 2027 cachet is an enterprise HTTP cache and edge delivery node at feature parity with Varnish Enterprise, NGINX Plus and Apache Traffic Server, and with the cache features of Fastly, Akamai and Cloudflare that a self-hosted fleet can offer: TLS with automatic certificates, HTTP/2 and HTTP/3, streaming and sliced storage of large objects on many disks, a Raft-replicated control plane with sub-second global purge, tiered caching, a rules language and Wasm plugins, WAF, bot and rate controls, image and video delivery, multi-tenancy with SSO, RBAC and audit, encryption at rest, a Kubernetes operator and Gateway API support, a console, a Terraform provider and SDKs. It stays one small static Rust binary with no unsafe code, and it beats the leaders where a small project can: published, reproducible benchmarks against Varnish, NGINX and Apache Traffic Server with CI-enforced targets (cache hit p99 under 1 ms at 300,000 requests per second on 16 cores, under 80 bytes of index memory per object, a container image under 15 MB, warm restart with 10 million disk objects in under 2 seconds, global purge p99 under 150 ms across 50 nodes).

## Benchmarks and targets

Compared products: Varnish Enterprise, Fastly, Akamai, Cloudflare, NGINX Plus and Apache Traffic Server on features, and Varnish Cache, NGINX and Apache Traffic Server measured head to head on the same machine. Each week names the matrix rows it closes. Targets, first measured in week 13 and re-measured in weeks 26, 39 and 50, enforced as a CI regression gate from week 13: hit p99 under 1 ms at 300,000 requests per second on 16 cores; miss overhead over direct origin under 100 µs at p50; under 80 bytes of index memory per cached object; container image under 15 MB; cold start under 20 ms and warm restart with 10 million disk objects under 2 seconds; global purge p99 under 150 ms across 50 nodes.

## 2026-11 · Storage, transport, protocols, tiers and control plane

Five core themes, one per week: large-object storage and RFC conformance, TLS and HTTP/2, HTTP/3 and compression, tiered caching, and a Raft control plane that fixes purge delivery.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 1 | 2026-11-02 to 2026-11-08 | RFC conformance, streaming fill, sliced large objects and a segment store | Objects over 8 MiB are not cached and the disk tier is one file per URL; every leader caches large media in slices. Closes matrix rows H4 and H5 and extends H1 and S2. |
| 2 | 2026-11-09 to 2026-11-15 | TLS with automatic certificates, HTTP/2 and mutual TLS to origins and peers | cachet speaks plain HTTP/1.1 only; every leader terminates TLS and speaks HTTP/2. Closes matrix rows H6, H8, H9 and X5. |
| 3 | 2026-11-16 to 2026-11-22 | HTTP/3, 0-RTT policy, compression variants and Early Hints | Fastly, Akamai, Cloudflare and NGINX serve HTTP/3 and compress at the edge. Closes matrix rows H7, H10 and H11. |
| 4 | 2026-11-23 to 2026-11-29 | Tiered caching: membership, peer fetch, origin shield and hot-key replication | Each node goes to the origin on its own; leaders fetch from a parent or peer first. Closes matrix rows T1 and T2. |
| 5 | 2026-11-30 to 2026-12-06 | Raft control plane: replicated purge log, config rollout and multi-service routing | Purges are flooded with O(N squared) messages and a peer down for ten minutes misses them. Closes matrix rows T3 and U1 (delivery) and T4 (routing). |

## 2026-12 · Programmable edge, protection, media and eviction research

A rules language with Wasm plugins and ESI, request protection, media delivery, and trace-driven eviction work.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 6 | 2026-12-07 to 2026-12-13 | Rules language, Wasm plugins, ESI and signed URLs | Varnish has VCL, Fastly has Compute and Akamai has EdgeWorkers; cachet has only fixed behaviour. Closes matrix rows G1, G2, G3, G4 and G5. |
| 7 | 2026-12-14 to 2026-12-20 | Rate limits, request filters, adaptive shedding and poisoning defences | A cache in front of an origin is the first line of defence, and every leader sells rate limiting. Closes matrix rows X3 and R5 and extends X4. |
| 8 | 2026-12-21 to 2026-12-27 | Media delivery: images, segment prefetch, preload and early refresh | Images and video are most of an edge's bytes, and Akamai, Fastly and Cloudflare sell media features. Closes matrix rows M1, M2 and S5. |
| 9 | 2026-12-28 to 2027-01-03 | Trace-driven eviction: simulator, SIEVE and W-TinyLFU, disk admission | Hit ratio is the cheapest performance there is; leaders tune it with traces. Extends matrix rows S1 and S2. |

## 2027-01 · Operations, origins, release and the benchmark

Console, hot upgrades and tracing, origin load balancing and hedging, release engineering, and the published head-to-head benchmark that becomes a CI gate.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 10 | 2027-01-04 to 2027-01-10 | Operator experience: console, scoped tokens, hot upgrades and tracing | The one week of cross-cutting operations: observability, admin access and upgrades. Closes matrix rows O1, O6 (partial) and O9. |
| 11 | 2027-01-11 to 2027-01-17 | Origin features: load balancing, hedging, outlier detection and POST caching | NGINX Plus, Varnish and Akamai balance and protect origins far beyond ordered failover. Closes matrix rows R1, R2 and R3 and extends R5. |
| 12 | 2027-01-18 to 2027-01-24 | Release engineering, per-service budgets and versioned formats | The one release-engineering week of the year, plus the isolation and format guarantees operators need before they rely on cachet. Closes matrix row T4 (budgets). |
| 13 | 2027-01-25 to 2027-01-31 | Head-to-head benchmark against Varnish, NGINX and Apache Traffic Server, enforced in CI | Lightweight and fast is cachet's claim; Varnish publishes terabit numbers and NGINX and ATS publish none, so cachet measures all of them the same way. Matrix row F1. |

## 2027-02 · Tenancy, identity, audit and encryption

Multi-tenancy, SSO with RBAC for operators, compliance-grade audit logs and encryption at rest of cached objects.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 14 | 2027-02-01 to 2027-02-07 | Multi-tenancy: isolated services, quotas and tenant keys | Varnish Controller, Fastly, Akamai and Cloudflare all serve many customers from one fleet. Matrix row T4. |
| 15 | 2027-02-08 to 2027-02-14 | Operator single sign-on and role-based access | Every commercial leader gives operators SSO and roles; cachet has one admin token. Matrix row O4. |
| 16 | 2027-02-15 to 2027-02-21 | Compliance-grade audit logs streamed to SIEMs | Fastly, Akamai and Cloudflare keep audit logs of every change. Matrix row O5. |
| 17 | 2027-02-22 to 2027-02-28 | Encryption at rest for cached objects | Varnish Enterprise sells Total Encryption; regulated customers cannot write cleartext responses to disk. Matrix row S4. |

## 2027-03 · Storage scale, lightweight core and Kubernetes ingress

A multi-disk storage engine, a hundred million objects per node, the memory and throughput work, and a Gateway API implementation.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 18 | 2027-03-01 to 2027-03-07 | Multi-disk storage engine | Varnish MSE stores petabytes per machine across many disks and survives disk failure; cachet has one directory. Matrix row S3. |
| 19 | 2027-03-08 to 2027-03-14 | A hundred million objects per node | Large edge nodes hold far more objects than fit in a naive index. Matrix rows S3 and F1. |
| 20 | 2027-03-15 to 2027-03-21 | Memory footprint: compact objects and zero-copy reads | Small memory per object is where cachet can beat Varnish and ATS. Matrix row F1. |
| 21 | 2027-03-22 to 2027-03-28 | Throughput: thread-per-core serving and kernel-assisted I/O | Top throughput and p99 on the same hardware as the leaders. Matrix row F1. |
| 22 | 2027-03-29 to 2027-04-04 | Kubernetes Gateway API and ingress controller | NGINX ships a Kubernetes ingress controller; caching at ingress is a common deployment. Matrix row O8. |

## 2027-04 · Kubernetes operator, migration from Varnish and cold start

A cluster operator, a VCL translator, Varnish-style bans and the cold-start and image-size work with the second benchmark.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 23 | 2027-04-05 to 2027-04-11 | Kubernetes operator for cachet clusters | Fleet operators expect clusters, upgrades and purges as resources. Matrix row O8. |
| 24 | 2027-04-12 to 2027-04-18 | VCL translator for migrating from Varnish | Varnish users have years of VCL; a translator removes the biggest switching cost. Matrix row G1. |
| 25 | 2027-04-19 to 2027-04-25 | Bans, regex purge and surrogate-key purge at scale | Varnish bans and regex purge, Fastly soft purge and ATS regex_revalidate are standard tools. Matrix rows U3 and U4. |
| 26 | 2027-04-26 to 2027-05-02 | Static binary, fast cold start and the second-quarter benchmark | Small images and instant restarts make scaling and recovery cheap. Matrix row F1. |

## 2027-05 · Edge compute, analytics, media and traffic steering

WASI HTTP components, real-time analytics, modern image formats, low-latency HLS and DNS-based traffic steering.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 27 | 2027-05-03 to 2027-05-09 | Edge compute on WASI HTTP components | Fastly Compute, Akamai EdgeWorkers and Cloudflare Workers let customers run code at the edge; cachet's plugins are hooks only. Matrix row G2. |
| 28 | 2027-05-10 to 2027-05-16 | Real-time traffic analytics | Fastly, Akamai, Cloudflare and NGINX Plus show live traffic per service; cachet has raw Prometheus counters. Matrix row O7. |
| 29 | 2027-05-17 to 2027-05-23 | Real-time log streaming | Fastly real-time logging, Akamai DataStream and Cloudflare Logpush stream every request to the customer. Matrix row O6. |
| 30 | 2027-05-24 to 2027-05-30 | Low-latency HLS and CMAF chunked delivery | Akamai Adaptive Media Delivery supports low-latency HLS; live video needs it. Matrix row M3. |
| 31 | 2027-05-31 to 2027-06-06 | DNS-based traffic steering | CDNs steer clients to the nearest healthy node; self-hosted fleets need the same. Matrix rows R1 and R3. |

## 2027-06 · Private origins and security

Origin tunnels, a ModSecurity-compatible WAF, bot management and layer 7 DDoS mitigation.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 32 | 2027-06-07 to 2027-06-13 | Private origins over outbound tunnels | Cloudflare Tunnel lets origins stay off the internet; no self-hosted cache offers it. Matrix row R4. |
| 33 | 2027-06-14 to 2027-06-20 | Web application firewall with ModSecurity-compatible rules | Every commercial leader sells a WAF; the OWASP Core Rule Set is the common baseline. Matrix row X1. |
| 34 | 2027-06-21 to 2027-06-27 | Bot management | Akamai Bot Manager, Cloudflare and Fastly sell bot detection; cachet has none. Matrix row X2. |
| 35 | 2027-06-28 to 2027-07-04 | Layer 7 DDoS mitigation | Akamai, Cloudflare and Fastly mitigate HTTP floods automatically. Matrix row X4. |

## 2027-07 · API gateway, instant purge and multi-region

API gateway features, purge under 150 ms across the fleet, multi-region control planes and the third benchmark.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 36 | 2027-07-05 to 2027-07-11 | API gateway features at the edge | Caching APIs needs authentication, transformation and quotas at the same layer; NGINX Plus, Akamai and Varnish sell them. Extends matrix rows G4 and X3. |
| 37 | 2027-07-12 to 2027-07-18 | Instant purge under 150 ms across the fleet | Fastly publishes a 150 ms mean global purge and Cloudflare under 150 ms P50; cachet's purge goes through the Raft log with no published number. Matrix row U2. |
| 38 | 2027-07-19 to 2027-07-25 | Multi-region control planes | Fleets span regions, and one Raft group across continents is slow and fragile. Extends matrix row T3. |
| 39 | 2027-07-26 to 2027-08-01 | Tail latency under overload and the third-quarter benchmark | p99.9 under overload is where outages begin. Matrix row F1. |

## 2027-08 · Console, automation and certificates at scale

Console v2, a Terraform provider, SDKs and a CLI, cache transfer between nodes and a hundred thousand certificates per node.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 40 | 2027-08-02 to 2027-08-08 | Console v2: config editing, diffs and analytics | Varnish Controller, Fastly and Akamai manage everything from their consoles. Extends matrix row O1. |
| 41 | 2027-08-09 to 2027-08-15 | Terraform provider | Fastly, Akamai and Cloudflare are managed with Terraform; enterprises expect the same. Matrix row O3. |
| 42 | 2027-08-16 to 2027-08-22 | SDKs and the cachetctl CLI | Automation outside Terraform needs SDKs and a CLI. Matrix row O2. |
| 43 | 2027-08-23 to 2027-08-29 | Cache transfer between nodes and control-plane backups | New nodes start cold and hammer origins; leaders prewarm. Extends matrix row S5. |
| 44 | 2027-08-30 to 2027-09-05 | Certificates at scale and keyless TLS | CDNs serve hundreds of thousands of customer certificates and offer keyless TLS for keys that never leave the customer. Extends matrix row H9. |

## 2027-09 · Standards, compliance and upgrades

Full HTTP caching conformance, a FIPS build with data residency, per-tenant origin fairness and rolling cluster upgrades.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 45 | 2027-09-06 to 2027-09-12 | Full HTTP caching conformance and targeted cache control | Standards compliance is what lets origins trust a cache. Closes matrix rows H1 and H12. |
| 46 | 2027-09-13 to 2027-09-19 | FIPS build and data residency | Public sector and finance require validated cryptography and data kept in region. Extends matrix row S4. |
| 47 | 2027-09-20 to 2027-09-26 | Per-tenant origin fairness and request queueing | NGINX Plus queues upstream requests; shared fleets need fairness between tenants. Matrix row R5. |
| 48 | 2027-09-27 to 2027-10-03 | Rolling cluster upgrades with mixed versions | Enterprises upgrade fleets without downtime. Extends matrix row O9. |

## 2027-10 · Soak, benchmark and 1.0

A week-long soak with linearizability checks, the final benchmark report, the documentation site and the 1.0 release.

| Week | Dates | Deliverable | Why |
|---|---|---|---|
| 49 | 2027-10-04 to 2027-10-10 | Week-long soak with linearizability checks | Uptime claims need evidence over days. Extends matrix row T3. |
| 50 | 2027-10-11 to 2027-10-17 | Final benchmark report | The year closes with numbers that back the lightweight claim. Matrix row F1. |
| 51 | 2027-10-18 to 2027-10-24 | Documentation site and migration guides | The one documentation-site week of the year, plus guides that lower the cost of switching from the leaders. All matrix rows. |
| 52 | 2027-10-25 to 2027-10-31 | cachet 1.0 | A stable release with frozen formats and APIs. Closes the remaining partial rows of the matrix. |

## Next year

Gaps from the comparison still open after October 2027: a global managed anycast network and operations team (what Fastly, Akamai and Cloudflare sell), video transcoding and packaging (row M2 beyond caching), machine-learning bot scores trained on cross-customer traffic (row X2), PCI DSS and SOC 2 attestations for a hosted offering, and Encrypted Client Hello once rustls support settles.
