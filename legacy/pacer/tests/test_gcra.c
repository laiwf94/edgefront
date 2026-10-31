#include "pacer.h"
#include "test.h"

#include <string.h>

#define SEC 1000000000ull
#define MS  1000000ull

TEST(gcra_rejects_invalid_rates)
{
    pacer_gcra g;
    pacer_rate zero_tokens = {0, SEC, 1};
    pacer_rate zero_period = {1, 0, 1};
    pacer_rate zero_burst = {1, SEC, 0};
    pacer_rate huge_burst = {1, SEC, UINT64_MAX};
    CHECK_EQ_INT(pacer_gcra_init(&g, &zero_tokens), PACER_EINVAL);
    CHECK_EQ_INT(pacer_gcra_init(&g, &zero_period), PACER_EINVAL);
    CHECK_EQ_INT(pacer_gcra_init(&g, &zero_burst), PACER_EINVAL);
    CHECK_EQ_INT(pacer_gcra_init(&g, &huge_burst), PACER_EINVAL);
    CHECK_EQ_INT(pacer_gcra_init(&g, NULL), PACER_EINVAL);
}

TEST(gcra_burst_then_deny)
{
    pacer_gcra g;
    pacer_rate r = {10, SEC, 5};
    CHECK_EQ_INT(pacer_gcra_init(&g, &r), PACER_OK);
    uint64_t now = 42 * SEC;
    pacer_decision d;
    for (int i = 0; i < 5; i++) {
        CHECK_EQ_INT(pacer_gcra_take(&g, now, 1, &d), 1);
        CHECK_EQ_U64(d.granted, 1);
        CHECK_EQ_U64(d.remaining, (uint64_t)(4 - i));
    }
    CHECK_EQ_INT(pacer_gcra_take(&g, now, 1, &d), 0);
    CHECK_EQ_U64(d.granted, 0);
    CHECK_EQ_U64(d.remaining, 0);
    CHECK_EQ_U64(d.retry_after_ns, 100 * MS);
}

TEST(gcra_retry_after_is_exact)
{
    pacer_gcra g;
    pacer_rate r = {10, SEC, 5};
    pacer_gcra_init(&g, &r);
    uint64_t now = SEC;
    pacer_decision d;
    while (pacer_gcra_take(&g, now, 1, &d) == 1) {
    }
    uint64_t wait = d.retry_after_ns;
    CHECK_EQ_INT(pacer_gcra_take(&g, now + wait - 1, 1, NULL), 0);
    CHECK_EQ_INT(pacer_gcra_take(&g, now + wait, 1, NULL), 1);
    /* A denied multi-token take reports when that whole cost would fit. */
    CHECK_EQ_INT(pacer_gcra_take(&g, now + wait, 3, &d), 0);
    CHECK_EQ_INT(pacer_gcra_take(&g, now + wait + d.retry_after_ns, 3, NULL), 1);
}

TEST(gcra_steady_rate_never_denied)
{
    pacer_gcra g;
    pacer_rate r = {1000, SEC, 1};
    pacer_gcra_init(&g, &r);
    uint64_t denied = 0;
    for (uint64_t i = 0; i < 100000; i++) {
        denied += pacer_gcra_take(&g, SEC + i * MS, 1, NULL) != 1;
    }
    CHECK_EQ_U64(denied, 0);
}

TEST(gcra_long_run_rate_matches_config)
{
    /* Hammer every microsecond for 10 s at 100/s, burst 10: exactly
     * 10 + 100 * 10 tokens are granted. */
    pacer_gcra g;
    pacer_rate r = {100, SEC, 10};
    pacer_gcra_init(&g, &r);
    uint64_t granted = 0;
    for (uint64_t t = 0; t <= 10 * SEC; t += 1000) {
        granted += pacer_gcra_take(&g, SEC + t, 1, NULL) == 1;
    }
    CHECK_EQ_U64(granted, 1010);
}

TEST(gcra_cost_larger_than_burst_is_an_error)
{
    pacer_gcra g;
    pacer_rate r = {10, SEC, 5};
    pacer_gcra_init(&g, &r);
    pacer_decision d;
    memset(&d, 0xff, sizeof d);
    CHECK_EQ_INT(pacer_gcra_take(&g, SEC, 6, &d), PACER_ERANGE);
    CHECK_EQ_U64(d.granted, 0);
    CHECK_EQ_INT(pacer_gcra_take(&g, SEC, 5, &d), 1);
}

TEST(gcra_zero_cost_reports_without_taking)
{
    pacer_gcra g;
    pacer_rate r = {10, SEC, 5};
    pacer_gcra_init(&g, &r);
    pacer_decision d;
    CHECK_EQ_INT(pacer_gcra_take(&g, SEC, 0, &d), 1);
    CHECK_EQ_U64(d.remaining, 5);
    CHECK_EQ_U64(pacer_gcra_remaining(&g, SEC), 5);
}

TEST(gcra_zero_cost_is_denied_while_debt_exceeds_the_burst)
{
    pacer_gcra g;
    pacer_rate big = {1000, SEC, 100};
    pacer_gcra_init(&g, &big);
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, SEC, 100, NULL), 100);
    pacer_rate small = {1000, SEC, 10};
    CHECK_EQ_INT(pacer_gcra_reconfigure(&g, &small), PACER_OK);
    /* 100 ms of debt against a 10 ms bucket: 90 ms until the level is 0. */
    pacer_decision d;
    CHECK_EQ_INT(pacer_gcra_take(&g, SEC, 0, &d), 0);
    CHECK_EQ_U64(d.retry_after_ns, 90 * MS);
    CHECK_EQ_INT(pacer_gcra_take(&g, SEC + 90 * MS, 0, &d), 1);
    CHECK_EQ_U64(d.remaining, 0);
}

TEST(gcra_remaining_refills_over_time)
{
    pacer_gcra g;
    pacer_rate r = {10, SEC, 5};
    pacer_gcra_init(&g, &r);
    CHECK_EQ_INT(pacer_gcra_take(&g, SEC, 5, NULL), 1);
    CHECK_EQ_U64(pacer_gcra_remaining(&g, SEC), 0);
    CHECK_EQ_U64(pacer_gcra_remaining(&g, SEC + 250 * MS), 2);
    CHECK_EQ_U64(pacer_gcra_remaining(&g, SEC + 10 * SEC), 5);
}

TEST(gcra_rounds_interval_up_never_over_grants)
{
    /* 3 per second does not divide a second evenly in ticks. Rounding the
     * interval up means a full second never yields more than 3 + burst. */
    pacer_gcra g;
    pacer_rate r = {3, SEC, 1};
    pacer_gcra_init(&g, &r);
    uint64_t granted = 0;
    for (uint64_t t = 0; t < 3 * SEC; t += MS) {
        granted += pacer_gcra_take(&g, SEC + t, 1, NULL) == 1;
    }
    CHECK(granted <= 1 + 9);
    CHECK(granted >= 9);
}

TEST(gcra_high_byte_rates_keep_precision)
{
    /* 1 GB/s is exactly 64 ticks per byte. */
    uint64_t iv = 0, tol = 0;
    pacer_rate r = {1000000000ull, SEC, 1 << 20};
    CHECK_EQ_INT(pacer_rate_ticks(&r, &iv, &tol), PACER_OK);
    CHECK_EQ_U64(iv, 64);
    CHECK_EQ_U64(tol, 64ull << 20);
    pacer_rate too_fast = {UINT64_MAX, 1, 1};
    CHECK_EQ_INT(pacer_rate_ticks(&too_fast, &iv, &tol), PACER_OK);
    CHECK_EQ_U64(iv, 1);
}

TEST(gcra_clamps_time_past_the_horizon)
{
    pacer_gcra g;
    pacer_rate r = {1, SEC, 1};
    pacer_gcra_init(&g, &r);
    CHECK_EQ_INT(pacer_gcra_take(&g, UINT64_MAX, 1, NULL), 1);
    CHECK_EQ_INT(pacer_gcra_take(&g, UINT64_MAX, 1, NULL), 0);
}

TEST(strerror_covers_every_code)
{
    CHECK(strcmp(pacer_strerror(PACER_OK), "ok") == 0);
    CHECK(strcmp(pacer_strerror(PACER_EINVAL), "invalid argument") == 0);
    CHECK(strcmp(pacer_strerror(PACER_ERANGE), "cost exceeds burst") == 0);
    CHECK(strcmp(pacer_strerror(PACER_ENOMEM), "out of memory") == 0);
    CHECK(strcmp(pacer_strerror(PACER_EFULL), "table full") == 0);
    CHECK(strcmp(pacer_strerror(PACER_ENOENT), "no such key") == 0);
    CHECK(strcmp(pacer_strerror(PACER_ELIMIT), "concurrency limit reached") == 0);
    CHECK(strcmp(pacer_strerror(12345), "unknown error") == 0);
}

TEST(gcra_take_upto_grants_what_is_available)
{
    pacer_gcra g;
    pacer_rate r = {1000, SEC, 100};
    pacer_gcra_init(&g, &r);
    pacer_decision d;
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, SEC, 60, &d), 60);
    CHECK_EQ_U64(d.remaining, 40);
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, SEC, 60, &d), 40);
    CHECK_EQ_U64(d.remaining, 0);
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, SEC, 60, &d), 0);
    CHECK_EQ_U64(d.retry_after_ns, MS);
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, SEC + MS, 60, NULL), 1);
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, SEC + 11 * MS, 60, NULL), 10);
}

TEST(gcra_take_upto_zero_asks_for_nothing)
{
    pacer_gcra g;
    pacer_rate r = {10, SEC, 1};
    pacer_gcra_init(&g, &r);
    pacer_decision d;
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, SEC, 0, &d), 0);
    CHECK_EQ_U64(d.retry_after_ns, 0);
    CHECK_EQ_U64(d.remaining, 1);
}

TEST(gcra_refund_restores_state)
{
    pacer_gcra g;
    pacer_rate r = {1000, SEC, 100};
    pacer_gcra_init(&g, &r);
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, SEC, 100, NULL), 100);
    pacer_gcra_refund(&g, SEC, 30);
    CHECK_EQ_U64(pacer_gcra_remaining(&g, SEC), 30);
    pacer_gcra_refund(&g, SEC, 70);
    CHECK_EQ_U64(pacer_gcra_remaining(&g, SEC), 100);
}

TEST(gcra_refund_never_exceeds_a_full_bucket)
{
    pacer_gcra g;
    pacer_rate r = {1000, SEC, 100};
    pacer_gcra_init(&g, &r);
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, SEC, 10, NULL), 10);
    pacer_gcra_refund(&g, SEC, 500);
    CHECK_EQ_U64(pacer_gcra_remaining(&g, SEC), 100);
    pacer_gcra_refund(&g, SEC, UINT64_MAX);
    CHECK_EQ_U64(pacer_gcra_remaining(&g, SEC), 100);
    /* Refunding into an idle limiter changes nothing. */
    pacer_gcra_refund(&g, 5 * SEC, 10);
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, 5 * SEC, 1000, NULL), 100);
}

TEST(gcra_reconfigure_keeps_debt_in_time)
{
    pacer_gcra g;
    pacer_rate slow = {1, SEC, 1};
    pacer_gcra_init(&g, &slow);
    CHECK_EQ_INT(pacer_gcra_take(&g, SEC, 1, NULL), 1);
    pacer_rate fast = {1000, SEC, 1};
    CHECK_EQ_INT(pacer_gcra_reconfigure(&g, &fast), PACER_OK);
    pacer_decision d;
    /* The client owed one second at the old rate; that wait still stands. */
    CHECK_EQ_INT(pacer_gcra_take(&g, SEC, 1, &d), 0);
    CHECK_EQ_U64(d.retry_after_ns, SEC);
    CHECK_EQ_INT(pacer_gcra_take(&g, 2 * SEC - 1, 1, NULL), 0);
    CHECK_EQ_INT(pacer_gcra_take(&g, 2 * SEC, 1, NULL), 1);
    pacer_rate bad = {0, SEC, 1};
    CHECK_EQ_INT(pacer_gcra_reconfigure(&g, &bad), PACER_EINVAL);
}

TEST(gcra_refund_after_a_shrink_gives_back_every_token)
{
    /* 100 tokens of debt at 1 ms each, then the burst shrinks to 10: the
     * debt stays 100 ms, 90 tokens more than the new bucket holds. */
    pacer_gcra g;
    pacer_rate big = {1000, SEC, 100};
    pacer_gcra_init(&g, &big);
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, SEC, 100, NULL), 100);
    pacer_rate small = {1000, SEC, 10};
    CHECK_EQ_INT(pacer_gcra_reconfigure(&g, &small), PACER_OK);
    /* Giving back 50 leaves 50 ms of debt, so one token is 41 ms away. A
     * refund capped at one burst would leave it 81 ms away. */
    pacer_gcra_refund(&g, SEC, 50);
    pacer_decision d;
    CHECK_EQ_INT(pacer_gcra_take(&g, SEC, 1, &d), 0);
    CHECK_EQ_U64(d.retry_after_ns, 41 * MS);
    pacer_gcra_refund(&g, SEC, UINT64_MAX);
    CHECK_EQ_U64(pacer_gcra_remaining(&g, SEC), 10);
}

TEST(gcra_take_range_waits_for_the_minimum)
{
    pacer_gcra g;
    pacer_rate r = {1000, SEC, 100};
    pacer_gcra_init(&g, &r);
    pacer_decision d;
    CHECK_EQ_U64(pacer_gcra_take_upto(&g, SEC, 95, NULL), 95);
    /* 5 left: a minimum of 10 is not met; 10 are ready 5 ms later. */
    CHECK_EQ_U64(pacer_gcra_take_range(&g, SEC, 10, 50, &d), 0);
    CHECK_EQ_U64(d.remaining, 5);
    CHECK_EQ_U64(d.retry_after_ns, 5 * MS);
    CHECK_EQ_U64(pacer_gcra_take_range(&g, SEC + 5 * MS, 10, 50, &d), 10);
    /* A minimum above the burst is lowered to the burst. */
    CHECK_EQ_U64(pacer_gcra_take_range(&g, 10 * SEC, 1000, 1000, &d), 100);
    /* A minimum of 0 behaves like 1. */
    CHECK_EQ_U64(pacer_gcra_take_range(&g, 10 * SEC, 0, 5, &d), 0);
    CHECK_EQ_U64(d.retry_after_ns, MS);
}
