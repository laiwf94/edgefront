/* Failure modes: crashes, stalled peers and connection churn. */
#include "harness.h"
#include "test.h"
#include "upstream.h"

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

static struct proc p;
static struct upstream u;
static uint16_t port;
static char sock[64];

static void config(char *buf, size_t len, const char *extra)
{
    snprintf(buf, len,
             "workers 4\n"
             "drain_timeout 5s\n"
             "admin_socket %s\n"
             "listener f {\n"
             "  listen 127.0.0.1:%u\n"
             "  upstream 127.0.0.1:%u\n"
             "%s"
             "}\n",
             sock, port, u.port, extra);
}

static void start(enum upstream_mode mode, uint64_t param, const char *extra)
{
    char conf[2048];
    port = free_port();
    snprintf(sock, sizeof sock, "/tmp/pacerd-fail-%d-%u.sock", (int)getpid(), port);
    CHECK_EQ_INT(upstream_start(&u, mode, param), 0);
    config(conf, sizeof conf, extra);
    CHECK_EQ_INT(proc_start(&p, conf), 0);
}

static void finish(void)
{
    if (test_failed_in_current()) {
        proc_dump(&p);
    }
    CHECK_EQ_INT(proc_stop(&p, SIGTERM, 10000), 0);
    proc_cleanup(&p);
    upstream_stop(&u);
}

static int works(int fd)
{
    char buf[4];
    return fd >= 0 && send_all(fd, "ping", 4) == 4 && recv_all(fd, buf, 4, 3000) == 4 &&
           memcmp(buf, "ping", 4) == 0;
}

static uint64_t stat_value(const char *stats, const char *name)
{
    char key[64];
    snprintf(key, sizeof key, " %s=", name);
    const char *s = strstr(stats, key);
    return s ? strtoull(s + strlen(key), NULL, 10) : UINT64_MAX;
}

TEST(failure_kill_and_restart_on_the_same_port)
{
    start(UP_ECHO, 0, "");
    int open_fds[8];
    for (int i = 0; i < 8; i++) {
        open_fds[i] = tcp_connect(port);
        CHECK(works(open_fds[i]));
    }
    /* SIGKILL: no drain, no cleanup. Clients see their connections die. */
    CHECK_EQ_INT(proc_stop(&p, SIGKILL, 5000), 128 + SIGKILL);
    for (int i = 0; i < 8; i++) {
        char buf[4];
        CHECK(recv(open_fds[i], buf, sizeof buf, 0) <= 0);
        close(open_fds[i]);
    }
    /* The restart binds the same port despite TIME_WAIT, and takes over
     * the admin socket file the dead process left behind. */
    char conf[2048];
    config(conf, sizeof conf, "");
    proc_cleanup(&p);
    CHECK_EQ_INT(proc_start(&p, conf), 0);
    int fd = tcp_connect(port);
    CHECK(works(fd));
    close(fd);
    char out[4096];
    const char *args[] = {"-s", sock, "ping", NULL};
    CHECK_EQ_INT(run_ctl(out, sizeof out, args), 0);
    finish();
}

TEST(failure_stalled_upstream_hits_the_idle_timeout)
{
    /* The upstream accepts and then goes silent, as behind a partition:
     * the connection is closed by the idle timeout, not held for ever. */
    start(UP_STALL, 0, "  idle_timeout 1s\n");
    int fd = tcp_connect(port);
    CHECK_EQ_INT(send_all(fd, "hello", 5), 5);
    uint64_t t0 = now_ms();
    CHECK_EQ_INT(recv_to_eof(fd, NULL, 5000), 0);
    uint64_t ms = now_ms() - t0;
    CHECK(ms >= 800);
    CHECK(ms < 2500);
    close(fd);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=idle_timeout bytes_up=5 bytes_down=0", &from, 3000));
    finish();
}

TEST(failure_stalled_upstream_with_a_full_buffer)
{
    /* The client keeps sending into a stalled upstream: the proxy's buffer
     * and the socket buffers fill, the client blocks, and the slow-client
     * check closes the connection; the proxy never buffers more. */
    start(UP_STALL, 0, "  buffer 16k\n  min_rate 1k per 1s\n");
    int fd = tcp_connect(port);
    struct timeval tv = {0, 200000};
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
    static char chunk[65536];
    long sent = 0;
    uint64_t t0 = now_ms();
    while (now_ms() - t0 < 4000) {
        ssize_t n = send(fd, chunk, sizeof chunk, MSG_NOSIGNAL);
        if (n > 0) {
            sent += n;
        } else if (n < 0 && errno != EAGAIN && errno != EWOULDBLOCK) {
            break;
        }
    }
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=slow_client", &from, 3000));
    /* Kernel buffers on both sides plus one 16 KiB proxy buffer. */
    CHECK(sent < 32L * 1024 * 1024);
    close(fd);
    finish();
}

TEST(failure_upstream_closes_first_while_client_sends)
{
    /* The upstream answers 5 bytes and closes without reading; the client
     * still gets the 5 bytes and then EOF. */
    start(UP_SOURCE, 5, "");
    int fd = tcp_connect(port);
    CHECK_EQ_INT(recv_to_eof(fd, NULL, 3000), 5);
    close(fd);
    finish();
}

struct churner {
    uint16_t port;
    char src[16];
    int done;
};

static void *churn(void *arg)
{
    struct churner *c = arg;
    for (int i = 0; i < 150; i++) {
        int fd = tcp_connect_from(c->port, c->src);
        if (fd < 0) {
            continue;
        }
        if (i % 3 == 0) {
            struct linger lg = {1, 0};
            setsockopt(fd, SOL_SOCKET, SO_LINGER, &lg, sizeof lg);
        } else if (works(fd)) {
            c->done++;
        }
        close(fd);
    }
    return NULL;
}

TEST(failure_connection_churn_keeps_books_balanced)
{
    /* Eight clients open and close connections as fast as they can, a
     * third of them with a reset, under per-client limits that refuse
     * some. Afterwards every accepted connection is accounted for, no
     * connection is open and no hold is left in the client table. */
    start(UP_ECHO, 0, "  per_ip_max_conns 2\n  per_ip_conn_rate 200/s burst 50\n");
    enum { N = 8 };
    struct churner cs[N];
    pthread_t th[N];
    for (int i = 0; i < N; i++) {
        cs[i].port = port;
        snprintf(cs[i].src, sizeof cs[i].src, "127.0.1.%d", i % 4 + 1);
        cs[i].done = 0;
        pthread_create(&th[i], NULL, churn, &cs[i]);
    }
    int done = 0;
    for (int i = 0; i < N; i++) {
        pthread_join(th[i], NULL);
        done += cs[i].done;
    }
    CHECK(done > 0);
    char out[8192];
    const char *args[] = {"-s", sock, "stats", NULL};
    uint64_t open = UINT64_MAX;
    for (int tries = 0; tries < 50 && open != 0; tries++) {
        CHECK_EQ_INT(run_ctl(out, sizeof out, args), 0);
        const char *o = strstr(out, " open=");
        open = o ? strtoull(o + 6, NULL, 10) : UINT64_MAX;
        usleep(100 * 1000);
    }
    CHECK_EQ_U64(open, 0);
    uint64_t accepted = stat_value(out, "accepted");
    uint64_t closed = 0;
    const char *closes[] = {"closed_done",  "closed_idle",  "closed_slow", "closed_upstream_failed",
                            "closed_reset", "closed_drain", "closed_other"};
    for (size_t i = 0; i < sizeof closes / sizeof closes[0]; i++) {
        closed += stat_value(out, closes[i]);
    }
    CHECK(accepted > 0);
    CHECK_EQ_U64(closed, accepted);
    uint64_t refused =
        stat_value(out, "rejected_per_ip_conns") + stat_value(out, "rejected_per_ip_rate");
    CHECK(refused > 0);
    CHECK_EQ_U64(accepted + refused, (uint64_t)N * 150);
    finish();
    /* Every hold was released: the drain log shows no table errors. */
    CHECK(strstr(p.log, "client table lost a hold") == NULL);
}
