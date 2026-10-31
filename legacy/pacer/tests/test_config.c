#include "pacerd/config.h"
#include "test.h"

#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define SEC 1000000000ull

static struct pd_config cfg;
static char err[PD_ERR_MAX];

static int parse(const char *text)
{
    return pd_config_parse(text, strlen(text), &cfg, err, sizeof err);
}

/* Parse a one-listener config with `extra` inside the block and expect the
 * error text to contain `want`. */
static void expect_listener_error(const char *extra, const char *want)
{
    char buf[2048];
    (void)snprintf(buf, sizeof buf,
                   "listener a {\n listen 127.0.0.1:1\n upstream 127.0.0.1:2\n %s\n}\n", extra);
    int rc = parse(buf);
    CHECK_EQ_INT(rc, -1);
    if (!strstr(err, want)) {
        fprintf(stderr, "    for \"%s\": got \"%s\", want \"%s\"\n", extra, err, want);
        CHECK(strstr(err, want) != NULL);
    }
}

static void expect_error(const char *text, const char *want)
{
    CHECK_EQ_INT(parse(text), -1);
    if (!strstr(err, want)) {
        fprintf(stderr, "    got \"%s\", want \"%s\"\n", err, want);
        CHECK(strstr(err, want) != NULL);
    }
}

TEST(config_full_example)
{
    const char *text = "# pacerd example\n"
                       "workers 4\n"
                       "admin_socket /run/pacerd.sock\n"
                       "drain_timeout 10s\n"
                       "log_level debug\n"
                       "\n"
                       "listener web {   # public side\n"
                       "\tlisten 0.0.0.0:8080\n"
                       "    upstream [::1]:9000\r\n"
                       "    max_conns 5000\n"
                       "    buffer 32k\n"
                       "    connect_timeout 250ms\n"
                       "    idle_timeout 2m\n"
                       "    min_rate 64 per 10s\n"
                       "    per_ip_max_conns 20\n"
                       "    per_ip_conn_rate 10/s burst 20\n"
                       "    per_ip_bandwidth 1m/s burst 256k\n"
                       "    bandwidth 600/m\n"
                       "    table_size 4096\n"
                       "    ipv6_prefix 56\n"
                       "    table_full evict\n"
                       "}\n"
                       "listener ssh {\n"
                       "    listen [::]:2222\n"
                       "    upstream 10.0.0.2:22\n"
                       "}";
    CHECK_EQ_INT(parse(text), 0);
    CHECK_EQ_U64(cfg.workers, 4);
    CHECK(strcmp(cfg.admin_socket, "/run/pacerd.sock") == 0);
    CHECK_EQ_U64(cfg.drain_timeout_ns, 10 * SEC);
    CHECK_EQ_INT(cfg.log_level, PD_LOG_DEBUG);
    CHECK_EQ_U64(cfg.nlisteners, 2);
    const struct pd_listener_conf *l = &cfg.listeners[0];
    CHECK(strcmp(l->name, "web") == 0);
    CHECK_EQ_INT(l->listen.ss.ss_family, AF_INET);
    CHECK_EQ_INT(l->upstream.ss.ss_family, AF_INET6);
    CHECK_EQ_U64(l->max_conns, 5000);
    CHECK_EQ_U64(l->buffer_size, 32 * 1024);
    CHECK_EQ_U64(l->connect_timeout_ns, 250 * 1000000ull);
    CHECK_EQ_U64(l->idle_timeout_ns, 120 * SEC);
    CHECK_EQ_U64(l->min_rate_bytes, 64);
    CHECK_EQ_U64(l->min_rate_window_ns, 10 * SEC);
    CHECK_EQ_U64(l->per_ip_max_conns, 20);
    CHECK(l->per_ip_conn_rate.on);
    CHECK_EQ_U64(l->per_ip_conn_rate.rate.tokens, 10);
    CHECK_EQ_U64(l->per_ip_conn_rate.rate.burst, 20);
    CHECK(l->per_ip_bandwidth.on);
    CHECK_EQ_U64(l->per_ip_bandwidth.rate.tokens, 1024 * 1024);
    CHECK_EQ_U64(l->per_ip_bandwidth.rate.burst, 256 * 1024);
    CHECK(l->bandwidth.on);
    CHECK_EQ_U64(l->bandwidth.rate.tokens, 600);
    CHECK_EQ_U64(l->bandwidth.rate.period_ns, 60 * SEC);
    CHECK_EQ_U64(l->bandwidth.rate.burst, 600);
    CHECK_EQ_U64(l->table_size, 4096);
    CHECK_EQ_U64(l->ipv6_prefix, 56);
    CHECK_EQ_INT(l->table_full, PACER_FULL_EVICT);
    l = &cfg.listeners[1];
    CHECK_EQ_U64(l->max_conns, 1024);
    CHECK_EQ_U64(l->buffer_size, 16 * 1024);
    CHECK(!l->per_ip_conn_rate.on);
    CHECK_EQ_U64(l->min_rate_bytes, 0);
    CHECK_EQ_INT(l->table_full, PACER_FULL_DENY);
}

TEST(config_defaults)
{
    CHECK_EQ_INT(parse("listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n"), 0);
    CHECK_EQ_U64(cfg.workers, 1);
    CHECK_EQ_U64(cfg.drain_timeout_ns, 30 * SEC);
    CHECK_EQ_INT(cfg.log_level, PD_LOG_INFO);
    CHECK(cfg.admin_socket[0] == '\0');
    CHECK_EQ_U64(cfg.listeners[0].connect_timeout_ns, 5 * SEC);
    CHECK_EQ_U64(cfg.listeners[0].idle_timeout_ns, 300 * SEC);
    CHECK_EQ_U64(cfg.listeners[0].ipv6_prefix, 64);
    CHECK_EQ_U64(cfg.listeners[0].table_size, 16384);
}

TEST(config_rate_units)
{
    CHECK_EQ_INT(parse("listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n"
                       "per_ip_conn_rate 100/h\nbandwidth 1g/s burst 1m\n}\n"),
                 0);
    CHECK_EQ_U64(cfg.listeners[0].per_ip_conn_rate.rate.period_ns, 3600 * SEC);
    CHECK_EQ_U64(cfg.listeners[0].bandwidth.rate.tokens, 1024ull * 1024 * 1024);
    CHECK_EQ_U64(cfg.listeners[0].bandwidth.rate.burst, 1024ull * 1024);
}

TEST(config_min_rate_off)
{
    CHECK_EQ_INT(parse("listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n"
                       "min_rate off\n}\n"),
                 0);
    CHECK_EQ_U64(cfg.listeners[0].min_rate_bytes, 0);
}

TEST(config_value_errors)
{
    expect_listener_error("max_conns 0", "max_conns: must be between 1 and 1000000");
    expect_listener_error("max_conns 1000001", "max_conns: must be between");
    expect_listener_error("max_conns -1", "max_conns: expected a whole number, got \"-1\"");
    expect_listener_error("max_conns 1.5", "expected a whole number");
    expect_listener_error("max_conns 0x10", "expected a whole number");
    expect_listener_error("max_conns 99999999999999999999999", "expected a whole number");
    expect_listener_error("max_conns ten", "expected a whole number");
    expect_listener_error("buffer 512", "buffer: must be between 1024 and 1048576 bytes");
    expect_listener_error("buffer 2m", "buffer: must be between");
    expect_listener_error("buffer 16q", "buffer: expected a size like 16k");
    expect_listener_error("buffer k", "buffer: expected a size like 16k");
    expect_listener_error("buffer 99999999999999999g", "expected a size");
    expect_listener_error("connect_timeout 5", "expected a duration like 5s");
    expect_listener_error("connect_timeout 5d", "expected a duration");
    expect_listener_error("connect_timeout 0ms", "connect_timeout: must be between 1ms");
    expect_listener_error("idle_timeout 99999999999h", "expected a duration");
    expect_listener_error("idle_timeout 2d", "expected a duration");
    expect_listener_error("per_ip_conn_rate 10", "expected a rate like 10/s");
    expect_listener_error("per_ip_conn_rate 0/s", "expected a rate");
    expect_listener_error("per_ip_conn_rate 10/d", "expected a rate");
    expect_listener_error("per_ip_conn_rate x/s", "expected a rate");
    expect_listener_error("per_ip_conn_rate 99999999999999999999999999999999/s", "expected a rate");
    expect_listener_error("per_ip_conn_rate 10/s burst", "expected RATE or RATE burst N");
    expect_listener_error("per_ip_conn_rate 10/s bust 5", "expected \"burst\"");
    expect_listener_error("per_ip_conn_rate 10/s burst 0", "a burst like 20 or 64k");
    expect_listener_error("per_ip_conn_rate 10/s burst many", "a burst like 20 or 64k");
    expect_listener_error("per_ip_conn_rate 1/h burst 100000", "too large together");
    expect_listener_error("min_rate 64", "expected BYTES per DURATION, or off");
    expect_listener_error("min_rate 64 over 10s", "expected BYTES per DURATION");
    expect_listener_error("min_rate 0 per 10s", "a byte count of at least 1");
    expect_listener_error("min_rate 64 per 100ms", "a window between 1s and 10m");
    expect_listener_error("min_rate 64 per 11m", "a window between 1s and 10m");
    expect_listener_error("table_full maybe", "deny or evict");
    expect_listener_error("table_size 8", "table_size: must be between 16");
    expect_listener_error("ipv6_prefix 16", "ipv6_prefix: must be between 32 and 128");
    expect_listener_error("ipv6_prefix 129", "ipv6_prefix: must be between 32 and 128");
}

TEST(config_address_errors)
{
    expect_error("listener a {\nlisten 127.0.0.1\n}\n", "listen: expected an address");
    expect_error("listener a {\nlisten 127.0.0.1:0\n}\n", "expected an address");
    expect_error("listener a {\nlisten 127.0.0.1:65536\n}\n", "expected an address");
    expect_error("listener a {\nlisten localhost:80\n}\n", "expected an address");
    expect_error("listener a {\nlisten ::1:80\n}\n", "expected an address");
    expect_error("listener a {\nlisten [::1]80\n}\n", "expected an address");
    expect_error("listener a {\nlisten [::1:80\n}\n", "expected an address");
    expect_error("listener a {\nlisten [127.0.0.1]:80\n}\n", "expected an address");
    expect_error("listener a {\nlisten 1.2.3.4:http\n}\n", "expected an address");
    expect_error("listener a {\nlisten "
                 "[0000:0000:0000:0000:0000:0000:0000:0000:0000:0000:0000:0000:0000]:1\n}\n",
                 "expected an address");
    expect_error("listener a {\nlisten "
                 "0000000000000000000000000000000000000000000000000000000000000000000000:1\n}\n",
                 "expected an address");
}

TEST(config_structure_errors)
{
    expect_error("", "no listener defined");
    expect_error("# only a comment\n", "no listener defined");
    expect_error("workers 2\nworkers 3\nlistener a {\n}\n", "line 2: workers set twice");
    expect_error("workers 0\n", "workers: must be between 1 and 64");
    expect_error("workers 1 2\n", "workers: wrong number of values");
    expect_error("frobnicate 1\n", "line 1: unknown directive \"frobnicate\"");
    expect_error("listen 127.0.0.1:1\n", "listen belongs inside a listener block");
    expect_error("listener a {\nworkers 2\n}\n", "workers belongs outside listener blocks");
    expect_error("listener a {\nlistener b {\n", "line 2: listener blocks cannot be nested");
    expect_error("listener a\n", "expected: listener NAME {");
    expect_error("listener a b {\n", "expected: listener NAME {");
    expect_error("listener bad.name {\n", "listener name \"bad.name\" must be");
    expect_error("listener aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa {\n", "must be 1-31 letters");
    expect_error("}\n", "line 1: unexpected }");
    expect_error("listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n} x\n", "unexpected }");
    expect_error("listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n", "a: missing }");
    expect_error("listener a {\nupstream 127.0.0.1:2\n}\n", "line 3: listener a: missing listen");
    expect_error("listener a {\nlisten 127.0.0.1:2\n}\n", "listener a: missing upstream");
    expect_error("listener a {\nlisten 127.0.0.1:1\nlisten 127.0.0.1:3\n}\n",
                 "line 3: listen set twice");
    expect_error("listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n"
                 "listener a {\n",
                 "line 5: listener a defined twice");
    expect_error("listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n"
                 "listener b {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n",
                 "listener b: listen address already used by a");
    expect_error("a b c d e f g h i\n", "too many words");
    expect_error("log_level loud\n", "expected debug, info, warn or error");
    expect_error("admin_socket relative.sock\n", "an absolute path");
    expect_error("drain_timeout 2h\n", "drain_timeout: must be between 0ms and 3600000ms");
}

TEST(config_top_level_duplicates_span_blocks)
{
    expect_error("workers 2\nlistener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n"
                 "workers 3\n",
                 "line 6: workers set twice");
    /* Directives inside each block are tracked separately. */
    CHECK_EQ_INT(parse("listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n"
                       "listener b {\nlisten 127.0.0.1:3\nupstream 127.0.0.1:2\n}\n"),
                 0);
}

TEST(config_hostile_input)
{
    char nul[] = "workers 1\nwork\0ers 2\n";
    CHECK_EQ_INT(pd_config_parse(nul, sizeof nul - 1, &cfg, err, sizeof err), -1);
    CHECK(strstr(err, "line 2: unexpected NUL byte") != NULL);

    static char longline[PD_LINE_MAX + 10];
    memset(longline, 'a', sizeof longline - 1);
    longline[sizeof longline - 1] = '\0';
    expect_error(longline, "line 1: line longer than 1024 bytes");

    /* Control bytes from the file never reach the message. */
    expect_error("\x1b[31mevil 1\n", "unknown directive \"?[31mevil\"");
    expect_error("listener a {\nmax_conns \"7\n", "got \"?7\"");
    /* Bytes above 0x7f are replaced too, whether char is signed or not. */
    expect_error("\xc3\xa9vil\xff 1\n", "unknown directive \"??vil?\"");
    /* Long tokens are cut short in messages. */
    expect_error("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1\n",
                 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa...");

    size_t big = PD_CONFIG_MAX_SIZE + 1;
    char *huge = calloc(1, big);
    CHECK(huge != NULL);
    CHECK_EQ_INT(pd_config_parse(huge, big, &cfg, err, sizeof err), -1);
    CHECK(strstr(err, "config larger than") != NULL);
    free(huge);

    /* A tiny error buffer is truncated, never overrun. */
    char small[8];
    CHECK_EQ_INT(pd_config_parse("bogus\n", 6, &cfg, small, sizeof small), -1);
    CHECK_EQ_U64(strlen(small), 7);
}

TEST(config_load_from_file)
{
    char path[] = "/tmp/pacer-config-XXXXXX";
    int fd = mkstemp(path);
    CHECK(fd >= 0);
    const char *text = "listener a {\nlisten 127.0.0.1:1\nupstream 127.0.0.1:2\n}\n";
    CHECK_EQ_INT(write(fd, text, strlen(text)), (long long)strlen(text));
    close(fd);
    CHECK_EQ_INT(pd_config_load(path, &cfg, err, sizeof err), 0);
    CHECK_EQ_U64(cfg.nlisteners, 1);

    /* Oversized files are refused after reading one byte past the limit. */
    FILE *f = fopen(path, "w");
    CHECK(f != NULL);
    for (int i = 0; i < PD_CONFIG_MAX_SIZE / 10 + 2; i++) {
        fputs("#########\n", f);
    }
    fclose(f);
    CHECK_EQ_INT(pd_config_load(path, &cfg, err, sizeof err), -1);
    CHECK(strstr(err, "config larger than") != NULL);
    unlink(path);

    CHECK_EQ_INT(pd_config_load("/nonexistent/pacerd.conf", &cfg, err, sizeof err), -1);
    CHECK(strstr(err, "No such file") != NULL);
    CHECK_EQ_INT(pd_config_load("/", &cfg, err, sizeof err), -1);
    CHECK(strstr(err, "Is a directory") != NULL);
}

TEST(config_addr_format_round_trip)
{
    struct pd_addr a;
    char buf[64];
    CHECK_EQ_INT(pd_addr_parse("192.0.2.7:443", &a), 0);
    pd_addr_format(&a, buf, sizeof buf);
    CHECK(strcmp(buf, "192.0.2.7:443") == 0);
    CHECK_EQ_INT(pd_addr_parse("[2001:db8::1]:8443", &a), 0);
    pd_addr_format(&a, buf, sizeof buf);
    CHECK(strcmp(buf, "[2001:db8::1]:8443") == 0);
}
