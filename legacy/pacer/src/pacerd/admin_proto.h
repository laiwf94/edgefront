/*
 * Admin protocol: one request line per connection, one response, then the
 * server closes. Requests are a single word ("stats", "ping", "reload",
 * "drain") ended by "\n" (an optional "\r" before it is ignored), at most
 * PD_ADMIN_LINE_MAX bytes. Responses are text; an error response starts with
 * "error ".
 */
#ifndef PACERD_ADMIN_PROTO_H
#define PACERD_ADMIN_PROTO_H

#include <stddef.h>

#define PD_ADMIN_LINE_MAX 256

enum pd_admin_cmd {
    PD_ADMIN_INCOMPLETE = 0, /* no newline yet; read more */
    PD_ADMIN_STATS,
    PD_ADMIN_PING,
    PD_ADMIN_RELOAD,
    PD_ADMIN_DRAIN,
    PD_ADMIN_TOO_LONG, /* no newline within the limit */
    PD_ADMIN_BAD_BYTE, /* a control or non-ASCII byte in the line */
    PD_ADMIN_UNKNOWN,  /* a well-formed line that names no command */
};

/* Parse the bytes received so far. Pure: no I/O, no allocation. */
enum pd_admin_cmd pd_admin_parse(const char *buf, size_t len);

/* The error text sent back for a parse failure, or NULL for a command. */
const char *pd_admin_error(enum pd_admin_cmd cmd);

#endif
