/*
 * Differential driver for libpacer. It reads one operation per line on
 * stdin, applies it to the C library and prints exactly one line per
 * operation on stdout. edgefront-difftest feeds the same script to this
 * program and to the Rust port in edgefront-limiter and compares the
 * transcripts line by line, so the two can be checked against each other
 * with no FFI.
 *
 * Every operation runs at the driver's clock, which starts at 0 and only
 * moves when the script says so. Objects are numbered 0 to 15 per kind.
 * Keys are written in hex, or "-" for the empty key. Numbers are decimal
 * u64. Blank lines and lines starting with '#' are skipped and print
 * nothing. Return codes print as numbers, errors by name (EINVAL, ERANGE,
 * EFULL, ENOENT, ELIMIT).
 *
 *   now N                          set the clock         -> now=N
 *   advance N                      add N, saturating     -> now=N
 *   echo TEXT                      print TEXT
 *   sync                           print "sync" and flush stdout
 *
 *   gcra I init T P B              rate T tokens per P ns, burst B
 *   gcra I take COST
 *   gcra I take_upto MAX
 *   gcra I take_range MIN MAX
 *   gcra I refund N
 *   gcra I remaining
 *   gcra I reconfigure T P B
 *   gcra I state
 *       Every gcra line ends with the state: tat= iv= tol= (ticks).
 *
 *   table I create MAX SHARDS T P B deny|evict HASHKEY
 *       HASHKEY is 32 hex digits. SHARDS 0 picks the default.
 *   table I take KEY COST
 *   table I take_upto KEY MAX
 *   table I take_range KEY MIN MAX
 *   table I refund KEY N
 *   table I hold KEY MAX_HOLDS
 *   table I release KEY
 *   table I remove KEY
 *   table I get KEY                -> tat= holds=, or absent
 *   table I reconfigure T P B
 *   table I sweep NSHARDS          -> swept=
 *   table I stats
 *   table I check                  -> check= (0 when the invariants hold)
 *   table I destroy
 *
 *   window I init WINDOW_NS SLOTS
 *   window I add N
 *   window I count                 -> count=
 *       Every window line ends with the state: start= total=.
 *
 * A malformed line is a bug in the generator, not a decision to compare:
 * the driver names it on stderr and exits with status 2.
 */
#include "lib/table_check.h"
#include "pacer.h"

#include <errno.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define MAX_LINE   4096
#define MAX_TOKENS 12
#define MAX_OBJS   16
#define MAX_KEY    64 /* longer than PACER_KEY_MAX, so oversized keys can be sent */

static unsigned long line_no;
static uint64_t now_ns;
static pacer_gcra gcras[MAX_OBJS];
static int gcra_live[MAX_OBJS];
static pacer_table *tables[MAX_OBJS];
static pacer_window windows[MAX_OBJS];
static int window_live[MAX_OBJS];

static void destroy_all(void)
{
    for (int i = 0; i < MAX_OBJS; i++) {
        pacer_table_destroy(tables[i]);
        tables[i] = NULL;
    }
}

__attribute__((noreturn, format(printf, 1, 2))) static void die(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "diffdrive: line %lu: ", line_no);
    vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
    (void)fflush(stdout);
    destroy_all();
    _Exit(2);
}

static const char *err_name(int rc)
{
    switch (rc) {
    case PACER_EINVAL:
        return "EINVAL";
    case PACER_ERANGE:
        return "ERANGE";
    case PACER_ENOMEM:
        return "ENOMEM";
    case PACER_EFULL:
        return "EFULL";
    case PACER_ENOENT:
        return "ENOENT";
    case PACER_ELIMIT:
        return "ELIMIT";
    default:
        return "EUNKNOWN";
    }
}

static void print_rc(int rc)
{
    if (rc < 0) {
        printf("rc=%s", err_name(rc));
    } else {
        printf("rc=%d", rc);
    }
}

static void print_decision(const pacer_decision *d)
{
    printf(" granted=%llu remaining=%llu retry=%llu", (unsigned long long)d->granted,
           (unsigned long long)d->remaining, (unsigned long long)d->retry_after_ns);
}

static uint64_t parse_u64(const char *s)
{
    if (s[0] < '0' || s[0] > '9') {
        die("not a number: %s", s);
    }
    char *end = NULL;
    errno = 0;
    unsigned long long v = strtoull(s, &end, 10);
    if (errno != 0 || *end != '\0') {
        die("not a u64: %s", s);
    }
    return (uint64_t)v;
}

static uint32_t parse_u32(const char *s)
{
    uint64_t v = parse_u64(s);
    if (v > UINT32_MAX) {
        die("not a u32: %s", s);
    }
    return (uint32_t)v;
}

static int hex_digit(char c)
{
    if (c >= '0' && c <= '9') {
        return c - '0';
    }
    if (c >= 'a' && c <= 'f') {
        return c - 'a' + 10;
    }
    if (c >= 'A' && c <= 'F') {
        return c - 'A' + 10;
    }
    return -1;
}

/* Decode hex into buf and return the length; "-" is the empty string. */
static size_t parse_hex(const char *s, uint8_t *buf, size_t cap)
{
    if (strcmp(s, "-") == 0) {
        return 0;
    }
    size_t n = strlen(s);
    if (n % 2 != 0 || n / 2 > cap) {
        die("bad hex of %zu digits: %s", n, s);
    }
    for (size_t i = 0; i < n / 2; i++) {
        int hi = hex_digit(s[2 * i]);
        int lo = hex_digit(s[2 * i + 1]);
        if (hi < 0 || lo < 0) {
            die("bad hex: %s", s);
        }
        buf[i] = (uint8_t)(hi << 4 | lo);
    }
    return n / 2;
}

static void need(int argc, int want, const char *usage)
{
    if (argc != want) {
        die("usage: %s", usage);
    }
}

static int parse_index(const char *s)
{
    uint64_t i = parse_u64(s);
    if (i >= MAX_OBJS) {
        die("object index %s out of range", s);
    }
    return (int)i;
}

static pacer_rate parse_rate(char **argv)
{
    pacer_rate r = {parse_u64(argv[0]), parse_u64(argv[1]), parse_u64(argv[2])};
    return r;
}

static void print_gcra_state(pacer_gcra *g)
{
    printf(" tat=%llu iv=%llu tol=%llu", (unsigned long long)atomic_load(&g->tat),
           (unsigned long long)atomic_load(&g->interval),
           (unsigned long long)atomic_load(&g->tolerance));
}

static void op_gcra(int argc, char **argv)
{
    if (argc < 3) {
        die("usage: gcra I OP ...");
    }
    int i = parse_index(argv[1]);
    const char *op = argv[2];
    pacer_gcra *g = &gcras[i];
    pacer_decision d = {0, 0, 0};

    if (strcmp(op, "init") == 0) {
        need(argc, 6, "gcra I init T P B");
        pacer_rate r = parse_rate(argv + 3);
        int rc = pacer_gcra_init(g, &r);
        if (rc == PACER_OK) {
            gcra_live[i] = 1;
        }
        print_rc(rc);
        if (gcra_live[i]) {
            print_gcra_state(g);
        }
        putchar('\n');
        return;
    }
    if (!gcra_live[i]) {
        die("gcra %d is not initialised", i);
    }
    if (strcmp(op, "take") == 0) {
        need(argc, 4, "gcra I take COST");
        print_rc(pacer_gcra_take(g, now_ns, parse_u64(argv[3]), &d));
        print_decision(&d);
    } else if (strcmp(op, "take_upto") == 0) {
        need(argc, 4, "gcra I take_upto MAX");
        uint64_t got = pacer_gcra_take_upto(g, now_ns, parse_u64(argv[3]), &d);
        printf("got=%llu", (unsigned long long)got);
        print_decision(&d);
    } else if (strcmp(op, "take_range") == 0) {
        need(argc, 5, "gcra I take_range MIN MAX");
        uint64_t got = pacer_gcra_take_range(g, now_ns, parse_u64(argv[3]), parse_u64(argv[4]), &d);
        printf("got=%llu", (unsigned long long)got);
        print_decision(&d);
    } else if (strcmp(op, "refund") == 0) {
        need(argc, 4, "gcra I refund N");
        pacer_gcra_refund(g, now_ns, parse_u64(argv[3]));
        printf("ok");
    } else if (strcmp(op, "remaining") == 0) {
        need(argc, 3, "gcra I remaining");
        printf("remaining=%llu", (unsigned long long)pacer_gcra_remaining(g, now_ns));
    } else if (strcmp(op, "reconfigure") == 0) {
        need(argc, 6, "gcra I reconfigure T P B");
        pacer_rate r = parse_rate(argv + 3);
        print_rc(pacer_gcra_reconfigure(g, &r));
    } else if (strcmp(op, "state") == 0) {
        need(argc, 3, "gcra I state");
        printf("ok");
    } else {
        die("unknown gcra op %s", op);
    }
    print_gcra_state(g);
    putchar('\n');
}

static void table_create(int i, int argc, char **argv)
{
    need(argc, 10, "table I create MAX SHARDS T P B deny|evict HASHKEY");
    if (tables[i]) {
        die("table %d already exists", i);
    }
    int policy = PACER_FULL_DENY;
    if (strcmp(argv[8], "evict") == 0) {
        policy = PACER_FULL_EVICT;
    } else if (strcmp(argv[8], "deny") != 0) {
        die("unknown full policy %s", argv[8]);
    }
    uint8_t hash_key[16];
    if (parse_hex(argv[9], hash_key, sizeof hash_key) != sizeof hash_key) {
        die("hash key must be 32 hex digits");
    }
    pacer_table_config cfg = {parse_u32(argv[3]), parse_u32(argv[4]), parse_rate(argv + 5),
                              hash_key, policy};
    print_rc(pacer_table_create(&cfg, &tables[i]));
    putchar('\n');
}

static void table_stats(pacer_table *t)
{
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    printf("entries=%llu capacity=%llu inserts=%llu removals=%llu reclaims=%llu evictions=%llu "
           "rejections=%llu holds=%llu",
           (unsigned long long)st.entries, (unsigned long long)st.capacity,
           (unsigned long long)st.inserts, (unsigned long long)st.removals,
           (unsigned long long)st.reclaims, (unsigned long long)st.evictions,
           (unsigned long long)st.rejections, (unsigned long long)st.holds);
}

/* Operations that name no key. Returns 0 if `op` is not one of them. */
static int table_op_unkeyed(pacer_table **slot, const char *op, int argc, char **argv)
{
    pacer_table *t = *slot;
    if (strcmp(op, "reconfigure") == 0) {
        need(argc, 6, "table I reconfigure T P B");
        pacer_rate r = parse_rate(argv + 3);
        print_rc(pacer_table_reconfigure(t, &r));
    } else if (strcmp(op, "sweep") == 0) {
        need(argc, 4, "table I sweep NSHARDS");
        size_t n = pacer_table_sweep(t, now_ns, parse_u32(argv[3]));
        printf("swept=%zu", n);
    } else if (strcmp(op, "stats") == 0) {
        need(argc, 3, "table I stats");
        table_stats(t);
    } else if (strcmp(op, "check") == 0) {
        need(argc, 3, "table I check");
        printf("check=%d", pacer_table_check(t));
    } else if (strcmp(op, "destroy") == 0) {
        need(argc, 3, "table I destroy");
        pacer_table_destroy(t);
        *slot = NULL;
        printf("ok");
    } else {
        return 0;
    }
    putchar('\n');
    return 1;
}

static void table_op_keyed(pacer_table *t, const char *op, int argc, char **argv)
{
    if (argc < 4) {
        die("usage: table I %s KEY ...", op);
    }
    uint8_t key[MAX_KEY];
    size_t len = parse_hex(argv[3], key, sizeof key);
    pacer_decision d = {0, 0, 0};

    if (strcmp(op, "take") == 0) {
        need(argc, 5, "table I take KEY COST");
        print_rc(pacer_table_take(t, key, len, now_ns, parse_u64(argv[4]), &d));
        print_decision(&d);
    } else if (strcmp(op, "take_upto") == 0) {
        need(argc, 5, "table I take_upto KEY MAX");
        print_rc(pacer_table_take_upto(t, key, len, now_ns, parse_u64(argv[4]), &d));
        print_decision(&d);
    } else if (strcmp(op, "take_range") == 0) {
        need(argc, 6, "table I take_range KEY MIN MAX");
        print_rc(pacer_table_take_range(t, key, len, now_ns, parse_u64(argv[4]), parse_u64(argv[5]),
                                        &d));
        print_decision(&d);
    } else if (strcmp(op, "refund") == 0) {
        need(argc, 5, "table I refund KEY N");
        pacer_table_refund(t, key, len, now_ns, parse_u64(argv[4]));
        printf("ok");
    } else if (strcmp(op, "hold") == 0) {
        need(argc, 5, "table I hold KEY MAX_HOLDS");
        print_rc(pacer_table_hold(t, key, len, now_ns, parse_u32(argv[4]), &d));
        print_decision(&d);
    } else if (strcmp(op, "release") == 0) {
        need(argc, 4, "table I release KEY");
        print_rc(pacer_table_release(t, key, len));
    } else if (strcmp(op, "remove") == 0) {
        need(argc, 4, "table I remove KEY");
        print_rc(pacer_table_remove(t, key, len));
    } else if (strcmp(op, "get") == 0) {
        need(argc, 4, "table I get KEY");
        uint64_t tat = 0;
        uint32_t holds = 0;
        int rc = pacer_table_peek(t, key, len, &tat, &holds);
        if (rc == PACER_OK) {
            printf("tat=%llu holds=%u", (unsigned long long)tat, holds);
        } else if (rc == PACER_ENOENT) {
            printf("absent");
        } else {
            print_rc(rc);
        }
    } else {
        die("unknown table op %s", op);
    }
    putchar('\n');
}

static void op_table(int argc, char **argv)
{
    if (argc < 3) {
        die("usage: table I OP ...");
    }
    int i = parse_index(argv[1]);
    const char *op = argv[2];
    if (strcmp(op, "create") == 0) {
        table_create(i, argc, argv);
        return;
    }
    if (!tables[i]) {
        die("table %d does not exist", i);
    }
    if (!table_op_unkeyed(&tables[i], op, argc, argv)) {
        table_op_keyed(tables[i], op, argc, argv);
    }
}

static void print_window_state(const pacer_window *w)
{
    printf(" start=%llu total=%llu", (unsigned long long)w->head_start,
           (unsigned long long)w->total);
}

static void op_window(int argc, char **argv)
{
    if (argc < 3) {
        die("usage: window I OP ...");
    }
    int i = parse_index(argv[1]);
    const char *op = argv[2];
    pacer_window *w = &windows[i];

    if (strcmp(op, "init") == 0) {
        need(argc, 5, "window I init WINDOW_NS SLOTS");
        int rc = pacer_window_init(w, parse_u64(argv[3]), parse_u32(argv[4]), now_ns);
        if (rc == PACER_OK) {
            window_live[i] = 1;
        }
        print_rc(rc);
        if (window_live[i]) {
            print_window_state(w);
        }
        putchar('\n');
        return;
    }
    if (!window_live[i]) {
        die("window %d is not initialised", i);
    }
    if (strcmp(op, "add") == 0) {
        need(argc, 4, "window I add N");
        pacer_window_add(w, now_ns, parse_u64(argv[3]));
        printf("ok");
    } else if (strcmp(op, "count") == 0) {
        need(argc, 3, "window I count");
        printf("count=%llu", (unsigned long long)pacer_window_count(w, now_ns));
    } else {
        die("unknown window op %s", op);
    }
    print_window_state(w);
    putchar('\n');
}

/* Split on spaces and tabs in place. Returns the number of tokens. */
static int split(char *line, char **argv)
{
    int argc = 0;
    char *p = line;
    for (;;) {
        while (*p == ' ' || *p == '\t') {
            p++;
        }
        if (*p == '\0') {
            return argc;
        }
        if (argc == MAX_TOKENS) {
            die("more than %d words", MAX_TOKENS);
        }
        argv[argc++] = p;
        while (*p != '\0' && *p != ' ' && *p != '\t') {
            p++;
        }
        if (*p != '\0') {
            *p++ = '\0';
        }
    }
}

static void run_line(char *line)
{
    while (*line == ' ' || *line == '\t') {
        line++;
    }
    if (*line == '\0' || *line == '#') {
        return;
    }
    if (strncmp(line, "echo", 4) == 0 && (line[4] == ' ' || line[4] == '\0')) {
        printf("%s\n", line[4] == ' ' ? line + 5 : "");
        return;
    }
    char *argv[MAX_TOKENS] = {NULL};
    int argc = split(line, argv);
    if (argc == 0) {
        return;
    }
    const char *cmd = argv[0];
    if (strcmp(cmd, "now") == 0) {
        need(argc, 2, "now N");
        now_ns = parse_u64(argv[1]);
        printf("now=%llu\n", (unsigned long long)now_ns);
    } else if (strcmp(cmd, "advance") == 0) {
        need(argc, 2, "advance N");
        uint64_t by = parse_u64(argv[1]);
        now_ns = by > UINT64_MAX - now_ns ? UINT64_MAX : now_ns + by;
        printf("now=%llu\n", (unsigned long long)now_ns);
    } else if (strcmp(cmd, "sync") == 0) {
        need(argc, 1, "sync");
        printf("sync\n");
        (void)fflush(stdout);
    } else if (strcmp(cmd, "gcra") == 0) {
        op_gcra(argc, argv);
    } else if (strcmp(cmd, "table") == 0) {
        op_table(argc, argv);
    } else if (strcmp(cmd, "window") == 0) {
        op_window(argc, argv);
    } else {
        die("unknown command %s", cmd);
    }
}

int main(void)
{
    static char line[MAX_LINE];
    while (fgets(line, sizeof line, stdin)) {
        line_no++;
        size_t n = strlen(line);
        if (n > 0 && line[n - 1] == '\n') {
            line[--n] = '\0';
        } else if (!feof(stdin)) {
            die("line longer than %d bytes", MAX_LINE - 2);
        }
        if (n > 0 && line[n - 1] == '\r') {
            line[--n] = '\0';
        }
        run_line(line);
    }
    if (ferror(stdin)) {
        die("read error");
    }
    destroy_all();
    if (fflush(stdout) != 0 || ferror(stdout)) {
        fprintf(stderr, "diffdrive: write error\n");
        return 1;
    }
    return 0;
}
