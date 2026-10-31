/*
 * libpacer: rate limiting primitives.
 *
 * Ownership: every object is allocated by the caller or by a *_create
 * function and released by the matching *_destroy. The library never keeps a
 * pointer to caller memory after a call returns.
 *
 * Time: every decision takes `now_ns`, a monotonic timestamp in nanoseconds
 * chosen by the caller. pacer_now_ns() returns one relative to the first call
 * in the process. Passing time explicitly keeps the library free of hidden
 * clock reads and makes tests exact.
 */
#ifndef PACER_H
#define PACER_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define PACER_VERSION "0.1.0"

/* Library version string, for logs and `pacerd -v`. */
const char *pacer_version(void);

/*
 * Monotonic nanoseconds since the first call in this process. Thread-safe.
 * Starting near zero keeps the fixed-point limiter math far from overflow.
 */
uint64_t pacer_now_ns(void);

/* Error codes. Functions that can fail return 0 or one of these. */
enum {
    PACER_OK = 0,
    PACER_EINVAL = -1, /* argument or configuration out of range */
    PACER_ERANGE = -2, /* cost larger than the burst: can never be granted */
    PACER_ENOMEM = -3, /* allocation failed */
    PACER_EFULL = -4,  /* keyed table full; the key was not admitted */
    PACER_ENOENT = -5, /* key not present */
    PACER_ELIMIT = -6, /* concurrency limit for the key reached */
};

/* Short description of an error code, never NULL. */
const char *pacer_strerror(int err);

/*
 * A rate: `tokens` per `period_ns`, with a bucket `burst` tokens deep. A
 * fresh limiter grants `burst` tokens at once, then refills at the rate.
 * Example: 10 per second with burst 20 is {10, 1000000000, 20}.
 */
typedef struct pacer_rate {
    uint64_t tokens;
    uint64_t period_ns;
    uint64_t burst;
} pacer_rate;

/*
 * Internally time is kept in ticks of 1/64 ns so that high byte rates keep
 * their precision. The emission interval is rounded up, so the limiter never
 * grants faster than configured; the relative error is below 1/interval,
 * under 0.2% for rates up to 10^8 per second. Faster rates are capped at
 * one token per tick. Timestamps past PACER_MAX_NOW_NS (about 4.5 years of
 * process uptime) are clamped.
 */
#define PACER_TICK_SHIFT    6
#define PACER_MAX_NOW_NS    ((uint64_t)1 << 57)
#define PACER_MAX_TOL_TICKS ((uint64_t)1 << 62)

/* The outcome of a take. */
typedef struct pacer_decision {
    uint64_t granted;        /* tokens granted; 0 when denied */
    uint64_t remaining;      /* tokens still available after this call */
    uint64_t retry_after_ns; /* when nothing was granted: wait this long */
} pacer_decision;

/*
 * Generic cell rate algorithm limiter. The whole state is one 64-bit
 * theoretical arrival time updated with compare-and-swap, so takes from any
 * number of threads are lock-free. The struct may be embedded anywhere;
 * there is nothing to destroy.
 */
typedef struct pacer_gcra {
    _Atomic uint64_t tat;       /* theoretical arrival time, ticks */
    _Atomic uint64_t interval;  /* ticks per token */
    _Atomic uint64_t tolerance; /* burst * interval, ticks */
} pacer_gcra;

/* Validate a rate and compute its interval and tolerance in ticks. */
int pacer_rate_ticks(const pacer_rate *r, uint64_t *interval, uint64_t *tolerance);

/* Initialise a limiter with a full bucket. Not thread-safe. */
int pacer_gcra_init(pacer_gcra *g, const pacer_rate *r);

/*
 * Take `cost` tokens at `now_ns`, all or nothing. Returns 1 when granted, 0
 * when denied (state unchanged, d->retry_after_ns says when the same cost
 * would fit), or PACER_ERANGE when cost exceeds the burst. A cost of 0
 * takes nothing and only reports `remaining`; it is granted unless a
 * reconfigure has left more debt than the new burst. `d` may be NULL.
 */
int pacer_gcra_take(pacer_gcra *g, uint64_t now_ns, uint64_t cost, pacer_decision *d);

/* Tokens available at `now_ns` without taking any. */
uint64_t pacer_gcra_remaining(pacer_gcra *g, uint64_t now_ns);

/*
 * Take as many tokens as are available, up to `max`. Returns the number
 * granted. When it is 0, d->retry_after_ns says when one token will be
 * ready. Used for byte shaping, where a read is sized by the grant and the
 * unused part handed back with pacer_gcra_refund. `d` may be NULL.
 */
uint64_t pacer_gcra_take_upto(pacer_gcra *g, uint64_t now_ns, uint64_t max, pacer_decision *d);

/*
 * Like take_upto, but grant nothing unless at least `min` tokens are
 * available; then d->retry_after_ns says when `min` will be. Several
 * consumers sharing one limiter use a min so that none of them can keep
 * the others out by taking every token the moment it appears. A min above
 * the burst is lowered to the burst.
 */
uint64_t pacer_gcra_take_range(pacer_gcra *g, uint64_t now_ns, uint64_t min, uint64_t max,
                               pacer_decision *d);

/*
 * Give back `tokens` previously granted and not used. The state never moves
 * before `now_ns`, so a refund cannot create more than a full bucket.
 */
void pacer_gcra_refund(pacer_gcra *g, uint64_t now_ns, uint64_t tokens);

/*
 * Change the rate while other threads keep taking. Debt is kept as time:
 * a client that had to wait 2 s still waits 2 s. A take running at the same
 * moment may combine the old interval with the new tolerance; the result
 * is bounded by the larger of the two configurations.
 */
int pacer_gcra_reconfigure(pacer_gcra *g, const pacer_rate *r);

/*
 * Keyed table: one GCRA state per key (a client address, an API key) with
 * the same rate for every key.
 *
 * Memory is allocated once by pacer_table_create and never grows: the table
 * holds at most `max_entries` keys. It is split into shards, each with its
 * own lock and open-addressing array, so threads working on different keys
 * rarely meet. Keys are hashed with SipHash-2-4 under a random per-table key.
 *
 * All functions are thread-safe. Keys are copied; the caller keeps ownership
 * of the buffer it passes.
 */
#define PACER_KEY_MAX 32

typedef struct pacer_table pacer_table;

/*
 * What to do when a new key arrives and its shard is full. The table first
 * reclaims an idle entry (one whose bucket has refilled and that has no
 * holds, which is identical to a fresh entry, so dropping it changes no
 * decision). If none of the eight least recently used entries is idle:
 *   PACER_FULL_DENY refuses the new key and keeps everyone's debt;
 *   PACER_FULL_EVICT drops the least recently used of those eight that has
 *   no holds, and its debt. Entries with holds are never dropped.
 * DENY is the safe default against an attacker spraying source addresses:
 * EVICT would let them reset other clients' limits.
 */
enum { PACER_FULL_DENY = 0, PACER_FULL_EVICT = 1 };

typedef struct pacer_table_config {
    uint32_t max_entries;    /* capacity in keys, >= 1 */
    uint32_t shards;         /* power of two; 0 picks up to 1024, >= 64 keys each */
    pacer_rate rate;         /* applied to every key */
    const uint8_t *hash_key; /* 16 bytes, or NULL for a random key */
    int full_policy;         /* PACER_FULL_DENY or PACER_FULL_EVICT */
} pacer_table_config;

typedef struct pacer_table_stats {
    uint64_t entries;    /* keys currently held */
    uint64_t capacity;   /* maximum keys */
    uint64_t inserts;    /* keys admitted since create */
    uint64_t removals;   /* keys removed explicitly or by sweep */
    uint64_t reclaims;   /* idle keys dropped to make room */
    uint64_t evictions;  /* keys with debt dropped under PACER_FULL_EVICT */
    uint64_t rejections; /* keys refused because the table was full */
    uint64_t holds;      /* holds currently open across all keys */
} pacer_table_stats;

/* Create a table. On success *out owns the table; free it with destroy. */
int pacer_table_create(const pacer_table_config *cfg, pacer_table **out);

/* Free the table. No other thread may be using it. NULL is a no-op. */
void pacer_table_destroy(pacer_table *t);

/*
 * Take `cost` tokens for `key`, all or nothing, adding the key if needed.
 * Returns 1 granted, 0 denied, PACER_ERANGE if cost exceeds the burst,
 * PACER_EINVAL for an empty or oversized key, or PACER_EFULL when the key
 * is new and there is no room. `d` may be NULL.
 */
int pacer_table_take(pacer_table *t, const void *key, size_t len, uint64_t now_ns, uint64_t cost,
                     pacer_decision *d);

/*
 * Take up to `max` tokens for `key`. Returns PACER_OK with the grant in
 * d->granted (0 means wait d->retry_after_ns), or an error as for take.
 * `d` is required here: a NULL `d` returns PACER_EINVAL.
 */
int pacer_table_take_upto(pacer_table *t, const void *key, size_t len, uint64_t now_ns,
                          uint64_t max, pacer_decision *d);

/* take_range for one key; see pacer_gcra_take_range. */
int pacer_table_take_range(pacer_table *t, const void *key, size_t len, uint64_t now_ns,
                           uint64_t min, uint64_t max, pacer_decision *d);

/* Hand back unused tokens for `key`. Unknown keys are ignored: an evicted
 * key is already at a full bucket. */
void pacer_table_refund(pacer_table *t, const void *key, size_t len, uint64_t now_ns,
                        uint64_t tokens);

/*
 * Concurrency holds: count how many things (connections) a key has open.
 * hold admits one more if the key has fewer than `max_holds` and a take of
 * one token is granted, so a single call enforces both a concurrency cap and
 * an arrival rate. Returns 1 admitted, 0 rate-denied (see d), PACER_ELIMIT
 * at the cap (no token taken), or an error as for take. Every admitted hold
 * must be matched by exactly one release. `d` may be NULL.
 */
int pacer_table_hold(pacer_table *t, const void *key, size_t len, uint64_t now_ns,
                     uint32_t max_holds, pacer_decision *d);

/* Drop one hold. Returns PACER_OK, or PACER_ENOENT if the key has none,
 * which means the caller released more than it held. */
int pacer_table_release(pacer_table *t, const void *key, size_t len);

/* Remove `key`, holds and all. Returns PACER_OK or PACER_ENOENT. */
int pacer_table_remove(pacer_table *t, const void *key, size_t len);

/* Change the per-key rate for every key, keeping debt as time. */
int pacer_table_reconfigure(pacer_table *t, const pacer_rate *r);

/*
 * Remove idle entries from the next `nshards` shards (round robin across
 * calls) and return how many were removed. Each shard is scanned under its
 * lock in one pass. Calling it is optional: inserts reclaim idle entries on
 * their own; sweeping keeps `entries` honest and room ready.
 */
size_t pacer_table_sweep(pacer_table *t, uint64_t now_ns, uint32_t nshards);

/* Snapshot of the counters, summed over shards. */
void pacer_table_get_stats(pacer_table *t, pacer_table_stats *out);

/* Bytes allocated by the table, fixed at create time. */
size_t pacer_table_memory(const pacer_table *t);

/*
 * Sliding-window counter: how many events (bytes, requests) happened in the
 * last `window_ns`. The window is cut into `slots` buckets plus the one
 * being filled. The oldest bucket is weighted by how much of it is still
 * inside the window, so the count is within one bucket's worth of exact.
 *
 * Not thread-safe: each window belongs to one owner (a connection). It is a
 * plain struct with no allocation; embed it and init it.
 */
#define PACER_WINDOW_SLOTS_MAX 16

typedef struct pacer_window {
    uint64_t slot_ns;    /* width of one bucket */
    uint64_t head_start; /* start of the bucket being filled */
    uint64_t total;      /* sum of all buckets */
    uint32_t n;          /* buckets in the ring: slots + 1 */
    uint32_t head;       /* index of the bucket being filled */
    uint64_t counts[PACER_WINDOW_SLOTS_MAX + 1];
} pacer_window;

/* Set up an empty window starting at `now_ns`. `slots` is 1 to
 * PACER_WINDOW_SLOTS_MAX and window_ns must be at least `slots`. */
int pacer_window_init(pacer_window *w, uint64_t window_ns, uint32_t slots, uint64_t now_ns);

/* Record `n` events at `now_ns`. Times earlier than the newest seen are
 * counted as the newest. */
void pacer_window_add(pacer_window *w, uint64_t now_ns, uint64_t n);

/* Events in (now_ns - window_ns, now_ns], with the oldest bucket weighted. */
uint64_t pacer_window_count(pacer_window *w, uint64_t now_ns);

#endif
