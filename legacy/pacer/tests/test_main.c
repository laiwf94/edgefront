#include "test.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define MAX_TESTS 512

struct test_case {
    const char *name;
    test_fn fn;
};

static struct test_case tests[MAX_TESTS];
static int test_count;
static int current_failures;

void test_register(const char *name, test_fn fn)
{
    if (test_count == MAX_TESTS) {
        fprintf(stderr, "too many tests; raise MAX_TESTS\n");
        abort();
    }
    tests[test_count].name = name;
    tests[test_count].fn = fn;
    test_count++;
}

void test_fail(const char *file, int line, const char *expr, const char *detail)
{
    current_failures++;
    fprintf(stderr, "  %s:%d: CHECK(%s) failed%s%s\n", file, line, expr, detail ? ": " : "",
            detail ? detail : "");
}

int test_failed_in_current(void)
{
    return current_failures != 0;
}

/* Usage: unit [substring]. Runs every test whose name contains the
 * substring, or all tests. */
int main(int argc, char **argv)
{
    const char *filter = argc > 1 ? argv[1] : NULL;
    int ran = 0, failed = 0;
    for (int i = 0; i < test_count; i++) {
        if (filter && !strstr(tests[i].name, filter)) {
            continue;
        }
        current_failures = 0;
        tests[i].fn();
        ran++;
        if (current_failures) {
            failed++;
            fprintf(stderr, "FAIL %s\n", tests[i].name);
        }
    }
    printf("%d tests, %d failed\n", ran, failed);
    return failed ? 1 : 0;
}
