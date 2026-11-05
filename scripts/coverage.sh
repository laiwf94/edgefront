#!/usr/bin/env bash
# Runs the workspace tests once under cargo-llvm-cov, then holds every crate
# to the line-coverage floor on its own sources, and the workspace as a whole
# too. A well-tested crate cannot carry a poorly tested one past the floor.
#
# Prints one `coverage <crate> lines <percent>` line per crate and one for
# the workspace, then the report of each crate that is under the floor.
# Exits non-zero if any of them is.
#
# Usage: scripts/coverage.sh [floor]     (default 90)
# Needs cargo-llvm-cov and llvm-tools-preview.
set -euo pipefail

floor=${1:-90}
root=$(git rev-parse --show-toplevel)
cd "$root"
log=$(mktemp -d)
trap 'rm -rf "$log"' EXIT

# Every crate that has sources, as "name directory". The root package is
# cachet until the W01 cache tasks retire it; the rest live under crates/.
crates=()
[[ -d src ]] && crates+=("cachet src")
for dir in crates/*/; do
    dir=${dir%/}
    [[ -d $dir/src ]] && crates+=("${dir#crates/} $dir/src")
done

cargo llvm-cov clean --workspace
cargo llvm-cov --workspace --no-report

# The source roots as one alternation, for the regex that leaves every crate
# but one out of its report. cargo-llvm-cov's own default regex, which drops
# tests/, benches/ and dependencies, still applies. Every report needs
# --workspace: without it cargo-llvm-cov reports on the root package alone.
quote() { sed 's/[][\.^$*+?(){}|]/\\&/g' <<<"$1"; }
report() { # name, extra report arguments...
    local name=$1
    shift
    local out=$log/$name
    if cargo llvm-cov report --workspace --summary-only --fail-under-lines "$floor" "$@" >"$out" 2>&1; then
        echo pass >"$out.status"
    else
        echo fail >"$out.status"
    fi
    local lines
    lines=$(awk '$1 == "TOTAL" { print $10 }' "$out")
    # A report with no lines in it means the regex left the crate out.
    [[ $lines =~ ^[0-9.]+%$ ]] || echo fail >"$out.status"
    printf 'coverage %-20s lines %-8s %s\n' "$name" "${lines:-none}" "$(cat "$out.status")"
}

for entry in "${crates[@]}"; do
    read -r name dir <<<"$entry"
    others=()
    for other in "${crates[@]}"; do
        read -r _ odir <<<"$other"
        [[ $odir == "$dir" ]] || others+=("$(quote "$root/$odir")/")
    done
    regex="^($(IFS='|'; echo "${others[*]}"))"
    report "$name" --ignore-filename-regex "$regex"
done
report workspace

failed=0
# Every source file must land in exactly one crate's report: the crates'
# line counts add up to the workspace's.
count() { awk '$1 == "TOTAL" { print $8 }' "$log/$1"; }
sum=0
for entry in "${crates[@]}"; do
    read -r name _ <<<"$entry"
    sum=$((sum + $(count "$name")))
done
if [[ $sum -ne $(count workspace) ]]; then
    failed=1
    echo "the crates' reports cover $sum lines but the workspace has $(count workspace)"
fi
for status in "$log"/*.status; do
    name=$(basename "$status" .status)
    if [[ $(cat "$status") != pass ]]; then
        failed=1
        echo
        echo "$name is under the ${floor}% line floor:"
        cat "$log/$name"
    fi
done
exit $failed
