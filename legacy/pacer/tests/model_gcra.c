/*
 * Model check for the GCRA limiter. Random operation sequences drive
 * pacer_gcra_take, take_upto, take_range, refund, remaining and reconfigure
 * and the same operations on a slow reference: a plain token bucket whose
 * level is an exact fraction, refilled continuously and capped at the burst.
 * The two must agree on every return value and every decision field.
 *
 * The reference knows nothing about arrival times or ticks. What it shares
 * with the library is the documented contract only:
 *   - the refill rate is tokens/period with the time per token rounded up
 *     to the next 1/64 ns (checked here to be never faster than configured
 *     and within 1/interval of it);
 *   - reconfigure keeps debt as time, so the level becomes
 *     burst' - (burst - level) * rate' / rate, which can be negative;
 *   - retry_after_ns is the exact wait rounded up to a whole nanosecond.
 *
 * Every seed is reproducible. The first divergence is reported with its
 * seed, step and the operations leading to it; rerun one seed with
 * PACER_MODEL_SEED=<seed> build/asan/unit model_gcra, or widen the search
 * with PACER_MODEL_SEEDS=<count>.
 */
#include "pacer.h"
#include "test.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

__extension__ typedef __int128 i128;

#define DEFAULT_SEEDS 300
#define STEPS         2000
#define HISTORY       8
#define HORIZON_NS    ((uint64_t)1 << 52) /* well inside the clamp at 2^57 */

/* ---- exact rationals: num/den, den > 0, always reduced ---- */

typedef struct {
    i128 num;
    i128 den;
} rat;

static void rat_overflow(void)
{
    fprintf(stderr, "model_gcra: rational overflow; the generator produced values too large\n");
    abort();
}

static i128 mul_or_die(i128 a, i128 b)
{
    i128 r = 0;
    if (__builtin_mul_overflow(a, b, &r)) {
        rat_overflow();
    }
    return r;
}

static i128 add_or_die(i128 a, i128 b)
{
    i128 r = 0;
    if (__builtin_add_overflow(a, b, &r)) {
        rat_overflow();
    }
    return r;
}

static i128 gcd(i128 a, i128 b)
{
    if (a < 0) {
        a = -a;
    }
    if (b < 0) {
        b = -b;
    }
    while (b != 0) {
        i128 t = a % b;
        a = b;
        b = t;
    }
    return a;
}

static rat rat_make(i128 num, i128 den)
{
    if (den == 0) {
        fprintf(stderr, "model_gcra: division by zero in the reference\n");
        abort();
    }
    if (den < 0) {
        num = -num;
        den = -den;
    }
    i128 g = gcd(num, den);
    if (g > 1) {
        num /= g;
        den /= g;
    }
    return (rat){num, den};
}

static rat rat_int(uint64_t v)
{
    return (rat){(i128)v, 1};
}

static rat rat_add(rat a, rat b)
{
    i128 g = gcd(a.den, b.den);
    i128 num = add_or_die(mul_or_die(a.num, b.den / g), mul_or_die(b.num, a.den / g));
    return rat_make(num, mul_or_die(a.den / g, b.den));
}

static rat rat_neg(rat a)
{
    return (rat){-a.num, a.den};
}

static rat rat_sub(rat a, rat b)
{
    return rat_add(a, rat_neg(b));
}

static rat rat_mul(rat a, rat b)
{
    /* Cross-reduce first so the products stay small. */
    i128 g1 = gcd(a.num, b.den), g2 = gcd(b.num, a.den);
    if (g1 == 0) {
        g1 = 1;
    }
    if (g2 == 0) {
        g2 = 1;
    }
    return rat_make(mul_or_die(a.num / g1, b.num / g2), mul_or_die(a.den / g2, b.den / g1));
}

static rat rat_div(rat a, rat b)
{
    return rat_mul(a, rat_make(b.den, b.num));
}

static int rat_cmp(rat a, rat b)
{
    i128 l = mul_or_die(a.num, b.den), r = mul_or_die(b.num, a.den);
    return (l > r) - (l < r);
}

static rat rat_min(rat a, rat b)
{
    return rat_cmp(a, b) <= 0 ? a : b;
}

/* floor(a) for a >= 0, as an integer. */
static uint64_t rat_floor_nonneg(rat a)
{
    return (uint64_t)(a.num / a.den);
}

/* ceil(a) for a >= 0. */
static uint64_t rat_ceil_nonneg(rat a)
{
    return (uint64_t)((a.num + a.den - 1) / a.den);
}

/* ---- the reference token bucket ---- */

typedef struct {
    uint64_t burst;
    rat rate; /* tokens per ns, after the documented rounding */
    rat level;
    uint64_t now; /* ns of the last operation */
} ref_bucket;

/* The documented rate rule: time per token is period/tokens ns rounded up
 * to the next 1/64 ns. Returns 0 when the library must refuse the rate. */
static int ref_rate(const pacer_rate *r, rat *rate)
{
    if (r->tokens == 0 || r->period_ns == 0 || r->burst == 0) {
        return 0;
    }
    rat per_token = rat_make((i128)r->period_ns, (i128)r->tokens); /* ns */
    rat ticks = rat_mul(per_token, rat_int(64));
    i128 iv = (ticks.num + ticks.den - 1) / ticks.den;
    /* The tolerance, burst * per-token time, is limited to 2^62 ticks. */
    if (mul_or_die(iv, (i128)r->burst) > (i128)PACER_MAX_TOL_TICKS) {
        return 0;
    }
    *rate = rat_make(64, iv);
    return 1;
}

static void ref_advance(ref_bucket *m, uint64_t now)
{
    rat refill = rat_mul(rat_int(now - m->now), m->rate);
    m->level = rat_min(rat_int(m->burst), rat_add(m->level, refill));
    m->now = now;
}

static uint64_t ref_whole(const ref_bucket *m)
{
    return m->level.num <= 0 ? 0 : rat_floor_nonneg(m->level);
}

/* Nanoseconds, rounded up, until the level reaches `want`. */
static uint64_t ref_wait(const ref_bucket *m, uint64_t want)
{
    return rat_ceil_nonneg(rat_div(rat_sub(rat_int(want), m->level), m->rate));
}

static int ref_take(ref_bucket *m, uint64_t now, uint64_t cost, pacer_decision *d)
{
    ref_advance(m, now);
    if (cost > m->burst) {
        *d = (pacer_decision){0, 0, 0};
        return PACER_ERANGE;
    }
    if (rat_cmp(m->level, rat_int(cost)) < 0) {
        *d = (pacer_decision){0, ref_whole(m), ref_wait(m, cost)};
        return 0;
    }
    m->level = rat_sub(m->level, rat_int(cost));
    *d = (pacer_decision){cost, ref_whole(m), 0};
    return 1;
}

static uint64_t ref_take_range(ref_bucket *m, uint64_t now, uint64_t min, uint64_t max,
                               pacer_decision *d)
{
    ref_advance(m, now);
    uint64_t avail = ref_whole(m);
    if (min == 0) {
        min = 1;
    } else if (min > m->burst) {
        min = m->burst;
    }
    *d = (pacer_decision){0, avail, 0};
    if (max == 0) {
        return 0;
    }
    if (avail < min) {
        d->retry_after_ns = ref_wait(m, min);
        return 0;
    }
    uint64_t grant = avail < max ? avail : max;
    m->level = rat_sub(m->level, rat_int(grant));
    *d = (pacer_decision){grant, avail - grant, 0};
    return grant;
}

static void ref_refund(ref_bucket *m, uint64_t now, uint64_t tokens)
{
    ref_advance(m, now);
    m->level = rat_min(rat_int(m->burst), rat_add(m->level, rat_int(tokens)));
}

static uint64_t ref_remaining(ref_bucket *m, uint64_t now)
{
    ref_advance(m, now);
    return ref_whole(m);
}

static int ref_reconfigure(ref_bucket *m, const pacer_rate *r)
{
    rat rate = {0, 1};
    if (!ref_rate(r, &rate)) {
        return PACER_EINVAL;
    }
    /* Debt in ns stays the same; the level is what that debt leaves of the
     * new burst at the new rate. */
    rat debt_ns = rat_div(rat_sub(rat_int(m->burst), m->level), m->rate);
    m->level = rat_sub(rat_int(r->burst), rat_mul(debt_ns, rate));
    m->burst = r->burst;
    m->rate = rate;
    return PACER_OK;
}

/* ---- random generation ---- */

static uint64_t rng_next(uint64_t *s)
{
    /* splitmix64 */
    uint64_t z = (*s += 0x9e3779b97f4a7c15ull);
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
    return z ^ (z >> 31);
}

static uint64_t rng_below(uint64_t *s, uint64_t n)
{
    return n == 0 ? 0 : rng_next(s) % n;
}

/* Roughly log-uniform in [1, 10^digits]. */
static uint64_t rng_log(uint64_t *s, unsigned digits)
{
    uint64_t scale = 1;
    for (uint64_t k = rng_below(s, digits + 1); k > 0; k--) {
        scale *= 10;
    }
    return 1 + rng_below(s, scale);
}

static pacer_rate gen_rate(uint64_t *s)
{
    pacer_rate r = {rng_log(s, 9), rng_log(s, 11), 0};
    switch (rng_below(s, 10)) {
    case 0:
        r.burst = 1;
        break;
    case 1:
        r.burst = rng_log(s, 6);
        break;
    default:
        r.burst = 1 + rng_below(s, 40);
        break;
    }
    /* Now and then an invalid rate, which must be refused by both. */
    switch (rng_below(s, 40)) {
    case 0:
        r.tokens = 0;
        break;
    case 1:
        r.burst = (uint64_t)1 << 62;
        break;
    case 2:
        r.period_ns = 0;
        break;
    default:
        break;
    }
    return r;
}

/* A time step that keeps the bucket moving between empty and full. */
static uint64_t gen_step(uint64_t *s, const ref_bucket *m, uint64_t steps_left)
{
    rat ns_per_token = rat_div(rat_int(1), m->rate);
    uint64_t token_ns = rat_ceil_nonneg(ns_per_token);
    uint64_t dt = 0;
    switch (rng_below(s, 8)) {
    case 0:
    case 1:
        dt = 0; /* several operations at the same instant */
        break;
    case 2:
        dt = 1 + rng_below(s, 3); /* sub-token steps exercise rounding */
        break;
    case 3:
    case 4:
    case 5:
        dt = rng_below(s, token_ns + 1);
        break;
    case 6:
        dt = rng_below(s, token_ns * 4 + 1);
        break;
    default:
        dt = rng_below(s, token_ns * (m->burst < 64 ? m->burst : 64) * 2 + 1);
        break;
    }
    uint64_t room = (HORIZON_NS - m->now) / (steps_left + 1);
    return dt < room ? dt : room;
}

/* ---- the driver ---- */

typedef struct {
    char ops[HISTORY][96];
    unsigned n;
} history;

static void hist_push(history *h, const char *op)
{
    snprintf(h->ops[h->n % HISTORY], sizeof h->ops[0], "%s", op);
    h->n++;
}

static void report(uint64_t seed, unsigned step, const history *h, const char *what,
                   const pacer_decision *want, int want_rc, const pacer_decision *got, int got_rc)
{
    fprintf(stderr, "  model_gcra: divergence at seed %" PRIu64 " step %u: %s\n", seed, step, what);
    unsigned first = h->n > HISTORY ? h->n - HISTORY : 0;
    for (unsigned i = first; i < h->n; i++) {
        fprintf(stderr, "    %s %s\n", i + 1 == h->n ? ">" : " ", h->ops[i % HISTORY]);
    }
    fprintf(stderr,
            "    reference: rc=%d granted=%" PRIu64 " remaining=%" PRIu64 " retry=%" PRIu64 "\n",
            want_rc, want->granted, want->remaining, want->retry_after_ns);
    fprintf(stderr,
            "    library:   rc=%d granted=%" PRIu64 " remaining=%" PRIu64 " retry=%" PRIu64 "\n",
            got_rc, got->granted, got->remaining, got->retry_after_ns);
    fprintf(stderr, "    replay: PACER_MODEL_SEED=%" PRIu64 " unit model_gcra\n", seed);
}

static int same(const pacer_decision *a, int arc, const pacer_decision *b, int brc)
{
    return arc == brc && a->granted == b->granted && a->remaining == b->remaining &&
           a->retry_after_ns == b->retry_after_ns;
}

/* The rounded rate is never faster than configured and is slower by less
 * than one part in the interval. */
static int rate_within_contract(const pacer_rate *r, rat rate)
{
    rat exact = rat_make((i128)r->tokens, (i128)r->period_ns);
    rat iv = rat_div(rat_int(64), rate);
    rat shortfall = rat_div(rat_sub(exact, rate), exact);
    return rat_cmp(rate, exact) <= 0 && rat_cmp(shortfall, rat_div(rat_int(1), iv)) < 0;
}

/* Runs one seed. Returns 0 when the library matched the model throughout. */
static int run_seed(uint64_t seed)
{
    uint64_t s = seed;
    pacer_rate r;
    rat rate = {0, 1};
    do {
        r = gen_rate(&s);
    } while (!ref_rate(&r, &rate));
    ref_bucket m = {r.burst, rate, rat_int(r.burst), 1 + rng_below(&s, 1000000000)};
    pacer_gcra g;
    history h = {0};
    char op[96];
    if (pacer_gcra_init(&g, &r) != PACER_OK || !rate_within_contract(&r, rate)) {
        fprintf(stderr,
                "  model_gcra: seed %" PRIu64 " rate %" PRIu64 "/%" PRIu64 "ns b%" PRIu64
                " refused or outside the rounding contract\n",
                seed, r.tokens, r.period_ns, r.burst);
        return 1;
    }
    snprintf(op, sizeof op, "init %" PRIu64 "/%" PRIu64 "ns burst %" PRIu64, r.tokens, r.period_ns,
             r.burst);
    hist_push(&h, op);

    for (unsigned step = 0; step < STEPS; step++) {
        uint64_t now = m.now + gen_step(&s, &m, STEPS - step);
        pacer_decision want = {0, 0, 0}, got = {0, 0, 0};
        int want_rc = 0, got_rc = 0;
        uint64_t b = m.burst;
        uint64_t kind = rng_below(&s, 100);
        if (kind < 35) {
            uint64_t cost = rng_below(&s, 8) == 0 ? rng_below(&s, b + 3) : rng_below(&s, 4);
            snprintf(op, sizeof op, "t=%" PRIu64 " take %" PRIu64, now, cost);
            want_rc = ref_take(&m, now, cost, &want);
            got_rc = pacer_gcra_take(&g, now, cost, &got);
        } else if (kind < 50) {
            uint64_t max = rng_below(&s, 2 * b + 2);
            snprintf(op, sizeof op, "t=%" PRIu64 " take_upto %" PRIu64, now, max);
            want_rc = (int)ref_take_range(&m, now, 1, max, &want);
            got_rc = (int)pacer_gcra_take_upto(&g, now, max, &got);
        } else if (kind < 65) {
            uint64_t min = rng_below(&s, b + 3), max = rng_below(&s, 2 * b + 2);
            snprintf(op, sizeof op, "t=%" PRIu64 " take_range %" PRIu64 "..%" PRIu64, now, min,
                     max);
            want_rc = (int)ref_take_range(&m, now, min, max, &want);
            got_rc = (int)pacer_gcra_take_range(&g, now, min, max, &got);
        } else if (kind < 80) {
            uint64_t tokens = rng_below(&s, 20) == 0 ? UINT64_MAX : rng_below(&s, b + 2);
            snprintf(op, sizeof op, "t=%" PRIu64 " refund %" PRIu64, now, tokens);
            ref_refund(&m, now, tokens);
            pacer_gcra_refund(&g, now, tokens);
            want.remaining = ref_remaining(&m, now);
            got.remaining = pacer_gcra_remaining(&g, now);
        } else if (kind < 97) {
            snprintf(op, sizeof op, "t=%" PRIu64 " remaining", now);
            want.remaining = ref_remaining(&m, now);
            got.remaining = pacer_gcra_remaining(&g, now);
        } else {
            pacer_rate nr = gen_rate(&s);
            snprintf(op, sizeof op,
                     "t=%" PRIu64 " reconfigure %" PRIu64 "/%" PRIu64 "ns burst %" PRIu64, now,
                     nr.tokens, nr.period_ns, nr.burst);
            ref_advance(&m, now);
            want_rc = ref_reconfigure(&m, &nr);
            got_rc = pacer_gcra_reconfigure(&g, &nr);
            if (want_rc == PACER_OK && !rate_within_contract(&nr, m.rate)) {
                want_rc = 1; /* forces a report below */
            }
            want.remaining = ref_remaining(&m, now);
            got.remaining = pacer_gcra_remaining(&g, now);
        }
        hist_push(&h, op);
        if (!same(&want, want_rc, &got, got_rc)) {
            report(seed, step, &h, op, &want, want_rc, &got, got_rc);
            return 1;
        }
    }
    return 0;
}

static uint64_t env_u64(const char *name, uint64_t dflt)
{
    const char *v = getenv(name); // NOLINT(concurrency-mt-unsafe): env is never written
    if (!v || !*v) {
        return dflt;
    }
    char *end = NULL;
    unsigned long long n = strtoull(v, &end, 10);
    return *end == '\0' ? (uint64_t)n : dflt;
}

TEST(model_gcra_matches_reference_bucket)
{
    const char *one =
        getenv("PACER_MODEL_SEED"); // NOLINT(concurrency-mt-unsafe): env is never written
    if (one && *one) {
        CHECK_EQ_INT(run_seed(env_u64("PACER_MODEL_SEED", 0)), 0);
        return;
    }
    uint64_t seeds = env_u64("PACER_MODEL_SEEDS", DEFAULT_SEEDS);
    uint64_t failed = 0;
    for (uint64_t seed = 1; seed <= seeds && failed < 3; seed++) {
        failed += (uint64_t)run_seed(seed);
    }
    CHECK_EQ_U64(failed, 0);
}

/* The reference itself, on hand-worked cases, so a broken model cannot pass
 * by agreeing with a broken library. */
TEST(model_gcra_reference_by_hand)
{
    pacer_rate r = {10, 1000000000ull, 5}; /* 10/s, burst 5: 100 ms a token */
    rat rate = {0, 1};
    CHECK(ref_rate(&r, &rate));
    ref_bucket m = {5, rate, rat_int(5), 0};
    pacer_decision d;
    CHECK_EQ_INT(ref_take(&m, 0, 5, &d), 1);
    CHECK_EQ_U64(d.remaining, 0);
    CHECK_EQ_INT(ref_take(&m, 0, 2, &d), 0);
    CHECK_EQ_U64(d.retry_after_ns, 200000000ull);
    CHECK_EQ_INT(ref_take(&m, 150000000ull, 1, &d), 1); /* 1.5 tokens back */
    CHECK_EQ_U64(d.remaining, 0);
    CHECK_EQ_U64(ref_take_range(&m, 150000000ull, 2, 9, &d), 0);
    CHECK_EQ_U64(d.retry_after_ns, 150000000ull); /* from 0.5 to 2 tokens */
    ref_refund(&m, 150000000ull, 100);
    CHECK_EQ_U64(ref_remaining(&m, 150000000ull), 5);
    /* Halve the rate with 2 tokens of debt: 200 ms of debt stays 200 ms,
     * which is one token at 5/s, so 4 of the new burst of 5 remain. */
    CHECK_EQ_INT(ref_take(&m, 150000000ull, 2, &d), 1);
    pacer_rate slow = {5, 1000000000ull, 5};
    CHECK_EQ_INT(ref_reconfigure(&m, &slow), PACER_OK);
    CHECK_EQ_U64(ref_remaining(&m, 150000000ull), 4);
    /* Shrinking the burst below the debt leaves a negative level. */
    pacer_rate tiny = {5, 1000000000ull, 1};
    CHECK_EQ_INT(ref_take(&m, 150000000ull, 4, &d), 1);
    CHECK_EQ_INT(ref_reconfigure(&m, &tiny), PACER_OK);
    CHECK(rat_cmp(m.level, rat_int(0)) < 0);
    CHECK_EQ_U64(ref_remaining(&m, 150000000ull), 0);
    pacer_rate bad = {0, 1, 1};
    CHECK_EQ_INT(ref_reconfigure(&m, &bad), PACER_EINVAL);
    /* 3 per 7 ns rounds 7/3 ns up to 150/64 ns, slower but within 1/150. */
    pacer_rate odd = {3, 7, 1};
    CHECK(ref_rate(&odd, &rate));
    CHECK_EQ_INT(rat_cmp(rate, rat_make(64, 150)), 0);
    CHECK(rate_within_contract(&odd, rate));
}
