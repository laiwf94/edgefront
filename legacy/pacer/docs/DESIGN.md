# pacer design

pacer is two things: libpacer, a small C library of rate-limiting primitives, and pacerd, a TCP proxy built on it that limits clients by address and by listener. This document explains what each part does, why it is built the way it is, what it gives up, and how it behaves when things go wrong.

## Goals

The library should make a rate decision in a few nanoseconds without a lock, keep per-client state in memory that is fixed when the table is created, and stay correct when many threads share one limiter. Its API should be small enough to read in one sitting, with time passed in by the caller so tests are exact.

The proxy should protect an upstream from a single client and from floods: cap connections per address and per listener, cap new connections per second, cap bytes per second in both directions, cut off clients that hold connections open by trickling bytes, and never buffer more than a fixed amount per connection. It should drain without losing bytes in flight, reload limits without a restart, and report what it is doing.

Non-goals: TLS, HTTP awareness, DNS, load balancing across several upstreams, and limits shared between machines. pacerd sees bytes on TCP connections and nothing more.

## The limiter: GCRA in one word

A token bucket of depth `burst` refilled at `rate` can be kept as a single number, the theoretical arrival time (TAT): the time at which the bucket would be full again if nothing else arrived. This is the generic cell rate algorithm from ATM traffic management [1, 2]. With emission interval `T = 1/rate` and tolerance `tau = burst * T`, a request of `n` tokens at time `now` is allowed when

    max(TAT, now) + n*T - now <= tau

and on success `TAT` becomes `max(TAT, now) + n*T`. A denial leaves the state alone and the caller learns exactly when the same request would fit: `max(TAT, now) + n*T - now - tau`. The token-bucket view is still available: the tokens left are `(tau - (TAT - now)) / T`.

Because the whole state is one 64-bit word, a take is a load, a little arithmetic and a compare-and-swap. `pacer_gcra_take` loops on `atomic_compare_exchange_weak` until it either succeeds or computes a denial. The word publishes no other data, so relaxed ordering is enough; the CAS alone makes each update atomic [3]. A thread can starve in the CAS loop only if other threads keep succeeding, which means the limiter is making progress for someone, so the operation is lock-free.

Time inside the limiter is kept in ticks of 1/64 ns. Byte limits are the reason: at one byte per nanosecond the interval in whole nanoseconds is 1, and a rate of 3*10^8 bytes per second would round to either 3.33 or 4 ns per byte, an error of up to 25%. With 64 ticks per nanosecond the interval is rounded up to the next tick, so the limiter never grants faster than configured and the error stays below 1/interval: under 0.2% for rates up to 10^8 per second. The cost is the horizon. Timestamps are stored shifted left by six bits, and to leave room for `TAT + n*T` without overflow they are clamped at 2^57 ns, about 4.5 years of process uptime (`pacer_now_ns` counts from the first call in the process, not from boot). All rate conversions use 128-bit intermediates and configurations whose tolerance would exceed 2^62 ticks are rejected at init, so no later addition can wrap.

Three operations were added for the proxy. `take_upto` grants whatever is available up to a maximum, so a read can be sized by the grant. `refund` gives back what a read did not use; it never moves `TAT` before `now`, so refunds cannot create more than a full bucket. `take_range` grants nothing unless at least `min` tokens are available; the reason is fairness, explained under shaping below. `reconfigure` stores a new interval and tolerance with two relaxed stores. A take that runs at the same moment may combine the old interval with the new tolerance for one decision. That error is bounded by the larger of the two configurations, and it buys reloads that never block a reader. Debt is kept as time: a client that had to wait two seconds under the old rate still waits two seconds.

## The keyed table

Per-client limits need a map from client address to limiter state that cannot grow without bound when an attacker sprays source addresses. `pacer_table` allocates all of its memory in `pacer_table_create`, sized by `max_entries`, and never allocates again.

The table is split into a power-of-two number of shards, each a fixed open-addressing array with its own mutex. The top bits of a key's hash pick the shard and the low bits pick the home slot. By default there are as many shards as keep at least 64 keys each, up to 1024. More shards mean fewer threads meeting on one lock; with too few keys per shard, the unevenness of hashing fills some shards while others still have room. Each shard's array is sized to stay at most three-quarters full, and entries are 64 bytes so that one entry never spans two cache lines.

Keys are hashed with SipHash-2-4 [4] under a random 128-bit key drawn per table from the kernel. A predictable hash would let an attacker choose source addresses that all land in one probe chain and turn every lookup into a scan [5]. The implementation is checked against the 64 reference vectors.

Collisions use linear probing, and deletion shifts later members of the probe chain back into the hole instead of leaving tombstones [6, Algorithm R]. An entry at slot `j` may move back into hole `i` only if its home slot is not in the cyclic range `(i, j]`. Without tombstones, lookups never slow down as keys come and go. A test removes keys in scrambled order from a 75%-full shard and checks after every removal that each remaining key is still found from its home slot; an invariant checker also verifies the LRU links.

When a new key arrives at a full shard, the table looks at the eight least recently used entries for an idle one. An entry is idle when its TAT is in the past and it has no concurrency holds: such an entry is indistinguishable from a fresh one, so dropping it changes no decision. This keeps the table clean without a sweeper thread; `pacer_table_sweep` exists to clear idle entries ahead of time, a shard at a time. If no idle entry is found, the policy decides. `PACER_FULL_DENY` refuses the new key and keeps everyone's debt. `PACER_FULL_EVICT` drops the least recently used entry without holds among those same eight, and its debt with it. DENY is the default in pacerd: under a flood of new addresses, existing clients keep their limits and new ones are refused, whereas EVICT would let an attacker reset other clients' limits by cycling through addresses.

Concurrency holds count how many connections a key has open. `pacer_table_hold` admits one more if the key is under its cap and a one-token take is granted, so a single call under one lock checks both the concurrency cap and the connection rate. A refusal at the cap takes no token. Entries with holds are never reclaimed, evicted or swept, which is what makes `release` reliable: a release that finds no hold is reported as an error, because it can only mean the caller released more than it held.

## The sliding-window counter

`pacer_window` counts events in the last `W` nanoseconds with `slots + 1` buckets in a ring that advances lazily on each call. The oldest bucket is only partly inside the window, so it is weighted by the part that is, which keeps the count within one bucket's worth of the exact value; a test with a steady stream measures the worst error. This is the simple bucketed scheme; Datar, Gionis, Indyk and Motwani give the general treatment and its bounds [7]. Windows are not thread-safe by design: each belongs to one connection, owned by one worker.

## pacerd

### Threads and sockets

The main thread blocks SIGTERM, SIGINT and SIGHUP before any other thread exists, so every thread inherits the mask and the signals arrive only through a signalfd. It owns config reloads and the admin socket. Each worker thread runs its own epoll loop, accepts on its own listening sockets and owns the connections it accepted until they close; a connection never moves between threads.

Each worker has its own listening socket per listener with `SO_REUSEPORT`, and the kernel hashes incoming connections across them [8]. The first version shared one socket between workers with `EPOLLEXCLUSIVE`. The proxy benchmark showed every connection of a burst landing on one worker: the waiter woken first accepts the whole queue, and it is woken again for the next burst. A reuseport group has a catch: a second pacerd run by the same user could join it and take a share of the connections. So each address is first bound once without `SO_REUSEPORT`; if that fails, the port is in use and pacerd refuses to start. The window between that probe and the real bind is a few system calls long.

What the threads share is small: the per-listener limiters (the lock-free GCRA and the sharded tables), one atomic connection count per listener, the config objects, and counters. Counters are kept per worker and listener with a single writer each, so an increment is a relaxed load and store rather than a locked add, and the admin thread sums them with relaxed loads.

Config objects are immutable and reference counted. A reload builds a new one, takes one reference for each worker and swaps it into that worker's pending slot with an atomic exchange, then wakes the worker through its eventfd. The worker swaps it in between events and drops its old reference. If a worker had not yet picked up the previous config, the exchange hands that one back and the main thread drops it, so nothing leaks however fast reloads arrive.

### The event loop

Interest is level-triggered and recomputed after every event from the connection's state: read from a side while its direction's buffer has room, it has not sent EOF and shaping is not holding it back; write to a side while there is data buffered for it. A fd with nothing to wait for is removed from epoll entirely rather than left registered with an empty mask. Otherwise a peer that has fully hung up keeps reporting `EPOLLHUP` while the other direction is still draining, and the loop spins.

Closing a connection inside an event batch is the classic epoll hazard: a later event in the same batch can point at the freed connection, and its fd number may already belong to a new connection. Closed connections are therefore unlinked, marked and put on a list that is freed after the batch, and events for marked connections are skipped.

Each connection has at most one entry in the worker's binary min-heap of deadlines: the earliest of its connect timeout, any shaping wait, its idle timeout and its next slow-client check. Updating it is O(log n), and the loop sleeps until the top of the heap.

### Buffers, backpressure and half-close

Each connection gets one buffer per direction, 16 KiB by default, allocated at accept. Nothing else is ever buffered. When a buffer is full, the proxy stops reading from that side, the kernel's receive buffer fills, and TCP flow control slows the sender. A test pushes 256 MiB at a client that does not read and checks that the proxy's resident memory stays flat.

EOF in one direction becomes `shutdown(SHUT_WR)` on the other side once the buffer for that direction has been written out. A client can send its request, close its writing side and still receive the whole response; the connection is closed when both directions have finished.

### Admission

A new connection is checked against the listener's connection cap (an atomic counter), then against its address's concurrency cap and connection rate with one `pacer_table_hold`. Refused connections are closed at once. Every refusal is counted, and refusals are logged through a GCRA of their own, 10 lines per second with a burst of 20 per listener; the next line written reports how many similar lines were dropped. A flood therefore shows up in the log without filling the disk.

Client keys are normalised first. An IPv4-mapped IPv6 address counts as its IPv4 address, so one client cannot count as two. Other IPv6 clients are grouped by prefix, /64 by default, because one host usually controls a whole /64 and per-/128 limits would be no limits at all.

When the process runs out of file descriptors, the pending connection stays in the accept queue, the listening socket stays readable and a level-triggered loop would spin. Each worker keeps a spare descriptor open on `/dev/null`; at `EMFILE` it closes the spare, accepts the connection, closes it at once so the client sees a clean close rather than a hang, and reopens the spare. If even that fails, the worker stops accepting for 100 ms. A test runs pacerd with 20 descriptors and checks both the refusals and that the loop stays idle.

### Shaping

Before each read, the proxy reserves tokens from the client's byte limiter and the listener's, reads at most that many bytes, and refunds what the read did not use. When either limiter is empty, the side is left unread until the retry time and TCP flow control pushes back. Both directions are charged to the client's address.

Reads are granted with a minimum of 4 KiB. Without it, the benchmark exposed a starvation case: a connection whose socket is always readable takes each token the moment it appears, while a connection waiting on a timer always finds the bucket empty. With a minimum, a connection that finds less than 4 KiB available waits until there is enough, and the waiters take turns. This is best-effort fairness, not a scheduler.

### Slow clients and timeouts

A slowloris client sends a byte every few seconds, just enough to stay under any idle timeout, and holds its connection slot for ever. Each connection feeds the bytes it moves, in either direction, into a sliding window. Once a full window has passed, a check every tenth of a window closes the connection if fewer than `min_rate` bytes moved. Connections that shaping held back during the window are exempt, measured from the end of their last wait, because that slowness was ours. The same rule closes connections whose peer stops reading, since nothing moves in either direction. The idle timeout closes connections on which nothing has moved at all.

### Drain and reload

On SIGTERM (or the admin `drain` command) every worker stops accepting and tells the main thread, which closes the listening sockets once no worker can touch them, so new connections are refused immediately. Open connections run to completion. Whatever is still open at `drain_timeout` is closed, and the log says how many were cut. A test sends SIGTERM in the middle of a shaped 4 MiB download and checks that new connections are refused, that the download arrives byte for byte, and that pacerd exits 0.

SIGHUP (or the admin `reload` command) parses the file again. Limits, timeouts, upstream addresses and the log level change in place: limiters are reconfigured, and the new config is handed to the workers. Changes that are tied to a socket or to memory sized at start are refused with a message saying which: listen addresses, listener names, table sizes and policies, the number of workers and the admin socket path. A refused or unparsable file leaves the running config untouched. Limits apply to existing connections as well as new ones, except the buffer size, which a connection keeps from its accept.

### Admin socket and logs

The admin socket is a Unix stream socket created under a 0177 umask, so it is mode 0600 from the moment it exists. It takes one request line per connection (`stats`, `ping`, `reload`, `drain`) and answers once. The main thread serves at most eight admin clients at a time and gives each two seconds to send its line, so a stuck client cannot hold it. A socket file left by a crash is replaced; one that still answers belongs to a live pacerd, and a second instance refuses to start rather than steal it.

Logs are `key=value` lines written with a single `write(2)`, so lines from different threads never interleave. Values taken from the network are quoted and escaped down to printable ASCII, so they cannot start a new line or fake a field. Every line about an accepted connection carries its ID.

## Trade-offs and limits

The keyed table uses a mutex per shard rather than a lock-free design. Critical sections are a hash probe and a few arithmetic operations, and a lock keeps eviction, the LRU list and backward-shift deletion simple. The benchmark shows the cost: with keys spread over 1024 shards, eight threads reach about 1.5 times the single-thread rate, because each operation takes a shard's lock line from whichever core touched it last. At tens of millions of decisions per second the table is far from the proxy's bottleneck, which makes one decision per read of up to 16 KiB.

The proxy copies bytes through user space with `recv` and `send`. `splice(2)` through a pipe would avoid the copy for bulk transfers; it was left out because shaping wants to know and bound how many bytes move per step, and the bulk benchmark already reaches several GiB/s.

Limits are per process. Several pacerd instances, or several machines, each enforce their own; sharing state would need a store and a protocol, which is out of scope.

Addresses are numeric. pacerd does no DNS, which keeps start-up and reloads free of blocking lookups and lets the binary link libc statically.

Connection fairness across workers is the kernel's: `SO_REUSEPORT` hashes connections without regard to how busy each worker is. A worker that falls behind keeps receiving its share.

Rates above 6.4*10^10 per second are capped at one token per tick, and process uptime beyond about 4.5 years freezes the limiter's clock (see the GCRA section).

Listener changes, table sizes and the worker count need a restart. Passing listening sockets between processes would remove that limit and is not implemented.

## Failure modes

An upstream that refuses, cannot be reached or does not answer within `connect_timeout` closes the client connection with a reason in the log. An upstream or client that resets the connection closes both sides. An upstream that accepts and then goes silent is closed by the idle timeout, or sooner by the slow-client check while a client keeps pushing into a full buffer. Tests cover each of these against the real binary.

When the client table is full of active entries under DENY, new clients are refused (`rejected_table_full`) and existing ones keep their limits. When the byte table cannot track a client for the same reason, that client's reads wait and are retried every 50 ms rather than going unshaped. When memory for a new connection cannot be allocated, that connection is refused and everything taken for it is released. When the kernel cannot register a fd with epoll, that connection is closed rather than left deaf.

When pacerd is killed with SIGKILL, open connections die with it. A restart binds the same port at once (`SO_REUSEADDR`) and replaces the stale admin socket file; a test does exactly this.

If a worker hits an unrecoverable error (epoll itself failing), it logs it and the whole process drains and exits 1, rather than run on with fewer workers than configured.

## How it is tested

`make check` runs clang-format, clang-tidy with warnings as errors, a `-Wall -Wextra -Werror` build with more warnings on top, the unit and integration tests under AddressSanitizer and UndefinedBehaviorSanitizer, the same tests under ThreadSanitizer, a 90% line coverage floor measured with llvm-cov across the library, pacerd and pacerctl, and five seconds of fuzzing per parser.

The concurrency tests race eight threads on one limiter and on a small table with the clock frozen, which makes the expected totals exact: a hot key must grant exactly its burst, `take_upto` and `refund` must conserve tokens, and a limiter being reconfigured must never grant more than the larger burst. Replacing the CAS with a plain store makes the first of these fail every time.

A model test checks the limiter against a reference that shares none of its arithmetic: a plain token bucket whose level is an exact fraction, refilled continuously and capped at the burst. Seeded random sequences of `take`, `take_upto`, `take_range`, `refund`, `remaining` and `reconfigure`, with time steps sized to keep the bucket moving between empty and full and with rates from one per 100 s to faster than one per tick, must agree with it on every return value and every decision field. The reference shares only the documented contract: the time per token rounded up to the next tick (checked to be never faster than configured and within one part in the interval), debt kept as time across a reconfigure, and waits rounded up to a whole nanosecond. The first divergence is printed with its seed, its step and the operations leading to it, and `PACER_MODEL_SEED` replays one seed. Its first run found that a refund was capped at one burst, which gave back too little once a reconfigure had shrunk the burst below the debt. Each mutation tried on the decision steps, moving a comparison in take, take_range or refund by one or rounding a wait down, makes it fail on the first seed.

The integration tests start the real pacerd binary, built with the same sanitizers, against test upstreams and check outcomes rather than the absence of crashes: byte-for-byte transfers, timings within bounds, log lines and admin counters that balance. Coverage includes the daemon because the instrumented binary writes its profile when it exits after a drain.

Two libFuzzer targets cover the config parser and the admin request parser. Beyond crashes, each checks its parser's contract: a config that parses is usable and every rate in it is valid; every error message is one printable line naming a line number; the admin parser's verdict agrees with the bytes it was given.

## Toolchain and dependencies

pacer has no dependencies beyond libc. The toolchain is clang 18 from Ubuntu 24.04, in an image whose base is pinned by digest. The apt packages inside it come from the archive at build time and are not pinned to exact versions; they only build and check the code and never reach the runtime image, which is built `FROM scratch` with statically linked binaries. `scripts/audit.sh` checks that the binaries link only the C library, that the sources include only C library, POSIX and Linux headers, that base images are pinned by digest, and that CI actions are pinned to commits.

## References

1. The ATM Forum, Traffic Management Specification Version 4.0, af-tm-0056.000. Defines the generic cell rate algorithm in its virtual scheduling and continuous-state leaky bucket forms.
2. ITU-T Recommendation I.371, Traffic control and congestion control in B-ISDN.
3. ISO/IEC 9899 (C17), section 7.17, Atomics.
4. J.-P. Aumasson and D. J. Bernstein, SipHash: a fast short-input PRF. INDOCRYPT.
5. S. A. Crosby and D. S. Wallach, Denial of Service via Algorithmic Complexity Attacks. USENIX Security Symposium.
6. D. E. Knuth, The Art of Computer Programming, Volume 3: Sorting and Searching, section 6.4, Algorithm R (deletion with linear probing).
7. M. Datar, A. Gionis, P. Indyk and R. Motwani, Maintaining Stream Statistics over Sliding Windows. SIAM Journal on Computing.
8. Linux man-pages: socket(7) for `SO_REUSEPORT`, epoll(7) and epoll_ctl(2) for level-triggered interest and `EPOLLEXCLUSIVE`, signalfd(2), eventfd(2).
9. RFC 9293, Transmission Control Protocol, section 3.6 on closing a connection, for half-close.
10. RFC 2697, A Single Rate Three Color Marker, for the token bucket as used in traffic policing.
