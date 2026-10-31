#include "lib/gcra_math.h"
#include "pacer.h"

#include <stdatomic.h>

const char *pacer_strerror(int err)
{
    switch (err) {
    case PACER_OK:
        return "ok";
    case PACER_EINVAL:
        return "invalid argument";
    case PACER_ERANGE:
        return "cost exceeds burst";
    case PACER_ENOMEM:
        return "out of memory";
    case PACER_EFULL:
        return "table full";
    case PACER_ENOENT:
        return "no such key";
    case PACER_ELIMIT:
        return "concurrency limit reached";
    default:
        return "unknown error";
    }
}

int pacer_rate_ticks(const pacer_rate *r, uint64_t *interval, uint64_t *tolerance)
{
    if (!r || r->tokens == 0 || r->period_ns == 0 || r->burst == 0) {
        return PACER_EINVAL;
    }
    /* interval = ceil(period * 64 / tokens), in 128 bits so nothing wraps. */
    pacer_u128 num = (pacer_u128)r->period_ns << PACER_TICK_SHIFT;
    pacer_u128 iv = (num + r->tokens - 1) / r->tokens;
    pacer_u128 tol = iv * r->burst;
    if (tol > PACER_MAX_TOL_TICKS) {
        return PACER_EINVAL;
    }
    *interval = (uint64_t)iv;
    *tolerance = (uint64_t)tol;
    return PACER_OK;
}

int pacer_gcra_init(pacer_gcra *g, const pacer_rate *r)
{
    uint64_t iv = 0, tol = 0;
    int rc = pacer_rate_ticks(r, &iv, &tol);
    if (rc != PACER_OK) {
        return rc;
    }
    atomic_init(&g->tat, 0);
    atomic_init(&g->interval, iv);
    atomic_init(&g->tolerance, tol);
    return PACER_OK;
}

/*
 * The state is a single word and publishes nothing else, so relaxed ordering
 * is enough everywhere below: the CAS alone makes each update atomic, and a
 * failed CAS reloads the current value for the next attempt.
 */

int pacer_gcra_take(pacer_gcra *g, uint64_t now_ns, uint64_t cost, pacer_decision *d)
{
    pacer_decision local;
    if (!d) {
        d = &local;
    }
    uint64_t iv = atomic_load_explicit(&g->interval, memory_order_relaxed);
    uint64_t tol = atomic_load_explicit(&g->tolerance, memory_order_relaxed);
    uint64_t now = pacer_ticks(now_ns);
    if (cost > tol / iv) {
        *d = (pacer_decision){0, 0, 0};
        return PACER_ERANGE;
    }
    uint64_t tat = atomic_load_explicit(&g->tat, memory_order_relaxed);
    uint64_t next = 0;
    while (pacer_step_take(tat, now, cost, iv, tol, &next, d)) {
        if (atomic_compare_exchange_weak_explicit(&g->tat, &tat, next, memory_order_relaxed,
                                                  memory_order_relaxed)) {
            return 1;
        }
    }
    return 0;
}

uint64_t pacer_gcra_remaining(pacer_gcra *g, uint64_t now_ns)
{
    uint64_t iv = atomic_load_explicit(&g->interval, memory_order_relaxed);
    uint64_t tol = atomic_load_explicit(&g->tolerance, memory_order_relaxed);
    uint64_t now = pacer_ticks(now_ns);
    uint64_t tat = atomic_load_explicit(&g->tat, memory_order_relaxed);
    return pacer_avail(tat > now ? tat - now : 0, tol, iv);
}

uint64_t pacer_gcra_take_upto(pacer_gcra *g, uint64_t now_ns, uint64_t max, pacer_decision *d)
{
    return pacer_gcra_take_range(g, now_ns, 1, max, d);
}

uint64_t pacer_gcra_take_range(pacer_gcra *g, uint64_t now_ns, uint64_t min, uint64_t max,
                               pacer_decision *d)
{
    pacer_decision local;
    if (!d) {
        d = &local;
    }
    uint64_t iv = atomic_load_explicit(&g->interval, memory_order_relaxed);
    uint64_t tol = atomic_load_explicit(&g->tolerance, memory_order_relaxed);
    uint64_t now = pacer_ticks(now_ns);
    uint64_t tat = atomic_load_explicit(&g->tat, memory_order_relaxed);
    uint64_t next = 0, grant = 0;
    while ((grant = pacer_step_range(tat, now, min, max, iv, tol, &next, d)) != 0) {
        if (atomic_compare_exchange_weak_explicit(&g->tat, &tat, next, memory_order_relaxed,
                                                  memory_order_relaxed)) {
            return grant;
        }
    }
    return 0;
}

void pacer_gcra_refund(pacer_gcra *g, uint64_t now_ns, uint64_t tokens)
{
    uint64_t iv = atomic_load_explicit(&g->interval, memory_order_relaxed);
    uint64_t now = pacer_ticks(now_ns);
    uint64_t tat = atomic_load_explicit(&g->tat, memory_order_relaxed);
    for (;;) {
        uint64_t next = pacer_step_refund(tat, now, tokens, iv);
        if (next == tat || atomic_compare_exchange_weak_explicit(
                               &g->tat, &tat, next, memory_order_relaxed, memory_order_relaxed)) {
            return;
        }
    }
}

int pacer_gcra_reconfigure(pacer_gcra *g, const pacer_rate *r)
{
    uint64_t iv = 0, tol = 0;
    int rc = pacer_rate_ticks(r, &iv, &tol);
    if (rc != PACER_OK) {
        return rc;
    }
    atomic_store_explicit(&g->interval, iv, memory_order_relaxed);
    atomic_store_explicit(&g->tolerance, tol, memory_order_relaxed);
    return PACER_OK;
}
