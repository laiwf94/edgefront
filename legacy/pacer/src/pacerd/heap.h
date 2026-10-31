/*
 * Binary min-heap of deadlines. Each connection embeds one pd_timer and is
 * in the heap at most once, at its earliest deadline. Updating a deadline
 * is O(log n); the event loop sleeps until the top.
 */
#ifndef PACERD_HEAP_H
#define PACERD_HEAP_H

#include <stdint.h>

struct pd_timer {
    uint64_t deadline;
    uint32_t slot; /* index + 1 in the heap, 0 when not queued */
};

struct pd_heap_slot {
    struct pd_timer *t;
};

struct pd_heap {
    struct pd_heap_slot *items;
    uint32_t len;
    uint32_t cap;
};

int pd_heap_init(struct pd_heap *h, uint32_t cap);
void pd_heap_free(struct pd_heap *h);

/* Queue t at `deadline`, or move it there if already queued. Returns 0, or
 * -1 if the heap could not grow. */
int pd_heap_set(struct pd_heap *h, struct pd_timer *t, uint64_t deadline);

/* Unqueue t if queued. */
void pd_heap_remove(struct pd_heap *h, struct pd_timer *t);

/* Earliest timer, or NULL when empty. */
struct pd_timer *pd_heap_peek(const struct pd_heap *h);

/* Remove and return the earliest timer if its deadline is <= now. */
struct pd_timer *pd_heap_pop_due(struct pd_heap *h, uint64_t now);

#endif
