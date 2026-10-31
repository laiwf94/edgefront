/* Graceful drain and config reload against the real binary. */
#include "harness.h"
#include "test.h"
#include "upstream.h"

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define MIB (1024ull * 1024)

static struct proc p;
static struct upstream u;
static uint16_t port;

static void config(char *buf, size_t len, const char *extra)
{
    snprintf(buf, len,
             "workers 2\n"
             "drain_timeout 10s\n"
             "listener life {\n"
             "  listen 127.0.0.1:%u\n"
             "  upstream 127.0.0.1:%u\n"
             "%s"
             "}\n",
             port, u.port, extra);
}

static void start(enum upstream_mode mode, uint64_t param, const char *extra)
{
    char conf[2048];
    port = free_port();
    CHECK_EQ_INT(upstream_start(&u, mode, param), 0);
    config(conf, sizeof conf, extra);
    CHECK_EQ_INT(proc_start(&p, conf), 0);
}

static void finish(int expect_running)
{
    if (test_failed_in_current()) {
        proc_dump(&p);
    }
    if (expect_running) {
        CHECK_EQ_INT(proc_stop(&p, SIGTERM, 15000), 0);
    }
    proc_cleanup(&p);
    upstream_stop(&u);
}

static int refused_now(void)
{
    int fd = tcp_connect(port);
    if (fd < 0) {
        return errno == ECONNREFUSED;
    }
    close(fd);
    return 0;
}

struct transfer {
    uint64_t total;
    long got;
    uint64_t hash;
};

static void *download(void *arg)
{
    struct transfer *t = arg;
    int fd = tcp_connect(port);
    t->got = recv_to_eof(fd, &t->hash, 30000);
    close(fd);
    return NULL;
}

TEST(drain_completes_transfers_in_flight)
{
    /* A shaped 4 MiB download takes about 2 s. SIGTERM arrives in the
     * middle: new connections are refused at once, but the download
     * finishes byte-for-byte before the process exits. */
    const uint64_t total = 4 * MIB;
    start(UP_SOURCE, total, "  per_ip_bandwidth 2m/s burst 256k\n");
    struct transfer t = {total, 0, 0};
    pthread_t th;
    pthread_create(&th, NULL, download, &t);
    usleep(500 * 1000);
    kill(p.pid, SIGTERM);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "msg=draining reason=SIGTERM", &from, 3000));
    int refused = 0;
    for (int i = 0; i < 100 && !refused; i++) {
        refused = refused_now();
        usleep(10 * 1000);
    }
    CHECK(refused);
    pthread_join(th, NULL);
    CHECK_EQ_INT(t.got, (long long)total);
    static uint8_t buf[65536];
    uint64_t want = FNV_START;
    for (uint64_t off = 0; off < total; off += sizeof buf) {
        pattern(buf, sizeof buf, off);
        want = fnv(want, buf, sizeof buf);
    }
    CHECK_EQ_U64(t.hash, want);
    CHECK_EQ_INT(proc_wait_exit(&p, 10000), 0);
    CHECK(strstr(p.log, "reason=done bytes_up=0 bytes_down=4194304") != NULL);
    CHECK(strstr(p.log, "msg=drained") != NULL);
    CHECK(strstr(p.log, "drain timeout") == NULL);
    finish(0);
}

TEST(drain_timeout_closes_stragglers)
{
    char conf[2048];
    port = free_port();
    CHECK_EQ_INT(upstream_start(&u, UP_ECHO, 0), 0);
    snprintf(conf, sizeof conf,
             "drain_timeout 1s\n"
             "listener life {\n"
             "  listen 127.0.0.1:%u\n"
             "  upstream 127.0.0.1:%u\n"
             "}\n",
             port, u.port);
    CHECK_EQ_INT(proc_start(&p, conf), 0);
    int fd = tcp_connect(port);
    char buf[4];
    CHECK_EQ_INT(send_all(fd, "idle", 4), 4);
    CHECK_EQ_INT(recv_all(fd, buf, 4, 3000), 4);
    uint64_t t0 = now_ms();
    kill(p.pid, SIGTERM);
    CHECK_EQ_INT(proc_wait_exit(&p, 10000), 0);
    uint64_t ms = now_ms() - t0;
    CHECK(ms >= 900);
    CHECK(ms < 3000);
    CHECK(strstr(p.log, "msg=\"drain timeout\" worker=0 closing=1") != NULL);
    CHECK(strstr(p.log, "reason=drain_timeout") != NULL);
    close(fd);
    finish(0);
}

static int works(int fd)
{
    char buf[4];
    return fd >= 0 && send_all(fd, "ping", 4) == 4 && recv_all(fd, buf, 4, 3000) == 4;
}

TEST(reload_applies_new_limits)
{
    start(UP_ECHO, 0, "  per_ip_max_conns 1\n");
    int a = tcp_connect(port);
    CHECK(works(a));
    int b = tcp_connect(port);
    CHECK(!works(b));
    close(b);

    char conf[2048];
    config(conf, sizeof conf, "  per_ip_max_conns 3\n");
    CHECK_EQ_INT(proc_rewrite_config(&p, conf), 0);
    kill(p.pid, SIGHUP);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "msg=reloaded source=SIGHUP", &from, 3000));
    int c = tcp_connect(port);
    int d = tcp_connect(port);
    CHECK(works(c));
    CHECK(works(d));
    int e = tcp_connect(port);
    CHECK(!works(e));
    close(a);
    close(c);
    close(d);
    close(e);
    finish(1);
}

TEST(reload_rejects_what_needs_a_restart)
{
    start(UP_ECHO, 0, "");
    char conf[2048];
    snprintf(conf, sizeof conf,
             "listener life {\n"
             "  listen 127.0.0.1:%u\n"
             "  upstream 127.0.0.1:%u\n"
             "}\n",
             free_port(), u.port);
    CHECK_EQ_INT(proc_rewrite_config(&p, conf), 0);
    kill(p.pid, SIGHUP);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "error=\"workers cannot change without a restart\"", &from, 3000));

    config(conf, sizeof conf, "  table_size 1024\n");
    CHECK_EQ_INT(proc_rewrite_config(&p, conf), 0);
    kill(p.pid, SIGHUP);
    CHECK(proc_wait_log(&p, "listener life: table_size cannot change", &from, 3000));

    snprintf(conf, sizeof conf,
             "workers 2\n"
             "listener life {\n"
             "  listen 127.0.0.1:%u\n"
             "  upstream 127.0.0.1:%u\n"
             "}\n",
             free_port(), u.port);
    CHECK_EQ_INT(proc_rewrite_config(&p, conf), 0);
    kill(p.pid, SIGHUP);
    CHECK(proc_wait_log(&p, "listener life: listen cannot change", &from, 3000));

    CHECK_EQ_INT(proc_rewrite_config(&p, "workers 2\nlistener life {\n"), 0);
    kill(p.pid, SIGHUP);
    CHECK(proc_wait_log(&p, "error=\"line 2: listener life: missing }\"", &from, 3000));

    /* The running config is untouched. */
    int fd = tcp_connect(port);
    CHECK(works(fd));
    close(fd);
    finish(1);
}

TEST(reload_can_move_the_upstream)
{
    start(UP_SOURCE, 5, "");
    struct upstream other;
    CHECK_EQ_INT(upstream_start(&other, UP_SOURCE, 7), 0);
    int fd = tcp_connect(port);
    CHECK_EQ_INT(recv_to_eof(fd, NULL, 3000), 5);
    close(fd);
    char conf[2048];
    snprintf(conf, sizeof conf,
             "workers 2\n"
             "drain_timeout 10s\n"
             "listener life {\n"
             "  listen 127.0.0.1:%u\n"
             "  upstream 127.0.0.1:%u\n"
             "}\n",
             port, other.port);
    CHECK_EQ_INT(proc_rewrite_config(&p, conf), 0);
    kill(p.pid, SIGHUP);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "msg=reloaded", &from, 3000));
    fd = tcp_connect(port);
    CHECK_EQ_INT(recv_to_eof(fd, NULL, 3000), 7);
    close(fd);
    finish(1);
    upstream_stop(&other);
}
