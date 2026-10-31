/* pacerd: a TCP proxy that rate-limits clients. See docs/DESIGN.md. */
#include "pacer.h"
#include "pacerd/config.h"
#include "pacerd/daemon.h"

#include <stdio.h>
#include <string.h>
#include <unistd.h>

static void usage(FILE *out)
{
    fprintf(out, "usage: pacerd -c CONFIG [-t]\n"
                 "       pacerd -v\n"
                 "  -c CONFIG  config file\n"
                 "  -t         check the config and exit\n"
                 "  -v         print the version and exit\n");
}

int main(int argc, char **argv)
{
    const char *path = NULL;
    int check_only = 0;
    int opt = 0;
    /* Argument parsing happens before any thread exists. */
    while ((opt = getopt(argc, argv, "c:tvh")) != -1) { // NOLINT(concurrency-mt-unsafe)
        switch (opt) {
        case 'c':
            path = optarg;
            break;
        case 't':
            check_only = 1;
            break;
        case 'v':
            printf("pacerd %s\n", pacer_version());
            return 0;
        case 'h':
            usage(stdout);
            return 0;
        default:
            usage(stderr);
            return 2;
        }
    }
    if (!path || optind != argc) {
        usage(stderr);
        return 2;
    }
    if (check_only) {
        static struct pd_config cfg;
        char err[PD_ERR_MAX];
        if (pd_config_load(path, &cfg, err, sizeof err) != 0) {
            fprintf(stderr, "pacerd: %s\n", err);
            return 2;
        }
        printf("pacerd: %s ok, %u listener%s\n", path, cfg.nlisteners,
               cfg.nlisteners == 1 ? "" : "s");
        return 0;
    }
    return pd_daemon_run(path);
}
