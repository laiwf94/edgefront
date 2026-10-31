#include "pacerd/daemon.h"
#include "pacerd/log.h"
#include "pacerd/util.h"

#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <unistd.h>

#define LISTEN_BACKLOG 1024

struct pd_cfg *pd_cfg_load(const char *path, char *err, size_t errlen)
{
    struct pd_cfg *cfg = malloc(sizeof *cfg);
    if (!cfg) {
        (void)snprintf(err, errlen, "out of memory");
        return NULL;
    }
    if (pd_config_load(path, &cfg->c, err, errlen) != 0) {
        free(cfg);
        return NULL;
    }
    atomic_init(&cfg->refs, 1);
    return cfg;
}

void pd_cfg_ref(struct pd_cfg *cfg)
{
    atomic_fetch_add_explicit(&cfg->refs, 1, memory_order_relaxed);
}

void pd_cfg_unref(struct pd_cfg *cfg)
{
    /* acq_rel: the thread that frees must see every other thread's last use. */
    if (cfg && atomic_fetch_sub_explicit(&cfg->refs, 1, memory_order_acq_rel) == 1) {
        free(cfg);
    }
}

void pd_wake(int eventfd)
{
    uint64_t one = 1;
    /* A full counter already means "wake up"; nothing else can fail here. */
    (void)write(eventfd, &one, sizeof one);
}

static void log_errno(const char *msg, const char *what, int err)
{
    char eb[128];
    struct pd_log_line l;
    pd_log_begin(&l, PD_LOG_ERROR, msg);
    if (what) {
        pd_log_str(&l, "what", what);
    }
    pd_log_str(&l, "error", pd_strerror(err, eb, sizeof eb));
    pd_log_end(&l);
}

/*
 * Open a listening socket. Each worker gets its own with SO_REUSEPORT. A
 * reuseport group would also admit a second pacerd run by the same user,
 * which would then take a share of the connections, so the address is
 * first claimed once without SO_REUSEPORT: if that fails, someone else
 * holds the port.
 */
static int open_listener(const struct pd_listener_conf *lc, int reuseport)
{
    char addr[64];
    pd_addr_format(&lc->listen, addr, sizeof addr);
    int fd = socket(lc->listen.ss.ss_family, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (fd < 0) {
        log_errno("cannot create listening socket", addr, errno);
        return -1;
    }
    int one = 1;
    /* SO_REUSEADDR lets a restart bind while old connections sit in
     * TIME_WAIT. V6ONLY keeps [::]:port from also claiming IPv4. */
    (void)setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (reuseport) {
        (void)setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &one, sizeof one);
    }
    if (lc->listen.ss.ss_family == AF_INET6) {
        (void)setsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    }
    if (bind(fd, (const struct sockaddr *)&lc->listen.ss, lc->listen.len) != 0 ||
        (reuseport && listen(fd, LISTEN_BACKLOG) != 0)) {
        log_errno("cannot listen", addr, errno);
        (void)close(fd);
        return -1;
    }
    return fd;
}

static void close_listeners(struct pd_daemon *d)
{
    for (uint32_t w = 0; w < d->nworkers; w++) {
        for (uint32_t i = 0; i < d->nlisteners; i++) {
            struct pd_listen_sock *ls = &d->workers[w].lsocks[i];
            if (ls->h.fd >= 0) {
                (void)close(ls->h.fd);
                ls->h.fd = -1;
            }
        }
    }
}

static void free_listeners(struct pd_daemon *d)
{
    for (uint32_t i = 0; i < d->nlisteners; i++) {
        pacer_table_destroy(d->listeners[i].ip_conns);
        pacer_table_destroy(d->listeners[i].ip_bytes);
        d->listeners[i].ip_conns = NULL;
        d->listeners[i].ip_bytes = NULL;
    }
}

/* A limit that is switched off still gets a limiter so a reload can switch
 * it on in place; the proxy skips limiters that are off. */
static pacer_rate rate_or_unlimited(const struct pd_rate_opt *opt)
{
    pacer_rate unlimited = {1000000000ull, 1000000000ull, 1000000000ull};
    return opt->on ? opt->rate : unlimited;
}

static int setup_limits(const struct pd_listener_conf *lc, struct pd_listener *l)
{
    pacer_table_config tc = {lc->table_size, 0, rate_or_unlimited(&lc->per_ip_conn_rate), NULL,
                             lc->table_full};
    int rc = pacer_table_create(&tc, &l->ip_conns);
    if (rc == PACER_OK) {
        tc.rate = rate_or_unlimited(&lc->per_ip_bandwidth);
        rc = pacer_table_create(&tc, &l->ip_bytes);
    }
    if (rc == PACER_OK) {
        pacer_rate r = rate_or_unlimited(&lc->bandwidth);
        rc = pacer_gcra_init(&l->bytes, &r);
    }
    if (rc != PACER_OK) {
        struct pd_log_line line;
        pd_log_begin(&line, PD_LOG_ERROR, "cannot create client table");
        pd_log_str(&line, "listener", lc->name);
        pd_log_str(&line, "error", pacer_strerror(rc));
        pd_log_end(&line);
        return -1;
    }
    atomic_init(&l->conns, 0);
    pd_log_limit_init(&l->reject_log, 10, 20);
    return 0;
}

static int setup_listeners(struct pd_daemon *d)
{
    const struct pd_config *c = &d->cfg->c;
    d->nlisteners = c->nlisteners;
    for (uint32_t i = 0; i < c->nlisteners; i++) {
        d->listeners[i].index = i;
    }
    for (uint32_t i = 0; i < c->nlisteners; i++) {
        int probe = open_listener(&c->listeners[i], 0);
        if (probe < 0) {
            return -1;
        }
        (void)close(probe);
        if (setup_limits(&c->listeners[i], &d->listeners[i]) != 0) {
            return -1;
        }
    }
    return 0;
}

static int start_workers(struct pd_daemon *d)
{
    d->nworkers = d->cfg->c.workers;
    d->workers = calloc(d->nworkers, sizeof *d->workers);
    if (!d->workers) {
        pd_log_msg(PD_LOG_ERROR, "out of memory");
        return -1;
    }
    for (uint32_t i = 0; i < d->nworkers; i++) {
        struct pd_worker *w = &d->workers[i];
        w->d = d;
        w->id = i;
        w->wake.kind = PD_H_WAKE;
        w->reserve_fd = -1;
        for (uint32_t li = 0; li < PD_MAX_LISTENERS; li++) {
            w->lsocks[li] = (struct pd_listen_sock){{PD_H_LISTENER, -1}, li};
        }
        pd_cfg_ref(d->cfg);
        w->cfg = d->cfg;
        atomic_init(&w->pending, NULL);
        /* Created here, not in the thread, so a drain request sent while a
         * worker is still starting is never lost. */
        w->epfd = epoll_create1(EPOLL_CLOEXEC);
        w->wake.fd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
        if (w->epfd < 0 || w->wake.fd < 0) {
            log_errno("cannot create worker event fds", NULL, errno);
            d->nworkers = i + 1;
            return -1;
        }
        for (uint32_t li = 0; li < d->nlisteners; li++) {
            w->lsocks[li].h.fd = open_listener(&d->cfg->c.listeners[li], 1);
            if (w->lsocks[li].h.fd < 0) {
                d->nworkers = i + 1;
                return -1;
            }
        }
    }
    for (uint32_t i = 0; i < d->nworkers; i++) {
        struct pd_worker *w = &d->workers[i];
        atomic_fetch_add(&d->workers_running, 1);
        int rc = pthread_create(&w->thread, NULL, pd_worker_main, w);
        if (rc == 0) {
            d->started = i + 1;
        }
        if (rc != 0) {
            atomic_fetch_sub(&d->workers_running, 1);
            log_errno("cannot start worker", NULL, rc);
            return -1;
        }
    }
    return 0;
}

void pd_daemon_drain(struct pd_daemon *d, const char *why)
{
    if (atomic_exchange(&d->draining, 1)) {
        return;
    }
    struct pd_log_line l;
    pd_log_begin(&l, PD_LOG_INFO, "draining");
    pd_log_str(&l, "reason", why);
    pd_log_u64(&l, "timeout_ms", d->cfg->c.drain_timeout_ns / 1000000);
    pd_log_end(&l);
    for (uint32_t i = 0; i < d->started; i++) {
        pd_wake(d->workers[i].wake.fd);
    }
}

static void join_workers(struct pd_daemon *d)
{
    for (uint32_t i = 0; i < d->started; i++) {
        (void)pthread_join(d->workers[i].thread, NULL);
    }
}

static void free_workers(struct pd_daemon *d)
{
    for (uint32_t i = 0; i < d->nworkers; i++) {
        struct pd_worker *w = &d->workers[i];
        if (w->epfd >= 0) {
            (void)close(w->epfd);
        }
        if (w->wake.fd >= 0) {
            (void)close(w->wake.fd);
        }
        /* A reload can reach a worker after it stopped for a failure, in
         * which case the config it never picked up is still pending. */
        pd_cfg_unref(atomic_exchange(&w->pending, NULL));
        pd_cfg_unref(w->cfg);
    }
    free(d->workers);
}

/* Why `next` cannot replace `cur` in place, or NULL if it can. */
static const char *reload_conflict(const struct pd_config *cur, const struct pd_config *next,
                                   char *buf, size_t len)
{
    if (next->workers != cur->workers) {
        return "workers cannot change without a restart";
    }
    if (strcmp(next->admin_socket, cur->admin_socket) != 0) {
        return "admin_socket cannot change without a restart";
    }
    if (next->nlisteners != cur->nlisteners) {
        return "listeners cannot be added or removed without a restart";
    }
    for (uint32_t i = 0; i < cur->nlisteners; i++) {
        const struct pd_listener_conf *a = &cur->listeners[i], *b = &next->listeners[i];
        const char *what = NULL;
        if (strcmp(a->name, b->name) != 0) {
            what = "name";
        } else if (a->listen.len != b->listen.len ||
                   memcmp(&a->listen.ss, &b->listen.ss, a->listen.len) != 0) {
            what = "listen";
        } else if (a->table_size != b->table_size) {
            what = "table_size";
        } else if (a->table_full != b->table_full) {
            what = "table_full";
        }
        if (what) {
            (void)snprintf(buf, len, "listener %s: %s cannot change without a restart", a->name,
                           what);
            return buf;
        }
    }
    return NULL;
}

int pd_daemon_reload(struct pd_daemon *d, char *err, size_t errlen)
{
    char why[PD_ERR_MAX];
    struct pd_cfg *next = pd_cfg_load(d->config_path, err, errlen);
    if (!next) {
        return -1;
    }
    const char *conflict = reload_conflict(&d->cfg->c, &next->c, why, sizeof why);
    if (conflict) {
        (void)snprintf(err, errlen, "%s", conflict);
        pd_cfg_unref(next);
        return -1;
    }
    /* The new rates were validated by the parser, so reconfigure cannot
     * fail here. */
    for (uint32_t i = 0; i < d->nlisteners; i++) {
        const struct pd_listener_conf *lc = &next->c.listeners[i];
        struct pd_listener *l = &d->listeners[i];
        pacer_rate conns = rate_or_unlimited(&lc->per_ip_conn_rate);
        pacer_rate ip_bytes = rate_or_unlimited(&lc->per_ip_bandwidth);
        pacer_rate bytes = rate_or_unlimited(&lc->bandwidth);
        (void)pacer_table_reconfigure(l->ip_conns, &conns);
        (void)pacer_table_reconfigure(l->ip_bytes, &ip_bytes);
        (void)pacer_gcra_reconfigure(&l->bytes, &bytes);
    }
    pd_log_set_level(next->c.log_level);
    /* Each worker gets its own reference. A worker that has not yet picked
     * up an earlier config hands it back here instead. */
    for (uint32_t i = 0; i < d->started; i++) {
        struct pd_worker *w = &d->workers[i];
        pd_cfg_ref(next);
        pd_cfg_unref(atomic_exchange(&w->pending, next));
        pd_wake(w->wake.fd);
    }
    pd_cfg_unref(d->cfg);
    d->cfg = next;
    return 0;
}

int pd_daemon_reload_logged(struct pd_daemon *d, const char *source, char *err, size_t errlen)
{
    struct pd_log_line l;
    if (atomic_load(&d->draining)) {
        (void)snprintf(err, errlen, "draining");
        pd_log_msg(PD_LOG_WARN, "reload ignored while draining");
        return -1;
    }
    if (pd_daemon_reload(d, err, errlen) != 0) {
        pd_log_begin(&l, PD_LOG_ERROR, "reload rejected, keeping the running config");
        pd_log_str(&l, "source", source);
        pd_log_str(&l, "error", err);
        pd_log_end(&l);
        return -1;
    }
    pd_log_begin(&l, PD_LOG_INFO, "reloaded");
    pd_log_str(&l, "source", source);
    pd_log_end(&l);
    return 0;
}

static int on_signal(struct pd_daemon *d, int sfd)
{
    struct signalfd_siginfo si;
    while (read(sfd, &si, sizeof si) == (ssize_t)sizeof si) {
        if (si.ssi_signo == SIGTERM || si.ssi_signo == SIGINT) {
            pd_daemon_drain(d, si.ssi_signo == SIGTERM ? "SIGTERM" : "SIGINT");
        } else if (si.ssi_signo == SIGHUP) {
            char err[PD_ERR_MAX];
            (void)pd_daemon_reload_logged(d, "SIGHUP", err, sizeof err);
        }
    }
    return 0;
}

/* Main thread loop: signals, worker notifications and the admin socket,
 * until every worker has finished. */
static int main_loop(struct pd_daemon *d)
{
    int ep = epoll_create1(EPOLL_CLOEXEC);
    if (ep < 0) {
        log_errno("epoll_create1 failed", NULL, errno);
        atomic_store(&d->failed, 1);
        pd_daemon_drain(d, "startup failure");
        return 1;
    }
    struct pd_handle *hs[] = {&d->signals, &d->main_wake_h, &d->admin};
    for (size_t i = 0; i < sizeof hs / sizeof hs[0]; i++) {
        struct epoll_event ev = {.events = EPOLLIN, .data.ptr = hs[i]};
        if (hs[i]->fd >= 0 && epoll_ctl(ep, EPOLL_CTL_ADD, hs[i]->fd, &ev) != 0) {
            log_errno("epoll_ctl failed", NULL, errno);
            atomic_store(&d->failed, 1);
            pd_daemon_drain(d, "startup failure");
        }
    }

    int listeners_closed = 0;
    while (atomic_load(&d->workers_running) > 0) {
        struct epoll_event events[16];
        int timeout = pd_admin_expire(d, pacer_now_ns());
        int n = epoll_wait(ep, events, 16, timeout);
        for (int i = 0; i < n; i++) {
            struct pd_handle *h = events[i].data.ptr;
            if (h->kind == PD_H_SIGNAL) {
                on_signal(d, h->fd);
            } else if (h->kind == PD_H_ADMIN_LISTEN) {
                pd_admin_accept(d, ep);
            } else if (h->kind == PD_H_ADMIN_CLIENT) {
                pd_admin_event(d, (struct pd_admin_client *)h);
            } else {
                uint64_t v = 0;
                (void)read(h->fd, &v, sizeof v);
            }
        }
        if (atomic_load(&d->failed)) {
            pd_daemon_drain(d, "worker failure");
        }
        /* Once no worker can call accept on them, the listening sockets
         * are closed so new connections are refused at once. */
        if (!listeners_closed && atomic_load(&d->workers_off_listeners) == d->started &&
            atomic_load(&d->draining)) {
            close_listeners(d);
            listeners_closed = 1;
        }
    }
    (void)close(ep);
    return atomic_load(&d->failed) ? 1 : 0;
}

int pd_daemon_run(const char *config_path)
{
    char err[PD_ERR_MAX];
    static struct pd_daemon d;
    memset(&d, 0, sizeof d);
    d.config_path = config_path;
    d.main_wake = -1;
    d.start_ns = pacer_now_ns();
    d.admin.kind = PD_H_ADMIN_LISTEN;
    d.admin.fd = -1;
    for (int i = 0; i < PD_ADMIN_CLIENTS; i++) {
        d.admin_clients[i].h.kind = PD_H_ADMIN_CLIENT;
        d.admin_clients[i].h.fd = -1;
    }
    d.cfg = pd_cfg_load(config_path, err, sizeof err);
    if (!d.cfg) {
        struct pd_log_line l;
        pd_log_begin(&l, PD_LOG_ERROR, "bad config");
        pd_log_str(&l, "error", err);
        pd_log_end(&l);
        return 2;
    }
    pd_log_set_level(d.cfg->c.log_level);

    /* Block the signals before any thread exists so every thread inherits
     * the mask and only the signalfd sees them. */
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGTERM);
    sigaddset(&mask, SIGINT);
    sigaddset(&mask, SIGHUP);
    (void)pthread_sigmask(SIG_BLOCK, &mask, NULL);
    (void)signal(SIGPIPE, SIG_IGN);

    int rc = 1;
    int sfd = signalfd(-1, &mask, SFD_NONBLOCK | SFD_CLOEXEC);
    d.main_wake = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    d.signals = (struct pd_handle){PD_H_SIGNAL, sfd};
    d.main_wake_h = (struct pd_handle){PD_H_WAKE, d.main_wake};
    if (sfd < 0 || d.main_wake < 0) {
        log_errno("cannot create signal or event fd", NULL, errno);
    } else if (setup_listeners(&d) == 0 && pd_admin_open(&d) == 0 && start_workers(&d) == 0) {
        struct pd_log_line l;
        pd_log_begin(&l, PD_LOG_INFO, "ready");
        pd_log_str(&l, "version", pacer_version());
        pd_log_u64(&l, "workers", d.nworkers);
        pd_log_u64(&l, "listeners", d.nlisteners);
        pd_log_end(&l);
        rc = main_loop(&d);
    } else {
        atomic_store(&d.failed, 1);
        pd_daemon_drain(&d, "startup failure");
    }
    join_workers(&d);
    pd_admin_close(&d);
    if (d.workers) {
        close_listeners(&d);
    }
    free_listeners(&d);
    if (rc == 0) {
        pd_log_msg(PD_LOG_INFO, "drained");
    }
    if (d.workers) {
        free_workers(&d);
    }
    pd_cfg_unref(d.cfg);
    if (sfd >= 0) {
        (void)close(sfd);
    }
    if (d.main_wake >= 0) {
        (void)close(d.main_wake);
    }
    return rc;
}
