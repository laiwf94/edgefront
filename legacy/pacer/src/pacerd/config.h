/*
 * pacerd configuration: the file format, its parser and the parsed form.
 *
 * The parser is a pure function over a buffer: no I/O, no allocation, no
 * global state. pd_config_load adds the file read with a size limit.
 */
#ifndef PACERD_CONFIG_H
#define PACERD_CONFIG_H

#include "pacer.h"
#include "pacerd/log.h"

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/socket.h>

#define PD_MAX_LISTENERS   16
#define PD_NAME_MAX        32
#define PD_PATH_MAX        108 /* sun_path in struct sockaddr_un */
#define PD_CONFIG_MAX_SIZE (1 << 20)
#define PD_LINE_MAX        1024
#define PD_ERR_MAX         256

struct pd_addr {
    struct sockaddr_storage ss;
    socklen_t len;
};

/* An optional rate limit: `on` is false when the directive is absent. */
struct pd_rate_opt {
    bool on;
    pacer_rate rate;
};

struct pd_listener_conf {
    char name[PD_NAME_MAX];
    struct pd_addr listen;
    struct pd_addr upstream;
    uint32_t max_conns;
    uint32_t buffer_size;
    uint64_t connect_timeout_ns;
    uint64_t idle_timeout_ns;
    uint64_t min_rate_bytes; /* 0 disables the slow-client check */
    uint64_t min_rate_window_ns;
    uint32_t per_ip_max_conns; /* 0 means no cap */
    uint32_t ipv6_prefix;      /* IPv6 clients are grouped by this prefix */
    struct pd_rate_opt per_ip_conn_rate;
    struct pd_rate_opt per_ip_bandwidth;
    struct pd_rate_opt bandwidth;
    uint32_t table_size;
    int table_full; /* PACER_FULL_DENY or PACER_FULL_EVICT */
};

struct pd_config {
    uint32_t workers;
    char admin_socket[PD_PATH_MAX]; /* empty: no admin socket */
    uint64_t drain_timeout_ns;
    int log_level;
    uint32_t nlisteners;
    struct pd_listener_conf listeners[PD_MAX_LISTENERS];
};

/*
 * Parse `len` bytes of config text into *out. Returns 0, or -1 with a
 * message such as "line 12: per_ip_conn_rate: expected a rate like 10/s"
 * in err. *out is fully written either way.
 */
int pd_config_parse(const char *buf, size_t len, struct pd_config *out, char *err, size_t errlen);

/* Read and parse a file of at most PD_CONFIG_MAX_SIZE bytes. */
int pd_config_load(const char *path, struct pd_config *out, char *err, size_t errlen);

/* Format an address as "1.2.3.4:80" or "[::1]:80". */
void pd_addr_format(const struct pd_addr *a, char *buf, size_t len);

/* Parse "1.2.3.4:80" or "[::1]:80". Numeric only; no name lookup. */
int pd_addr_parse(const char *s, struct pd_addr *out);

#endif
