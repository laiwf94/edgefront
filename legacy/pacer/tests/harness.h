/* Helpers for integration tests that drive the real pacerd binary. */
#ifndef PACER_HARNESS_H
#define PACER_HARNESS_H

#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>

#define PROC_LOG_MAX (1 << 20)

struct proc {
    pid_t pid;
    int err_fd;
    size_t len;
    char conf_path[64];
    char log[PROC_LOG_MAX];
};

/* Write config text to a temp file and start pacerd on it. Returns 0 once
 * the "ready" line appears, or -1 (the process is reaped) if it exits or
 * takes longer than 5 s. */
int proc_start(struct proc *p, const char *config_text);

/* Start pacerd with explicit arguments; the config path replaces "%c". */
int proc_spawn(struct proc *p, const char *config_text, const char *const *args);

/* Rewrite the config file in place (for reload tests). */
int proc_rewrite_config(struct proc *p, const char *config_text);

/* Read pending log output; wait up to timeout_ms for `needle` to appear
 * after offset *from (updated past the match). Returns 1 found, 0 not. */
int proc_wait_log(struct proc *p, const char *needle, size_t *from, int timeout_ms);

/* Count occurrences of needle in the log so far. */
int proc_log_count(struct proc *p, const char *needle);

/* Wait for exit; returns the exit code, 128+signal, or -1 on timeout. */
int proc_wait_exit(struct proc *p, int timeout_ms);

/* Send a signal and wait for exit. */
int proc_stop(struct proc *p, int sig, int timeout_ms);

/* Kill if still running and remove the config file. */
void proc_cleanup(struct proc *p);

/* Print the captured log to stderr (on test failure). */
void proc_dump(struct proc *p);

/* A TCP port that was free a moment ago. */
uint16_t free_port(void);

/* Monotonic milliseconds. */
uint64_t now_ms(void);

/* Connect to 127.0.0.1:port, blocking. Returns fd or -1. */
int tcp_connect(uint16_t port);

/* Same, from a chosen loopback source address such as "127.0.0.2". */
int tcp_connect_from(uint16_t port, const char *src);

/* Run pacerctl with the given arguments; output (stdout and stderr) goes to
 * out. Returns its exit code, or -1. */
int run_ctl(char *out, size_t outlen, const char *const *args);

/* When positive, the next spawned pacerd gets this RLIMIT_NOFILE. */
extern int harness_nofile;

#endif
