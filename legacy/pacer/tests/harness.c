#include "harness.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int harness_nofile;

uint64_t now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000 + (uint64_t)ts.tv_nsec / 1000000;
}

static int write_file(const char *path, const char *text)
{
    FILE *f = fopen(path, "w");
    if (!f) {
        return -1;
    }
    size_t n = strlen(text);
    int ok = fwrite(text, 1, n, f) == n;
    return fclose(f) == 0 && ok ? 0 : -1;
}

int proc_rewrite_config(struct proc *p, const char *config_text)
{
    return write_file(p->conf_path, config_text);
}

int proc_spawn(struct proc *p, const char *config_text, const char *const *args)
{
    memset(p, 0, sizeof *p);
    p->pid = -1;
    p->err_fd = -1;
    snprintf(p->conf_path, sizeof p->conf_path, "/tmp/pacerd-test-XXXXXX");
    int cfd = mkstemp(p->conf_path);
    if (cfd < 0) {
        return -1;
    }
    close(cfd);
    if (write_file(p->conf_path, config_text) != 0) {
        return -1;
    }
    int pipefd[2];
    if (pipe2(pipefd, O_CLOEXEC) != 0) {
        return -1;
    }
    const char *bin = getenv("PACERD_BIN"); // NOLINT(concurrency-mt-unsafe): env is never written
    if (!bin) {
        bin = "build/asan/pacerd";
    }
    /* Copies made before fork: the child may not allocate. */
    char *argv[16];
    int argc = 0;
    argv[argc++] = strdup(bin);
    for (int i = 0; args[i] && argc < 15; i++) {
        argv[argc++] = strdup(strcmp(args[i], "%c") == 0 ? p->conf_path : args[i]);
    }
    argv[argc] = NULL;
    struct rlimit rl = {(rlim_t)harness_nofile, (rlim_t)harness_nofile};
    pid_t pid = fork();
    if (pid == 0) {
        if (harness_nofile > 0) {
            setrlimit(RLIMIT_NOFILE, &rl);
        }
        dup2(pipefd[1], 1);
        dup2(pipefd[1], 2);
        execv(argv[0], argv);
        _exit(127);
    }
    for (int i = 0; i < argc; i++) {
        free(argv[i]);
    }
    if (pid < 0) {
        return -1;
    }
    close(pipefd[1]);
    fcntl(pipefd[0], F_SETFL, O_NONBLOCK);
    p->pid = pid;
    p->err_fd = pipefd[0];
    return 0;
}

int proc_start(struct proc *p, const char *config_text)
{
    const char *args[] = {"-c", "%c", NULL};
    if (proc_spawn(p, config_text, args) != 0) {
        return -1;
    }
    size_t from = 0;
    if (proc_wait_log(p, "msg=ready", &from, 5000)) {
        return 0;
    }
    proc_dump(p);
    proc_cleanup(p);
    return -1;
}

static void pump(struct proc *p)
{
    for (;;) {
        size_t room = sizeof p->log - 1 - p->len;
        if (room == 0 || p->err_fd < 0) {
            return;
        }
        ssize_t r = read(p->err_fd, p->log + p->len, room);
        if (r <= 0) {
            return;
        }
        p->len += (size_t)r;
        p->log[p->len] = '\0';
    }
}

int proc_wait_log(struct proc *p, const char *needle, size_t *from, int timeout_ms)
{
    uint64_t deadline = now_ms() + (uint64_t)timeout_ms;
    size_t start = from ? *from : 0;
    for (;;) {
        pump(p);
        const char *hit = strstr(p->log + start, needle);
        if (hit) {
            if (from) {
                *from = (size_t)(hit - p->log) + strlen(needle);
            }
            return 1;
        }
        uint64_t now = now_ms();
        if (now >= deadline) {
            return 0;
        }
        struct pollfd pfd = {p->err_fd, POLLIN, 0};
        poll(&pfd, 1, (int)(deadline - now));
    }
}

int proc_log_count(struct proc *p, const char *needle)
{
    pump(p);
    int n = 0;
    for (const char *s = p->log; (s = strstr(s, needle)) != NULL; s += strlen(needle)) {
        n++;
    }
    return n;
}

int proc_wait_exit(struct proc *p, int timeout_ms)
{
    uint64_t deadline = now_ms() + (uint64_t)timeout_ms;
    while (p->pid > 0) {
        int status = 0;
        pid_t r = waitpid(p->pid, &status, WNOHANG);
        if (r == p->pid) {
            p->pid = -1;
            pump(p);
            return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
        }
        if (now_ms() >= deadline) {
            return -1;
        }
        pump(p);
        usleep(5000);
    }
    return -1;
}

int proc_stop(struct proc *p, int sig, int timeout_ms)
{
    if (p->pid > 0) {
        kill(p->pid, sig);
    }
    return proc_wait_exit(p, timeout_ms);
}

void proc_cleanup(struct proc *p)
{
    if (p->pid > 0) {
        kill(p->pid, SIGKILL);
        waitpid(p->pid, NULL, 0);
        p->pid = -1;
    }
    if (p->err_fd >= 0) {
        close(p->err_fd);
        p->err_fd = -1;
    }
    if (p->conf_path[0]) {
        unlink(p->conf_path);
        p->conf_path[0] = '\0';
    }
}

void proc_dump(struct proc *p)
{
    pump(p);
    fprintf(stderr, "---- pacerd log ----\n%s---- end ----\n", p->log);
}

uint16_t free_port(void)
{
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t len = sizeof a;
    if (fd < 0 || bind(fd, (struct sockaddr *)&a, sizeof a) != 0 ||
        getsockname(fd, (struct sockaddr *)&a, &len) != 0) {
        abort();
    }
    close(fd);
    return ntohs(a.sin_port);
}

int tcp_connect(uint16_t port)
{
    return tcp_connect_from(port, NULL);
}

int tcp_connect_from(uint16_t port, const char *src)
{
    int fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (fd < 0) {
        return -1;
    }
    if (src) {
        struct sockaddr_in b = {0};
        b.sin_family = AF_INET;
        if (inet_pton(AF_INET, src, &b.sin_addr) != 1 ||
            bind(fd, (struct sockaddr *)&b, sizeof b) != 0) {
            close(fd);
            return -1;
        }
    }
    if (connect(fd, (struct sockaddr *)&a, sizeof a) != 0) {
        close(fd);
        return -1;
    }
    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    return fd;
}

int run_ctl(char *out, size_t outlen, const char *const *args)
{
    const char *bin = getenv("PACERCTL_BIN"); // NOLINT(concurrency-mt-unsafe): env is never written
    if (!bin) {
        bin = "build/asan/pacerctl";
    }
    int pipefd[2];
    if (pipe2(pipefd, O_CLOEXEC) != 0) {
        return -1;
    }
    char *argv[16];
    int argc = 0;
    argv[argc++] = strdup(bin);
    for (int i = 0; args[i] && argc < 15; i++) {
        argv[argc++] = strdup(args[i]);
    }
    argv[argc] = NULL;
    pid_t pid = fork();
    if (pid == 0) {
        dup2(pipefd[1], 1);
        dup2(pipefd[1], 2);
        execv(argv[0], argv);
        _exit(127);
    }
    for (int i = 0; i < argc; i++) {
        free(argv[i]);
    }
    close(pipefd[1]);
    size_t len = 0;
    ssize_t r = 0;
    while (len + 1 < outlen && (r = read(pipefd[0], out + len, outlen - 1 - len)) > 0) {
        len += (size_t)r;
    }
    out[len] = '\0';
    close(pipefd[0]);
    int status = 0;
    if (pid < 0 || waitpid(pid, &status, 0) != pid) {
        return -1;
    }
    return WIFEXITED(status) ? WEXITSTATUS(status) : -1;
}
