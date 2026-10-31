#include "pacerd/heap.h"
#include "test.h"

#include <stdlib.h>

static int heap_ok(const struct pd_heap *h)
{
    for (uint32_t i = 0; i < h->len; i++) {
        if (h->items[i].t->slot != i + 1) {
            return 0;
        }
        if (i > 0 && h->items[(i - 1) / 2].t->deadline > h->items[i].t->deadline) {
            return 0;
        }
    }
    return 1;
}

TEST(heap_orders_and_grows)
{
    struct pd_heap h;
    CHECK_EQ_INT(pd_heap_init(&h, 2), 0);
    struct pd_timer t[100] = {0};
    for (int i = 0; i < 100; i++) {
        CHECK_EQ_INT(pd_heap_set(&h, &t[i], (uint64_t)((i * 37) % 100)), 0);
    }
    CHECK(heap_ok(&h));
    CHECK(pd_heap_pop_due(&h, 0) != NULL);
    CHECK(pd_heap_pop_due(&h, 0) == NULL);
    uint64_t last = 0;
    int popped = 1;
    struct pd_timer *x = NULL;
    while ((x = pd_heap_pop_due(&h, 1000)) != NULL) {
        CHECK(x->deadline >= last);
        CHECK_EQ_U64(x->slot, 0);
        last = x->deadline;
        popped++;
    }
    CHECK_EQ_INT(popped, 100);
    CHECK(pd_heap_peek(&h) == NULL);
    pd_heap_free(&h);
}

TEST(heap_update_and_remove)
{
    struct pd_heap h;
    CHECK_EQ_INT(pd_heap_init(&h, 0), 0);
    struct pd_timer t[64] = {0};
    unsigned seed = 7;
    for (int i = 0; i < 64; i++) {
        pd_heap_set(&h, &t[i], (uint64_t)i * 10);
    }
    for (int round = 0; round < 2000; round++) {
        seed = seed * 1103515245u + 12345u;
        struct pd_timer *p = &t[(seed >> 16) % 64];
        switch ((seed >> 8) % 3) {
        case 0:
            pd_heap_remove(&h, p);
            break;
        default:
            CHECK_EQ_INT(pd_heap_set(&h, p, (seed >> 4) % 1000), 0);
            break;
        }
        if (!heap_ok(&h)) {
            CHECK(heap_ok(&h));
            break;
        }
    }
    /* Removing something not queued is a no-op. */
    struct pd_timer lone = {0};
    pd_heap_remove(&h, &lone);
    pd_heap_free(&h);
}
