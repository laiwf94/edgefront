/*
 * Structured logs: one `key=value` line per event, written with a single
 * write(2) so lines from different threads never interleave.
 *
 *   ts=2024-01-02T03:04:05.678Z level=info msg="conn closed" conn=17 ...
 *
 * Values are quoted when they contain spaces, quotes or '=', and every byte
 * outside printable ASCII is escaped, so a value taken from the network can
 * neither start a new line nor fake another field.
 */
#ifndef PACERD_LOG_H
#define PACERD_LOG_H

#include "pacer.h"

#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>

#define PD_LOG_LINE_MAX 1024

enum { PD_LOG_DEBUG, PD_LOG_INFO, PD_LOG_WARN, PD_LOG_ERROR };

struct pd_log_line {
    char buf[PD_LOG_LINE_MAX];
    size_t len;
    int skip; /* below the configured level: every call is a no-op */
};

/* Set the minimum level and the output fd (stderr by default). */
void pd_log_set_level(int level);
void pd_log_set_fd(int fd);

void pd_log_begin(struct pd_log_line *l, int level, const char *msg);
void pd_log_str(struct pd_log_line *l, const char *key, const char *value);
void pd_log_u64(struct pd_log_line *l, const char *key, uint64_t value);
void pd_log_i64(struct pd_log_line *l, const char *key, int64_t value);
void pd_log_end(struct pd_log_line *l);

/* Shorthand for a line with just a message. */
void pd_log_msg(int level, const char *msg);

/*
 * Rate limit for noisy lines (rejections under a flood). Lines over the
 * limit are counted, and the next line allowed reports how many were
 * dropped, so the log stays readable and nothing is silently lost.
 */
struct pd_log_limit {
    pacer_gcra gcra;
    _Atomic uint64_t suppressed;
};

void pd_log_limit_init(struct pd_log_limit *lim, uint64_t per_sec, uint64_t burst);

/* Returns 1 if a line may be written now, and stores the number dropped
 * since the last allowed line in *dropped. Returns 0 and counts otherwise. */
int pd_log_allow(struct pd_log_limit *lim, uint64_t now_ns, uint64_t *dropped);

/* Process-wide connection IDs, starting at 1. */
uint64_t pd_next_conn_id(void);

#endif
