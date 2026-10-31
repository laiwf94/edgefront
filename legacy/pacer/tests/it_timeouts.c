/* Idle timeout and slow-client cut-off against the real binary. */
#include "harness.h"
#include "test.h"
#include "upstream.h"

#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static struct proc p;
static struct upstream u;

static uint16_t start(enum upstream_mode mode, const char *extra)
{
    char conf[2048];
    uint16_t port = free_port();
    CHECK_EQ_INT(upstream_start(&u, mode, 0), 0);
    snprintf(conf, sizeof conf,
             "listener slow {\n"
             "  listen 127.0.0.1:%u\n"
             "  upstream 127.0.0.1:%u\n"
             "%s"
             "}\n",
             port, u.port, extra);
    CHECK_EQ_INT(proc_start(&p, conf), 0);
    return port;
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

/* Milliseconds until the proxy closes fd (EOF or reset), or -1. */
static long ms_until_closed(int fd, int timeout_ms)
{
    uint64_t t0 = now_ms();
    char buf[256];
    while (now_ms() - t0 < (uint64_t)timeout_ms) {
        long n = recv_all(fd, buf, 1, timeout_ms);
        if (n <= 0) {
            return (long)(now_ms() - t0);
        }
    }
    return -1;
}

TEST(timeout_idle_connection_is_closed)
{
    uint16_t port = start(UP_ECHO, "  idle_timeout 1s\n");
    int fd = tcp_connect(port);
    char buf[2];
    CHECK_EQ_INT(send_all(fd, "hi", 2), 2);
    CHECK_EQ_INT(recv_all(fd, buf, 2, 3000), 2);
    long ms = ms_until_closed(fd, 5000);
    CHECK(ms >= 800);
    CHECK(ms <= 2000);
    close(fd);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=idle_timeout", &from, 3000));
    finish();
}

struct trickle {
    int fd;
    int interval_ms;
    int chunk;
    int rounds;
    int sent;
};

static void *trickle(void *arg)
{
    struct trickle *t = arg;
    char buf[512];
    memset(buf, 'x', sizeof buf);
    for (int i = 0; i < t->rounds; i++) {
        if (send_all(t->fd, buf, (size_t)t->chunk) < 0) {
            break;
        }
        t->sent++;
        usleep((useconds_t)t->interval_ms * 1000);
    }
    return NULL;
}

TEST(timeout_slowloris_is_cut_off)
{
    /* One byte every 200 ms is 5 bytes/s: under the idle timeout, but far
     * below 100 bytes per 2 s. The check starts after the first window. */
    uint16_t port = start(UP_HASH, "  idle_timeout 1m\n  min_rate 100 per 2s\n");
    int fd = tcp_connect(port);
    struct trickle t = {fd, 200, 1, 50, 0};
    pthread_t th;
    uint64_t t0 = now_ms();
    pthread_create(&th, NULL, trickle, &t);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=slow_client", &from, 8000));
    uint64_t ms = now_ms() - t0;
    CHECK(ms >= 1800);
    CHECK(ms <= 3500);
    pthread_join(th, NULL);
    CHECK(t.sent < 50);
    close(fd);
    finish();
}

TEST(timeout_client_at_the_minimum_rate_survives)
{
    /* 64 bytes every 250 ms is 512 bytes per 2 s, above the 100 minimum. */
    uint16_t port = start(UP_HASH, "  min_rate 100 per 2s\n");
    int fd = tcp_connect(port);
    struct trickle t = {fd, 250, 64, 20, 0};
    trickle(&t);
    CHECK_EQ_INT(t.sent, 20);
    shutdown(fd, SHUT_WR);
    char reply[64] = {0};
    CHECK(recv_all(fd, reply, sizeof reply - 1, 3000) > 0);
    CHECK(strstr(reply, " 1280\n") != NULL);
    close(fd);
    CHECK_EQ_INT(proc_log_count(&p, "slow_client"), 0);
    finish();
}

TEST(timeout_shaped_client_is_not_called_slow)
{
    /* Our own shaping holds this client to 512 bytes/s, below its 1 KiB/s
     * minimum. It must not be cut for slowness we caused. */
    uint16_t port = start(UP_HASH, "  per_ip_bandwidth 512/s burst 512\n  min_rate 1k per 1s\n");
    int fd = tcp_connect(port);
    char buf[2048];
    memset(buf, 'y', sizeof buf);
    CHECK_EQ_INT(send_all(fd, buf, sizeof buf), (long long)sizeof buf);
    shutdown(fd, SHUT_WR);
    char reply[64] = {0};
    CHECK(recv_all(fd, reply, sizeof reply - 1, 10000) > 0);
    CHECK(strstr(reply, " 2048\n") != NULL);
    close(fd);
    CHECK_EQ_INT(proc_log_count(&p, "slow_client"), 0);
    finish();
}
