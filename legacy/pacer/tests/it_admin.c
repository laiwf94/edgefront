/* The admin socket and pacerctl against the real binary. */
#include "harness.h"
#include "test.h"
#include "upstream.h"

#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

static struct proc p;
static struct upstream u;
static uint16_t port;
static char sock[64];

static void config(char *buf, size_t len, const char *extra)
{
    snprintf(buf, len,
             "workers 2\n"
             "admin_socket %s\n"
             "listener adm {\n"
             "  listen 127.0.0.1:%u\n"
             "  upstream 127.0.0.1:%u\n"
             "%s"
             "}\n",
             sock, port, u.port, extra);
}

static void start(const char *extra)
{
    char conf[2048];
    port = free_port();
    snprintf(sock, sizeof sock, "/tmp/pacerd-admin-%d-%u.sock", (int)getpid(), port);
    CHECK_EQ_INT(upstream_start(&u, UP_ECHO, 0), 0);
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

static int ctl(char *out, size_t len, const char *cmd)
{
    const char *args[] = {"-s", sock, cmd, NULL};
    return run_ctl(out, len, args);
}

/* Raw request on the admin socket; returns the response. */
static void raw(const char *req, size_t len, char *out, size_t outlen)
{
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un sa = {0};
    sa.sun_family = AF_UNIX;
    snprintf(sa.sun_path, sizeof sa.sun_path, "%s", sock);
    CHECK_EQ_INT(connect(fd, (struct sockaddr *)&sa, sizeof sa), 0);
    if (len) {
        CHECK_EQ_INT(send_all(fd, req, len), (long long)len);
    }
    long n = recv_all(fd, out, outlen - 1, 5000);
    out[n > 0 ? n : 0] = '\0';
    close(fd);
}

static int works(int fd)
{
    char buf[4];
    return fd >= 0 && send_all(fd, "ping", 4) == 4 && recv_all(fd, buf, 4, 3000) == 4;
}

TEST(admin_stats_count_traffic_and_rejections)
{
    start("  per_ip_max_conns 2\n");
    int a = tcp_connect(port), b = tcp_connect(port);
    CHECK(works(a));
    CHECK(works(b));
    int c = tcp_connect(port);
    CHECK(!works(c));
    close(c);
    char out[8192];
    CHECK_EQ_INT(ctl(out, sizeof out, "stats"), 0);
    CHECK(strstr(out, "pacerd version=0.1.0 ") != NULL);
    CHECK(strstr(out, "workers=2 draining=0\n") != NULL);
    CHECK(strstr(out, "listener=adm open=2 accepted=2 ") != NULL);
    CHECK(strstr(out, " rejected_per_ip_conns=1 ") != NULL);
    CHECK(strstr(out, " bytes_up=8 bytes_down=8 ") != NULL);
    CHECK(strstr(out, " clients_tracked=1 client_table_capacity=16384 ") != NULL);
    close(a);
    close(b);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "reason=done", &from, 3000));
    CHECK(proc_wait_log(&p, "reason=done", &from, 3000));
    CHECK_EQ_INT(ctl(out, sizeof out, "stats"), 0);
    CHECK(strstr(out, "open=0 accepted=2 ") != NULL);
    CHECK(strstr(out, " closed_done=2 ") != NULL);
    finish();
}

TEST(admin_ping_reload_and_errors)
{
    start("");
    char out[4096];
    CHECK_EQ_INT(ctl(out, sizeof out, "ping"), 0);
    CHECK(strcmp(out, "ok pong\n") == 0);
    CHECK_EQ_INT(ctl(out, sizeof out, "reload"), 0);
    CHECK(strcmp(out, "ok reloaded\n") == 0);
    size_t from = 0;
    CHECK(proc_wait_log(&p, "msg=reloaded source=admin", &from, 3000));

    CHECK_EQ_INT(proc_rewrite_config(&p, "bogus\n"), 0);
    CHECK_EQ_INT(ctl(out, sizeof out, "reload"), 1);
    CHECK(strstr(out, "error line 1: unknown directive \"bogus\"") != NULL);

    CHECK_EQ_INT(ctl(out, sizeof out, "frobnicate"), 2);
    CHECK(strstr(out, "usage: pacerctl") != NULL);

    raw("STATS\n", 6, out, sizeof out);
    CHECK(strncmp(out, "error unknown command", 21) == 0);
    raw("st\x01ts\n", 6, out, sizeof out);
    CHECK(strncmp(out, "error request has a control", 27) == 0);
    char big[1000];
    memset(big, 'a', sizeof big);
    raw(big, sizeof big, out, sizeof out);
    CHECK(strcmp(out, "error request too long\n") == 0);
    finish();
}

TEST(admin_socket_is_private)
{
    start("");
    struct stat st;
    CHECK_EQ_INT(stat(sock, &st), 0);
    CHECK(S_ISSOCK(st.st_mode));
    CHECK_EQ_INT(st.st_mode & 0777, 0600);
    finish();
    /* Removed on exit. */
    CHECK(stat(sock, &st) != 0);
}

TEST(admin_slow_client_does_not_block_others)
{
    start("");
    int slow = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un sa = {0};
    sa.sun_family = AF_UNIX;
    snprintf(sa.sun_path, sizeof sa.sun_path, "%s", sock);
    CHECK_EQ_INT(connect(slow, (struct sockaddr *)&sa, sizeof sa), 0);
    CHECK_EQ_INT(send_all(slow, "sta", 3), 3);
    char out[4096];
    CHECK_EQ_INT(ctl(out, sizeof out, "ping"), 0);
    uint64_t t0 = now_ms();
    long n = recv_all(slow, out, sizeof out - 1, 5000);
    uint64_t ms = now_ms() - t0;
    CHECK(n > 0);
    out[n > 0 ? n : 0] = '\0';
    CHECK(strstr(out, "error timed out") != NULL);
    CHECK(ms >= 1500);
    CHECK(ms < 3000);
    close(slow);
    finish();
}

TEST(admin_drain_command)
{
    start("");
    char out[4096];
    CHECK_EQ_INT(ctl(out, sizeof out, "drain"), 0);
    CHECK(strcmp(out, "ok draining\n") == 0);
    CHECK_EQ_INT(proc_wait_exit(&p, 10000), 0);
    CHECK(strstr(p.log, "msg=draining reason=admin") != NULL);
    proc_cleanup(&p);
    upstream_stop(&u);
}

TEST(admin_stale_socket_is_replaced_and_live_one_refused)
{
    port = free_port();
    snprintf(sock, sizeof sock, "/tmp/pacerd-admin-%d-%u.sock", (int)getpid(), port);
    /* A socket file nobody listens on, as a crash would leave behind. */
    int stale = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un sa = {0};
    sa.sun_family = AF_UNIX;
    snprintf(sa.sun_path, sizeof sa.sun_path, "%s", sock);
    CHECK_EQ_INT(bind(stale, (struct sockaddr *)&sa, sizeof sa), 0);
    close(stale);
    CHECK_EQ_INT(upstream_start(&u, UP_ECHO, 0), 0);
    char conf[2048];
    config(conf, sizeof conf, "");
    CHECK_EQ_INT(proc_start(&p, conf), 0);

    /* A second instance on the same admin socket (and another port). */
    struct proc second;
    char conf2[2048];
    snprintf(conf2, sizeof conf2,
             "admin_socket %s\nlistener b {\n listen 127.0.0.1:%u\n upstream 127.0.0.1:1\n}\n",
             sock, free_port());
    const char *args[] = {"-c", "%c", NULL};
    CHECK_EQ_INT(proc_spawn(&second, conf2, args), 0);
    CHECK_EQ_INT(proc_wait_exit(&second, 5000), 1);
    CHECK(strstr(second.log, "admin_socket is in use by a running pacerd") != NULL);
    proc_cleanup(&second);

    /* A path that is not a socket is never deleted. */
    char notsock[96];
    snprintf(notsock, sizeof notsock, "%s.txt", sock);
    FILE *f = fopen(notsock, "w");
    fclose(f);
    snprintf(conf2, sizeof conf2,
             "admin_socket %s\nlistener b {\n listen 127.0.0.1:%u\n upstream 127.0.0.1:1\n}\n",
             notsock, free_port());
    CHECK_EQ_INT(proc_spawn(&second, conf2, args), 0);
    CHECK_EQ_INT(proc_wait_exit(&second, 5000), 1);
    CHECK(strstr(second.log, "is not a socket") != NULL);
    proc_cleanup(&second);
    CHECK_EQ_INT(unlink(notsock), 0);

    char out[4096];
    CHECK_EQ_INT(ctl(out, sizeof out, "ping"), 0);
    finish();
}

TEST(pacerctl_without_a_server)
{
    char out[4096];
    const char *args[] = {"-s", "/tmp/pacerd-no-such.sock", "ping", NULL};
    CHECK_EQ_INT(run_ctl(out, sizeof out, args), 1);
    CHECK(strstr(out, "cannot connect to /tmp/pacerd-no-such.sock") != NULL);
    const char *none[] = {NULL};
    CHECK_EQ_INT(run_ctl(out, sizeof out, none), 2);
    char longpath[200];
    memset(longpath, 'x', sizeof longpath - 1);
    longpath[sizeof longpath - 1] = '\0';
    const char *toolong[] = {"-s", longpath, "ping", NULL};
    CHECK_EQ_INT(run_ctl(out, sizeof out, toolong), 2);
}

TEST(admin_connections_spread_across_workers)
{
    /* Each worker has its own SO_REUSEPORT socket, so the kernel hashes new
     * connections across them instead of one worker taking a burst. */
    start("");
    for (int i = 0; i < 64; i++) {
        int fd = tcp_connect(port);
        CHECK(works(fd));
        close(fd);
    }
    char out[8192];
    CHECK_EQ_INT(ctl(out, sizeof out, "stats"), 0);
    const char *w0 = strstr(out, "worker=0 accepted=");
    const char *w1 = strstr(out, "worker=1 accepted=");
    CHECK(w0 != NULL);
    CHECK(w1 != NULL);
    long a = w0 ? strtol(w0 + 18, NULL, 10) : 0;
    long b = w1 ? strtol(w1 + 18, NULL, 10) : 0;
    CHECK_EQ_INT(a + b, 64);
    CHECK(a >= 8);
    CHECK(b >= 8);
    finish();
}
