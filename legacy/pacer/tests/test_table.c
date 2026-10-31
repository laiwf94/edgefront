#include "lib/table_check.h"
#include "pacer.h"
#include "test.h"

#include <stdio.h>
#include <string.h>

#define SEC 1000000000ull

static const uint8_t fixed_key[16] = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16};

static pacer_table *make_table(uint32_t max_entries, uint32_t shards, uint64_t burst)
{
    pacer_table_config cfg = {max_entries, shards, {10, SEC, burst}, fixed_key, PACER_FULL_DENY};
    pacer_table *t = NULL;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_OK);
    return t;
}

static size_t key_name(char *buf, size_t cap, unsigned i)
{
    return (size_t)snprintf(buf, cap, "10.0.%u.%u", i / 256, i % 256);
}

TEST(table_rejects_bad_config)
{
    pacer_table *t = NULL;
    pacer_table_config cfg = {0, 0, {10, SEC, 1}, NULL, PACER_FULL_DENY};
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_EINVAL);
    cfg.max_entries = 100;
    cfg.shards = 3;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_EINVAL);
    cfg.shards = 128;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_EINVAL);
    cfg.shards = 0;
    cfg.rate.tokens = 0;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_EINVAL);
    cfg.rate.tokens = 1;
    cfg.full_policy = 7;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_EINVAL);
    cfg.full_policy = PACER_FULL_DENY;
    cfg.max_entries = (1u << 28) + 1;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_EINVAL);
    CHECK_EQ_INT(pacer_table_create(NULL, &t), PACER_EINVAL);
    CHECK_EQ_INT(pacer_table_create(&cfg, NULL), PACER_EINVAL);
    CHECK(t == NULL);
    pacer_table_destroy(NULL);
}

TEST(table_keys_are_independent)
{
    pacer_table *t = make_table(1000, 0, 2);
    CHECK_EQ_INT(pacer_table_take(t, "a", 1, SEC, 2, NULL), 1);
    CHECK_EQ_INT(pacer_table_take(t, "a", 1, SEC, 1, NULL), 0);
    CHECK_EQ_INT(pacer_table_take(t, "b", 1, SEC, 1, NULL), 1);
    CHECK_EQ_INT(pacer_table_take(t, "ab", 2, SEC, 2, NULL), 1);
    pacer_decision d;
    CHECK_EQ_INT(pacer_table_take(t, "a", 1, SEC, 1, &d), 0);
    CHECK_EQ_U64(d.retry_after_ns, SEC / 10);
    CHECK_EQ_INT(pacer_table_take(t, "a", 1, SEC + SEC / 10, 1, NULL), 1);
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK_EQ_U64(st.entries, 3);
    CHECK_EQ_U64(st.inserts, 3);
    CHECK_EQ_U64(st.capacity, 1000);
    pacer_table_destroy(t);
}

TEST(table_rejects_bad_keys_and_costs)
{
    pacer_table *t = make_table(100, 0, 2);
    char long_key[PACER_KEY_MAX + 1];
    memset(long_key, 'k', sizeof long_key);
    pacer_decision d;
    CHECK_EQ_INT(pacer_table_take(t, "", 0, SEC, 1, NULL), PACER_EINVAL);
    CHECK_EQ_INT(pacer_table_take(t, NULL, 4, SEC, 1, NULL), PACER_EINVAL);
    CHECK_EQ_INT(pacer_table_take(t, long_key, sizeof long_key, SEC, 1, NULL), PACER_EINVAL);
    CHECK_EQ_INT(pacer_table_take(t, long_key, PACER_KEY_MAX, SEC, 1, NULL), 1);
    CHECK_EQ_INT(pacer_table_take(t, "a", 1, SEC, 3, NULL), PACER_ERANGE);
    CHECK_EQ_INT(pacer_table_take_upto(t, "", 0, SEC, 1, &d), PACER_EINVAL);
    CHECK_EQ_INT(pacer_table_take_upto(t, "a", 1, SEC, 1, NULL), PACER_EINVAL);
    CHECK_EQ_INT(pacer_table_remove(t, "", 0), PACER_EINVAL);
    pacer_table_refund(t, "", 0, SEC, 1);
    pacer_table_destroy(t);
}

TEST(table_full_refuses_new_keys)
{
    pacer_table *t = make_table(12, 1, 1);
    char k[32];
    for (unsigned i = 0; i < 12; i++) {
        size_t n = key_name(k, sizeof k, i);
        CHECK_EQ_INT(pacer_table_take(t, k, n, SEC, 1, NULL), 1);
    }
    size_t n = key_name(k, sizeof k, 99);
    CHECK_EQ_INT(pacer_table_take(t, k, n, SEC, 1, NULL), PACER_EFULL);
    pacer_decision d;
    CHECK_EQ_INT(pacer_table_take_upto(t, k, n, SEC, 1, &d), PACER_EFULL);
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK_EQ_U64(st.entries, 12);
    CHECK_EQ_U64(st.rejections, 2);
    /* Existing keys still work when the table is full. */
    n = key_name(k, sizeof k, 3);
    CHECK_EQ_INT(pacer_table_take(t, k, n, 2 * SEC, 1, NULL), 1);
    pacer_table_destroy(t);
}

/* Each key takes its whole burst, so a later take tells whether the entry
 * is still there (denied) or was lost (admitted fresh). */
static int key_present(pacer_table *t, unsigned i)
{
    char k[32];
    size_t n = key_name(k, sizeof k, i);
    return pacer_table_take(t, k, n, SEC, 1, NULL) == 0;
}

TEST(table_delete_keeps_probe_chains_intact)
{
    /* 48 keys in 64 slots of one shard produce long probe chains. Remove keys
     * in a scrambled order and check every survivor after each removal. */
    const unsigned n = 48;
    pacer_table *t = make_table(n, 1, 1);
    char k[32];
    for (unsigned i = 0; i < n; i++) {
        size_t len = key_name(k, sizeof k, i);
        CHECK_EQ_INT(pacer_table_take(t, k, len, SEC, 1, NULL), 1);
    }
    unsigned removed[48] = {0};
    for (unsigned step = 0; step < n; step++) {
        unsigned victim = (step * 29 + 7) % n;
        size_t len = key_name(k, sizeof k, victim);
        CHECK_EQ_INT(pacer_table_remove(t, k, len), PACER_OK);
        CHECK_EQ_INT(pacer_table_remove(t, k, len), PACER_ENOENT);
        CHECK_EQ_INT(pacer_table_check(t), 0);
        removed[victim] = 1;
        unsigned lost = 0;
        for (unsigned i = 0; i < n; i++) {
            lost += !removed[i] && !key_present(t, i);
        }
        CHECK_EQ_U64(lost, 0);
    }
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK_EQ_U64(st.entries, 0);
    CHECK_EQ_U64(st.removals, n);
    pacer_table_destroy(t);
}

TEST(table_refund_after_a_shrink_gives_back_every_token)
{
    /* 10 tokens of debt at 10/s is 1 s; then the burst shrinks to 1.
     * Refunding 6 tokens leaves 400 ms of debt, so the next token is 400 ms
     * away. A refund capped at one burst would leave it 900 ms away. */
    pacer_table *t = make_table(100, 0, 100);
    pacer_decision d;
    CHECK_EQ_INT(pacer_table_take_upto(t, "k", 1, SEC, 10, &d), PACER_OK);
    CHECK_EQ_U64(d.granted, 10);
    pacer_rate small = {10, SEC, 1};
    CHECK_EQ_INT(pacer_table_reconfigure(t, &small), PACER_OK);
    pacer_table_refund(t, "k", 1, SEC, 6);
    CHECK_EQ_INT(pacer_table_take(t, "k", 1, SEC, 1, &d), 0);
    CHECK_EQ_U64(d.retry_after_ns, 4 * SEC / 10);
    pacer_table_destroy(t);
}

TEST(table_take_upto_and_refund)
{
    pacer_table *t = make_table(100, 0, 100);
    pacer_decision d;
    CHECK_EQ_INT(pacer_table_take_upto(t, "k", 1, SEC, 70, &d), PACER_OK);
    CHECK_EQ_U64(d.granted, 70);
    CHECK_EQ_INT(pacer_table_take_upto(t, "k", 1, SEC, 70, &d), PACER_OK);
    CHECK_EQ_U64(d.granted, 30);
    CHECK_EQ_INT(pacer_table_take_upto(t, "k", 1, SEC, 70, &d), PACER_OK);
    CHECK_EQ_U64(d.granted, 0);
    CHECK_EQ_U64(d.retry_after_ns, SEC / 10);
    pacer_table_refund(t, "k", 1, SEC, 20);
    CHECK_EQ_INT(pacer_table_take_upto(t, "k", 1, SEC, 70, &d), PACER_OK);
    CHECK_EQ_U64(d.granted, 20);
    /* Refunding an unknown key is harmless. */
    pacer_table_refund(t, "zz", 2, SEC, 20);
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK_EQ_U64(st.entries, 1);
    pacer_table_destroy(t);
}

TEST(table_reconfigure_applies_to_every_key)
{
    pacer_table *t = make_table(100, 0, 1);
    CHECK_EQ_INT(pacer_table_take(t, "a", 1, SEC, 1, NULL), 1);
    pacer_rate r = {10, SEC, 5};
    CHECK_EQ_INT(pacer_table_reconfigure(t, &r), PACER_OK);
    CHECK_EQ_INT(pacer_table_take(t, "b", 1, SEC, 5, NULL), 1);
    CHECK_EQ_INT(pacer_table_take(t, "a", 1, SEC, 4, NULL), 1);
    pacer_rate bad = {0, SEC, 5};
    CHECK_EQ_INT(pacer_table_reconfigure(t, &bad), PACER_EINVAL);
    pacer_table_destroy(t);
}

TEST(table_memory_is_fixed_at_create)
{
    pacer_table *t = make_table(65536, 0, 1);
    size_t before = pacer_table_memory(t);
    /* 65536 keys in 1024 shards of 64, each in 128 slots of 64 bytes:
     * 8 MiB of entries plus the shard headers. */
    CHECK(before >= (size_t)1024 * 128 * 64);
    CHECK(before < (size_t)1024 * 128 * 64 + (size_t)1024 * 256);
    char k[32];
    for (unsigned i = 0; i < 70000; i++) {
        size_t n = key_name(k, sizeof k, i);
        (void)pacer_table_take(t, k, n, SEC, 1, NULL);
    }
    CHECK_EQ_U64(pacer_table_memory(t), before);
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK(st.entries <= 65536);
    CHECK_EQ_U64(st.entries + st.rejections, 70000);
    pacer_table_destroy(t);
}

TEST(table_random_hash_key)
{
    pacer_table_config cfg = {10, 0, {10, SEC, 1}, NULL, PACER_FULL_DENY};
    pacer_table *t = NULL;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_OK);
    CHECK_EQ_INT(pacer_table_take(t, "a", 1, SEC, 1, NULL), 1);
    CHECK_EQ_INT(pacer_table_take(t, "a", 1, SEC, 1, NULL), 0);
    pacer_table_destroy(t);
}

static pacer_table *make_policy_table(uint32_t max_entries, int policy)
{
    pacer_table_config cfg = {max_entries, 1, {10, SEC, 1}, fixed_key, policy};
    pacer_table *t = NULL;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_OK);
    return t;
}

static int take_key(pacer_table *t, unsigned i, uint64_t now)
{
    char k[32];
    size_t n = key_name(k, sizeof k, i);
    return pacer_table_take(t, k, n, now, 1, NULL);
}

TEST(table_reclaims_idle_entries_without_a_sweep)
{
    /* At 10/s with burst 1, an entry is idle 100 ms after its last take. */
    pacer_table *t = make_policy_table(16, PACER_FULL_DENY);
    for (unsigned i = 0; i < 16; i++) {
        CHECK_EQ_INT(take_key(t, i, SEC), 1);
    }
    CHECK_EQ_INT(take_key(t, 100, SEC), PACER_EFULL);
    uint64_t later = SEC + SEC / 10;
    for (unsigned i = 100; i < 116; i++) {
        CHECK_EQ_INT(take_key(t, i, later), 1);
    }
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK_EQ_U64(st.entries, 16);
    CHECK_EQ_U64(st.reclaims, 16);
    CHECK_EQ_U64(st.rejections, 1);
    pacer_table_destroy(t);
}

TEST(table_deny_policy_keeps_active_debt)
{
    pacer_table *t = make_policy_table(16, PACER_FULL_DENY);
    for (unsigned i = 0; i < 16; i++) {
        CHECK_EQ_INT(take_key(t, i, SEC), 1);
    }
    for (unsigned i = 100; i < 200; i++) {
        CHECK_EQ_INT(take_key(t, i, SEC), PACER_EFULL);
    }
    /* Every original key still owes its debt. */
    for (unsigned i = 0; i < 16; i++) {
        CHECK_EQ_INT(take_key(t, i, SEC), 0);
    }
    pacer_table_destroy(t);
}

TEST(table_evict_policy_drops_least_recently_used)
{
    pacer_table *t = make_policy_table(16, PACER_FULL_EVICT);
    for (unsigned i = 0; i < 16; i++) {
        CHECK_EQ_INT(take_key(t, i, SEC), 1);
    }
    /* Touch key 0 so key 1 becomes the coldest. */
    CHECK_EQ_INT(take_key(t, 0, SEC), 0);
    CHECK_EQ_INT(take_key(t, 100, SEC), 1);
    CHECK_EQ_INT(take_key(t, 1, SEC), 1); /* forgotten, so admitted fresh */
    CHECK_EQ_INT(take_key(t, 0, SEC), 0); /* still remembered */
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK_EQ_U64(st.evictions, 2);
    CHECK_EQ_U64(st.entries, 16);
    pacer_table_destroy(t);
}

TEST(table_sweep_removes_only_idle_entries)
{
    pacer_table_config cfg = {1024, 4, {10, SEC, 1}, fixed_key, PACER_FULL_DENY};
    pacer_table *t = NULL;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_OK);
    for (unsigned i = 0; i < 500; i++) {
        CHECK_EQ_INT(take_key(t, i, SEC), 1);
    }
    for (unsigned i = 500; i < 800; i++) {
        CHECK_EQ_INT(take_key(t, i, 2 * SEC), 1);
    }
    /* At 2.05 s the first 500 are idle and the last 300 are not. */
    CHECK_EQ_U64(pacer_table_sweep(t, 2 * SEC + SEC / 20, 100), 500);
    CHECK_EQ_INT(pacer_table_check(t), 0);
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK_EQ_U64(st.entries, 300);
    for (unsigned i = 500; i < 800; i++) {
        CHECK_EQ_INT(take_key(t, i, 2 * SEC), 0);
    }
    /* Sweeping one shard at a time visits them round robin. */
    size_t removed = 0;
    for (int n = 0; n < 4; n++) {
        removed += pacer_table_sweep(t, 10 * SEC, 1);
    }
    CHECK_EQ_U64(removed, 300);
    pacer_table_destroy(t);
}

TEST(table_lru_survives_heavy_churn)
{
    /* Random-looking churn with removes, reclaims and evictions; the LRU
     * list must stay consistent with the slot array throughout. */
    pacer_table *t = make_policy_table(64, PACER_FULL_EVICT);
    uint64_t now = SEC;
    for (unsigned step = 0; step < 20000; step++) {
        unsigned k = (step * 2654435761u) % 300;
        if (step % 7 == 0) {
            char buf[32];
            size_t n = key_name(buf, sizeof buf, k);
            (void)pacer_table_remove(t, buf, n);
        } else {
            CHECK(take_key(t, k, now) >= 0);
        }
        now += SEC / 1000;
        if (pacer_table_check(t) != 0) {
            CHECK_EQ_INT(pacer_table_check(t), 0);
            break;
        }
    }
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK(st.entries <= 64);
    CHECK_EQ_U64(st.inserts, st.entries + st.removals + st.reclaims + st.evictions);
    pacer_table_destroy(t);
}

TEST(table_hold_caps_concurrency)
{
    pacer_table_config cfg = {100, 1, {1000, SEC, 100}, fixed_key, PACER_FULL_DENY};
    pacer_table *t = NULL;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_OK);
    for (int i = 0; i < 3; i++) {
        CHECK_EQ_INT(pacer_table_hold(t, "ip", 2, SEC, 3, NULL), 1);
    }
    pacer_decision d;
    CHECK_EQ_INT(pacer_table_hold(t, "ip", 2, SEC, 3, &d), PACER_ELIMIT);
    CHECK_EQ_INT(pacer_table_hold(t, "other", 5, SEC, 3, NULL), 1);
    CHECK_EQ_INT(pacer_table_release(t, "ip", 2), PACER_OK);
    CHECK_EQ_INT(pacer_table_hold(t, "ip", 2, SEC, 3, NULL), 1);
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK_EQ_U64(st.holds, 4);
    /* The cap check costs no token: 4 grants out of a burst of 100. */
    CHECK_EQ_U64(pacer_table_take(t, "ip", 2, SEC, 96, NULL), 1);
    pacer_table_destroy(t);
}

TEST(table_hold_respects_the_rate)
{
    pacer_table_config cfg = {100, 1, {10, SEC, 2}, fixed_key, PACER_FULL_DENY};
    pacer_table *t = NULL;
    CHECK_EQ_INT(pacer_table_create(&cfg, &t), PACER_OK);
    CHECK_EQ_INT(pacer_table_hold(t, "ip", 2, SEC, 100, NULL), 1);
    CHECK_EQ_INT(pacer_table_hold(t, "ip", 2, SEC, 100, NULL), 1);
    pacer_decision d;
    CHECK_EQ_INT(pacer_table_hold(t, "ip", 2, SEC, 100, &d), 0);
    CHECK_EQ_U64(d.retry_after_ns, SEC / 10);
    CHECK_EQ_INT(pacer_table_hold(t, "ip", 2, SEC + SEC / 10, 100, NULL), 1);
    CHECK_EQ_INT(pacer_table_hold(t, "", 0, SEC, 1, NULL), PACER_EINVAL);
    pacer_table_destroy(t);
}

TEST(table_release_without_hold_is_an_error)
{
    pacer_table *t = make_policy_table(16, PACER_FULL_DENY);
    CHECK_EQ_INT(pacer_table_release(t, "nobody", 6), PACER_ENOENT);
    CHECK_EQ_INT(take_key(t, 1, SEC), 1);
    char k[32];
    size_t n = key_name(k, sizeof k, 1);
    CHECK_EQ_INT(pacer_table_release(t, k, n), PACER_ENOENT);
    CHECK_EQ_INT(pacer_table_release(t, "", 0), PACER_EINVAL);
    pacer_table_destroy(t);
}

TEST(table_held_entries_are_never_dropped)
{
    pacer_table *t = make_policy_table(16, PACER_FULL_EVICT);
    char k[32];
    for (unsigned i = 0; i < 16; i++) {
        size_t n = key_name(k, sizeof k, i);
        CHECK_EQ_INT(pacer_table_hold(t, k, n, SEC, 10, NULL), 1);
    }
    /* Long after their buckets refilled, held entries are neither idle nor
     * evictable, so a new key is refused even under EVICT. */
    CHECK_EQ_INT(take_key(t, 100, 100 * SEC), PACER_EFULL);
    CHECK_EQ_U64(pacer_table_sweep(t, 100 * SEC, 1), 0);
    /* Releasing one makes it idle and reclaimable. */
    size_t n = key_name(k, sizeof k, 5);
    CHECK_EQ_INT(pacer_table_release(t, k, n), PACER_OK);
    CHECK_EQ_INT(take_key(t, 100, 100 * SEC), 1);
    pacer_table_stats st;
    pacer_table_get_stats(t, &st);
    CHECK_EQ_U64(st.reclaims, 1);
    CHECK_EQ_U64(st.holds, 15);
    /* Removing a held key drops its holds from the count. */
    n = key_name(k, sizeof k, 6);
    CHECK_EQ_INT(pacer_table_remove(t, k, n), PACER_OK);
    pacer_table_get_stats(t, &st);
    CHECK_EQ_U64(st.holds, 14);
    CHECK_EQ_INT(pacer_table_check(t), 0);
    pacer_table_destroy(t);
}

TEST(table_evict_skips_held_entries)
{
    pacer_table *t = make_policy_table(16, PACER_FULL_EVICT);
    char k[32];
    size_t n = key_name(k, sizeof k, 0);
    CHECK_EQ_INT(pacer_table_hold(t, k, n, SEC, 10, NULL), 1);
    for (unsigned i = 1; i < 16; i++) {
        CHECK_EQ_INT(take_key(t, i, SEC), 1);
    }
    /* Key 0 is the coldest but held, so key 1 is evicted instead. */
    CHECK_EQ_INT(take_key(t, 100, SEC), 1);
    CHECK_EQ_INT(take_key(t, 1, SEC), 1);
    CHECK_EQ_INT(pacer_table_release(t, k, n), PACER_OK);
    pacer_table_destroy(t);
}

TEST(table_take_range)
{
    pacer_table *t = make_table(100, 0, 100);
    pacer_decision d;
    CHECK_EQ_INT(pacer_table_take_range(t, "k", 1, SEC, 1, 95, &d), PACER_OK);
    CHECK_EQ_U64(d.granted, 95);
    CHECK_EQ_INT(pacer_table_take_range(t, "k", 1, SEC, 10, 95, &d), PACER_OK);
    CHECK_EQ_U64(d.granted, 0);
    CHECK_EQ_U64(d.retry_after_ns, SEC / 2);
    pacer_table_destroy(t);
}
