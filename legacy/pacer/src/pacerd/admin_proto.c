#include "pacerd/admin_proto.h"

#include <string.h>

static const struct {
    const char *word;
    enum pd_admin_cmd cmd;
} commands[] = {
    {"stats", PD_ADMIN_STATS},
    {"ping", PD_ADMIN_PING},
    {"reload", PD_ADMIN_RELOAD},
    {"drain", PD_ADMIN_DRAIN},
};

enum pd_admin_cmd pd_admin_parse(const char *buf, size_t len)
{
    const char *nl = memchr(buf, '\n', len < PD_ADMIN_LINE_MAX ? len : PD_ADMIN_LINE_MAX);
    if (!nl) {
        return len >= PD_ADMIN_LINE_MAX ? PD_ADMIN_TOO_LONG : PD_ADMIN_INCOMPLETE;
    }
    size_t n = (size_t)(nl - buf);
    if (n > 0 && buf[n - 1] == '\r') {
        n--;
    }
    for (size_t i = 0; i < n; i++) {
        unsigned char c = (unsigned char)buf[i];
        if (c < 0x20 || c >= 0x7f) {
            return PD_ADMIN_BAD_BYTE;
        }
    }
    for (size_t i = 0; i < sizeof commands / sizeof commands[0]; i++) {
        if (strlen(commands[i].word) == n && memcmp(commands[i].word, buf, n) == 0) {
            return commands[i].cmd;
        }
    }
    return PD_ADMIN_UNKNOWN;
}

const char *pd_admin_error(enum pd_admin_cmd cmd)
{
    switch (cmd) {
    case PD_ADMIN_TOO_LONG:
        return "error request too long\n";
    case PD_ADMIN_BAD_BYTE:
        return "error request has a control or non-ASCII byte\n";
    case PD_ADMIN_UNKNOWN:
        return "error unknown command; try stats, ping, reload or drain\n";
    case PD_ADMIN_INCOMPLETE:
        return "error request incomplete\n";
    default:
        return NULL;
    }
}
