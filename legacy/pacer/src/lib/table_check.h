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

#endif
