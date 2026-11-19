#!/usr/bin/env python3
"""The Go-vs-Rust tables of docs/benchmarks/steer-go-vs-rust.md, from the
result JSON that examples/bench-steer.sh writes.

Usage: scripts/steer-bench-summary.py RESULTS_DIR...

Each RESULTS_DIR is one repetition. Prints Markdown: one row per
constant-rate scenario with each side's throughput, p50, p99, peak RSS and
CPU seconds; then, for each ramp, the highest step each side held with
every request answered and p99 under 10 ms; then binary and image sizes.
Every number is the median over the repetitions, and the p99 columns also
give the range.
"""
import json
import statistics
import sys
from pathlib import Path

PAIRS = [("steer", "edgefront-proxy"), ("steer-tls", "edgefront-proxy-tls")]
RAMP_P99_MS = 10.0


def mib(b):
    return f"{b / 2**20:.1f} MiB"


def load(path):
    return json.loads(path.read_text())


def med(xs):
    return statistics.median(xs)


def main():
    dirs = [Path(d) for d in sys.argv[1:]]
    if not dirs:
        sys.exit(__doc__)
    runs = {}
    for root in dirs:
        for f in sorted(root.glob("*/*.json")):
            r = load(f)
            runs.setdefault((r["scenario"]["name"], r["target"]["name"]), []).append(r)
    reps = len(dirs)
    for key, rs in runs.items():
        if len(rs) != reps:
            sys.exit(f"{key}: {len(rs)} results for {reps} repetitions")

    def num(key, get):
        return med([get(r) for r in runs[key]])

    def p99(key):
        xs = sorted(r["measured"]["latency"]["p99_ms"] for r in runs[key])
        return f"{med(xs):.3f} ms ({xs[0]:.3f} to {xs[-1]:.3f})"

    print(f"Medians of {reps} repetitions; p99 ranges in brackets.")
    print()
    print("| Scenario | Conns | Offered | Throughput steer / edgefront "
          "| p50 steer / edgefront | p99 steer | p99 edgefront "
          "| Peak RSS steer / edgefront | CPU s steer / edgefront |")
    print("|---|---|---|---|---|---|---|---|---|")
    names = sorted({s for s, _ in runs}, key=order)
    for s in names:
        if s.endswith("-ramp"):
            continue
        for go, rust in PAIRS:
            g, r = (s, go), (s, rust)
            if g not in runs:
                continue
            first = runs[g][0]["scenario"]
            thr = lambda k: num(k, lambda x: x["measured"]["throughput_rps"])
            p50 = lambda k: num(k, lambda x: x["measured"]["latency"]["p50_ms"])
            rss = lambda k: num(k, lambda x: x["resources"]["peak_rss_bytes"])
            cpu = lambda k: num(k, lambda x: x["resources"]["cpu_seconds"])
            print(
                f"| {s} | {first['connections']:,} | {first['rate']:,.0f}/s "
                f"| {thr(g):,.0f} / {thr(r):,.0f} "
                f"| {p50(g):.3f} / {p50(r):.3f} ms | {p99(g)} | {p99(r)} "
                f"| {mib(rss(g))} / {mib(rss(r))} | {cpu(g):.1f} / {cpu(r):.1f} |"
            )
    print()
    print("| Ramp | steer: highest step held | edgefront: highest step held |")
    print("|---|---|---|")
    for s in names:
        if not s.endswith("-ramp"):
            continue
        for go, rust in PAIRS:
            if (s, go) not in runs:
                continue
            cells = []
            for t in (go, rust):
                best = []
                for r in runs[(s, t)]:
                    held = [p["rate"] for p in r["phases"]
                            if not p["warmup"] and p["errors"] == 0
                            and p["completed"] == p["scheduled"]
                            and p["latency"]["p99_ms"] < RAMP_P99_MS]
                    best.append(max(held, default=0))
                cells.append(f"{med(best):,.0f}/s (runs: {', '.join(f'{b:,.0f}' for b in best)})")
            print(f"| {s} | {cells[0]} | {cells[1]} |")
    print()
    print("| Target | Binary | Image |")
    print("|---|---|---|")
    seen = set()
    for (s, t) in sorted(runs, key=lambda k: k[1]):
        if t in seen:
            continue
        seen.add(t)
        r = runs[(s, t)][0]
        print(f"| {t} | {mib(r['extra']['binary_bytes'])} | {mib(r['target']['image_size_bytes'])} |")


def order(name):
    parts = name.split("-")
    conns = int(parts[3]) if len(parts) > 3 and parts[3].isdigit() else 0
    return ("tls" in parts, conns, name)


if __name__ == "__main__":
    main()
