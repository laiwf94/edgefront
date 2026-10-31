#include "pacerd/admin_proto.h"
#include "test.h"

#include <string.h>

static enum pd_admin_cmd parse(const char *s)
{
    return pd_admin_parse(s, strlen(s));
}

TEST(admin_parses_commands)
{
    CHECK_EQ_INT(parse("stats\n"), PD_ADMIN_STATS);
    CHECK_EQ_INT(parse("ping\r\n"), PD_ADMIN_PING);
    CHECK_EQ_INT(parse("reload\n"), PD_ADMIN_RELOAD);
    CHECK_EQ_INT(parse("drain\nextra"), PD_ADMIN_DRAIN);
}

TEST(admin_waits_for_a_full_line)
{
    CHECK_EQ_INT(parse(""), PD_ADMIN_INCOMPLETE);
    CHECK_EQ_INT(parse("sta"), PD_ADMIN_INCOMPLETE);
    CHECK_EQ_INT(parse("stats"), PD_ADMIN_INCOMPLETE);
}

TEST(admin_rejects_bad_requests)
{
    char big[PD_ADMIN_LINE_MAX + 10];
    memset(big, 'a', sizeof big);
    CHECK_EQ_INT(pd_admin_parse(big, sizeof big), PD_ADMIN_TOO_LONG);
    CHECK_EQ_INT(pd_admin_parse(big, PD_ADMIN_LINE_MAX), PD_ADMIN_TOO_LONG);
    CHECK_EQ_INT(pd_admin_parse(big, PD_ADMIN_LINE_MAX - 1), PD_ADMIN_INCOMPLETE);
    /* A newline past the limit does not count. */
    big[PD_ADMIN_LINE_MAX + 1] = '\n';
    CHECK_EQ_INT(pd_admin_parse(big, sizeof big), PD_ADMIN_TOO_LONG);
    CHECK_EQ_INT(parse("STATS\n"), PD_ADMIN_UNKNOWN);
    CHECK_EQ_INT(parse("stats now\n"), PD_ADMIN_UNKNOWN);
    CHECK_EQ_INT(parse("\n"), PD_ADMIN_UNKNOWN);
    CHECK_EQ_INT(parse("sta\x01ts\n"), PD_ADMIN_BAD_BYTE);
    CHECK_EQ_INT(parse("st\xc3\xa4ts\n"), PD_ADMIN_BAD_BYTE);
    CHECK_EQ_INT(pd_admin_parse("st\0ats\n", 7), PD_ADMIN_BAD_BYTE);
}

TEST(admin_error_texts)
{
    CHECK(strncmp(pd_admin_error(PD_ADMIN_TOO_LONG), "error ", 6) == 0);
    CHECK(strncmp(pd_admin_error(PD_ADMIN_BAD_BYTE), "error ", 6) == 0);
    CHECK(strncmp(pd_admin_error(PD_ADMIN_UNKNOWN), "error ", 6) == 0);
    CHECK(strncmp(pd_admin_error(PD_ADMIN_INCOMPLETE), "error ", 6) == 0);
    CHECK(pd_admin_error(PD_ADMIN_STATS) == NULL);
}
