/* Connection admission limits against the real binary. */
#include "harness.h"
#include "test.h"
#include "upstream.h"

#include <arpa/inet.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static struct proc p;
static struct upstream u;

static uint16_t start(const char *extra, int workers)
{
    char conf[2048];
    uint16_t port = free_port();
    CHECK_EQ_INT(upstream_start(&u, UP_ECHO, 0), 0);
    snprintf(conf, sizeof conf,
             "workers %d\n"
             "listener lim {\n"
             "  listen 127.0.0.1:%u\n"
             "  upstream 127.0.0.1:%u\n"
             "%s"
             "}\n",
             workers, port, u.port, extra);
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

/* 1 if the connection proxies an echo, 0 if the proxy closed it. */
static int works(int fd)
{
    char buf[4];
    return fd >= 0 && send_all(fd, "ping", 4) == 4 && recv_all(fd, buf, 4, 3000) == 4 &&
           memcmp(buf, "ping", 4) == 0;
}

static int refused(int fd)
{
    char buf[4];
    /* Admission happens before any byte is read: a refused connection is
     * closed by the proxy without data. */
    return fd >= 0 && recv_all(fd, buf, 1, 3000) == -1;
}

TEST(limits_per_ip_concurrent_connections)
{
    uint16_t port = start("  per_ip_max_conns 3\n", 2);
    int fds[3];
    for (int i = 0; i < 3; i++) {
        fds[i] = tcp_connect(port);
        CHECK(works(fds[i]));
    }
    int extra = tcp_connect(port);
    CHECK(refused(extra));
    close(extra);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=per_ip_conns", &from, 3000));
    /* Another address is not affected. */
    int other = tcp_connect_from(port, "127.0.0.2");
    CHECK(works(other));
    close(other);
    /* Closing one frees a slot for the first address once the proxy has
     * seen it go. */
    struct sockaddr_in local = {0};
    socklen_t llen = sizeof local;
    CHECK_EQ_INT(getsockname(fds[0], (struct sockaddr *)&local, &llen), 0);
    char line[64];
    snprintf(line, sizeof line, "peer=127.0.0.1:%u reason=done", ntohs(local.sin_port));
    close(fds[0]);
    CHECK(proc_wait_log(&p, line, &from, 3000));
    int again = tcp_connect(port);
    CHECK(works(again));
    close(again);
    close(fds[1]);
    close(fds[2]);
    finish();
}

TEST(limits_per_ip_connection_rate_recovers)
{
    uint16_t port = start("  per_ip_conn_rate 5/s burst 5\n", 1);
    int ok = 0;
    for (int i = 0; i < 5; i++) {
        int fd = tcp_connect(port);
        ok += works(fd);
        close(fd);
    }
    CHECK_EQ_INT(ok, 5);
    int fd = tcp_connect(port);
    CHECK(refused(fd));
    close(fd);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=per_ip_rate", &from, 3000));
    /* One token returns every 200 ms. */
    usleep(250 * 1000);
    fd = tcp_connect(port);
    CHECK(works(fd));
    close(fd);
    finish();
}

TEST(limits_listener_connection_cap)
{
    uint16_t port = start("  max_conns 4\n", 2);
    int fds[4];
    char src[32];
    for (int i = 0; i < 4; i++) {
        snprintf(src, sizeof src, "127.0.0.%d", 10 + i);
        fds[i] = tcp_connect_from(port, src);
        CHECK(works(fds[i]));
    }
    int extra = tcp_connect_from(port, "127.0.0.20");
    CHECK(refused(extra));
    close(extra);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=listener_full", &from, 3000));
    for (int i = 0; i < 4; i++) {
        close(fds[i]);
    }
    finish();
}

static long cpu_ticks(pid_t pid)
{
    char path[64], buf[1024];
    snprintf(path, sizeof path, "/proc/%d/stat", (int)pid);
    FILE *f = fopen(path, "r");
    if (!f) {
        return -1;
    }
    size_t n = fread(buf, 1, sizeof buf - 1, f);
    fclose(f);
    buf[n] = '\0';
    /* utime and stime are fields 14 and 15, after the ")" of the name. */
    const char *s = strrchr(buf, ')');
    long utime = 0, stime = 0;
    for (int field = 3; s && *s; s++) {
        if (*s == ' ') {
            field++;
            if (field == 14) {
                utime = strtol(s + 1, NULL, 10);
            } else if (field == 15) {
                stime = strtol(s + 1, NULL, 10);
                break;
            }
        }
    }
    return utime + stime;
}

TEST(limits_fd_exhaustion_sheds_without_spinning)
{
    /* With 20 fds, after its own a single worker has room for a handful of
     * connections (two fds each). Further clients are accepted and closed
     * at once instead of hanging in the queue, and the loop stays idle. */
    harness_nofile = 20;
    uint16_t port = start("", 1);
    harness_nofile = 0;
    int held[16];
    int nheld = 0;
    for (int i = 0; i < 16; i++) {
        int fd = tcp_connect(port);
        if (works(fd)) {
            held[nheld++] = fd;
        } else {
            close(fd);
        }
    }
    CHECK(nheld >= 2);
    CHECK(nheld < 16);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=fd_exhausted", &from, 3000));
    int pending = tcp_connect(port);
    long before = cpu_ticks(p.pid);
    usleep(1000 * 1000);
    long after = cpu_ticks(p.pid);
    CHECK(refused(pending));
    close(pending);
    /* Less than a quarter of one CPU over that second. */
    CHECK(after - before < 25);
    for (int i = 0; i < nheld; i++) {
        close(held[i]);
    }
    usleep(200 * 1000);
    int fd = tcp_connect(port);
    CHECK(works(fd));
    close(fd);
    finish();
}
