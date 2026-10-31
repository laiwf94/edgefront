#!/bin/sh
# Run a command inside the toolchain image, building it first if the
# Dockerfile changed. Example: scripts/dev.sh make check
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
tag="pacer-dev:$(cksum < "$root/docker/dev.Dockerfile" | cut -d' ' -f1)"
if ! docker image inspect "$tag" >/dev/null 2>&1; then
    docker build -q -t "$tag" -f "$root/docker/dev.Dockerfile" "$root/docker" >/dev/null
fi
tty=""
[ -t 0 ] && [ -t 1 ] && tty="-it"
# shellcheck disable=SC2086
exec docker run --rm $tty --init --cap-add SYS_PTRACE \
    -v "$root:/src" -w /src "$tag" "$@"
