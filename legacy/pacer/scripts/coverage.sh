#!/bin/sh
# Merge raw profiles from a cov build and fail if line coverage is below the
# floor. Usage: coverage.sh <build-dir> <min-percent> <binary>...
set -eu
dir=$1 min=$2
shift 2
llvm-profdata merge -sparse "$dir"/prof/*.profraw -o "$dir/merged.profdata"
first=$1
shift
objs=""
for b in "$@"; do objs="$objs -object $b"; done
ignore='(tests|bench|fuzz)/'
# shellcheck disable=SC2086
llvm-cov report "$first" $objs -instr-profile="$dir/merged.profdata" \
    -ignore-filename-regex="$ignore" | tee "$dir/coverage.txt"
# shellcheck disable=SC2086
pct=$(llvm-cov export "$first" $objs -instr-profile="$dir/merged.profdata" \
    -ignore-filename-regex="$ignore" -summary-only |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["data"][0]["totals"]["lines"]["percent"])')
echo "line coverage: $pct% (floor $min%)"
python3 -c "import sys; sys.exit(0 if float('$pct') >= float('$min') else 1)" ||
    { echo "coverage below floor" >&2; exit 1; }
