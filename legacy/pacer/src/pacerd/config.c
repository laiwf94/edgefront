#include "pacerd/config.h"
#include "pacerd/util.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define MAX_TOKENS 8
#define SEC        1000000000ull
#define MAX_SIZE   ((uint64_t)1 << 50)
#define MAX_DUR    (86400ull * SEC)

struct parser {
    int line;
    char *err;
    size_t errlen;
    struct pd_config *cfg;
    struct pd_listener_conf *cur; /* open listener block, or NULL */
    uint32_t seen;                /* bit per directive already set in this scope */
    uint32_t top_seen;            /* the top-level mask while inside a block */
};

static int fail(struct parser *p, const char *fmt, ...) __attribute__((format(printf, 2, 3)));

static int fail(struct parser *p, const char *fmt, ...)
{
    int n = snprintf(p->err, p->errlen, "line %d: ", p->line);
    if (n >= 0 && (size_t)n < p->errlen) {
        va_list ap;
        va_start(ap, fmt);
        (void)vsnprintf(p->err + n, p->errlen - (size_t)n, fmt, ap);
        va_end(ap);
    }
    return -1;
}

/* Copy a token for an error message, replacing anything unprintable so a
 * hostile file cannot put control bytes into the log. */
static const char *shown(const char *tok, char *buf, size_t len)
{
    size_t i = 0;
    for (; tok[i] && i + 4 < len; i++) {
        unsigned char c = (unsigned char)tok[i];
        if (c >= 0x20 && c < 0x7f && c != '"') {
            buf[i] = tok[i];
        } else {
            buf[i] = '?';
        }
    }
    if (tok[i]) {
        buf[i++] = '.';
        buf[i++] = '.';
        buf[i++] = '.';
    }
    buf[i] = '\0';
    return buf;
}

static int parse_u64(const char *s, uint64_t *out)
{
    if (!*s) {
        return -1;
    }
    uint64_t v = 0;
    for (; *s; s++) {
        if (*s < '0' || *s > '9') {
            return -1;
        }
        uint64_t d = (uint64_t)(*s - '0');
        if (v > (UINT64_MAX - d) / 10) {
            return -1;
        }
        v = v * 10 + d;
    }
    *out = v;
    return 0;
}

/* Split a number from its unit suffix: "16k" -> "16", "k". */
static int split_unit(const char *s, char *num, size_t numlen, const char **unit)
{
    size_t i = 0;
    while (s[i] >= '0' && s[i] <= '9') {
        i++;
    }
    if (i == 0 || i >= numlen) {
        return -1;
    }
    memcpy(num, s, i);
    num[i] = '\0';
    *unit = s + i;
    return 0;
}

static int mul_ok(uint64_t a, uint64_t b, uint64_t max, uint64_t *out)
{
    if (a != 0 && b > max / a) {
        return -1;
    }
    *out = a * b;
    return 0;
}

/* A byte size: digits with an optional k, m or g (powers of 1024). */
static int parse_size(const char *s, uint64_t *out)
{
    char num[24];
    const char *unit = NULL;
    uint64_t v = 0, mul = 1;
    if (split_unit(s, num, sizeof num, &unit) != 0 || parse_u64(num, &v) != 0) {
        return -1;
    }
    if (strcmp(unit, "k") == 0) {
        mul = 1024;
    } else if (strcmp(unit, "m") == 0) {
        mul = 1024ull * 1024;
    } else if (strcmp(unit, "g") == 0) {
        mul = 1024ull * 1024 * 1024;
    } else if (*unit) {
        return -1;
    }
    return mul_ok(v, mul, MAX_SIZE, out);
}

/* A duration: digits with a required ms, s, m or h. */
static int parse_duration(const char *s, uint64_t *out)
{
    char num[24];
    const char *unit = NULL;
    uint64_t v = 0, mul = 0;
    if (split_unit(s, num, sizeof num, &unit) != 0 || parse_u64(num, &v) != 0) {
        return -1;
    }
    if (strcmp(unit, "ms") == 0) {
        mul = 1000000;
    } else if (strcmp(unit, "s") == 0) {
        mul = SEC;
    } else if (strcmp(unit, "m") == 0) {
        mul = 60 * SEC;
    } else if (strcmp(unit, "h") == 0) {
        mul = 3600 * SEC;
    } else {
        return -1;
    }
    return mul_ok(v, mul, MAX_DUR, out);
}

/* "10/s", "1m/s" (sizes allowed), "600/m", "100/h". */
static int parse_rate_value(const char *s, pacer_rate *r)
{
    const char *slash = strchr(s, '/');
    char num[32];
    if (!slash || (size_t)(slash - s) >= sizeof num) {
        return -1;
    }
    memcpy(num, s, (size_t)(slash - s));
    num[slash - s] = '\0';
    uint64_t tokens = 0;
    if (parse_size(num, &tokens) != 0 || tokens == 0) {
        return -1;
    }
    uint64_t period = 0;
    if (strcmp(slash + 1, "s") == 0) {
        period = SEC;
    } else if (strcmp(slash + 1, "m") == 0) {
        period = 60 * SEC;
    } else if (strcmp(slash + 1, "h") == 0) {
        period = 3600 * SEC;
    } else {
        return -1;
    }
    r->tokens = tokens;
    r->period_ns = period;
    r->burst = tokens;
    return 0;
}

int pd_addr_parse(const char *s, struct pd_addr *out)
{
    char host[64];
    const char *port_s = NULL;
    memset(out, 0, sizeof *out);
    if (s[0] == '[') {
        const char *end = strchr(s, ']');
        if (!end || end[1] != ':' || (size_t)(end - s - 1) >= sizeof host) {
            return -1;
        }
        memcpy(host, s + 1, (size_t)(end - s - 1));
        host[end - s - 1] = '\0';
        port_s = end + 2;
    } else {
        const char *colon = strrchr(s, ':');
        if (!colon || (size_t)(colon - s) >= sizeof host) {
            return -1;
        }
        memcpy(host, s, (size_t)(colon - s));
        host[colon - s] = '\0';
        port_s = colon + 1;
    }
    uint64_t port = 0;
    if (parse_u64(port_s, &port) != 0 || port == 0 || port > 65535) {
        return -1;
    }
    struct sockaddr_in *v4 = (struct sockaddr_in *)&out->ss;
    struct sockaddr_in6 *v6 = (struct sockaddr_in6 *)&out->ss;
    if (s[0] != '[' && inet_pton(AF_INET, host, &v4->sin_addr) == 1) {
        v4->sin_family = AF_INET;
        v4->sin_port = htons((uint16_t)port);
        out->len = sizeof *v4;
        return 0;
    }
    if (s[0] == '[' && inet_pton(AF_INET6, host, &v6->sin6_addr) == 1) {
        v6->sin6_family = AF_INET6;
        v6->sin6_port = htons((uint16_t)port);
        out->len = sizeof *v6;
        return 0;
    }
    return -1;
}

void pd_addr_format(const struct pd_addr *a, char *buf, size_t len)
{
    char host[INET6_ADDRSTRLEN] = "?";
    if (a->ss.ss_family == AF_INET) {
        const struct sockaddr_in *v4 = (const struct sockaddr_in *)&a->ss;
        (void)inet_ntop(AF_INET, &v4->sin_addr, host, sizeof host);
        (void)snprintf(buf, len, "%s:%u", host, ntohs(v4->sin_port));
    } else {
        const struct sockaddr_in6 *v6 = (const struct sockaddr_in6 *)&a->ss;
        (void)inet_ntop(AF_INET6, &v6->sin6_addr, host, sizeof host);
        (void)snprintf(buf, len, "[%s]:%u", host, ntohs(v6->sin6_port));
    }
}

static void listener_defaults(struct pd_listener_conf *l)
{
    memset(l, 0, sizeof *l);
    l->max_conns = 1024;
    l->buffer_size = 16 * 1024;
    l->connect_timeout_ns = 5 * SEC;
    l->idle_timeout_ns = 300 * SEC;
    l->table_size = 16384;
    l->ipv6_prefix = 64;
    l->table_full = PACER_FULL_DENY;
}

static void config_defaults(struct pd_config *c)
{
    memset(c, 0, sizeof *c);
    c->workers = 1;
    c->drain_timeout_ns = 30 * SEC;
    c->log_level = PD_LOG_INFO;
}

/* Directive table. Each one knows its scope, argument count and setter. */
struct directive;
typedef int (*setter)(struct parser *p, const struct directive *d, char **arg, int nargs);

struct directive {
    const char *name;
    int in_listener; /* 1: inside a listener block, 0: top level */
    int min_args;
    int max_args;
    setter set;
    size_t offset; /* field offset for the generic setters */
    uint64_t min;
    uint64_t max;
};

#define BAD(p, d, what, tok)                                                                       \
    fail((p), "%s: expected %s, got \"%s\"", (d)->name, (what), shown((tok), sb, sizeof sb))

static void *field(struct parser *p, const struct directive *d)
{
    void *base = d->in_listener ? (void *)p->cur : (void *)p->cfg;
    return (char *)base + d->offset;
}

static int set_u32(struct parser *p, const struct directive *d, char **arg, int nargs)
{
    char sb[48];
    uint64_t v = 0;
    (void)nargs;
    if (parse_u64(arg[0], &v) != 0) {
        return BAD(p, d, "a whole number", arg[0]);
    }
    if (v < d->min || v > d->max) {
        return fail(p, "%s: must be between %llu and %llu", d->name, (unsigned long long)d->min,
                    (unsigned long long)d->max);
    }
    *(uint32_t *)field(p, d) = (uint32_t)v;
    return 0;
}

static int set_size32(struct parser *p, const struct directive *d, char **arg, int nargs)
{
    char sb[48];
    uint64_t v = 0;
    (void)nargs;
    if (parse_size(arg[0], &v) != 0) {
        return BAD(p, d, "a size like 16k", arg[0]);
    }
    if (v < d->min || v > d->max) {
        return fail(p, "%s: must be between %llu and %llu bytes", d->name,
                    (unsigned long long)d->min, (unsigned long long)d->max);
    }
    *(uint32_t *)field(p, d) = (uint32_t)v;
    return 0;
}

static int set_duration(struct parser *p, const struct directive *d, char **arg, int nargs)
{
    char sb[48];
    uint64_t v = 0;
    (void)nargs;
    if (parse_duration(arg[0], &v) != 0) {
        return BAD(p, d, "a duration like 5s or 250ms", arg[0]);
    }
    if (v < d->min || v > d->max) {
        return fail(p, "%s: must be between %llums and %llums", d->name,
                    (unsigned long long)(d->min / 1000000), (unsigned long long)(d->max / 1000000));
    }
    *(uint64_t *)field(p, d) = v;
    return 0;
}

static int set_addr(struct parser *p, const struct directive *d, char **arg, int nargs)
{
    char sb[48];
    (void)nargs;
    if (pd_addr_parse(arg[0], (struct pd_addr *)field(p, d)) != 0) {
        return BAD(p, d, "an address like 127.0.0.1:8080 or [::1]:8080", arg[0]);
    }
    return 0;
}

/* RATE [burst SIZE]. The rate struct sits right after its has_ flag. */
static int set_rate(struct parser *p, const struct directive *d, char **arg, int nargs)
{
    char sb[48];
    struct pd_rate_opt *opt = field(p, d);
    pacer_rate *r = &opt->rate;
    if (parse_rate_value(arg[0], r) != 0) {
        return BAD(p, d, "a rate like 10/s, 600/m or 1m/s", arg[0]);
    }
    if (nargs == 3) {
        uint64_t burst = 0;
        if (strcmp(arg[1], "burst") != 0) {
            return BAD(p, d, "\"burst\"", arg[1]);
        }
        if (parse_size(arg[2], &burst) != 0 || burst == 0) {
            return BAD(p, d, "a burst like 20 or 64k", arg[2]);
        }
        r->burst = burst;
    } else if (nargs != 1) {
        return fail(p, "%s: expected RATE or RATE burst N", d->name);
    }
    uint64_t iv = 0, tol = 0;
    if (pacer_rate_ticks(r, &iv, &tol) != PACER_OK) {
        return fail(p, "%s: rate and burst are too large together", d->name);
    }
    opt->on = true;
    return 0;
}

static int set_min_rate(struct parser *p, const struct directive *d, char **arg, int nargs)
{
    char sb[48];
    struct pd_listener_conf *l = p->cur;
    if (nargs == 1 && strcmp(arg[0], "off") == 0) {
        l->min_rate_bytes = 0;
        return 0;
    }
    if (nargs != 3 || strcmp(arg[1], "per") != 0) {
        return fail(p, "%s: expected BYTES per DURATION, or off", d->name);
    }
    uint64_t bytes = 0, window = 0;
    if (parse_size(arg[0], &bytes) != 0 || bytes == 0) {
        return BAD(p, d, "a byte count of at least 1", arg[0]);
    }
    if (parse_duration(arg[2], &window) != 0 || window < SEC || window > 600 * SEC) {
        return BAD(p, d, "a window between 1s and 10m", arg[2]);
    }
    l->min_rate_bytes = bytes;
    l->min_rate_window_ns = window;
    return 0;
}

static int set_table_full(struct parser *p, const struct directive *d, char **arg, int nargs)
{
    char sb[48];
    (void)nargs;
    if (strcmp(arg[0], "deny") == 0) {
        p->cur->table_full = PACER_FULL_DENY;
    } else if (strcmp(arg[0], "evict") == 0) {
        p->cur->table_full = PACER_FULL_EVICT;
    } else {
        return BAD(p, d, "deny or evict", arg[0]);
    }
    return 0;
}

static int set_log_level(struct parser *p, const struct directive *d, char **arg, int nargs)
{
    static const char *names[] = {"debug", "info", "warn", "error"};
    char sb[48];
    (void)nargs;
    for (int i = 0; i < 4; i++) {
        if (strcmp(arg[0], names[i]) == 0) {
            p->cfg->log_level = i;
            return 0;
        }
    }
    return BAD(p, d, "debug, info, warn or error", arg[0]);
}

static int set_admin(struct parser *p, const struct directive *d, char **arg, int nargs)
{
    char sb[48];
    (void)nargs;
    size_t n = strlen(arg[0]);
    if (arg[0][0] != '/' || n >= PD_PATH_MAX) {
        return BAD(p, d, "an absolute path shorter than 108 bytes", arg[0]);
    }
    memcpy(p->cfg->admin_socket, arg[0], n + 1);
    return 0;
}

#define L(f) offsetof(struct pd_listener_conf, f)
#define C(f) offsetof(struct pd_config, f)

static const struct directive directives[] = {
    {"workers", 0, 1, 1, set_u32, C(workers), 1, 64},
    {"admin_socket", 0, 1, 1, set_admin, 0, 0, 0},
    {"drain_timeout", 0, 1, 1, set_duration, C(drain_timeout_ns), 0, 3600 * SEC},
    {"log_level", 0, 1, 1, set_log_level, 0, 0, 0},
    {"listen", 1, 1, 1, set_addr, L(listen), 0, 0},
    {"upstream", 1, 1, 1, set_addr, L(upstream), 0, 0},
    {"max_conns", 1, 1, 1, set_u32, L(max_conns), 1, 1000000},
    {"buffer", 1, 1, 1, set_size32, L(buffer_size), 1024, 1024ull * 1024},
    {"connect_timeout", 1, 1, 1, set_duration, L(connect_timeout_ns), 1000000, 600 * SEC},
    {"idle_timeout", 1, 1, 1, set_duration, L(idle_timeout_ns), SEC, 86400 * SEC},
    {"min_rate", 1, 1, 3, set_min_rate, 0, 0, 0},
    {"per_ip_max_conns", 1, 1, 1, set_u32, L(per_ip_max_conns), 0, 1000000},
    {"ipv6_prefix", 1, 1, 1, set_u32, L(ipv6_prefix), 32, 128},
    {"per_ip_conn_rate", 1, 1, 3, set_rate, L(per_ip_conn_rate), 0, 0},
    {"per_ip_bandwidth", 1, 1, 3, set_rate, L(per_ip_bandwidth), 0, 0},
    {"bandwidth", 1, 1, 3, set_rate, L(bandwidth), 0, 0},
    {"table_size", 1, 1, 1, set_u32, L(table_size), 16, 1u << 24},
    {"table_full", 1, 1, 1, set_table_full, 0, 0, 0},
};

#define NDIRECTIVES (sizeof directives / sizeof directives[0])

_Static_assert(NDIRECTIVES <= 32, "seen is a 32-bit mask");

static int valid_name(const char *s)
{
    size_t n = 0;
    for (; s[n]; n++) {
        char c = s[n];
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
              c == '_' || c == '-')) {
            return 0;
        }
    }
    return n > 0 && n < PD_NAME_MAX;
}

static int same_addr(const struct pd_addr *a, const struct pd_addr *b)
{
    return a->len == b->len && memcmp(&a->ss, &b->ss, a->len) == 0;
}

static int close_listener(struct parser *p)
{
    struct pd_listener_conf *l = p->cur;
    if (l->listen.len == 0) {
        return fail(p, "listener %s: missing listen", l->name);
    }
    if (l->upstream.len == 0) {
        return fail(p, "listener %s: missing upstream", l->name);
    }
    for (uint32_t i = 0; i + 1 < p->cfg->nlisteners; i++) {
        if (same_addr(&p->cfg->listeners[i].listen, &l->listen)) {
            return fail(p, "listener %s: listen address already used by %s", l->name,
                        p->cfg->listeners[i].name);
        }
    }
    p->cur = NULL;
    p->seen = p->top_seen;
    return 0;
}

static int open_listener(struct parser *p, char **tok, int n)
{
    char sb[48];
    if (p->cur) {
        return fail(p, "listener blocks cannot be nested");
    }
    if (n != 3 || strcmp(tok[2], "{") != 0) {
        return fail(p, "expected: listener NAME {");
    }
    if (!valid_name(tok[1])) {
        return fail(p, "listener name \"%s\" must be 1-31 letters, digits, _ or -",
                    shown(tok[1], sb, sizeof sb));
    }
    for (uint32_t i = 0; i < p->cfg->nlisteners; i++) {
        if (strcmp(p->cfg->listeners[i].name, tok[1]) == 0) {
            return fail(p, "listener %s defined twice", tok[1]);
        }
    }
    if (p->cfg->nlisteners == PD_MAX_LISTENERS) {
        return fail(p, "at most %d listeners", PD_MAX_LISTENERS);
    }
    p->cur = &p->cfg->listeners[p->cfg->nlisteners++];
    listener_defaults(p->cur);
    memcpy(p->cur->name, tok[1], strlen(tok[1]) + 1);
    p->top_seen = p->seen;
    p->seen = 0;
    return 0;
}

static int directive_line(struct parser *p, char **tok, int n)
{
    for (size_t i = 0; i < NDIRECTIVES; i++) {
        const struct directive *d = &directives[i];
        if (strcmp(d->name, tok[0]) != 0) {
            continue;
        }
        if (d->in_listener != (p->cur != NULL)) {
            return fail(p, "%s belongs %s", d->name,
                        d->in_listener ? "inside a listener block" : "outside listener blocks");
        }
        if (p->seen & (1u << i)) {
            return fail(p, "%s set twice", d->name);
        }
        if (n - 1 < d->min_args || n - 1 > d->max_args) {
            return fail(p, "%s: wrong number of values", d->name);
        }
        p->seen |= 1u << i;
        return d->set(p, d, tok + 1, n - 1);
    }
    char sb[48];
    return fail(p, "unknown directive \"%s\"", shown(tok[0], sb, sizeof sb));
}

/* Split a line in place into whitespace-separated tokens, dropping a
 * trailing # comment. Returns the token count or -1 if there are too many. */
static int tokenize(char *line, char **tok)
{
    int n = 0;
    char *s = line;
    for (;;) {
        while (*s == ' ' || *s == '\t' || *s == '\r') {
            s++;
        }
        if (*s == '\0' || *s == '#') {
            return n;
        }
        if (n == MAX_TOKENS) {
            return -1;
        }
        tok[n++] = s;
        while (*s && *s != ' ' && *s != '\t' && *s != '\r' && *s != '#') {
            s++;
        }
        if (*s == '#') {
            *s = '\0';
            return n;
        }
        if (*s) {
            *s++ = '\0';
        }
    }
}

static int parse_line(struct parser *p, char *line)
{
    char *tok[MAX_TOKENS];
    int n = tokenize(line, tok);
    if (n < 0) {
        return fail(p, "too many words on one line");
    }
    if (n == 0) {
        return 0;
    }
    if (strcmp(tok[0], "listener") == 0) {
        return open_listener(p, tok, n);
    }
    if (strcmp(tok[0], "}") == 0) {
        if (!p->cur || n != 1) {
            return fail(p, "unexpected }");
        }
        return close_listener(p);
    }
    return directive_line(p, tok, n);
}

int pd_config_parse(const char *buf, size_t len, struct pd_config *out, char *err, size_t errlen)
{
    struct parser p = {0, err, errlen, out, NULL, 0, 0};
    char line[PD_LINE_MAX + 1];
    config_defaults(out);
    if (errlen > 0) {
        err[0] = '\0';
    }
    if (len > PD_CONFIG_MAX_SIZE) {
        return fail(&p, "config larger than %d bytes", PD_CONFIG_MAX_SIZE);
    }
    size_t pos = 0;
    while (pos < len) {
        p.line++;
        size_t end = pos;
        while (end < len && buf[end] != '\n') {
            end++;
        }
        size_t n = end - pos;
        if (n > PD_LINE_MAX) {
            return fail(&p, "line longer than %d bytes", PD_LINE_MAX);
        }
        if (memchr(buf + pos, '\0', n)) {
            return fail(&p, "unexpected NUL byte");
        }
        memcpy(line, buf + pos, n);
        line[n] = '\0';
        if (parse_line(&p, line) != 0) {
            return -1;
        }
        pos = end + 1;
    }
    if (p.cur) {
        return fail(&p, "listener %s: missing }", p.cur->name);
    }
    if (out->nlisteners == 0) {
        return fail(&p, "no listener defined");
    }
    return 0;
}

int pd_config_load(const char *path, struct pd_config *out, char *err, size_t errlen)
{
    const size_t cap = PD_CONFIG_MAX_SIZE + 1;
    char eb[128];
    config_defaults(out);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        (void)snprintf(err, errlen, "%s: %s", path, pd_strerror(errno, eb, sizeof eb));
        return -1;
    }
    char *buf = malloc(cap);
    if (!buf) {
        (void)snprintf(err, errlen, "%s: out of memory", path);
        (void)close(fd);
        return -1;
    }
    size_t len = 0;
    for (;;) {
        ssize_t r = read(fd, buf + len, cap - len);
        if (r < 0 && errno == EINTR) {
            continue;
        }
        if (r < 0) {
            (void)snprintf(err, errlen, "%s: %s", path, pd_strerror(errno, eb, sizeof eb));
            (void)close(fd);
            free(buf);
            return -1;
        }
        if (r == 0) {
            break;
        }
        len += (size_t)r;
        if (len == cap) {
            break;
        }
    }
    (void)close(fd);
    int rc = pd_config_parse(buf, len, out, err, errlen);
    free(buf);
    return rc;
}
