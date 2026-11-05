#!/usr/bin/env bash
# Holds the workspace to its direct dependency budget. Lists every crate the
# workspace members take from a registry, fails if one is not pinned to an
# exact version (`=x.y.z`) or if two members pin it differently (take it from
# [workspace.dependencies] instead), and fails if the runtime (normal and build)
# dependencies, counted once each across all members, exceed the budget.
# Development dependencies are listed and must be exact pins, but they do not
# count, because they never reach the binary.
#
# Usage: scripts/direct-deps.sh [budget]     (default 14)
# Needs cargo and jq.
set -euo pipefail

budget=${1:-14}
cd "$(git rev-parse --show-toplevel)"

deps=$(cargo metadata --format-version 1 --no-deps --locked | jq -r '
    .packages[] | .name as $crate | .dependencies[]
    | select(.source != null)
    | [(if .kind == "dev" then "dev" else "runtime" end), .name, .req, $crate]
    | @tsv')

status=0
loose=$(awk -F'\t' '$3 !~ /^=[0-9]+\.[0-9]+\.[0-9]+$/ { printf "  %s %s in %s\n", $2, $3, $4 }' <<<"$deps")
if [[ -n $loose ]]; then
    echo "not pinned to an exact version:"
    echo "$loose"
    status=1
fi

drift=$(awk -F'\t' '{ pins[$2] = pins[$2] " " $3 " in " $4 ","; seen[$2 "\t" $3] = 1 }
    END {
        for (key in seen) { split(key, f, "\t"); count[f[1]]++ }
        for (name in count) if (count[name] > 1) printf "  %s:%s\n", name, pins[name]
    }' <<<"$deps" | sort)
if [[ -n $drift ]]; then
    echo "pinned to more than one version:"
    echo "$drift"
    status=1
fi

for kind in runtime dev; do
    names=$(awk -F'\t' -v k="$kind" '$1 == k { print $2 }' <<<"$deps" | sort -u)
    count=$(grep -c . <<<"$names" || true)
    printf '%s dependencies (%d): %s\n' "$kind" "$count" "$(tr '\n' ' ' <<<"$names")"
    if [[ $kind == runtime && $count -gt $budget ]]; then
        echo "the workspace has $count direct runtime dependencies; the budget is $budget"
        status=1
    fi
done
exit $status
