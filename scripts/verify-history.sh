#!/usr/bin/env bash
# Checks out every commit on the current branch in a scratch worktree and
# runs that commit's own gate (`make check`). Prints one line per commit
# and exits non-zero if any commit fails.
#
# Usage: scripts/verify-history.sh [first-commit]
set -uo pipefail

cd "$(git rev-parse --show-toplevel)"
SCRATCH=$(mktemp -d)
# One target directory for every commit, so dependencies build once.
export CARGO_TARGET_DIR="$SCRATCH/target"
trap 'git worktree remove --force "$SCRATCH/tree" >/dev/null 2>&1; rm -rf "$SCRATCH"' EXIT

range=HEAD
[[ $# -gt 0 ]] && range="$1^..HEAD"
failed=0
total=0
for sha in $(git rev-list --reverse "$range"); do
    total=$((total + 1))
    git worktree add --detach --force "$SCRATCH/tree" "$sha" >/dev/null 2>&1
    subject=$(git log -1 --format=%s "$sha")
    if (cd "$SCRATCH/tree" && make check >"$SCRATCH/log" 2>&1); then
        coverage=$(grep '^TOTAL' "$SCRATCH/log" | awk '{print $10}')
        printf 'pass  %s  lines %-7s  %s\n' "${sha:0:7}" "$coverage" "$subject"
    else
        failed=$((failed + 1))
        printf 'FAIL  %s  %s\n' "${sha:0:7}" "$subject"
        tail -20 "$SCRATCH/log" | sed 's/^/      /'
    fi
    git worktree remove --force "$SCRATCH/tree" >/dev/null 2>&1
done
echo "$((total - failed)) of $total commits pass their gate"
[[ $failed -eq 0 ]]
