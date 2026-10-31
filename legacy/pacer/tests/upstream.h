/* Test upstream servers and client helpers for integration tests. */
#ifndef PACER_UPSTREAM_H
#define PACER_UPSTREAM_H

#include <pthread.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>

enum upstream_mode {
    UP_ECHO,   /* echo until EOF, then close */
    UP_HASH,   /* read to EOF, reply "<fnv64 hex> <count>\n", close */
    UP_SOURCE, /* send `param` pattern bytes, then close */
    UP_RESET,  /* read `param` bytes, then reset the connection */
    UP_STALL,  /* accept, then neither read nor write until stopped */
    UP_ZEROS,  /* send `param` zero bytes as fast as possible, then close */
};

struct upstream {
    int fd;
    uint16_t port;
    enum upstream_mode mode;
    uint64_t param;
    pthread_t thread;
    atomic_int stop;
    atomic_int active;
    atomic_int accepted;
};

int upstream_start(struct upstream *u, enum upstream_mode mode, uint64_t param);
void upstream_stop(struct upstream *u);

/* FNV-1a over a buffer, continuing from h (start with FNV_START). */
#define FNV_START 0xcbf29ce484222325ull
uint64_t fnv(uint64_t h, const void *buf, size_t len);

/* Deterministic test bytes: fill buf with the stream at offset `off`. */
void pattern(uint8_t *buf, size_t len, uint64_t off);

/* Blocking helpers with a receive timeout. Return bytes moved or -1. */
long send_all(int fd, const void *buf, size_t len);
long recv_all(int fd, void *buf, size_t len, int timeout_ms);

/* Read until EOF or timeout; returns bytes read, or -1 on error/timeout.
 * The FNV hash of the data goes to *hash; with hash NULL nothing is
 * hashed, so the reader is not the bottleneck in benchmarks. */
long recv_to_eof(int fd, uint64_t *hash, int timeout_ms);

#endif
