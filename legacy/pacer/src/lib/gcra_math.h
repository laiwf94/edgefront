/*
 * GCRA decision steps as pure functions of the arrival time. The lock-free
 * limiter runs them inside a CAS loop; the keyed table runs them under its
 * shard lock. All times are in ticks.
 */
#ifndef PACER_GCRA_MATH_H
#define PACER_GCRA_MATH_H

#include "pacer.h"

/* 128-bit intermediates keep rate conversions exact; both supported
 * compilers provide the type. */
__extension__ typedef unsigned __int128 pacer_u128;

static inline uint64_t pacer_ticks(uint64_t now_ns)
{
    if (now_ns >= PACER_MAX_NOW_NS) {
        now_ns = PACER_MAX_NOW_NS - 1;
    }
    return now_ns << PACER_TICK_SHIFT;
}

static inline uint64_t pacer_ticks_to_ns_ceil(uint64_t ticks)
{
    return (ticks >> PACER_TICK_SHIFT) + ((ticks & ((1u << PACER_TICK_SHIFT) - 1)) != 0);
}

/* Whole tokens available when the bucket is `debt` ticks behind. */
static inline uint64_t pacer_avail(uint64_t debt, uint64_t tol, uint64_t iv)
{
    return debt >= tol ? 0 : (tol - debt) / iv;
}

/*
 * All-or-nothing take. The caller has checked cost <= tol / iv, so
 * cost * iv cannot overflow. Returns 1 and sets *next when granted.
 */
static inline int pacer_step_take(uint64_t tat, uint64_t now, uint64_t cost, uint64_t iv,
                                  uint64_t tol, uint64_t *next, pacer_decision *d)
{
    uint64_t base = tat > now ? tat : now;
    uint64_t want = base + cost * iv;
    if (want - now > tol) {
        d->granted = 0;
        d->remaining = pacer_avail(base - now, tol, iv);
        d->retry_after_ns = pacer_ticks_to_ns_ceil(want - now - tol);
        return 0;
    }
    *next = want;
    d->granted = cost;
    d->remaining = pacer_avail(want - now, tol, iv);
    d->retry_after_ns = 0;
    return 1;
}

/*
 * Take between min and max tokens: as many as are available up to max, or
 * nothing if fewer than min are. Returns the grant; *next is set only when
 * it is non-zero. A min above the burst is lowered to the burst, and a min
 * of 0 counts as 1.
 */
static inline uint64_t pacer_step_range(uint64_t tat, uint64_t now, uint64_t min, uint64_t max,
                                        uint64_t iv, uint64_t tol, uint64_t *next,
                                        pacer_decision *d)
{
    uint64_t base = tat > now ? tat : now;
    uint64_t avail = pacer_avail(base - now, tol, iv);
    uint64_t depth = tol / iv;
    if (min == 0) {
        min = 1;
    } else if (min > depth) {
        min = depth;
    }
    d->granted = 0;
    d->remaining = avail;
    d->retry_after_ns = 0;
    if (max == 0) {
        return 0;
    }
    if (avail < min) {
        /* min tokens fit once base - now drops to tol - min * iv. */
        d->retry_after_ns = pacer_ticks_to_ns_ceil(base - now + min * iv - tol);
        return 0;
    }
    uint64_t grant = avail < max ? avail : max;
    d->granted = grant;
    d->remaining = avail - grant;
    *next = base + grant * iv;
    return grant;
}

/*
 * Arrival time after giving back `tokens`. Never moves before now, so a
 * refund cannot create more than a full bucket. The debt can exceed the
 * tolerance after a reconfigure shrinks the burst, so a refund is measured
 * against the debt, not capped at one burst. When tokens <= debt / iv,
 * tokens * iv <= debt and cannot overflow.
 */
static inline uint64_t pacer_step_refund(uint64_t tat, uint64_t now, uint64_t tokens, uint64_t iv)
{
    if (tat <= now) {
        return tat;
    }
    uint64_t debt = tat - now;
    return tokens > debt / iv ? now : tat - tokens * iv;
}

#endif
