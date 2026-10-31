/*
 * Fuzz the admin request parser: whatever it returns must agree with the
 * bytes it was given.
 */
#include "pacerd/admin_proto.h"

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size);

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size)
{
    const char *buf = (const char *)data;
    enum pd_admin_cmd cmd = pd_admin_parse(buf, size);
    size_t window = size < PD_ADMIN_LINE_MAX ? size : PD_ADMIN_LINE_MAX;
    const char *nl = memchr(buf, '\n', window);
    switch (cmd) {
    case PD_ADMIN_INCOMPLETE:
        if (nl || size >= PD_ADMIN_LINE_MAX) {
            abort();
        }
        break;
    case PD_ADMIN_TOO_LONG:
        if (nl || size < PD_ADMIN_LINE_MAX) {
            abort();
        }
        break;
    case PD_ADMIN_STATS:
    case PD_ADMIN_PING:
    case PD_ADMIN_RELOAD:
    case PD_ADMIN_DRAIN: {
        static const char *const words[] = {"", "stats", "ping", "reload", "drain"};
        size_t n = strlen(words[cmd]);
        if (!nl || memcmp(buf, words[cmd], n) != 0 ||
            !(buf[n] == '\n' || (buf[n] == '\r' && buf[n + 1] == '\n'))) {
            abort();
        }
        break;
    }
    default:
        if (!nl || !pd_admin_error(cmd) || strncmp(pd_admin_error(cmd), "error ", 6) != 0) {
            abort();
        }
        break;
    }
    return 0;
}
