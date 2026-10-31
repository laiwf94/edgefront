/* Byte-level proxying through the real binary. */
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

static int start_proxy(uint16_t *port, uint16_t upstream_port, const char *extra)
{
    char conf[2048];
    *port = free_port();
    snprintf(conf, sizeof conf,
             "workers 2\n"
             "drain_timeout 5s\n"
             "listener t {\n"
             "  listen 127.0.0.1:%u\n"
             "  upstream 127.0.0.1:%u\n"
             "%s"
             "}\n",
             *port, upstream_port, extra);
    return proc_start(&p, conf);
}

static void finish(void)
{
    if (test_failed_in_current()) {
        proc_dump(&p);
    }
    CHECK_EQ_INT(proc_stop(&p, SIGTERM, 10000), 0);
    proc_cleanup(&p);
}

TEST(proxy_echo_round_trip)
{
    struct upstream u;
    uint16_t port = 0;
    CHECK_EQ_INT(upstream_start(&u, UP_ECHO, 0), 0);
    CHECK_EQ_INT(start_proxy(&port, u.port, ""), 0);
    int fd = tcp_connect(port);
    CHECK(fd >= 0);
    char buf[16] = {0};
    CHECK_EQ_INT(send_all(fd, "hello pacer", 11), 11);
    CHECK_EQ_INT(recv_all(fd, buf, 11, 5000), 11);
    CHECK(memcmp(buf, "hello pacer", 11) == 0);
    shutdown(fd, SHUT_WR);
    CHECK_EQ_INT(recv_to_eof(fd, NULL, 5000), 0);
    close(fd);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=done bytes_up=11 bytes_down=11", &from, 5000));
    finish();
    upstream_stop(&u);
}

struct sender {
    int fd;
    uint64_t total;
    uint64_t hash;
};

static void *send_pattern(void *arg)
{
    struct sender *s = arg;
    static uint8_t buf[65536];
    s->hash = FNV_START;
    for (uint64_t off = 0; off < s->total; off += sizeof buf) {
        pattern(buf, sizeof buf, off);
        s->hash = fnv(s->hash, buf, sizeof buf);
        if (send_all(s->fd, buf, sizeof buf) < 0) {
            break;
        }
    }
    shutdown(s->fd, SHUT_WR);
    return NULL;
}

TEST(proxy_large_upload_arrives_intact_with_half_close)
{
    /* 64 MiB up, then FIN; the upstream answers only after it sees EOF, so
     * this also checks the half-closed connection still carries the reply. */
    struct upstream u;
    uint16_t port = 0;
    CHECK_EQ_INT(upstream_start(&u, UP_HASH, 0), 0);
    CHECK_EQ_INT(start_proxy(&port, u.port, "  buffer 64k\n"), 0);
    int fd = tcp_connect(port);
    CHECK(fd >= 0);
    struct sender s = {fd, 64ull << 20, 0};
    pthread_t th;
    pthread_create(&th, NULL, send_pattern, &s);
    char reply[64] = {0};
    long n = recv_all(fd, reply, sizeof reply - 1, 30000);
    pthread_join(th, NULL);
    char want[64];
    snprintf(want, sizeof want, "%016llx %llu\n", (unsigned long long)s.hash,
             (unsigned long long)s.total);
    CHECK(n > 0);
    CHECK(strcmp(reply, want) == 0);
    close(fd);
    finish();
    upstream_stop(&u);
}

TEST(proxy_large_download_arrives_intact)
{
    const uint64_t total = 64ull << 20;
    struct upstream u;
    uint16_t port = 0;
    CHECK_EQ_INT(upstream_start(&u, UP_SOURCE, total), 0);
    CHECK_EQ_INT(start_proxy(&port, u.port, ""), 0);
    int fd = tcp_connect(port);
    uint64_t got_hash = 0;
    CHECK_EQ_INT(recv_to_eof(fd, &got_hash, 30000), (long long)total);
    static uint8_t buf[65536];
    uint64_t want = FNV_START;
    for (uint64_t off = 0; off < total; off += sizeof buf) {
        pattern(buf, sizeof buf, off);
        want = fnv(want, buf, sizeof buf);
    }
    CHECK_EQ_U64(got_hash, want);
    close(fd);
    finish();
    upstream_stop(&u);
}

static long rss_kb(pid_t pid)
{
    char path[64], line[256];
    snprintf(path, sizeof path, "/proc/%d/status", (int)pid);
    FILE *f = fopen(path, "r");
    long kb = -1;
    while (f && fgets(line, sizeof line, f)) {
        if (strncmp(line, "VmRSS:", 6) == 0) {
            kb = strtol(line + 6, NULL, 10);
            break;
        }
    }
    if (f) {
        fclose(f);
    }
    return kb;
}

TEST(proxy_slow_reader_does_not_grow_memory)
{
    /* The upstream pushes 256 MiB at a client that does not read for a
     * while. The proxy must stop reading from the upstream once its one
     * buffer is full, so its memory stays flat. */
    const uint64_t total = 256ull << 20;
    struct upstream u;
    uint16_t port = 0;
    CHECK_EQ_INT(upstream_start(&u, UP_SOURCE, total), 0);
    CHECK_EQ_INT(start_proxy(&port, u.port, ""), 0);
    long before = rss_kb(p.pid);
    int fd = tcp_connect(port);
    usleep(1500 * 1000);
    long during = rss_kb(p.pid);
    CHECK(before > 0);
    CHECK(during - before < 16L * 1024);
    uint64_t h = 0;
    CHECK_EQ_INT(recv_to_eof(fd, &h, 60000), (long long)total);
    close(fd);
    finish();
    upstream_stop(&u);
}

TEST(proxy_upstream_refused_closes_client)
{
    uint16_t port = 0, dead = free_port();
    CHECK_EQ_INT(start_proxy(&port, dead, ""), 0);
    int fd = tcp_connect(port);
    CHECK(fd >= 0);
    CHECK_EQ_INT(recv_to_eof(fd, NULL, 5000), 0);
    close(fd);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=upstream_refused", &from, 5000));
    finish();
}

TEST(proxy_upstream_connect_timeout)
{
    /* A listener that never accepts, with its tiny backlog already full:
     * further SYNs are dropped and the connect hangs. */
    int lfd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_addr.s_addr = htonl(0x7f000001);
    socklen_t alen = sizeof a;
    CHECK_EQ_INT(bind(lfd, (struct sockaddr *)&a, sizeof a), 0);
    CHECK_EQ_INT(listen(lfd, 0), 0);
    CHECK_EQ_INT(getsockname(lfd, (struct sockaddr *)&a, &alen), 0);
    uint16_t blocked = ntohs(a.sin_port);
    int fillers[4];
    for (int i = 0; i < 4; i++) {
        fillers[i] = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
        /* Non-blocking: these either complete or stay pending. */
        (void)connect(fillers[i], (struct sockaddr *)&a, sizeof a);
    }
    usleep(100 * 1000);
    uint16_t port = 0;
    CHECK_EQ_INT(start_proxy(&port, blocked, "  connect_timeout 300ms\n"), 0);
    int fd = tcp_connect(port);
    uint64_t t0 = now_ms();
    CHECK_EQ_INT(recv_to_eof(fd, NULL, 5000), 0);
    uint64_t took = now_ms() - t0;
    CHECK(took >= 250);
    CHECK(took < 3000);
    close(fd);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=connect_timeout", &from, 5000));
    finish();
    for (int i = 0; i < 4; i++) {
        close(fillers[i]);
    }
    close(lfd);
}

TEST(proxy_upstream_reset_is_reported)
{
    struct upstream u;
    uint16_t port = 0;
    CHECK_EQ_INT(upstream_start(&u, UP_RESET, 5), 0);
    CHECK_EQ_INT(start_proxy(&port, u.port, ""), 0);
    int fd = tcp_connect(port);
    CHECK_EQ_INT(send_all(fd, "12345", 5), 5);
    char buf[8];
    CHECK(recv(fd, buf, sizeof buf, 0) <= 0);
    close(fd);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=upstream_reset", &from, 5000));
    finish();
    upstream_stop(&u);
}

TEST(proxy_client_reset_is_reported)
{
    struct upstream u;
    uint16_t port = 0;
    CHECK_EQ_INT(upstream_start(&u, UP_ECHO, 0), 0);
    CHECK_EQ_INT(start_proxy(&port, u.port, ""), 0);
    int fd = tcp_connect(port);
    char buf[4];
    CHECK_EQ_INT(send_all(fd, "ping", 4), 4);
    CHECK_EQ_INT(recv_all(fd, buf, 4, 5000), 4);
    struct linger lg = {1, 0};
    setsockopt(fd, SOL_SOCKET, SO_LINGER, &lg, sizeof lg);
    close(fd);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=client_reset", &from, 5000));
    finish();
    upstream_stop(&u);
}

struct echo_client {
    uint16_t port;
    int ok;
};

static void *echo_many(void *arg)
{
    struct echo_client *e = arg;
    int fd = tcp_connect(e->port);
    char out[256], in[256];
    e->ok = fd >= 0;
    for (int i = 0; e->ok && i < 50; i++) {
        int n = snprintf(out, sizeof out, "message %d from %p", i, arg);
        e->ok = send_all(fd, out, (size_t)n) == n && recv_all(fd, in, (size_t)n, 5000) == n &&
                memcmp(in, out, (size_t)n) == 0;
    }
    if (fd >= 0) {
        close(fd);
    }
    return NULL;
}

TEST(proxy_many_concurrent_clients)
{
    struct upstream u;
    uint16_t port = 0;
    CHECK_EQ_INT(upstream_start(&u, UP_ECHO, 0), 0);
    CHECK_EQ_INT(start_proxy(&port, u.port, ""), 0);
    enum { N = 64 };
    pthread_t th[N];
    struct echo_client ec[N];
    for (int i = 0; i < N; i++) {
        ec[i].port = port;
        pthread_create(&th[i], NULL, echo_many, &ec[i]);
    }
    int ok = 0;
    for (int i = 0; i < N; i++) {
        pthread_join(th[i], NULL);
        ok += ec[i].ok;
    }
    CHECK_EQ_INT(ok, N);
    finish();
    upstream_stop(&u);
}
