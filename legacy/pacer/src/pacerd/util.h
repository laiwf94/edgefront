/* Small helpers shared by the daemon. */
#ifndef PACERD_UTIL_H
#define PACERD_UTIL_H

#include <stddef.h>
#include <stdint.h>
#include <sys/socket.h>

#define PD_CLIENT_KEY_MAX 16

/* Thread-safe strerror: writes the message for `err` into buf and returns
 * buf (or a static string the C library chose). */
const char *pd_strerror(int err, char *buf, size_t len);

/*
 * The limiter key for a client address. IPv4 is its 4 bytes. An IPv4-mapped
 * IPv6 address (::ffff:a.b.c.d) is the same 4 bytes, so one client cannot
 * count as two. Other IPv6 addresses keep only their first `v6_prefix`
 * bits: a single host usually controls a whole /64. Returns the key length,
 * or 0 for an unknown family.
 */
size_t pd_client_key(const struct sockaddr_storage *ss, uint32_t v6_prefix,
                     uint8_t out[PD_CLIENT_KEY_MAX]);

#endif
