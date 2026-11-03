/* Invariant check for tests: not part of the public API. */
#ifndef PACER_TABLE_CHECK_H
#define PACER_TABLE_CHECK_H

#include "pacer.h"

/*
 * Walk every shard under its lock and verify that the LRU list links each
 * occupied slot exactly once in both directions, and that every key is
 * found by a probe from its home slot. Returns 0 or the number of the first
 * failed check.
 */
int pacer_table_check(pacer_table *t);

/*
 * Read a key's state without adding it or changing its recency: its
 * theoretical arrival time in ticks and its open holds. Returns PACER_OK,
 * PACER_ENOENT for a key that is not present, or PACER_EINVAL for an empty
 * or oversized key. The differential driver compares table states with it.
 */
int pacer_table_peek(pacer_table *t, const void *key, size_t len, uint64_t *tat, uint32_t *holds);

#endif
