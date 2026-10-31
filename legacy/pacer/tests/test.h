/* Minimal test harness: TEST() registers a function, CHECK*() records
 * failures and keeps going so one run shows every broken assertion. */
#ifndef PACER_TEST_H
#define PACER_TEST_H

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>

typedef void (*test_fn)(void);

void test_register(const char *name, test_fn fn);
void test_fail(const char *file, int line, const char *expr, const char *detail);
int test_failed_in_current(void);

#define TEST(name)                                                                                 \
    static void name(void);                                                                        \
    __attribute__((constructor)) static void register_##name(void)                                 \
    {                                                                                              \
        test_register(#name, name);                                                                \
    }                                                                                              \
    static void name(void)

#define CHECK(cond)                                                                                \
    do {                                                                                           \
        if (!(cond))                                                                               \
            test_fail(__FILE__, __LINE__, #cond, NULL);                                            \
    } while (0)

#define CHECK_EQ_U64(a, b)                                                                         \
    do {                                                                                           \
        uint64_t check_a_ = (uint64_t)(a), check_b_ = (uint64_t)(b);                               \
        if (check_a_ != check_b_) {                                                                \
            char check_buf_[96];                                                                   \
            (void)snprintf(check_buf_, sizeof check_buf_, "%" PRIu64 " != %" PRIu64, check_a_,     \
                           check_b_);                                                              \
            test_fail(__FILE__, __LINE__, #a " == " #b, check_buf_);                               \
        }                                                                                          \
    } while (0)

#define CHECK_EQ_INT(a, b)                                                                         \
    do {                                                                                           \
        long long check_a_ = (long long)(a), check_b_ = (long long)(b);                            \
        if (check_a_ != check_b_) {                                                                \
            char check_buf_[96];                                                                   \
            (void)snprintf(check_buf_, sizeof check_buf_, "%lld != %lld", check_a_, check_b_);     \
            test_fail(__FILE__, __LINE__, #a " == " #b, check_buf_);                               \
        }                                                                                          \
    } while (0)

#endif
