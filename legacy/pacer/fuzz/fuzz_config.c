/*
 * Fuzz the config parser. Besides the sanitizers, it checks that a parse
 * that succeeds produced a usable config and that every error message is a
 * single printable line pointing at a line number.
 */
#include "pacerd/config.h"

#include <stdlib.h>
#include <string.h>

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size);

static void check_ok(const struct pd_config *c)
{
    if (c->nlisteners < 1 || c->nlisteners > PD_MAX_LISTENERS || c->workers < 1 ||
        c->workers > 64) {
        abort();
    }
    for (uint32_t i = 0; i < c->nlisteners; i++) {
        const struct pd_listener_conf *l = &c->listeners[i];
        if (memchr(l->name, '\0', sizeof l->name) == NULL || l->name[0] == '\0' ||
            l->listen.len == 0 || l->upstream.len == 0 || l->buffer_size < 1024 ||
            l->max_conns < 1 || l->table_size < 16) {
            abort();
        }
        const struct pd_rate_opt *rates[] = {&l->per_ip_conn_rate, &l->per_ip_bandwidth,
                                             &l->bandwidth};
        for (size_t r = 0; r < 3; r++) {
            uint64_t iv = 0, tol = 0;
            if (rates[r]->on && pacer_rate_ticks(&rates[r]->rate, &iv, &tol) != PACER_OK) {
                abort();
            }
        }
    }
}

static void check_error(const char *err)
{
    if (strncmp(err, "line ", 5) != 0 && strncmp(err, "config larger", 13) != 0) {
        abort();
    }
    for (const char *p = err; *p; p++) {
        unsigned char ch = (unsigned char)*p;
        if (ch < 0x20 || ch >= 0x7f) {
            abort();
        }
    }
}

int LLVMFuzzerTestOneInput(const uint8_t *data, size_t size)
{
    static struct pd_config cfg;
    char err[PD_ERR_MAX];
    if (pd_config_parse((const char *)data, size, &cfg, err, sizeof err) == 0) {
        check_ok(&cfg);
    } else {
        check_error(err);
    }
    return 0;
}
