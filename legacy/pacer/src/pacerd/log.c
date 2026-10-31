#include "pacerd/log.h"

#include <errno.h>
#include <inttypes.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static atomic_int min_level = PD_LOG_INFO;
static atomic_int out_fd = 2;
static _Atomic uint64_t conn_ids;

static const char *const level_names[] = {"debug", "info", "warn", "error"};

/* Room kept free at the end of every line for the truncation marker and
 * the newline. */
#define RESERVE 5

void pd_log_set_level(int level)
{
    atomic_store(&min_level, level);
}

void pd_log_set_fd(int fd)
{
    atomic_store(&out_fd, fd);
}

uint64_t pd_next_conn_id(void)
{
    return atomic_fetch_add_explicit(&conn_ids, 1, memory_order_relaxed) + 1;
}

static void put(struct pd_log_line *l, const char *s, size_t n)
{
    size_t room = sizeof l->buf - RESERVE - l->len;
    if (n > room) {
        n = room;
    }
    memcpy(l->buf + l->len, s, n);
    l->len += n;
}

static int needs_quotes(const char *v)
{
    if (!*v) {
        return 1;
    }
    for (; *v; v++) {
        unsigned char c = (unsigned char)*v;
        if (c <= ' ' || c >= 0x7f || c == '"' || c == '=' || c == '\\') {
            return 1;
        }
    }
    return 0;
}

static void put_value(struct pd_log_line *l, const char *v)
{
    if (!needs_quotes(v)) {
        put(l, v, strlen(v));
        return;
    }
    put(l, "\"", 1);
    for (; *v; v++) {
        unsigned char c = (unsigned char)*v;
        char esc[5];
        switch (c) {
        case '"':
            put(l, "\\\"", 2);
            break;
        case '\\':
            put(l, "\\\\", 2);
            break;
        case '\n':
            put(l, "\\n", 2);
            break;
        case '\t':
            put(l, "\\t", 2);
            break;
        case '\r':
            put(l, "\\r", 2);
            break;
        default:
            if (c < 0x20 || c >= 0x7f) {
                (void)snprintf(esc, sizeof esc, "\\x%02x", c);
                put(l, esc, 4);
            } else {
                put(l, (const char *)&c, 1);
            }
        }
    }
    put(l, "\"", 1);
}

static void put_key(struct pd_log_line *l, const char *key)
{
    put(l, " ", 1);
    put(l, key, strlen(key));
    put(l, "=", 1);
}

void pd_log_begin(struct pd_log_line *l, int level, const char *msg)
{
    l->len = 0;
    if (level < PD_LOG_DEBUG || level > PD_LOG_ERROR) {
        level = PD_LOG_ERROR;
    }
    l->skip = level < atomic_load(&min_level);
    if (l->skip) {
        return;
    }
    struct timespec ts = {0, 0};
    struct tm tm;
    char stamp[48];
    (void)clock_gettime(CLOCK_REALTIME, &ts);
    (void)gmtime_r(&ts.tv_sec, &tm);
    int n =
        snprintf(stamp, sizeof stamp, "ts=%04d-%02d-%02dT%02d:%02d:%02d.%03ldZ", tm.tm_year + 1900,
                 tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec, ts.tv_nsec / 1000000);
    put(l, stamp, (size_t)n);
    put_key(l, "level");
    put(l, level_names[level], strlen(level_names[level]));
    put_key(l, "msg");
    put_value(l, msg);
}

void pd_log_str(struct pd_log_line *l, const char *key, const char *value)
{
    if (l->skip) {
        return;
    }
    put_key(l, key);
    put_value(l, value ? value : "");
}

void pd_log_u64(struct pd_log_line *l, const char *key, uint64_t value)
{
    char num[24];
    if (l->skip) {
        return;
    }
    int n = snprintf(num, sizeof num, "%" PRIu64, value);
    put_key(l, key);
    put(l, num, (size_t)n);
}

void pd_log_i64(struct pd_log_line *l, const char *key, int64_t value)
{
    char num[24];
    if (l->skip) {
        return;
    }
    int n = snprintf(num, sizeof num, "%" PRId64, value);
    put_key(l, key);
    put(l, num, (size_t)n);
}

void pd_log_end(struct pd_log_line *l)
{
    if (l->skip) {
        return;
    }
    if (l->len == sizeof l->buf - RESERVE) {
        memcpy(l->buf + l->len, " ...", 4);
        l->len += 4;
    }
    l->buf[l->len++] = '\n';
    int fd = atomic_load(&out_fd);
    size_t off = 0;
    while (off < l->len) {
        ssize_t w = write(fd, l->buf + off, l->len - off);
        if (w < 0 && errno == EINTR) {
            continue;
        }
        if (w <= 0) {
            /* Nowhere left to report a logging failure; drop the line. */
            return;
        }
        off += (size_t)w;
    }
}

void pd_log_msg(int level, const char *msg)
{
    struct pd_log_line l;
    pd_log_begin(&l, level, msg);
    pd_log_end(&l);
}

void pd_log_limit_init(struct pd_log_limit *lim, uint64_t per_sec, uint64_t burst)
{
    pacer_rate r = {per_sec, 1000000000ull, burst};
    /* The rate comes from code, never from config, so it is always valid. */
    (void)pacer_gcra_init(&lim->gcra, &r);
    atomic_init(&lim->suppressed, 0);
}

int pd_log_allow(struct pd_log_limit *lim, uint64_t now_ns, uint64_t *dropped)
{
    if (pacer_gcra_take(&lim->gcra, now_ns, 1, NULL) != 1) {
        atomic_fetch_add_explicit(&lim->suppressed, 1, memory_order_relaxed);
        return 0;
    }
    *dropped = atomic_exchange_explicit(&lim->suppressed, 0, memory_order_relaxed);
    return 1;
}
