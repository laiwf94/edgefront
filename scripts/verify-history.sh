#!/usr/bin/env bash
# Checks out every commit on the current branch in a scratch worktree and
# runs that commit's own gate (`make check`), which holds every crate to the
# line-coverage floor. Prints one line per commit with its lowest coverage
# and exits non-zero if any commit fails.
#
# Usage: scripts/verify-history.sh [first-commit]
set -uo pipefail

cd "$(git rev-parse --show-toplevel)"
SCRATCH=$(mktemp -d)
# One target directory for every commit, so dependencies build once.
export CARGO_TARGET_DIR="$SCRATCH/target"
trap 'git worktree remove --force "$SCRATCH/tree" >/dev/null 2>&1; rm -rf "$SCRATCH"' EXIT

# The lowest line coverage the commit's gate printed. From the per-crate
# floor on, scripts/coverage.sh prints `coverage <crate> lines <percent>`
# for every crate and this is the lowest crate, with its name; earlier
# commits printed one workspace TOTAL line.
coverage() {
    local lowest
    lowest=$(awk '$1 == "coverage" { print $4, $2 }' "$SCRATCH/log" | sort -n | head -1)
    if [[ -n $lowest ]]; then
        echo "${lowest% *} (${lowest#* })"
    else
        grep '^TOTAL' "$SCRATCH/log" | tail -1 | awk '{print $10}'
    fi
}

range=HEAD
[[ $# -gt 0 ]] && range="$1^..HEAD"
failed=0
total=0
for sha in $(git rev-list --reverse "$range"); do
    total=$((total + 1))
    git worktree add --detach --force "$SCRATCH/tree" "$sha" >/dev/null 2>&1
    subject=$(git log -1 --format=%s "$sha")
    if (cd "$SCRATCH/tree" && make check >"$SCRATCH/log" 2>&1); then
        printf 'pass  %s  lines %-7s  %s\n' "${sha:0:7}" "$(coverage)" "$subject"
    else
        failed=$((failed + 1))
        printf 'FAIL  %s  %s\n' "${sha:0:7}" "$subject"
        # The crates under the floor, then the end of the log.
        grep '^coverage .* fail$' "$SCRATCH/log" | sed 's/^/      /'
        tail -20 "$SCRATCH/log" | sed 's/^/      /'
    fi
    git worktree remove --force "$SCRATCH/tree" >/dev/null 2>&1
done
echo "$((total - failed)) of $total commits pass their gate"
[[ $failed -eq 0 ]]
