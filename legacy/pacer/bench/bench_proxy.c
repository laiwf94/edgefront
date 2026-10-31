/*
 * Proxy cost: throughput of one bulk download and request/response latency,
 * each measured on a direct connection to the upstream and through pacerd
 * with no limits configured.
 */
#include "../tests/harness.h"
#include "../tests/upstream.h"

#include <netinet/in.h>
#include <netinet/tcp.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define BULK_BYTES   (4ull << 30)
#define ROUND_TRIPS  50000
#define CONC_CLIENTS 32
#define CONC_TRIPS   5000

static int cmp_u64(const void *a, const void *b)
{
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static uint64_t now_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

/* MiB/s for one bulk download, or -1 if it came up short. */
static double bulk(uint16_t port)
{
    int fd = tcp_connect(port);
    uint64_t t0 = now_ns();
    long got = recv_to_eof(fd, NULL, 120000);
    double secs = (double)(now_ns() - t0) / 1e9;
    close(fd);
    if (got != (long)BULK_BYTES) {
        fprintf(stderr, "bulk transfer short: %ld\n", got);
        return -1;
    }
    return (double)BULK_BYTES / secs / (1 << 20);
}

static int latency(uint16_t port, double *p50, double *p99)
{
    static uint64_t samples[ROUND_TRIPS];
    int fd = tcp_connect(port);
    char buf[64];
    memset(buf, 'r', sizeof buf);
    for (int i = 0; i < ROUND_TRIPS; i++) {
        uint64_t t0 = now_ns();
        if (send_all(fd, buf, sizeof buf) < 0 || recv_all(fd, buf, sizeof buf, 5000) < 0) {
            fprintf(stderr, "round trip failed\n");
            close(fd);
            return -1;
        }
        samples[i] = now_ns() - t0;
    }
    close(fd);
    qsort(samples, ROUND_TRIPS, sizeof samples[0], cmp_u64);
    size_t mid = ROUND_TRIPS / 2, tail = (size_t)ROUND_TRIPS * 99 / 100;
    *p50 = (double)samples[mid] / 1000.0;
    *p99 = (double)samples[tail] / 1000.0;
    return 0;
}

struct conc {
    uint16_t port;
};

static void *conc_client(void *arg)
{
    struct conc *c = arg;
    int fd = tcp_connect(c->port);
    char buf[64];
    memset(buf, 'c', sizeof buf);
    for (int i = 0; i < CONC_TRIPS; i++) {
        if (send_all(fd, buf, sizeof buf) < 0 || recv_all(fd, buf, sizeof buf, 5000) < 0) {
            break;
        }
    }
    close(fd);
    return NULL;
}

static double concurrent(uint16_t port)
{
    pthread_t th[CONC_CLIENTS];
    struct conc c = {port};
    uint64_t t0 = now_ns();
    for (int i = 0; i < CONC_CLIENTS; i++) {
        pthread_create(&th[i], NULL, conc_client, &c);
    }
    for (int i = 0; i < CONC_CLIENTS; i++) {
        pthread_join(th[i], NULL);
    }
    double secs = (double)(now_ns() - t0) / 1e9;
    return (double)CONC_CLIENTS * CONC_TRIPS / secs;
}

/* CPU time (user + system) a process has used, in seconds. */
static double cpu_seconds(pid_t pid)
{
    char path[64], buf[1024];
    snprintf(path, sizeof path, "/proc/%d/stat", (int)pid);
    FILE *f = fopen(path, "r");
    if (!f) {
        return 0;
    }
    size_t n = fread(buf, 1, sizeof buf - 1, f);
    fclose(f);
    buf[n] = '\0';
    const char *s = strrchr(buf, ')');
    unsigned long long ut = 0, st = 0;
    int field = 2;
    for (; s && *s; s++) {
        if (*s == ' ' && ++field == 14) {
            ut = strtoull(s + 1, NULL, 10);
        } else if (*s == ' ' && field == 15) {
            st = strtoull(s + 1, NULL, 10);
            break;
        }
    }
    return (double)(ut + st) / (double)sysconf(_SC_CLK_TCK);
}

static int start_proxy(struct proc *p, uint16_t upstream, uint16_t *port, int workers)
{
    char conf[512];
    *port = free_port();
    snprintf(conf, sizeof conf,
             "workers %d\nlistener bench {\n listen 127.0.0.1:%u\n"
             " upstream 127.0.0.1:%u\n buffer 64k\n max_conns 10000\n}\n",
             workers, *port, upstream);
    return proc_start(p, conf);
}

int main(void)
{
    static struct proc p;
    struct upstream src, echo;
    uint16_t px_src = 0, px_echo = 0;
    if (upstream_start(&src, UP_ZEROS, BULK_BYTES) != 0 || upstream_start(&echo, UP_ECHO, 0) != 0) {
        fprintf(stderr, "cannot start upstreams\n");
        return 1;
    }
    static struct proc p2;
    if (start_proxy(&p, src.port, &px_src, 1) != 0 ||
        start_proxy(&p2, echo.port, &px_echo, 4) != 0) {
        fprintf(stderr, "cannot start pacerd\n");
        return 1;
    }
    double d50 = 0, d99 = 0, x50 = 0, x99 = 0;
    double direct_bulk = bulk(src.port);
    double proxied_bulk = bulk(px_src);
    int failed = direct_bulk < 0 || proxied_bulk < 0;
    failed |= latency(echo.port, &d50, &d99) != 0;
    failed |= latency(px_echo, &x50, &x99) != 0;
    double direct_conc = concurrent(echo.port);
    double cpu0 = cpu_seconds(p2.pid);
    double proxied_conc = concurrent(px_echo);
    double cpu_per_trip = (cpu_seconds(p2.pid) - cpu0) / (CONC_CLIENTS * CONC_TRIPS) * 1e6;

    printf("%-46s %12s %12s\n", "", "direct", "via pacerd");
    printf("%-46s %12.0f %12.0f\n", "bulk download, one connection (MiB/s)", direct_bulk,
           proxied_bulk);
    printf("%-46s %12.1f %12.1f\n", "64-byte round trip, p50 (us)", d50, x50);
    printf("%-46s %12.1f %12.1f\n", "64-byte round trip, p99 (us)", d99, x99);
    printf("%-46s %12.0f %12.0f\n", "round trips/s, 32 connections", direct_conc, proxied_conc);
    printf("%-46s %12s %12.1f\n", "pacerd CPU per round trip (us)", "-", cpu_per_trip);

    proc_stop(&p, SIGTERM, 10000);
    proc_stop(&p2, SIGTERM, 10000);
    proc_cleanup(&p);
    proc_cleanup(&p2);
    upstream_stop(&src);
    upstream_stop(&echo);
    return failed;
}
