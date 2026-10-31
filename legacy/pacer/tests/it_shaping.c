/* Bytes-per-second shaping against the real binary. */
#include "harness.h"
#include "test.h"
#include "upstream.h"

#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define KIB 1024ull
#define MIB (1024ull * 1024)

static struct proc p;
static struct upstream u;

static uint16_t start(enum upstream_mode mode, uint64_t param, const char *extra)
{
    char conf[2048];
    uint16_t port = free_port();
    CHECK_EQ_INT(upstream_start(&u, mode, param), 0);
    snprintf(conf, sizeof conf,
             "workers 2\n"
             "listener shape {\n"
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

struct download {
    uint16_t port;
    const char *src;
    long got;
    uint64_t ms;
};

static void *download(void *arg)
{
    struct download *d = arg;
    uint64_t t0 = now_ms();
    int fd = tcp_connect_from(d->port, d->src);
    d->got = recv_to_eof(fd, NULL, 30000);
    d->ms = now_ms() - t0;
    close(fd);
    return NULL;
}

TEST(shaping_per_ip_download_rate)
{
    /* 1 MiB/s with a 64 KiB burst: 2 MiB + 64 KiB takes about 2 s. */
    const uint64_t total = 2 * MIB + 64 * KIB;
    uint16_t port = start(UP_SOURCE, total, "  per_ip_bandwidth 1m/s burst 64k\n");
    struct download d = {port, NULL, 0, 0};
    download(&d);
    CHECK_EQ_INT(d.got, (long long)total);
    CHECK(d.ms >= 1800);
    CHECK(d.ms <= 2400);
    finish();
}

struct upload {
    int fd;
    uint64_t total;
};

static void *upload(void *arg)
{
    struct upload *up = arg;
    static uint8_t buf[16384];
    for (uint64_t off = 0; off < up->total; off += sizeof buf) {
        pattern(buf, sizeof buf, off);
        if (send_all(up->fd, buf, sizeof buf) < 0) {
            break;
        }
    }
    shutdown(up->fd, SHUT_WR);
    return NULL;
}

TEST(shaping_per_ip_upload_rate)
{
    const uint64_t total = 2 * MIB + 64 * KIB;
    uint16_t port = start(UP_HASH, 0, "  per_ip_bandwidth 1m/s burst 64k\n");
    int fd = tcp_connect(port);
    struct upload up = {fd, total};
    uint64_t t0 = now_ms();
    pthread_t th;
    pthread_create(&th, NULL, upload, &up);
    char reply[64] = {0};
    CHECK(recv_all(fd, reply, sizeof reply - 1, 30000) > 0);
    uint64_t ms = now_ms() - t0;
    pthread_join(th, NULL);
    char count[32];
    snprintf(count, sizeof count, " %llu\n", (unsigned long long)total);
    CHECK(strstr(reply, count) != NULL);
    CHECK(ms >= 1800);
    CHECK(ms <= 2400);
    close(fd);
    finish();
}

TEST(shaping_listener_rate_is_shared)
{
    /* Two clients from different addresses, 1 MiB each, through a listener
     * capped at 1 MiB/s: together they take about 2 s, and neither gets
     * the whole link. */
    const uint64_t each = MIB;
    uint16_t port = start(UP_SOURCE, each, "  bandwidth 1m/s burst 32k\n");
    struct download a = {port, "127.0.0.2", 0, 0}, b = {port, "127.0.0.3", 0, 0};
    pthread_t ta, tb;
    pthread_create(&ta, NULL, download, &a);
    pthread_create(&tb, NULL, download, &b);
    pthread_join(ta, NULL);
    pthread_join(tb, NULL);
    CHECK_EQ_INT(a.got, (long long)each);
    CHECK_EQ_INT(b.got, (long long)each);
    uint64_t slow = a.ms > b.ms ? a.ms : b.ms;
    uint64_t fast = a.ms > b.ms ? b.ms : a.ms;
    CHECK(slow >= 1800);
    CHECK(slow <= 2600);
    CHECK(fast >= 1200);
    finish();
}

TEST(shaping_off_is_fast)
{
    const uint64_t total = 32 * MIB;
    uint16_t port = start(UP_SOURCE, total, "");
    struct download d = {port, NULL, 0, 0};
    download(&d);
    CHECK_EQ_INT(d.got, (long long)total);
    CHECK(d.ms < 1500);
    finish();
}
