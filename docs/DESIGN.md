# edgefront design notes

The first two sections cover the workspace and the steer port. The rest are cachet's design notes, which still describe the cache.

## The workspace

edgefront is one Cargo workspace. Every crate lives under `crates/` and takes its version, edition, lints and dependency pins from the root `Cargo.toml`, and the whole workspace shares one `Cargo.lock`. `crates/edgefront` is the binary. `edgefront-core` holds what every product needs: the HTTP/1.1 listener with steer's connection limits and graceful drain, the pooled client, steer's config schema, metrics and admin plumbing. `edgefront-proxy` is steer's routing and request path, `edgefront-cache` is cachet's engine, `edgefront-limiter` is pacer's algorithms, and `edgefront-difftest` diffs the ports against the legacy programs. The root package is cachet as it was before the split, kept until its sources have all moved into the member crates. `legacy/steer` (Go) and `legacy/pacer` (C) keep building and testing in CI, with their full history, until their ports pass their differential suites. Each is then deleted, and its history stays reachable through the merge commit.

## Porting steer: the request path and the differential tests

The port keeps steer's behaviour exactly, byte for byte where bytes are visible, so every decision can be diffed against the Go code before the Go code goes. That means porting the parts of Go's standard library steer leaned on, not only steer's own code. The config reader follows `encoding/json`'s rules (case-insensitive keys, duplicate keys merging, invalid UTF-8 becoming U+FFFD). Durations, addresses, `strconv.Quote` and `path.Clean` follow Go. Request targets are decoded and re-encoded with `net/url`'s rules, and Go's server checks (`Host` validity, `OPTIONS *`, `CONNECT`) come first.

The HTTP/1.1 request path is `edgefront_proxy::server::Proxy`, a handler for the core listener:

1. Go's server checks.
2. Route on the host and the cleaned path.
3. Take a place under the cluster's `max_requests`.
4. Remove hop-by-hop headers and the headers `Connection` names, rewrite the forwarding headers, and set the request ID.
5. Send the upstream target: the client's own encoding when cleaning changed nothing, Go's encoding of the cleaned path otherwise.

The body keeps the client's framing, because Go's transport copied `TransferEncoding`: a chunked body stays chunked even after steer buffered it for replay. The cluster's `timeout` covers the whole exchange, so it answers `504` before the response head and cuts the body after it. The place under `max_requests` is held until the response body is dropped, which is when steer's handler returned.

Three layers check the port:

- **Unit and property tests** use Go's own test vectors and, for the trickier functions, fixtures recorded by running steer's Go code.
- **`edgefront-difftest`** (`make difftest`, in CI on every push) feeds 10,000 generated configs and 100,000 generated requests to `legacy/steer/cmd/diffdrive` and to the Rust crates. It fails on any difference in acceptance, error text, route choice or cleaned path.
- **`crates/edgefront/tests/e2e_port_http1.rs`** starts the binary with `legacy/steer/examples/steer.json` and three recording fake upstreams. It sends 30 raw requests: the README's traffic, path cleaning, spoofed forwarding headers, request IDs, hop-by-hop headers both ways, body framing, upstream errors and the requests Go refuses. It then compares the response and the upstream's view of each request with a transcript recorded from steer. `make e2e-steer`, also in CI, runs steer live on the same cases and requires it to still produce that transcript.

Until the balancer port lands, each cluster takes turns over its endpoints of the most preferred priority. The end-to-end transcript only records which priority level answered, so it holds when steer's `least_loaded` picks either primary.

## What cachet is for

cachet is a shared HTTP cache that sits in front of one or more origins, close to clients. It should follow the HTTP caching rules closely enough that an origin can control it entirely through standard headers, keep the origin's load flat when popular objects expire, keep serving when the origin is slow or down if the origin allows it, and let an operator remove content from every node with one request.

It is a single binary with no external services. A node keeps its cache in memory with an optional disk tier, and nodes coordinate only to spread purges.

It does not terminate TLS, speak HTTP/2 or HTTP/3, run edge code, rewrite cache keys, or fetch from other nodes before the origin. Each of these is real work that a production edge needs, and each would hide the parts this project is about.

## Following one request

A request is handled in `proxy.rs`. GET and HEAD go through the cache; every other method passes through to the origin. A request with `Cache-Control: no-store` also passes through.

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

Clients get 10 seconds to send their request head, which may be at most 64 KiB; larger heads get 431. Request targets over 8 KiB get 414. Request bodies over 10 MiB get 413, checked against Content-Length before reading and enforced while streaming otherwise. A request whose `Via` names this node gets 508. Origin bodies are buffered only up to the per-object limit (8 MiB); the declared length is checked before reading, and a body that turns out larger streams through to the one client that asked, uncached, without buffering the rest. Only the first 64 surrogate keys of a response are kept, keys over 128 bytes are dropped, and a URL keeps at most eight Vary variants, so an origin varying on something like User-Agent cannot fill the cache with one URL.

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
| Shutdown | Stops accepting, waits up to 10 s for open requests, flushes the disk queue. Queued purge deliveries are lost. |

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
