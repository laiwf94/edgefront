/*
 * Limiter decisions per second: one GCRA single-threaded and contended,
 * against a mutex-guarded version of the same algorithm, and the keyed
 * table with spread and hot keys.
 */
#include "pacer.h"

#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define OPS_PER_THREAD 4000000ull
#define SEC            1000000000ull

static double wall(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

/* The same GCRA under a mutex, as a baseline for the lock-free version. */
struct locked_gcra {
    pthread_mutex_t mu;
    uint64_t tat, iv, tol;
};

static int locked_take(struct locked_gcra *g, uint64_t now)
{
    pthread_mutex_lock(&g->mu);
    uint64_t base = g->tat > now ? g->tat : now;
    int ok = base + g->iv - now <= g->tol;
    if (ok) {
        g->tat = base + g->iv;
    }
    pthread_mutex_unlock(&g->mu);
    return ok;
}

enum kind { K_GCRA, K_LOCKED, K_TABLE_SPREAD, K_TABLE_HOT };

struct job {
    enum kind kind;
    int id;
    pacer_gcra *g;
    struct locked_gcra *lg;
    pacer_table *t;
    uint64_t allowed;
};

static atomic_int go;

static void *worker(void *arg)
{
    struct job *j = arg;
    while (!atomic_load_explicit(&go, memory_order_acquire)) {
    }
    /* Time advances 50 ns per decision per thread, so a mix of grants and
     * denials is exercised rather than one or the other. */
    uint64_t now = SEC;
    char key[16];
    for (uint64_t i = 0; i < OPS_PER_THREAD; i++, now += 50) {
        switch (j->kind) {
        case K_GCRA:
            j->allowed += pacer_gcra_take(j->g, now, 1, NULL) == 1;
            break;
        case K_LOCKED:
            j->allowed += (uint64_t)locked_take(j->lg, now << 6);
            break;
        case K_TABLE_SPREAD: {
            uint32_t k = (uint32_t)(i * 2654435761u) % 10000 + (uint32_t)j->id * 10000;
            memcpy(key, &k, sizeof k);
            j->allowed += pacer_table_take(j->t, key, sizeof k, now, 1, NULL) == 1;
            break;
        }
        case K_TABLE_HOT:
            j->allowed += pacer_table_take(j->t, "hot", 3, now, 1, NULL) == 1;
            break;
        }
    }
    return NULL;
}

static double run(enum kind kind, int threads)
{
    pacer_gcra g;
    pacer_rate r = {10000000, SEC, 1000};
    pacer_gcra_init(&g, &r);
    struct locked_gcra lg;
    pthread_mutex_init(&lg.mu, NULL);
    lg.tat = 0;
    pacer_rate_ticks(&r, &lg.iv, &lg.tol);
    pacer_table *t = NULL;
    pacer_table_config tc = {200000, 0, r, NULL, PACER_FULL_EVICT};
    if (pacer_table_create(&tc, &t) != PACER_OK) {
        abort();
    }
    struct job jobs[16];
    pthread_t th[16];
    atomic_store(&go, 0);
    for (int i = 0; i < threads; i++) {
        jobs[i] = (struct job){kind, i, &g, &lg, t, 0};
        pthread_create(&th[i], NULL, worker, &jobs[i]);
    }
    double t0 = wall();
    atomic_store_explicit(&go, 1, memory_order_release);
    for (int i = 0; i < threads; i++) {
        pthread_join(th[i], NULL);
    }
    double secs = wall() - t0;
    pacer_table_destroy(t);
    pthread_mutex_destroy(&lg.mu);
    return (double)OPS_PER_THREAD * threads / secs;
}

int main(void)
{
    static const struct {
        enum kind kind;
        const char *name;
    } kinds[] = {
        {K_GCRA, "gcra, one shared limiter (lock-free)"},
        {K_LOCKED, "gcra, one shared limiter (mutex baseline)"},
        {K_TABLE_SPREAD, "table, 10k keys per thread"},
        {K_TABLE_HOT, "table, one hot key"},
    };
    static const int threads[] = {1, 2, 4, 8};
    printf("%-44s %12s %12s %12s %12s\n", "decisions per second (millions)", "1 thread",
           "2 threads", "4 threads", "8 threads");
    for (size_t k = 0; k < sizeof kinds / sizeof kinds[0]; k++) {
        printf("%-44s", kinds[k].name);
        for (size_t i = 0; i < sizeof threads / sizeof threads[0]; i++) {
            printf(" %12.1f", run(kinds[k].kind, threads[i]) / 1e6);
            fflush(stdout);
        }
        printf("\n");
    }
    return 0;
}
