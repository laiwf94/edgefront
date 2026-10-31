#include "pacerd/util.h"

#include <netinet/in.h>
#include <stdio.h>
#include <string.h>

const char *pd_strerror(int err, char *buf, size_t len)
{
#if defined(__GLIBC__) && defined(_GNU_SOURCE)
    return strerror_r(err, buf, len);
#else
    if (strerror_r(err, buf, len) != 0) {
        (void)snprintf(buf, len, "error %d", err);
    }
    return buf;
#endif
}

size_t pd_client_key(const struct sockaddr_storage *ss, uint32_t v6_prefix,
                     uint8_t out[PD_CLIENT_KEY_MAX])
{
    if (ss->ss_family == AF_INET) {
        const struct sockaddr_in *v4 = (const struct sockaddr_in *)ss;
        memcpy(out, &v4->sin_addr, 4);
        return 4;
    }
    if (ss->ss_family != AF_INET6) {
        return 0;
    }
    const struct sockaddr_in6 *v6 = (const struct sockaddr_in6 *)ss;
    const uint8_t *a = v6->sin6_addr.s6_addr;
    static const uint8_t mapped[12] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff};
    if (memcmp(a, mapped, sizeof mapped) == 0) {
        memcpy(out, a + 12, 4);
        return 4;
    }
    memset(out, 0, PD_CLIENT_KEY_MAX);
    uint32_t bits = v6_prefix > 128 ? 128 : v6_prefix;
    memcpy(out, a, bits / 8);
    if (bits % 8) {
        out[bits / 8] = (uint8_t)(a[bits / 8] & (0xff00u >> (bits % 8)));
    }
    return PD_CLIENT_KEY_MAX;
}
