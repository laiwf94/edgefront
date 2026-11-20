#!/usr/bin/env python3
"""The proxy p99 regression gate's verdict.

Usage: scripts/check-p99-gate.py LIMIT RUN_DIR...

Each RUN_DIR holds one alternating A/B repetition of the gate scenario:
<RUN_DIR>/parent/<scenario>/edgefront-proxy.json (the parent commit) and
<RUN_DIR>/this/<scenario>/edgefront-proxy.json (the commit under test). A single run's p99 on a
shared runner moves by an order of magnitude from noise alone, so the gate
fails only on evidence: when the commit's median p99 is more than LIMIT
(0.05 is 5%) above the parent's AND a one-sided Mann-Whitney U test says the
commit's p99s are larger at p < 0.01, the noise model bench/gates.toml uses
for the cache. Every request of every run must have been answered, or the
comparison means nothing and the gate fails too.
Prints a Markdown table for the job summary.
"""
import json
import statistics
import sys
from pathlib import Path


def load(run_dir, side):
    files = list(Path(run_dir).glob(f"{side}/*/edgefront-proxy.json"))
    if len(files) != 1:
        sys.exit(f"{run_dir}/{side}: expected one edgefront-proxy.json, found {len(files)}")
    r = json.loads(files[0].read_text())
    m = r["measured"]
    errors = sum(m["errors"].values())
    if m["completed"] != m["scheduled"] or errors:
        sys.exit(f"{files[0]}: {m['completed']} of {m['scheduled']} answered, {errors} errors")
    return m["latency"]["p99_ms"]


ALPHA = 0.01


def mann_whitney_greater(a, b):
    """One-sided p-value that `b` tends to be larger than `a`: the exact
    distribution of the rank sum by counting, with ties at their mid ranks
    and the exact null over all ways to split the pooled ranks."""
    pooled = sorted((v, i) for i, v in enumerate(a + b))
    ranks = [0.0] * len(pooled)
    i = 0
    while i < len(pooled):
        j = i
        while j + 1 < len(pooled) and pooled[j + 1][0] == pooled[i][0]:
            j += 1
        for k in range(i, j + 1):
            ranks[pooled[k][1]] = (i + j) / 2 + 1
        i = j + 1
    n = len(b)
    observed = sum(ranks[len(a):])
    # Doubled ranks are integers even with mid ranks.
    doubled = [int(round(2 * r)) for r in ranks]
    # counts[k][s]: ways to choose k of the ranks summing (doubled) to s.
    counts = [dict() for _ in range(n + 1)]
    counts[0][0] = 1
    for r in doubled:
        for k in range(n, 0, -1):
            for total, ways in counts[k - 1].items():
                counts[k][total + r] = counts[k].get(total + r, 0) + ways
    target = int(round(2 * observed))
    total = sum(counts[n].values())
    at_least = sum(w for s, w in counts[n].items() if s >= target)
    return at_least / total


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    limit = float(sys.argv[1])
    runs = sys.argv[2:]
    base = [load(d, "parent") for d in runs]
    head = [load(d, "this") for d in runs]
    mb, mh = statistics.median(base), statistics.median(head)
    change = mh / mb - 1
    p = mann_whitney_greater(base, head)
    print("| | parent | this commit |")
    print("|---|---|---|")
    for i, (b, h) in enumerate(zip(base, head), 1):
        print(f"| run {i} p99 | {b:.3f} ms | {h:.3f} ms |")
    print(f"| median p99 | {mb:.3f} ms | {mh:.3f} ms |")
    print()
    verdict = "fail" if change > limit and p < ALPHA else "pass"
    print(
        f"median p99 change {change:+.1%} (limit +{limit:.0%}), "
        f"Mann-Whitney p = {p:.4f} (fails below {ALPHA}): **{verdict}**"
    )
    sys.exit(1 if verdict == "fail" else 0)


if __name__ == "__main__":
    main()
