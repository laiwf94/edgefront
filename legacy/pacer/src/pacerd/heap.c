#include "pacerd/heap.h"

#include <stdlib.h>

int pd_heap_init(struct pd_heap *h, uint32_t cap)
{
    h->len = 0;
    h->cap = cap ? cap : 16;
    h->items = calloc(h->cap, sizeof *h->items);
    return h->items ? 0 : -1;
}

void pd_heap_free(struct pd_heap *h)
{
    free(h->items);
    h->items = NULL;
    h->len = h->cap = 0;
}

static void place(struct pd_heap *h, uint32_t i, struct pd_timer *t)
{
    h->items[i].t = t;
    t->slot = i + 1;
}

static void sift_up(struct pd_heap *h, uint32_t i)
{
    struct pd_timer *t = h->items[i].t;
    while (i > 0) {
        uint32_t parent = (i - 1) / 2;
        if (h->items[parent].t->deadline <= t->deadline) {
            break;
        }
        place(h, i, h->items[parent].t);
        i = parent;
    }
    place(h, i, t);
}

static void sift_down(struct pd_heap *h, uint32_t i)
{
    struct pd_timer *t = h->items[i].t;
    for (;;) {
        uint32_t child = 2 * i + 1;
        if (child >= h->len) {
            break;
        }
        if (child + 1 < h->len && h->items[child + 1].t->deadline < h->items[child].t->deadline) {
            child++;
        }
        if (t->deadline <= h->items[child].t->deadline) {
            break;
        }
        place(h, i, h->items[child].t);
        i = child;
    }
    place(h, i, t);
}

int pd_heap_set(struct pd_heap *h, struct pd_timer *t, uint64_t deadline)
{
    if (t->slot) {
        uint64_t old = t->deadline;
        t->deadline = deadline;
        if (deadline < old) {
            sift_up(h, t->slot - 1);
        } else {
            sift_down(h, t->slot - 1);
        }
        return 0;
    }
    if (h->len == h->cap) {
        uint32_t cap = h->cap * 2;
        struct pd_heap_slot *items = realloc(h->items, (size_t)cap * sizeof *items);
        if (!items) {
            return -1;
        }
        h->items = items;
        h->cap = cap;
    }
    t->deadline = deadline;
    place(h, h->len++, t);
    sift_up(h, h->len - 1);
    return 0;
}

void pd_heap_remove(struct pd_heap *h, struct pd_timer *t)
{
    if (!t->slot) {
        return;
    }
    uint32_t i = t->slot - 1;
    t->slot = 0;
    h->len--;
    if (i == h->len) {
        return;
    }
    struct pd_timer *last = h->items[h->len].t;
    place(h, i, last);
    if (i > 0 && h->items[(i - 1) / 2].t->deadline > last->deadline) {
        sift_up(h, i);
    } else {
        sift_down(h, i);
    }
}

struct pd_timer *pd_heap_peek(const struct pd_heap *h)
{
    return h->len ? h->items[0].t : NULL;
}

struct pd_timer *pd_heap_pop_due(struct pd_heap *h, uint64_t now)
{
    struct pd_timer *t = pd_heap_peek(h);
    if (!t || t->deadline > now) {
        return NULL;
    }
    pd_heap_remove(h, t);
    return t;
}
