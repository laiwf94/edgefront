#include "pacer.h"

#include <pthread.h>
#include <time.h>

static pthread_once_t epoch_once = PTHREAD_ONCE_INIT;
static uint64_t epoch_ns;

static uint64_t raw_monotonic_ns(void)
{
    struct timespec ts = {0, 0};
    /* POSIX lists only EINVAL (unsupported clock) and EFAULT for this call.
     * CLOCK_MONOTONIC is mandatory and ts is valid, so neither can happen. */
    (void)clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

static void set_epoch(void)
{
    epoch_ns = raw_monotonic_ns();
}

const char *pacer_version(void)
{
    return PACER_VERSION;
}

uint64_t pacer_now_ns(void)
{
    /* pthread_once orders the epoch write before every later read, and the
     * clock read below happens after it, so the result never underflows. The
     * +1 keeps 0 free as a "never" value for callers. pthread_once cannot
     * fail with a valid once object. */
    (void)pthread_once(&epoch_once, set_epoch);
    return raw_monotonic_ns() - epoch_ns + 1;
}
