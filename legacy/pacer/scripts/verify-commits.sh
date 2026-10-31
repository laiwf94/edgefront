#!/bin/sh
# Check out every commit in a scratch worktree and run the gate there, so
# each commit is known to pass on its own. Usage: verify-commits.sh [REV]
set -eu
root=$(git rev-parse --show-toplevel)
cd "$root"
scratch=$(mktemp -d)
cleanup() {
    for wt in "$scratch"/wt-*; do
        [ -d "$wt" ] && git worktree remove --force "$wt" >/dev/null 2>&1
    done
    git worktree prune
    rm -rf "$scratch"
}
trap cleanup EXIT INT TERM

pass=0
fail=0
for sha in $(git rev-list --reverse "${1:-HEAD}"); do
    short=$(git rev-parse --short "$sha")
    subject=$(git log -1 --format=%s "$sha")
    wt="$scratch/wt-$short"
    git worktree add -q --detach "$wt" "$sha"
    if (cd "$wt" && scripts/dev.sh make check >"$scratch/$short.log" 2>&1); then
        echo "pass $short $subject"
        pass=$((pass + 1))
    else
        echo "FAIL $short $subject"
        tail -n 30 "$scratch/$short.log" | sed 's/^/    /'
        fail=$((fail + 1))
    fi
    git worktree remove --force "$wt"
done
echo "verify-commits: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
