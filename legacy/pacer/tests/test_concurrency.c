/*
 * Races on the shared limiter paths. The clock is frozen in each test, so
 * the expected outcome is exact: a lost update shows up as a wrong count,
 * not just as a sanitizer report. Built and run under TSan by the gate.
 */
#include "lib/table_check.h"
#include "pacer.h"
#include "test.h"

#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>

#define SEC     1000000000ull
#define THREADS 8

static atomic_int go;

static void wait_for_go(void)
{
    while (!atomic_load_explicit(&go, memory_order_acquire)) {
    }
}

static void run_threads(void *(*fn)(void *), void *args, size_t arg_size)
{
    pthread_t th[THREADS];
    atomic_store(&go, 0);
    for (int i = 0; i < THREADS; i++) {
        CHECK_EQ_INT(pthread_create(&th[i], NULL, fn, (char *)args + (size_t)i * arg_size), 0);
    }
    atomic_store_explicit(&go, 1, memory_order_release);
    for (int i = 0; i < THREADS; i++) {
        CHECK_EQ_INT(pthread_join(th[i], NULL), 0);
    }
}

struct gcra_arg {
    pacer_gcra *g;
    uint64_t granted;
    uint64_t refunded;
    unsigned seed;
};

static void *take_many(void *p)
{
    struct gcra_arg *a = p;
    wait_for_go();
    for (int i = 0; i < 20000; i++) {
        a->granted += pacer_gcra_take(a->g, 5 * SEC, 1, NULL) == 1;
    }
    return NULL;
}

TEST(race_gcra_grants_exactly_the_burst)
{
    pacer_gcra g;
    pacer_rate r = {100, SEC, 1000};
    pacer_gcra_init(&g, &r);
    struct gcra_arg args[THREADS] = {0};
    for (int i = 0; i < THREADS; i++) {
        args[i].g = &g;
    }
    run_threads(take_many, args, sizeof args[0]);
    uint64_t total = 0;
    for (int i = 0; i < THREADS; i++) {
        total += args[i].granted;
    }
    CHECK_EQ_U64(total, 1000);
}

static unsigned next_rand(unsigned *s)
{
    *s = *s * 1103515245u + 12345u;
    return (*s >> 16) & 0x7fff;
}

static void *upto_and_refund(void *p)
{
    struct gcra_arg *a = p;
    wait_for_go();
    for (int i = 0; i < 20000; i++) {
        uint64_t want = 1 + next_rand(&a->seed) % 64;
        uint64_t got = pacer_gcra_take_upto(a->g, 5 * SEC, want, NULL);
        a->granted += got;
        uint64_t back = got ? next_rand(&a->seed) % (got + 1) : 0;
        if (back) {
            pacer_gcra_refund(a->g, 5 * SEC, back);
            a->refunded += back;
        }
    }
    return NULL;
}

TEST(race_gcra_take_upto_and_refund_conserve_tokens)
{
    pacer_gcra g;
    pacer_rate r = {1000, SEC, 100000};
    pacer_gcra_init(&g, &r);
    struct gcra_arg args[THREADS] = {0};
    for (int i = 0; i < THREADS; i++) {
        args[i].g = &g;
        args[i].seed = (unsigned)i + 1;
    }
    run_threads(upto_and_refund, args, sizeof args[0]);
    uint64_t used = 0;
    for (int i = 0; i < THREADS; i++) {
        used += args[i].granted - args[i].refunded;
    }
    CHECK_EQ_U64(used + pacer_gcra_remaining(&g, 5 * SEC), 100000);
}

struct flip_arg {
    pacer_gcra *g;
    atomic_int *stop;
    uint64_t granted;
    int flipper;
};

static void *flip_or_take(void *p)
{
    struct flip_arg *a = p;
    wait_for_go();
    if (a->flipper) {
        pacer_rate small = {100, SEC, 10}, large = {100, SEC, 50};
        for (int i = 0; i < 20000; i++) {
            (void)pacer_gcra_reconfigure(a->g, i % 2 ? &small : &large);
        }
        atomic_store(a->stop, 1);
        return NULL;
    }
    while (!atomic_load(a->stop)) {
        a->granted += pacer_gcra_take(a->g, 5 * SEC, 1, NULL) == 1;
    }
    return NULL;
}

TEST(race_gcra_reconfigure_while_taking)
{
    /* Same interval, burst flipping between 10 and 50: whatever the
     * interleaving, no more than the larger burst may be granted. */
    pacer_gcra g;
    pacer_rate r = {100, SEC, 10};
    pacer_gcra_init(&g, &r);
    atomic_int stop = 0;
    struct flip_arg args[THREADS] = {0};
    for (int i = 0; i < THREADS; i++) {
        args[i].g = &g;
        args[i].stop = &stop;
        args[i].flipper = i == 0;
    }
    run_threads(flip_or_take, args, sizeof args[0]);
    uint64_t total = 0;
    for (int i = 0; i < THREADS; i++) {
        total += args[i].granted;
    }
    CHECK(total >= 10);
    CHECK(total <= 50);
}

struct table_arg {
    pacer_table *t;
    unsigned seed;
    int errors;
};

static void *table_churn(void *p)
{
    struct table_arg *a = p;
    wait_for_go();
    char key[16];
    uint64_t now = SEC;
    for (int i = 0; i < 20000; i++) {
        int n = snprintf(key, sizeof key, "k%u", next_rand(&a->seed) % 200);
        size_t len = (size_t)n;
        switch (next_rand(&a->seed) % 4) {
        case 0:
            if (pacer_table_hold(a->t, key, len, now, 4, NULL) == 1 &&
                pacer_table_release(a->t, key, len) != PACER_OK) {
                a->errors++;
            }
            break;
        case 1:
            (void)pacer_table_take(a->t, key, len, now, 1, NULL);
            break;
        case 2:
            (void)pacer_table_sweep(a->t, now, 1);
            break;
        default: {
            pacer_decision d;
            if (pacer_table_take_upto(a->t, key, len, now, 3, &d) == PACER_OK && d.granted > 1) {
                pacer_table_refund(a->t, key, len, now, 1);
            }
            break;
        }
        }
        now += 1000;
    }
    return NULL;
}

TEST(race_table_churn_keeps_invariants)
{
    /* 200 keys over 128 slots of capacity in 4 shards: constant reclaim,
     * eviction and sweep across threads. A hold must always be releasable,
     * because held entries are never dropped. */
    pacer_table_config cfg = {128, 4, {1000, SEC, 4}, NULL, PACER_FULL_EVICT};
    pacer_table *t = NULL;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_OK);
    struct table_arg args[THREADS] = {0};
    for (int i = 0; i < THREADS; i++) {
        args[i].t = t;
        args[i].seed = (unsigned)i * 7 + 3;
    }
    run_threads(table_churn, args, sizeof args[0]);
    for (int i = 0; i < THREADS; i++) {
        CHECK_EQ_INT(args[i].errors, 0);
    }
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK_EQ_U64(st.holds, 0);
    CHECK(st.entries <= 128);
    CHECK_EQ_U64(st.inserts, st.entries + st.removals + st.reclaims + st.evictions);
    CHECK(st.evictions > 0);
    CHECK_EQ_INT(pacer_table_check(t), 0);
    pacer_table_destroy(t);
}

static void *table_same_key(void *p)
{
    struct table_arg *a = p;
    wait_for_go();
    for (int i = 0; i < 20000; i++) {
        a->errors += pacer_table_take(a->t, "hot", 3, 5 * SEC, 1, NULL) == 1;
    }
    return NULL;
}

TEST(race_table_one_hot_key_grants_exactly_the_burst)
{
    pacer_table_config cfg = {1024, 0, {100, SEC, 500}, NULL, PACER_FULL_DENY};
    pacer_table *t = NULL;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_OK);
    struct table_arg args[THREADS] = {0};
    for (int i = 0; i < THREADS; i++) {
        args[i].t = t;
    }
    run_threads(table_same_key, args, sizeof args[0]);
    int total = 0;
    for (int i = 0; i < THREADS; i++) {
        total += args[i].errors;
    }
    CHECK_EQ_INT(total, 500);
    pacer_table_destroy(t);
}
