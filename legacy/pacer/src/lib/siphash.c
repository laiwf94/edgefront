#include "lib/siphash.h"

#include <errno.h>
#include <string.h>
#if defined(__linux__)
#include <sys/random.h>
#else
#include <stdlib.h>
#endif

static inline uint64_t rotl(uint64_t x, int b)
{
    return (x << b) | (x >> (64 - b));
}

static inline uint64_t load_le64(const uint8_t *p)
{
    uint64_t v = 0;
    for (int i = 7; i >= 0; i--) {
        v = (v << 8) | p[i];
    }
    return v;
}

#define SIPROUND                                                                                   \
    do {                                                                                           \
        v0 += v1;                                                                                  \
        v1 = rotl(v1, 13);                                                                         \
        v1 ^= v0;                                                                                  \
        v0 = rotl(v0, 32);                                                                         \
        v2 += v3;                                                                                  \
        v3 = rotl(v3, 16);                                                                         \
        v3 ^= v2;                                                                                  \
        v0 += v3;                                                                                  \
        v3 = rotl(v3, 21);                                                                         \
        v3 ^= v0;                                                                                  \
        v2 += v1;                                                                                  \
        v1 = rotl(v1, 17);                                                                         \
        v1 ^= v2;                                                                                  \
        v2 = rotl(v2, 32);                                                                         \
    } while (0)

uint64_t pacer_siphash24(const uint8_t key[PACER_SIPHASH_KEY_LEN], const void *data, size_t len)
{
    const uint8_t *in = data;
    uint64_t k0 = load_le64(key), k1 = load_le64(key + 8);
    uint64_t v0 = k0 ^ 0x736f6d6570736575ull;
    uint64_t v1 = k1 ^ 0x646f72616e646f6dull;
    uint64_t v2 = k0 ^ 0x6c7967656e657261ull;
    uint64_t v3 = k1 ^ 0x7465646279746573ull;

    size_t whole = len - (len % 8);
    for (size_t i = 0; i < whole; i += 8) {
        uint64_t m = load_le64(in + i);
        v3 ^= m;
        SIPROUND;
        SIPROUND;
        v0 ^= m;
    }

    /* Last block: the remaining bytes, zero padding, and the length's low
     * byte in the top position. */
    uint8_t tail[8] = {0};
    memcpy(tail, in + whole, len - whole);
    tail[7] = (uint8_t)len;
    uint64_t m = load_le64(tail);
    v3 ^= m;
    SIPROUND;
    SIPROUND;
    v0 ^= m;

    v2 ^= 0xff;
    SIPROUND;
    SIPROUND;
    SIPROUND;
    SIPROUND;
    return v0 ^ v1 ^ v2 ^ v3;
}

int pacer_random_bytes(void *buf, size_t len)
{
#if defined(__linux__)
    uint8_t *p = buf;
    while (len > 0) {
        ssize_t n = getrandom(p, len, 0);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        p += n;
        len -= (size_t)n;
    }
    return 0;
#else
    arc4random_buf(buf, len);
    return 0;
#endif
}
