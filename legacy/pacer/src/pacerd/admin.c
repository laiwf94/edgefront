/*
 * The admin socket: a Unix stream socket, mode 0600, served by the main
 * thread. Each connection sends one request line and gets one response.
 * At most PD_ADMIN_CLIENTS are served at once and each has two seconds to
 * send its line, so a stuck or hostile client cannot hold the main thread.
 */
#include "pacerd/admin_proto.h"
#include "pacerd/daemon.h"

#include <errno.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#define ADMIN_DEADLINE_NS 2000000000ull
#define STATS_MAX         (64 * 1024)

const char *const pd_stat_names[PD_ST_COUNT] = {
    "accepted",
    "rejected_listener_full",
    "rejected_per_ip_conns",
    "rejected_per_ip_rate",
    "rejected_table_full",
    "rejected_fd_exhausted",
    "closed_done",
    "closed_idle",
    "closed_slow",
    "closed_upstream_failed",
    "closed_reset",
    "closed_drain",
    "closed_other",
    "bytes_up",
    "bytes_down",
    "throttled",
};

static void log_admin_error(const char *msg, const char *path, int err)
{
    char eb[128];
    struct pd_log_line l;
    pd_log_begin(&l, PD_LOG_ERROR, msg);
    pd_log_str(&l, "path", path);
    pd_log_str(&l, "error", pd_strerror(err, eb, sizeof eb));
    pd_log_end(&l);
}

/*
 * A socket file left by a crashed run is removed; one that still answers
 * belongs to a live pacerd, and starting a second one on it is refused.
 */
static int clear_stale(const char *path, const struct sockaddr_un *sa)
{
    struct stat st;
    if (lstat(path, &st) != 0) {
        return 0;
    }
    if (!S_ISSOCK(st.st_mode)) {
        log_admin_error("admin_socket path exists and is not a socket", path, EEXIST);
        return -1;
    }
    int probe = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (probe >= 0 && connect(probe, (const struct sockaddr *)sa, sizeof *sa) == 0) {
        (void)close(probe);
        log_admin_error("admin_socket is in use by a running pacerd", path, EADDRINUSE);
        return -1;
    }
    if (probe >= 0) {
        (void)close(probe);
    }
    (void)unlink(path);
    return 0;
}

int pd_admin_open(struct pd_daemon *d)
{
    const char *path = d->cfg->c.admin_socket;
    if (!path[0]) {
        return 0;
    }
    struct sockaddr_un sa;
    memset(&sa, 0, sizeof sa);
    sa.sun_family = AF_UNIX;
    memcpy(sa.sun_path, path, strlen(path) + 1);
    if (clear_stale(path, &sa) != 0) {
        return -1;
    }
    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (fd < 0) {
        log_admin_error("cannot create admin socket", path, errno);
        return -1;
    }
    /* The umask makes the file 0600 from the moment it exists, so there is
     * no window in which another user could connect. */
    mode_t old = umask(0177);
    int rc = bind(fd, (const struct sockaddr *)&sa, sizeof sa);
    int err = errno;
    (void)umask(old);
    if (rc != 0 || listen(fd, PD_ADMIN_CLIENTS) != 0) {
        log_admin_error("cannot listen on admin socket", path, rc != 0 ? err : errno);
        (void)close(fd);
        return -1;
    }
    d->admin.fd = fd;
    return 0;
}

static void client_close(struct pd_admin_client *a)
{
    if (a->h.fd >= 0) {
        (void)close(a->h.fd);
        a->h.fd = -1;
    }
}

void pd_admin_close(struct pd_daemon *d)
{
    for (int i = 0; i < PD_ADMIN_CLIENTS; i++) {
        client_close(&d->admin_clients[i]);
    }
    if (d->admin.fd >= 0) {
        (void)close(d->admin.fd);
        d->admin.fd = -1;
        (void)unlink(d->cfg->c.admin_socket);
    }
}

void pd_admin_accept(struct pd_daemon *d, int epfd)
{
    for (;;) {
        int fd = accept4(d->admin.fd, NULL, NULL, SOCK_NONBLOCK | SOCK_CLOEXEC);
        if (fd < 0) {
            return;
        }
        struct pd_admin_client *a = NULL;
        for (int i = 0; i < PD_ADMIN_CLIENTS && !a; i++) {
            if (d->admin_clients[i].h.fd < 0) {
                a = &d->admin_clients[i];
            }
        }
        if (!a) {
            static const char busy[] = "error busy\n";
            (void)send(fd, busy, sizeof busy - 1, MSG_NOSIGNAL);
            (void)close(fd);
            continue;
        }
        a->h.fd = fd;
        a->len = 0;
        a->deadline = pacer_now_ns() + ADMIN_DEADLINE_NS;
        struct epoll_event ev = {.events = EPOLLIN, .data.ptr = &a->h};
        if (epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &ev) != 0) {
            client_close(a);
        }
    }
}

int pd_admin_expire(struct pd_daemon *d, uint64_t now)
{
    uint64_t next = 0;
    for (int i = 0; i < PD_ADMIN_CLIENTS; i++) {
        struct pd_admin_client *a = &d->admin_clients[i];
        if (a->h.fd < 0) {
            continue;
        }
        if (a->deadline <= now) {
            static const char slow[] = "error timed out waiting for the request\n";
            (void)send(a->h.fd, slow, sizeof slow - 1, MSG_NOSIGNAL);
            client_close(a);
            continue;
        }
        if (!next || a->deadline < next) {
            next = a->deadline;
        }
    }
    return next ? (int)((next - now + 999999) / 1000000) : -1;
}

static size_t append(char *buf, size_t len, size_t pos, const char *fmt, ...)
    __attribute__((format(printf, 4, 5)));

static size_t append(char *buf, size_t len, size_t pos, const char *fmt, ...)
{
    if (pos >= len) {
        return pos;
    }
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf + pos, len - pos, fmt, ap);
    va_end(ap);
    if (n < 0) {
        return pos;
    }
    return (size_t)n < len - pos ? pos + (size_t)n : len;
}

size_t pd_stats_render(struct pd_daemon *d, char *buf, size_t len)
{
    const struct pd_config *c = &d->cfg->c;
    size_t pos = 0;
    pos = append(buf, len, pos, "pacerd version=%s uptime_s=%llu workers=%u draining=%d\n",
                 pacer_version(),
                 (unsigned long long)((pacer_now_ns() - d->start_ns) / 1000000000ull), d->nworkers,
                 atomic_load(&d->draining));
    for (uint32_t li = 0; li < d->nlisteners; li++) {
        struct pd_listener *l = &d->listeners[li];
        pos = append(buf, len, pos, "listener=%s open=%u", c->listeners[li].name,
                     atomic_load_explicit(&l->conns, memory_order_relaxed));
        for (int st = 0; st < PD_ST_COUNT; st++) {
            uint64_t sum = 0;
            for (uint32_t w = 0; w < d->started; w++) {
                sum += atomic_load_explicit(&d->workers[w].stats[li][st], memory_order_relaxed);
            }
            pos = append(buf, len, pos, " %s=%llu", pd_stat_names[st], (unsigned long long)sum);
        }
        pacer_table_stats ts;
        pacer_table_get_stats(l->ip_conns, &ts);
        pos = append(buf, len, pos,
                     " clients_tracked=%llu client_table_capacity=%llu client_table_evictions=%llu"
                     "\n",
                     (unsigned long long)ts.entries, (unsigned long long)ts.capacity,
                     (unsigned long long)ts.evictions);
    }
    for (uint32_t w = 0; w < d->started; w++) {
        uint64_t accepted = 0;
        for (uint32_t li = 0; li < d->nlisteners; li++) {
            accepted += atomic_load_explicit(&d->workers[w].stats[li][PD_ST_ACCEPTED],
                                             memory_order_relaxed);
        }
        pos = append(buf, len, pos, "worker=%u accepted=%llu\n", w, (unsigned long long)accepted);
    }
    return pos;
}

static void respond(struct pd_admin_client *a, const char *text, size_t len)
{
    /* Responses are a few KiB and the socket buffer is empty, so one send
     * normally takes all of it; a client that does not read loses the rest. */
    (void)send(a->h.fd, text, len, MSG_NOSIGNAL);
    client_close(a);
}

static void run(struct pd_daemon *d, struct pd_admin_client *a, enum pd_admin_cmd cmd)
{
    static char out[STATS_MAX];
    char err[PD_ERR_MAX];
    size_t n = 0;
    switch (cmd) {
    case PD_ADMIN_STATS:
        n = pd_stats_render(d, out, sizeof out);
        break;
    case PD_ADMIN_PING:
        n = (size_t)snprintf(out, sizeof out, "ok pong\n");
        break;
    case PD_ADMIN_RELOAD:
        if (pd_daemon_reload_logged(d, "admin", err, sizeof err) == 0) {
            n = (size_t)snprintf(out, sizeof out, "ok reloaded\n");
        } else {
            n = (size_t)snprintf(out, sizeof out, "error %s\n", err);
        }
        break;
    case PD_ADMIN_DRAIN:
        pd_daemon_drain(d, "admin");
        n = (size_t)snprintf(out, sizeof out, "ok draining\n");
        break;
    default: {
        const char *e = pd_admin_error(cmd);
        n = strlen(e);
        memcpy(out, e, n);
        break;
    }
    }
    respond(a, out, n < sizeof out ? n : sizeof out - 1);
}

void pd_admin_event(struct pd_daemon *d, struct pd_admin_client *a)
{
    if (a->h.fd < 0) {
        return;
    }
    ssize_t r = recv(a->h.fd, a->buf + a->len, sizeof a->buf - a->len, 0);
    if (r < 0 && (errno == EAGAIN || errno == EINTR)) {
        return;
    }
    if (r <= 0) {
        client_close(a);
        return;
    }
    a->len += (size_t)r;
    enum pd_admin_cmd cmd = pd_admin_parse(a->buf, a->len);
    if (cmd != PD_ADMIN_INCOMPLETE) {
        run(d, a, cmd);
    }
}
