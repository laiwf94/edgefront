#include "pacerd/config.h"
#include "pacerd/util.h"
#include "test.h"

#include <errno.h>
#include <string.h>

static size_t key_of(const char *addr, uint32_t prefix, uint8_t out[PD_CLIENT_KEY_MAX])
{
    struct pd_addr a;
    CHECK_EQ_INT(pd_addr_parse(addr, &a), 0);
    return pd_client_key(&a.ss, prefix, out);
}

TEST(client_key_ipv4)
{
    uint8_t k[PD_CLIENT_KEY_MAX];
    CHECK_EQ_U64(key_of("192.0.2.9:1", 64, k), 4);
    CHECK(memcmp(k, "\xc0\x00\x02\x09", 4) == 0);
}

TEST(client_key_ipv4_mapped_matches_ipv4)
{
    uint8_t a[PD_CLIENT_KEY_MAX], b[PD_CLIENT_KEY_MAX];
    CHECK_EQ_U64(key_of("[::ffff:192.0.2.9]:1", 64, a), 4);
    CHECK_EQ_U64(key_of("192.0.2.9:2", 64, b), 4);
    CHECK(memcmp(a, b, 4) == 0);
}

TEST(client_key_ipv6_prefix_groups_a_subnet)
{
    uint8_t a[PD_CLIENT_KEY_MAX], b[PD_CLIENT_KEY_MAX], c[PD_CLIENT_KEY_MAX];
    CHECK_EQ_U64(key_of("[2001:db8:1:2:aaaa::1]:1", 64, a), 16);
    CHECK_EQ_U64(key_of("[2001:db8:1:2:bbbb::7]:1", 64, b), 16);
    CHECK_EQ_U64(key_of("[2001:db8:1:3::1]:1", 64, c), 16);
    CHECK(memcmp(a, b, 16) == 0);
    CHECK(memcmp(a, c, 16) != 0);
    /* /128 keeps every bit; an odd prefix masks inside a byte. */
    CHECK_EQ_U64(key_of("[2001:db8:1:2:aaaa::1]:1", 128, a), 16);
    CHECK_EQ_U64(key_of("[2001:db8:1:2:bbbb::7]:1", 128, b), 16);
    CHECK(memcmp(a, b, 16) != 0);
    CHECK_EQ_U64(key_of("[2001:db8:1:2f::]:1", 60, a), 16);
    CHECK_EQ_U64(key_of("[2001:db8:1:20::]:1", 60, b), 16);
    CHECK(memcmp(a, b, 16) == 0);
    CHECK_EQ_U64(key_of("[2001:db8:1:2f::]:1", 200, c), 16);
}

TEST(client_key_unknown_family)
{
    struct sockaddr_storage ss;
    memset(&ss, 0, sizeof ss);
    ss.ss_family = AF_UNIX;
    uint8_t k[PD_CLIENT_KEY_MAX];
    CHECK_EQ_U64(pd_client_key(&ss, 64, k), 0);
}

TEST(strerror_is_thread_safe_and_readable)
{
    char buf[64];
    CHECK(strstr(pd_strerror(ENOENT, buf, sizeof buf), "No such file") != NULL);
}
