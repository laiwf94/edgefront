#include "lib/gcra_math.h"
#include "pacer.h"

#include <string.h>

int pacer_window_init(pacer_window *w, uint64_t window_ns, uint32_t slots, uint64_t now_ns)
{
    if (!w || slots == 0 || slots > PACER_WINDOW_SLOTS_MAX || window_ns < slots) {
        return PACER_EINVAL;
    }
    memset(w, 0, sizeof *w);
    w->slot_ns = window_ns / slots;
    w->n = slots + 1;
    w->head_start = now_ns;
    return PACER_OK;
}

/* Move the ring forward so the head bucket contains now. */
static void advance(pacer_window *w, uint64_t now_ns)
{
    if (now_ns < w->head_start + w->slot_ns) {
        return;
    }
    uint64_t steps = (now_ns - w->head_start) / w->slot_ns;
    w->head_start += steps * w->slot_ns;
    if (steps >= w->n) {
        memset(w->counts, 0, sizeof w->counts);
        w->total = 0;
        return;
    }
    while (steps-- > 0) {
        w->head = (w->head + 1) % w->n;
        w->total -= w->counts[w->head];
        w->counts[w->head] = 0;
    }
}

void pacer_window_add(pacer_window *w, uint64_t now_ns, uint64_t n)
{
    advance(w, now_ns);
    w->counts[w->head] += n;
    w->total += n;
}

uint64_t pacer_window_count(pacer_window *w, uint64_t now_ns)
{
    advance(w, now_ns);
    uint64_t into = now_ns > w->head_start ? now_ns - w->head_start : 0;
    uint64_t oldest = w->counts[(w->head + 1) % w->n];
    /* The window starts `into` past the start of the oldest bucket, so only
     * (slot - into) / slot of it still counts. */
    uint64_t keep = (uint64_t)((pacer_u128)oldest * (w->slot_ns - into) / w->slot_ns);
    return w->total - oldest + keep;
}
