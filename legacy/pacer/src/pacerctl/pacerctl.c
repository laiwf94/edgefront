/* pacerctl: send one command to a running pacerd's admin socket. */
#include "pacerd/admin_proto.h"
#include "pacerd/util.h"

#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define DEFAULT_SOCKET "/run/pacerd/admin.sock"
#define TIMEOUT_MS     5000

static void usage(FILE *out)
{
    fprintf(out, "usage: pacerctl [-s SOCKET] stats|ping|reload|drain\n"
                 "  -s SOCKET  admin socket (default " DEFAULT_SOCKET ")\n");
}

static int fail(const char *what, const char *path)
{
    char eb[128];
    fprintf(stderr, "pacerctl: %s %s: %s\n", what, path, pd_strerror(errno, eb, sizeof eb));
    return 1;
}

int main(int argc, char **argv)
{
    const char *path = DEFAULT_SOCKET;
    int argi = 1;
    if (argi + 1 < argc && strcmp(argv[argi], "-s") == 0) {
        path = argv[argi + 1];
        argi += 2;
    }
    if (argi + 1 != argc) {
        usage(stderr);
        return 2;
    }
    const char *cmd = argv[argi];
    char line[PD_ADMIN_LINE_MAX];
    int n = snprintf(line, sizeof line, "%s\n", cmd);
    if (n < 0 || (size_t)n >= sizeof line || pd_admin_parse(line, (size_t)n) >= PD_ADMIN_TOO_LONG) {
        usage(stderr);
        return 2;
    }
    struct sockaddr_un sa;
    memset(&sa, 0, sizeof sa);
    sa.sun_family = AF_UNIX;
    if (strlen(path) >= sizeof sa.sun_path) {
        fprintf(stderr, "pacerctl: socket path too long\n");
        return 2;
    }
    memcpy(sa.sun_path, path, strlen(path) + 1);
    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0 || connect(fd, (struct sockaddr *)&sa, sizeof sa) != 0) {
        return fail("cannot connect to", path);
    }
    if (send(fd, line, (size_t)n, MSG_NOSIGNAL) != n) {
        return fail("cannot send to", path);
    }
    char buf[4096];
    int is_error = -1;
    for (;;) {
        struct pollfd p = {fd, POLLIN, 0};
        if (poll(&p, 1, TIMEOUT_MS) <= 0) {
            fprintf(stderr, "pacerctl: no answer from %s\n", path);
            return 1;
        }
        ssize_t r = recv(fd, buf, sizeof buf, 0);
        if (r < 0) {
            return fail("cannot read from", path);
        }
        if (r == 0) {
            break;
        }
        if (is_error < 0) {
            is_error = r >= 6 && memcmp(buf, "error ", 6) == 0;
        }
        (void)fwrite(buf, 1, (size_t)r, is_error ? stderr : stdout);
    }
    (void)close(fd);
    return is_error == 0 ? 0 : 1;
}
