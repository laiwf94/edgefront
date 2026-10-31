#include "pacer.h"
#include "test.h"

#include <stdio.h>
#include <string.h>

TEST(clock_is_monotonic_and_starts_near_zero)
{
    uint64_t a = pacer_now_ns();
    uint64_t b = pacer_now_ns();
    CHECK(a >= 1);
    CHECK(b >= a);
    /* The epoch is set on first use, so a test process sees small values. */
    CHECK(a < 3600ull * 1000000000ull);
}

TEST(version_matches_header)
{
    CHECK(strcmp(pacer_version(), PACER_VERSION) == 0);
}
