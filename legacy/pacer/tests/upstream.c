#include "upstream.h"
#include "harness.h"

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

uint64_t fnv(uint64_t h, const void *buf, size_t len)
{
    const uint8_t *p = buf;
    for (size_t i = 0; i < len; i++) {
        h ^= p[i];
        h *= 0x100000001b3ull;
    }
    return h;
}

void pattern(uint8_t *buf, size_t len, uint64_t off)
{
    for (size_t i = 0; i < len; i++) {
        uint64_t x = off + i;
        buf[i] = (uint8_t)((x * 2654435761u) >> 13 ^ x);
    }
}

long send_all(int fd, const void *buf, size_t len)
{
    size_t off = 0;
    while (off < len) {
        ssize_t n = send(fd, (const char *)buf + off, len - off, MSG_NOSIGNAL);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n <= 0) {
            return -1;
        }
        off += (size_t)n;
    }
    return (long)off;
}

static int wait_readable(int fd, int timeout_ms)
{
    struct pollfd p = {fd, POLLIN, 0};
    return poll(&p, 1, timeout_ms) > 0;
}

long recv_all(int fd, void *buf, size_t len, int timeout_ms)
{
    size_t off = 0;
    uint64_t deadline = now_ms() + (uint64_t)timeout_ms;
    while (off < len) {
        uint64_t now = now_ms();
        if (now >= deadline || !wait_readable(fd, (int)(deadline - now))) {
            return -1;
        }
        ssize_t n = recv(fd, (char *)buf + off, len - off, 0);
        if (n <= 0) {
            return off > 0 ? (long)off : -1;
        }
        off += (size_t)n;
    }
    return (long)off;
}

long recv_to_eof(int fd, uint64_t *hash, int timeout_ms)
{
    static _Thread_local uint8_t buf[65536];
    uint64_t deadline = now_ms() + (uint64_t)timeout_ms;
    long total = 0;
    uint64_t h = FNV_START;
    for (;;) {
        uint64_t now = now_ms();
        if (now >= deadline || !wait_readable(fd, (int)(deadline - now))) {
            return -1;
        }
        ssize_t n = recv(fd, buf, sizeof buf, 0);
        if (n == 0) {
            break;
        }
        if (n < 0) {
            return -1;
        }
        if (hash) {
            h = fnv(h, buf, (size_t)n);
        }
        total += n;
    }
    if (hash) {
        *hash = h;
    }
    return total;
}

struct session {
    struct upstream *u;
    int fd;
};

static void serve(struct upstream *u, int fd)
{
    static _Thread_local uint8_t buf[65536];
    switch (u->mode) {
    case UP_ECHO:
        for (;;) {
            ssize_t n = recv(fd, buf, sizeof buf, 0);
            if (n <= 0 || send_all(fd, buf, (size_t)n) < 0) {
                break;
            }
        }
        break;
    case UP_HASH: {
        uint64_t h = FNV_START, count = 0;
        ssize_t n = 0;
        while ((n = recv(fd, buf, sizeof buf, 0)) > 0) {
            h = fnv(h, buf, (size_t)n);
            count += (uint64_t)n;
        }
        char reply[64];
        int len = snprintf(reply, sizeof reply, "%016llx %llu\n", (unsigned long long)h,
                           (unsigned long long)count);
        send_all(fd, reply, (size_t)len);
        break;
    }
    case UP_SOURCE: {
        for (uint64_t off = 0; off < u->param;) {
            size_t n = u->param - off < sizeof buf ? (size_t)(u->param - off) : sizeof buf;
            pattern(buf, n, off);
            if (send_all(fd, buf, n) < 0) {
                break;
            }
            off += n;
        }
        break;
    }
    case UP_ZEROS: {
        static const uint8_t zeros[65536];
        for (uint64_t off = 0; off < u->param;) {
            size_t n = u->param - off < sizeof zeros ? (size_t)(u->param - off) : sizeof zeros;
            if (send_all(fd, zeros, n) < 0) {
                break;
            }
            off += n;
        }
        break;
    }
    case UP_STALL:
        while (!atomic_load(&u->stop)) {
            usleep(10 * 1000);
        }
        break;
    case UP_RESET: {
        uint64_t got = 0;
        while (got < u->param) {
            ssize_t n = recv(fd, buf, sizeof buf, 0);
            if (n <= 0) {
                break;
            }
            got += (uint64_t)n;
        }
        struct linger lg = {1, 0};
        setsockopt(fd, SOL_SOCKET, SO_LINGER, &lg, sizeof lg);
        break;
    }
    }
    close(fd);
}

static void *session_main(void *arg)
{
    struct session *s = arg;
    serve(s->u, s->fd);
    atomic_fetch_sub(&s->u->active, 1);
    free(s);
    return NULL;
}

static void *accept_main(void *arg)
{
    struct upstream *u = arg;
    while (!atomic_load(&u->stop)) {
        if (!wait_readable(u->fd, 50)) {
            continue;
        }
        int fd = accept(u->fd, NULL, NULL);
        if (fd < 0) {
            continue;
        }
        atomic_fetch_add(&u->accepted, 1);
        struct session *s = malloc(sizeof *s);
        s->u = u;
        s->fd = fd;
        atomic_fetch_add(&u->active, 1);
        pthread_t th;
        pthread_attr_t attr;
        pthread_attr_init(&attr);
        pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
        if (pthread_create(&th, &attr, session_main, s) != 0) {
            atomic_fetch_sub(&u->active, 1);
            close(fd);
            free(s);
        }
        pthread_attr_destroy(&attr);
    }
    return NULL;
}

int upstream_start(struct upstream *u, enum upstream_mode mode, uint64_t param)
{
    memset(u, 0, sizeof *u);
    u->mode = mode;
    u->param = param;
    u->fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t len = sizeof a;
    int one = 1;
    setsockopt(u->fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (bind(u->fd, (struct sockaddr *)&a, sizeof a) != 0 || listen(u->fd, 512) != 0 ||
        getsockname(u->fd, (struct sockaddr *)&a, &len) != 0) {
        close(u->fd);
        return -1;
    }
    u->port = ntohs(a.sin_port);
    return pthread_create(&u->thread, NULL, accept_main, u) == 0 ? 0 : -1;
}

void upstream_stop(struct upstream *u)
{
    atomic_store(&u->stop, 1);
    pthread_join(u->thread, NULL);
    close(u->fd);
    uint64_t deadline = now_ms() + 5000;
    while (atomic_load(&u->active) > 0 && now_ms() < deadline) {
        usleep(1000);
    }
}
