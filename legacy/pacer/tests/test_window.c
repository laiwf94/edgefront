#include "pacer.h"
#include "test.h"

#define SEC 1000000000ull
#define MS  1000000ull

TEST(window_rejects_bad_config)
{
    pacer_window w;
    CHECK_EQ_INT(pacer_window_init(&w, SEC, 0, 0), PACER_EINVAL);
    CHECK_EQ_INT(pacer_window_init(&w, SEC, PACER_WINDOW_SLOTS_MAX + 1, 0), PACER_EINVAL);
    CHECK_EQ_INT(pacer_window_init(&w, 3, 4, 0), PACER_EINVAL);
    CHECK_EQ_INT(pacer_window_init(NULL, SEC, 4, 0), PACER_EINVAL);
    CHECK_EQ_INT(pacer_window_init(&w, SEC, PACER_WINDOW_SLOTS_MAX, 0), PACER_OK);
}

TEST(window_counts_recent_events)
{
    pacer_window w;
    CHECK_EQ_INT(pacer_window_init(&w, 10 * SEC, 10, 0), PACER_OK);
    pacer_window_add(&w, 0, 5);
    pacer_window_add(&w, 3 * SEC, 7);
    CHECK_EQ_U64(pacer_window_count(&w, 3 * SEC), 12);
    CHECK_EQ_U64(pacer_window_count(&w, 9 * SEC), 12);
}

TEST(window_forgets_old_events)
{
    pacer_window w;
    pacer_window_init(&w, 10 * SEC, 10, 0);
    pacer_window_add(&w, 0, 100);
    /* At 10 s the first bucket is the oldest and fully inside; by 11 s it
     * has slid out completely. */
    CHECK_EQ_U64(pacer_window_count(&w, 10 * SEC), 100);
    CHECK_EQ_U64(pacer_window_count(&w, 10 * SEC + 500 * MS), 50);
    CHECK_EQ_U64(pacer_window_count(&w, 11 * SEC), 0);
}

TEST(window_large_jump_clears_everything)
{
    pacer_window w;
    pacer_window_init(&w, 4 * SEC, 4, SEC);
    for (uint64_t t = 0; t < 4; t++) {
        pacer_window_add(&w, SEC + t * SEC, 10);
    }
    CHECK_EQ_U64(pacer_window_count(&w, 4 * SEC), 40);
    CHECK_EQ_U64(pacer_window_count(&w, 1000 * SEC), 0);
    pacer_window_add(&w, 1000 * SEC, 3);
    CHECK_EQ_U64(pacer_window_count(&w, 1000 * SEC), 3);
}

TEST(window_ignores_time_going_backwards)
{
    pacer_window w;
    pacer_window_init(&w, 4 * SEC, 4, 10 * SEC);
    pacer_window_add(&w, 10 * SEC, 1);
    pacer_window_add(&w, 5 * SEC, 1);
    CHECK_EQ_U64(pacer_window_count(&w, 5 * SEC), 2);
}

TEST(window_stays_within_one_bucket_of_exact)
{
    /* A steady 1 event per ms against a 1 s window in 10 buckets: the exact
     * count is 1000 once warmed up; the estimate must be within 100. */
    pacer_window w;
    pacer_window_init(&w, SEC, 10, 0);
    uint64_t worst = 0;
    for (uint64_t t = 0; t < 5 * SEC; t += MS) {
        pacer_window_add(&w, t, 1);
        if (t >= 2 * SEC) {
            uint64_t c = pacer_window_count(&w, t);
            uint64_t err = c > 1000 ? c - 1000 : 1000 - c;
            worst = err > worst ? err : worst;
        }
    }
    CHECK(worst <= 100);
}
