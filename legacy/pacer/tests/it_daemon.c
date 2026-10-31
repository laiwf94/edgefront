/* Start-up, shutdown and command-line behaviour of the real binary. */
#include "harness.h"
#include "test.h"

#include <signal.h>
#include <stdio.h>
#include <string.h>

static struct proc p;

static void simple_config(char *buf, size_t len, uint16_t port, const char *extra)
{
    snprintf(buf, len,
             "workers 2\n"
             "drain_timeout 2s\n"
             "listener t {\n"
             "  listen 127.0.0.1:%u\n"
             "  upstream 127.0.0.1:1\n"
             "%s"
             "}\n",
             port, extra);
}

TEST(daemon_starts_and_drains_on_sigterm)
{
    char conf[512];
    simple_config(conf, sizeof conf, free_port(), "");
    CHECK_EQ_INT(proc_start(&p, conf), 0);
    CHECK(strstr(p.log, "workers=2 listeners=1") != NULL);
    CHECK_EQ_INT(proc_stop(&p, SIGTERM, 5000), 0);
    CHECK(strstr(p.log, "msg=draining reason=SIGTERM") != NULL);
    CHECK(strstr(p.log, "msg=drained") != NULL);
    proc_cleanup(&p);
}

TEST(daemon_stops_on_sigint)
{
    char conf[512];
    simple_config(conf, sizeof conf, free_port(), "");
    CHECK_EQ_INT(proc_start(&p, conf), 0);
    CHECK_EQ_INT(proc_stop(&p, SIGINT, 5000), 0);
    CHECK(strstr(p.log, "reason=SIGINT") != NULL);
    proc_cleanup(&p);
}

TEST(daemon_bad_config_exits_2)
{
    const char *args[] = {"-c", "%c", NULL};
    CHECK_EQ_INT(proc_spawn(&p, "listener x {\n", args), 0);
    CHECK_EQ_INT(proc_wait_exit(&p, 5000), 2);
    CHECK(strstr(p.log, "msg=\"bad config\" error=\"line 1: listener x: missing }\"") != NULL);
    proc_cleanup(&p);
}

TEST(daemon_port_in_use_exits_1)
{
    char conf[512];
    uint16_t port = free_port();
    simple_config(conf, sizeof conf, port, "");
    struct proc first;
    CHECK_EQ_INT(proc_start(&first, conf), 0);
    const char *args[] = {"-c", "%c", NULL};
    CHECK_EQ_INT(proc_spawn(&p, conf, args), 0);
    CHECK_EQ_INT(proc_wait_exit(&p, 5000), 1);
    CHECK(strstr(p.log, "msg=\"cannot listen\"") != NULL);
    CHECK(strstr(p.log, "Address already in use") != NULL);
    proc_cleanup(&p);
    CHECK_EQ_INT(proc_stop(&first, SIGTERM, 5000), 0);
    proc_cleanup(&first);
}

TEST(daemon_command_line)
{
    const char *version[] = {"-v", NULL};
    CHECK_EQ_INT(proc_spawn(&p, "", version), 0);
    CHECK_EQ_INT(proc_wait_exit(&p, 5000), 0);
    CHECK(strstr(p.log, "pacerd 0.1.0") != NULL);
    proc_cleanup(&p);

    const char *help[] = {"-h", NULL};
    CHECK_EQ_INT(proc_spawn(&p, "", help), 0);
    CHECK_EQ_INT(proc_wait_exit(&p, 5000), 0);
    proc_cleanup(&p);

    const char *none[] = {NULL};
    CHECK_EQ_INT(proc_spawn(&p, "", none), 0);
    CHECK_EQ_INT(proc_wait_exit(&p, 5000), 2);
    CHECK(strstr(p.log, "usage: pacerd") != NULL);
    proc_cleanup(&p);

    const char *bad[] = {"-x", NULL};
    CHECK_EQ_INT(proc_spawn(&p, "", bad), 0);
    CHECK_EQ_INT(proc_wait_exit(&p, 5000), 2);
    proc_cleanup(&p);

    char conf[512];
    simple_config(conf, sizeof conf, 9, "");
    const char *check[] = {"-t", "-c", "%c", NULL};
    CHECK_EQ_INT(proc_spawn(&p, conf, check), 0);
    CHECK_EQ_INT(proc_wait_exit(&p, 5000), 0);
    CHECK(strstr(p.log, " ok, 1 listener\n") != NULL);
    proc_cleanup(&p);

    const char *check_bad[] = {"-t", "-c", "%c", NULL};
    CHECK_EQ_INT(proc_spawn(&p, "nonsense\n", check_bad), 0);
    CHECK_EQ_INT(proc_wait_exit(&p, 5000), 2);
    CHECK(strstr(p.log, "pacerd: line 1: unknown directive") != NULL);
    proc_cleanup(&p);
}
