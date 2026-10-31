/*
 * Connection handling: accept, upstream connect, and moving bytes in both
 * directions through one fixed buffer each.
 *
 * Interest is level-triggered and recomputed after every event. A fd with
 * nothing to wait for is removed from epoll entirely, so a peer that has
 * hung up cannot make the loop spin on EPOLLHUP while the other direction
 * drains.
 *
 * Inner functions never free the connection. They return NULL to carry on
 * or a short reason, and the event entry points close with that reason.
 */
#include "pacerd/daemon.h"
#include "pacerd/log.h"
#include "pacerd/util.h"

#include <errno.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <unistd.h>

struct pd_conn *pd_conn_of_handle(struct pd_handle *h)
{
    size_t off = h->kind == PD_H_CLIENT ? offsetof(struct pd_conn, client)
                                        : offsetof(struct pd_conn, upstream);
    return (struct pd_conn *)((char *)h - off);
}

struct pd_conn *pd_conn_of_timer(struct pd_timer *t)
{
    return (struct pd_conn *)((char *)t - offsetof(struct pd_conn, timer));
}

static void format_peer(const struct sockaddr_storage *ss, char *buf, size_t len)
{
    struct pd_addr a;
    memset(&a, 0, sizeof a);
    memcpy(&a.ss, ss, sizeof *ss);
    pd_addr_format(&a, buf, len);
}

static const struct pd_listener_conf *lconf(const struct pd_conn *c)
{
    return &c->w->cfg->c.listeners[c->li];
}

static uint32_t buf_space(const struct pd_buf *b)
{
    return b->cap - b->end + b->start;
}

static uint32_t buf_used(const struct pd_buf *b)
{
    return b->end - b->start;
}

/* Source and destination fds of a direction. */
static int src_fd(const struct pd_conn *c, int dir)
{
    return dir == PD_UP ? c->client.fd : c->upstream.fd;
}

static int dst_fd(const struct pd_conn *c, int dir)
{
    return dir == PD_UP ? c->upstream.fd : c->client.fd;
}

static const char *set_interest(struct pd_conn *c, int side, uint32_t mask)
{
    struct pd_handle *h = side == 0 ? &c->client : &c->upstream;
    uint32_t cur = c->mask[side];
    if (mask == cur) {
        return NULL;
    }
    struct epoll_event ev = {.events = mask, .data.ptr = h};
    int op = EPOLL_CTL_MOD;
    if (cur == 0) {
        op = EPOLL_CTL_ADD;
    } else if (mask == 0) {
        op = EPOLL_CTL_DEL;
    }
    if (epoll_ctl(c->w->epfd, op, h->fd, &ev) != 0) {
        /* Only kernel memory exhaustion gets here; drop the connection
         * rather than leave it deaf. */
        return "epoll_error";
    }
    c->mask[side] = mask;
    return NULL;
}

static int can_read(const struct pd_conn *c, int dir)
{
    return !c->eof[dir] && buf_space(&c->buf[dir]) > 0 && c->throttled_to[dir] == 0;
}

/* Recompute what each fd waits for. */
static const char *update(struct pd_conn *c)
{
    uint32_t cm = 0, um = 0;
    if (can_read(c, PD_UP)) {
        cm |= EPOLLIN;
    }
    if (buf_used(&c->buf[PD_DOWN]) > 0) {
        cm |= EPOLLOUT;
    }
    if (c->connecting) {
        um = EPOLLOUT;
    } else {
        if (can_read(c, PD_DOWN)) {
            um |= EPOLLIN;
        }
        if (buf_used(&c->buf[PD_UP]) > 0) {
            um |= EPOLLOUT;
        }
    }
    const char *why = set_interest(c, 0, cm);
    return why ? why : set_interest(c, 1, um);
}

static const char *reset_reason(int dir_of_fd, int err)
{
    if (err == ECONNRESET || err == EPIPE) {
        return dir_of_fd == PD_UP ? "client_reset" : "upstream_reset";
    }
    return dir_of_fd == PD_UP ? "client_error" : "upstream_error";
}

/* Write what the buffer holds for `dir` to its destination. */
static const char *flush(struct pd_conn *c, int dir)
{
    struct pd_buf *b = &c->buf[dir];
    if (dir == PD_UP && c->connecting) {
        return NULL;
    }
    while (b->start < b->end) {
        ssize_t n = send(dst_fd(c, dir), b->data + b->start, b->end - b->start, MSG_NOSIGNAL);
        if (n < 0) {
            if (errno == EAGAIN || errno == EINTR) {
                return NULL;
            }
            return reset_reason(dir == PD_UP ? PD_DOWN : PD_UP, errno);
        }
        b->start += (uint32_t)n;
    }
    b->start = b->end = 0;
    if (c->eof[dir] && !c->shut[dir]) {
        /* Pass the FIN on once everything before it has been written. */
        (void)shutdown(dst_fd(c, dir), SHUT_WR);
        c->shut[dir] = 1;
    }
    return NULL;
}

/* When a table cannot track the client (full of active entries under the
 * deny policy), its bytes wait and are retried after this long. */
#define UNTRACKED_RETRY_NS 50000000ull

/* The smallest read worth waking up for while shaped. Without a minimum,
 * a connection whose socket is always readable would grab each token the
 * moment it appears and keep slower-waking connections out entirely. */
#define READ_QUANTUM 4096u

static uint64_t quantum(uint64_t want)
{
    return want < READ_QUANTUM ? want : READ_QUANTUM;
}

/*
 * Reserve up to `want` bytes from the client's and the listener's byte
 * limiters. Returns the grant; when it is 0, *retry is how long to wait.
 * Unused tokens go back through give_back.
 */
static uint64_t reserve(struct pd_conn *c, uint64_t want, uint64_t now, uint64_t *retry)
{
    struct pd_listener *l = &c->w->d->listeners[c->li];
    const struct pd_listener_conf *lc = lconf(c);
    uint64_t grant = want;
    *retry = 0;
    if (lc->per_ip_bandwidth.on) {
        pacer_decision d;
        if (pacer_table_take_range(l->ip_bytes, c->key, c->keylen, now, quantum(grant), grant,
                                   &d) != PACER_OK) {
            *retry = UNTRACKED_RETRY_NS;
            return 0;
        }
        grant = d.granted;
        *retry = d.retry_after_ns;
    }
    if (grant > 0 && lc->bandwidth.on) {
        pacer_decision d;
        uint64_t g = pacer_gcra_take_range(&l->bytes, now, quantum(grant), grant, &d);
        if (g < grant && lc->per_ip_bandwidth.on) {
            pacer_table_refund(l->ip_bytes, c->key, c->keylen, now, grant - g);
        }
        grant = g;
        *retry = d.retry_after_ns;
    }
    return grant;
}

static void give_back(struct pd_conn *c, uint64_t tokens, uint64_t now)
{
    struct pd_listener *l = &c->w->d->listeners[c->li];
    const struct pd_listener_conf *lc = lconf(c);
    if (tokens == 0) {
        return;
    }
    if (lc->per_ip_bandwidth.on) {
        pacer_table_refund(l->ip_bytes, c->key, c->keylen, now, tokens);
    }
    if (lc->bandwidth.on) {
        pacer_gcra_refund(&l->bytes, now, tokens);
    }
}

/* Read from the source of `dir` into its buffer, then try to write it on.
 * Reads are sized by the byte limiters; when they are empty the source is
 * left unread until the retry time, and TCP flow control pushes back. */
static const char *pump(struct pd_conn *c, int dir, uint64_t now)
{
    struct pd_buf *b = &c->buf[dir];
    if (b->end == b->cap && b->start > 0) {
        memmove(b->data, b->data + b->start, b->end - b->start);
        b->end -= b->start;
        b->start = 0;
    }
    uint32_t space = b->cap - b->end;
    if (space == 0 || c->eof[dir] || c->throttled_to[dir]) {
        return NULL;
    }
    uint64_t retry = 0;
    uint64_t grant = reserve(c, space, now, &retry);
    if (grant == 0) {
        c->throttled_to[dir] = now + (retry ? retry : 1);
        c->last_throttle = c->throttled_to[dir];
        pd_stat_add(c->w, c->li, PD_ST_THROTTLED, 1);
        return NULL;
    }
    ssize_t n = recv(src_fd(c, dir), b->data + b->end, (size_t)grant, 0);
    give_back(c, n > 0 ? grant - (uint64_t)n : grant, now);
    if (n < 0) {
        if (errno == EAGAIN || errno == EINTR) {
            return NULL;
        }
        return reset_reason(dir, errno);
    }
    if (n == 0) {
        c->eof[dir] = 1;
    } else {
        b->end += (uint32_t)n;
        c->bytes[dir] += (uint64_t)n;
        pd_stat_add(c->w, c->li, dir == PD_UP ? PD_ST_BYTES_UP : PD_ST_BYTES_DOWN, (uint64_t)n);
        c->last_io = now;
        if (c->next_rate_check) {
            pacer_window_add(&c->moved, now, (uint64_t)n);
        }
    }
    return flush(c, dir);
}

static int finished(const struct pd_conn *c)
{
    return c->shut[PD_UP] && c->shut[PD_DOWN];
}

static uint64_t idle_deadline(const struct pd_conn *c)
{
    return c->connecting ? 0 : c->last_io + lconf(c)->idle_timeout_ns;
}

static uint64_t earliest(uint64_t a, uint64_t b)
{
    if (a == 0) {
        return b;
    }
    return b != 0 && b < a ? b : a;
}

/* Queue the connection's earliest deadline. A connection that cannot get
 * a timer slot is closed: it could otherwise wait forever. */
static const char *arm_timer(struct pd_conn *c)
{
    uint64_t next = c->connecting ? c->connect_deadline : 0;
    next = earliest(next, c->throttled_to[PD_UP]);
    next = earliest(next, c->throttled_to[PD_DOWN]);
    next = earliest(next, idle_deadline(c));
    next = earliest(next, c->next_rate_check);
    if (next == 0) {
        pd_heap_remove(&c->w->timers, &c->timer);
        return NULL;
    }
    if (c->timer.slot && c->timer.deadline == next) {
        return NULL;
    }
    return pd_heap_set(&c->w->timers, &c->timer, next) != 0 ? "out_of_memory" : NULL;
}

static const char *finish_connect(struct pd_conn *c, uint64_t now)
{
    int err = 0;
    socklen_t len = sizeof err;
    if (getsockopt(c->upstream.fd, SOL_SOCKET, SO_ERROR, &err, &len) != 0) {
        err = errno;
    }
    if (err == EINPROGRESS) {
        return NULL;
    }
    if (err != 0) {
        return err == ECONNREFUSED ? "upstream_refused" : "upstream_unreachable";
    }
    c->connecting = 0;
    c->last_io = now;
    return flush(c, PD_UP);
}

static const char *handle_event(struct pd_conn *c, struct pd_handle *h, uint32_t events,
                                uint64_t now)
{
    const char *why = NULL;
    if (h->kind == PD_H_UPSTREAM && c->connecting) {
        return finish_connect(c, now);
    }
    int in_dir = h->kind == PD_H_CLIENT ? PD_UP : PD_DOWN;
    int out_dir = in_dir == PD_UP ? PD_DOWN : PD_UP;
    /* Errors and hang-ups surface through the read or write that follows. */
    if (events & (EPOLLIN | EPOLLERR | EPOLLHUP)) {
        why = pump(c, in_dir, now);
    }
    if (!why && (events & (EPOLLOUT | EPOLLERR | EPOLLHUP))) {
        why = flush(c, out_dir);
    }
    return why;
}

static void after(struct pd_conn *c, const char *why)
{
    if (!why && finished(c)) {
        why = "done";
    }
    if (!why) {
        why = update(c);
    }
    if (!why) {
        why = arm_timer(c);
    }
    if (why) {
        pd_conn_close(c, why);
    }
}

void pd_conn_event(struct pd_conn *c, struct pd_handle *h, uint32_t events, uint64_t now)
{
    if (c->closed) {
        return;
    }
    after(c, handle_event(c, h, events, now));
}

/*
 * Slow-client check. A slowloris client sends just enough to dodge the idle
 * timeout and holds its slot for ever. Once a full window has passed, a
 * connection that moved fewer than min_rate bytes in the last window, in
 * either direction, is cut. A connection that shaping held back during the
 * window is exempt: the slowness was ours.
 */
static const char *check_rate(struct pd_conn *c, uint64_t now)
{
    const struct pd_listener_conf *lc = lconf(c);
    if (!c->next_rate_check || now < c->next_rate_check) {
        return NULL;
    }
    if (lc->min_rate_bytes == 0) {
        /* Switched off by a reload. */
        c->next_rate_check = 0;
        return NULL;
    }
    uint64_t window = lc->min_rate_window_ns;
    c->next_rate_check = now + window / 10;
    if (c->last_throttle && c->last_throttle + window > now) {
        return NULL;
    }
    return pacer_window_count(&c->moved, now) < lc->min_rate_bytes ? "slow_client" : NULL;
}

void pd_conn_timer(struct pd_conn *c, uint64_t now)
{
    if (c->connecting && c->connect_deadline <= now) {
        pd_conn_close(c, "connect_timeout");
        return;
    }
    if (!c->connecting && idle_deadline(c) <= now) {
        pd_conn_close(c, "idle_timeout");
        return;
    }
    const char *why = check_rate(c, now);
    if (why) {
        pd_conn_close(c, why);
        return;
    }
    for (int dir = 0; dir < 2; dir++) {
        if (c->throttled_to[dir] && c->throttled_to[dir] <= now) {
            /* Interest is level-triggered: re-enabling EPOLLIN in after()
             * brings the next read if data is waiting. */
            c->throttled_to[dir] = 0;
        }
    }
    after(c, NULL);
}

void pd_conn_close(struct pd_conn *c, const char *reason)
{
    struct pd_worker *w = c->w;
    struct pd_log_line l;
    pd_log_begin(&l, PD_LOG_INFO, "conn closed");
    pd_log_u64(&l, "conn", c->id);
    pd_log_str(&l, "listener", lconf(c)->name);
    pd_log_str(&l, "peer", c->peer);
    pd_log_str(&l, "reason", reason);
    pd_log_u64(&l, "bytes_up", c->bytes[PD_UP]);
    pd_log_u64(&l, "bytes_down", c->bytes[PD_DOWN]);
    pd_log_u64(&l, "ms", (pacer_now_ns() - c->start_ns) / 1000000);
    pd_log_end(&l);

    pd_count(w, c->li, reason);
    pd_heap_remove(&w->timers, &c->timer);
    struct pd_listener *pl = &w->d->listeners[c->li];
    if (c->held && pacer_table_release(pl->ip_conns, c->key, c->keylen) != PACER_OK) {
        /* Holds are never evicted, so this would be a bookkeeping bug. */
        pd_log_msg(PD_LOG_ERROR, "client table lost a hold");
    }
    atomic_fetch_sub_explicit(&pl->conns, 1, memory_order_relaxed);
    /* Closing a fd removes it from epoll. */
    (void)close(c->client.fd);
    if (c->upstream.fd >= 0) {
        (void)close(c->upstream.fd);
    }
    if (c->prev) {
        c->prev->next = c->next;
    } else {
        w->conns = c->next;
    }
    if (c->next) {
        c->next->prev = c->prev;
    }
    w->nconns--;
    c->closed = 1;
    c->next = w->dead;
    w->dead = c;
}

void pd_conn_reap(struct pd_worker *w)
{
    while (w->dead) {
        struct pd_conn *c = w->dead;
        w->dead = c->next;
        free(c->buf[PD_UP].data);
        free(c);
    }
}

static void nodelay(int fd)
{
    int one = 1;
    /* Small writes are forwarded as they come; Nagle would add latency on
     * top of the peer's own. Failure only costs latency. */
    (void)setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
}

/* Start the upstream connect. Returns NULL or a close reason. */
static const char *connect_upstream(struct pd_conn *c, uint64_t now)
{
    const struct pd_listener_conf *lc = lconf(c);
    int fd = socket(lc->upstream.ss.ss_family, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (fd < 0) {
        return "upstream_socket";
    }
    c->upstream.fd = fd;
    nodelay(fd);
    c->connecting = 1;
    c->connect_deadline = now + lc->connect_timeout_ns;
    if (connect(fd, (const struct sockaddr *)&lc->upstream.ss, lc->upstream.len) != 0 &&
        errno != EINPROGRESS) {
        return errno == ECONNREFUSED ? "upstream_refused" : "upstream_unreachable";
    }
    return NULL;
}

static enum pd_stat stat_for(const char *reason)
{
    static const struct {
        const char *reason;
        enum pd_stat st;
    } map[] = {
        {"done", PD_ST_CLOSED_DONE},
        {"idle_timeout", PD_ST_CLOSED_IDLE},
        {"slow_client", PD_ST_CLOSED_SLOW},
        {"connect_timeout", PD_ST_CLOSED_UPSTREAM_FAILED},
        {"upstream_refused", PD_ST_CLOSED_UPSTREAM_FAILED},
        {"upstream_unreachable", PD_ST_CLOSED_UPSTREAM_FAILED},
        {"upstream_socket", PD_ST_CLOSED_UPSTREAM_FAILED},
        {"client_reset", PD_ST_CLOSED_RESET},
        {"client_error", PD_ST_CLOSED_RESET},
        {"upstream_reset", PD_ST_CLOSED_RESET},
        {"upstream_error", PD_ST_CLOSED_RESET},
        {"drain_timeout", PD_ST_CLOSED_DRAIN},
        {"shutdown", PD_ST_CLOSED_DRAIN},
        {"listener_full", PD_ST_REJ_LISTENER_FULL},
        {"per_ip_conns", PD_ST_REJ_PER_IP_CONNS},
        {"per_ip_rate", PD_ST_REJ_PER_IP_RATE},
        {"table_full", PD_ST_REJ_TABLE_FULL},
        {"fd_exhausted", PD_ST_REJ_FD_EXHAUSTED},
        {"fd_exhausted_paused", PD_ST_REJ_FD_EXHAUSTED},
    };
    for (size_t i = 0; i < sizeof map / sizeof map[0]; i++) {
        if (strcmp(map[i].reason, reason) == 0) {
            return map[i].st;
        }
    }
    return PD_ST_CLOSED_OTHER;
}

void pd_count(struct pd_worker *w, uint32_t li, const char *reason)
{
    pd_stat_add(w, li, stat_for(reason), 1);
}

void pd_log_reject(struct pd_listener *l, const char *listener, const char *peer,
                   const char *reason, uint64_t now)
{
    uint64_t dropped = 0;
    if (!pd_log_allow(&l->reject_log, now, &dropped)) {
        return;
    }
    struct pd_log_line line;
    pd_log_begin(&line, PD_LOG_INFO, "conn rejected");
    pd_log_str(&line, "listener", listener);
    pd_log_str(&line, "peer", peer);
    pd_log_str(&line, "reason", reason);
    if (dropped) {
        pd_log_u64(&line, "similar_dropped", dropped);
    }
    pd_log_end(&line);
}

/*
 * Admission: the listener's connection cap, then the client's concurrency
 * cap and connection rate in one table call. Returns NULL when admitted,
 * with the listener count and any hold taken, or the reason for refusal
 * with nothing taken.
 */
static const char *admit(struct pd_listener *l, const struct pd_listener_conf *lc,
                         const uint8_t *key, size_t keylen, uint64_t now, int *held)
{
    uint32_t n = atomic_fetch_add_explicit(&l->conns, 1, memory_order_relaxed);
    if (n >= lc->max_conns) {
        atomic_fetch_sub_explicit(&l->conns, 1, memory_order_relaxed);
        return "listener_full";
    }
    if (lc->per_ip_max_conns == 0 && !lc->per_ip_conn_rate.on) {
        return NULL;
    }
    uint32_t cap = lc->per_ip_max_conns ? lc->per_ip_max_conns : UINT32_MAX;
    int rc = pacer_table_hold(l->ip_conns, key, keylen, now, cap, NULL);
    if (rc == 1) {
        *held = 1;
        return NULL;
    }
    atomic_fetch_sub_explicit(&l->conns, 1, memory_order_relaxed);
    switch (rc) {
    case 0:
        return "per_ip_rate";
    case PACER_ELIMIT:
        return "per_ip_conns";
    default:
        /* PACER_EFULL. Accepted TCP peers are always IPv4 or IPv6, so the
         * key is never invalid. */
        return "table_full";
    }
}

void pd_conn_accept(struct pd_worker *w, struct pd_listener *l, int fd,
                    const struct sockaddr_storage *peer)
{
    uint32_t li = l->index;
    const struct pd_listener_conf *lc = &w->cfg->c.listeners[li];
    uint64_t now = pacer_now_ns();
    uint8_t key[PD_CLIENT_KEY_MAX];
    size_t keylen = pd_client_key(peer, lc->ipv6_prefix, key);
    int held = 0;
    const char *refused = admit(l, lc, key, keylen, now, &held);
    if (refused) {
        char peer_s[64];
        format_peer(peer, peer_s, sizeof peer_s);
        (void)close(fd);
        pd_count(w, li, refused);
        pd_log_reject(l, lc->name, peer_s, refused, now);
        return;
    }
    struct pd_conn *c = calloc(1, sizeof *c);
    char *mem = malloc((size_t)lc->buffer_size * 2);
    if (!c || !mem) {
        free(c);
        free(mem);
        (void)close(fd);
        if (held) {
            (void)pacer_table_release(l->ip_conns, key, keylen);
        }
        atomic_fetch_sub_explicit(&l->conns, 1, memory_order_relaxed);
        pd_log_msg(PD_LOG_WARN, "out of memory for a new connection");
        return;
    }
    pd_stat_add(w, li, PD_ST_ACCEPTED, 1);
    memcpy(c->key, key, sizeof c->key);
    c->keylen = (uint8_t)keylen;
    c->held = held;
    c->w = w;
    c->li = li;
    c->id = pd_next_conn_id();
    c->start_ns = now;
    c->last_io = now;
    if (lc->min_rate_bytes &&
        pacer_window_init(&c->moved, lc->min_rate_window_ns, 10, now) == PACER_OK) {
        c->next_rate_check = now + lc->min_rate_window_ns;
    }
    c->client.kind = PD_H_CLIENT;
    c->client.fd = fd;
    c->upstream.kind = PD_H_UPSTREAM;
    c->upstream.fd = -1;
    for (int d = 0; d < 2; d++) {
        c->buf[d].data = mem + (size_t)d * lc->buffer_size;
        c->buf[d].cap = lc->buffer_size;
    }
    format_peer(peer, c->peer, sizeof c->peer);
    nodelay(fd);
    c->next = w->conns;
    if (w->conns) {
        w->conns->prev = c;
    }
    w->conns = c;
    w->nconns++;

    struct pd_log_line line;
    pd_log_begin(&line, PD_LOG_DEBUG, "conn open");
    pd_log_u64(&line, "conn", c->id);
    pd_log_str(&line, "listener", lc->name);
    pd_log_str(&line, "peer", c->peer);
    pd_log_end(&line);

    const char *why = connect_upstream(c, now);
    after(c, why);
}
