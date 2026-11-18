#!/usr/bin/env bash
# Checks a failover drill transcript, from examples/failover.sh or
# legacy/steer/examples/failover.sh: with the resilience features on, every
# phase must succeed 100.00%, as in steer's published table; with them off,
# the baseline must succeed 100.00% and each later phase must not, which
# shows the fault reached the upstreams.
#
#   scripts/check-failover.sh TRANSCRIPT
set -euo pipefail
[ $# -eq 1 ] || { echo "usage: $0 TRANSCRIPT" >&2; exit 2; }
awk '
  /^### Resilience on/  { section = "on";  next }
  /^### Resilience off/ { section = "off"; next }
  section != "" && /^\| [1-4]\. / {
    split($0, col, "|")
    phase = col[2]; gsub(/^ +| +$/, "", phase)
    success = col[4]; gsub(/^ +| +$/, "", success)
    n = substr(phase, 1, 1)
    rows[section]++
    want = (section == "on" || n == 1) ? "100.00%" : "below 100.00%"
    ok = (want == "100.00%") ? (success == "100.00%") : (success != "100.00%")
    printf "%-4s %-28s %8s  want %-14s %s\n", section, phase, success, want, ok ? "ok" : "FAIL"
    if (!ok) bad++
  }
  END {
    if (rows["on"] != 4 || rows["off"] != 4) {
      printf "expected 4 phases with the features on and 4 off, found %d and %d\n", rows["on"], rows["off"]
      exit 1
    }
    exit bad > 0
  }
' "$1"
