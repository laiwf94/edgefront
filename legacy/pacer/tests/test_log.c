#include "pacerd/log.h"
#include "test.h"

#include <fcntl.h>
#include <string.h>
#include <unistd.h>

static int pipe_fds[2];

static void capture(void)
{
    CHECK_EQ_INT(pipe(pipe_fds), 0);
    pd_log_set_fd(pipe_fds[1]);
}

static size_t drain(char *buf, size_t cap)
{
    pd_log_set_fd(2);
    close(pipe_fds[1]);
    size_t n = 0;
    ssize_t r = 0;
    while (n + 1 < cap && (r = read(pipe_fds[0], buf + n, cap - 1 - n)) > 0) {
        n += (size_t)r;
    }
    buf[n] = '\0';
    close(pipe_fds[0]);
    return n;
}

TEST(log_writes_key_value_lines)
{
    char out[4096];
    capture();
    struct pd_log_line l;
    pd_log_begin(&l, PD_LOG_WARN, "conn closed");
    pd_log_u64(&l, "conn", 17);
    pd_log_str(&l, "peer", "192.0.2.1:5555");
    pd_log_i64(&l, "delta", -3);
    pd_log_str(&l, "empty", "");
    pd_log_str(&l, "null", NULL);
    pd_log_end(&l);
    drain(out, sizeof out);
    CHECK(strncmp(out, "ts=", 3) == 0);
    CHECK(out[26] == 'Z');
    CHECK(strstr(out, " level=warn msg=\"conn closed\" conn=17 peer=192.0.2.1:5555 delta=-3 "
                      "empty=\"\" null=\"\"\n") != NULL);
}

TEST(log_escapes_hostile_values)
{
    char out[4096];
    capture();
    struct pd_log_line l;
    pd_log_begin(&l, PD_LOG_ERROR, "x");
    pd_log_str(&l, "v", "a b\nlevel=error msg=fake\t\"q\"\\\x01\xff\r");
    pd_log_end(&l);
    size_t n = drain(out, sizeof out);
    CHECK(strstr(out, "v=\"a b\\nlevel=error msg=fake\\t\\\"q\\\"\\\\\\x01\\xff\\r\"\n") != NULL);
    /* Exactly one newline: the value cannot start a second line. */
    CHECK(memchr(out, '\n', n) == out + n - 1);
}

TEST(log_filters_by_level)
{
    char out[4096];
    capture();
    pd_log_set_level(PD_LOG_WARN);
    pd_log_msg(PD_LOG_INFO, "hidden");
    struct pd_log_line l;
    pd_log_begin(&l, PD_LOG_DEBUG, "also hidden");
    pd_log_str(&l, "k", "v");
    pd_log_u64(&l, "n", 1);
    pd_log_i64(&l, "i", 1);
    pd_log_end(&l);
    pd_log_msg(PD_LOG_ERROR, "shown");
    pd_log_set_level(PD_LOG_INFO);
    drain(out, sizeof out);
    CHECK(strstr(out, "hidden") == NULL);
    CHECK(strstr(out, "msg=shown") != NULL);
}

TEST(log_truncates_long_lines)
{
    char out[4096], big[3000];
    memset(big, 'x', sizeof big - 1);
    big[sizeof big - 1] = '\0';
    capture();
    struct pd_log_line l;
    pd_log_begin(&l, PD_LOG_INFO, "long");
    pd_log_str(&l, "v", big);
    pd_log_u64(&l, "after", 1);
    pd_log_end(&l);
    size_t n = drain(out, sizeof out);
    CHECK_EQ_U64(n, PD_LOG_LINE_MAX);
    CHECK(strcmp(out + n - 5, " ...\n") == 0);
}

TEST(log_survives_a_closed_fd)
{
    int fds[2];
    CHECK_EQ_INT(pipe(fds), 0);
    close(fds[0]);
    close(fds[1]);
    pd_log_set_fd(fds[1]);
    pd_log_msg(PD_LOG_ERROR, "dropped");
    pd_log_set_fd(2);
}

TEST(log_unknown_level_is_logged_as_error)
{
    char out[4096];
    capture();
    pd_log_msg(42, "odd");
    drain(out, sizeof out);
    CHECK(strstr(out, "level=error msg=odd") != NULL);
}

TEST(log_conn_ids_increase)
{
    uint64_t a = pd_next_conn_id();
    uint64_t b = pd_next_conn_id();
    CHECK(a >= 1);
    CHECK_EQ_U64(b, a + 1);
}

TEST(log_limit_counts_dropped_lines)
{
    struct pd_log_limit lim;
    pd_log_limit_init(&lim, 10, 2);
    uint64_t dropped = 99;
    uint64_t t = 1000000000ull;
    CHECK_EQ_INT(pd_log_allow(&lim, t, &dropped), 1);
    CHECK_EQ_U64(dropped, 0);
    CHECK_EQ_INT(pd_log_allow(&lim, t, &dropped), 1);
    for (int i = 0; i < 5; i++) {
        CHECK_EQ_INT(pd_log_allow(&lim, t, &dropped), 0);
    }
    CHECK_EQ_INT(pd_log_allow(&lim, t + 100000000ull, &dropped), 1);
    CHECK_EQ_U64(dropped, 5);
}
