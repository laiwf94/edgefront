/*
 * SipHash-2-4 (Aumasson and Bernstein, 2012) and a source of key material.
 * The keyed table hashes client addresses with a per-table random key so an
 * attacker cannot pick addresses that collide.
 */
#ifndef PACER_SIPHASH_H
#define PACER_SIPHASH_H

#include <stddef.h>
#include <stdint.h>

#define PACER_SIPHASH_KEY_LEN 16

uint64_t pacer_siphash24(const uint8_t key[PACER_SIPHASH_KEY_LEN], const void *data, size_t len);

/* Fill buf with bytes from the kernel's CSPRNG. Returns 0 or -1. */
int pacer_random_bytes(void *buf, size_t len);

#endif
