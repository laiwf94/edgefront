#!/usr/bin/env python3
"""The proxy p99 regression gate's verdict.

Usage: scripts/check-p99-gate.py LIMIT RUN_DIR...

Each RUN_DIR holds one alternating A/B repetition of the gate scenario:
<RUN_DIR>/parent/<scenario>/edgefront-proxy.json (the parent commit) and
<RUN_DIR>/this/<scenario>/edgefront-proxy.json (the commit under test). The gate takes the median p99
of each side over the repetitions and fails when the commit's is more than
LIMIT (0.05 is 5%) above the parent's. Every request of every run must have
been answered, or the comparison means nothing and the gate fails too.
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


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    limit = float(sys.argv[1])
    runs = sys.argv[2:]
    base = [load(d, "parent") for d in runs]
    head = [load(d, "this") for d in runs]
    mb, mh = statistics.median(base), statistics.median(head)
    change = mh / mb - 1
    print("| | parent | this commit |")
    print("|---|---|---|")
    for i, (b, h) in enumerate(zip(base, head), 1):
        print(f"| run {i} p99 | {b:.3f} ms | {h:.3f} ms |")
    print(f"| median p99 | {mb:.3f} ms | {mh:.3f} ms |")
    print()
    verdict = "fail" if change > limit else "pass"
    print(f"p99 change {change:+.1%} against a limit of +{limit:.0%}: **{verdict}**")
    sys.exit(1 if verdict == "fail" else 0)


if __name__ == "__main__":
    main()
