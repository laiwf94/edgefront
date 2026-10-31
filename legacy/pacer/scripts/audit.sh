#!/bin/sh
# Dependency audit. C has no package manager to ask, so this checks what
# actually ends up in the binaries and the image:
#   - the release binaries link nothing but the C library;
#   - the sources include only C library, POSIX and Linux headers, so no
#     third-party code is compiled in;
#   - every base image is pinned by digest and the runtime image has no OS
#     packages at all (it is built FROM scratch);
#   - every GitHub Action is pinned to a commit.
set -eu
cd "$(dirname "$0")/.."
make -s VARIANT=release build/release/pacerd build/release/pacerctl >/dev/null
fail=0

for bin in build/release/pacerd build/release/pacerctl; do
    needed=$(llvm-readelf-18 -d "$bin" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | sort -u)
    for lib in $needed; do
        case $lib in
        libc.so.6 | ld-linux-*.so.* ) ;;
        *) echo "audit: $bin links $lib"; fail=1 ;;
        esac
    done
    echo "audit: $bin links: $(echo $needed)"
done

allowed='arpa/inet.h errno.h fcntl.h inttypes.h netinet/in.h netinet/tcp.h poll.h pthread.h
signal.h stdarg.h stdatomic.h stdbool.h stddef.h stdint.h stdio.h stdlib.h string.h
sys/epoll.h sys/eventfd.h sys/random.h sys/signalfd.h sys/socket.h sys/stat.h sys/un.h
time.h unistd.h'
for h in $(grep -rhoE '#include <[^>]+>' src include | sed 's/#include <\(.*\)>/\1/' | sort -u); do
    if ! echo "$allowed" | tr ' ' '\n' | grep -qx "$h"; then
        echo "audit: unexpected header <$h>"
        fail=1
    fi
done
echo "audit: sources include only C library, POSIX and Linux headers"

for f in Dockerfile docker/dev.Dockerfile; do
    grep -E '^FROM ' "$f" | grep -vE '@sha256:[0-9a-f]{64}|^FROM scratch' && {
        echo "audit: $f has a base image not pinned by digest"
        fail=1
    }
done
last=$(grep -E '^FROM ' Dockerfile | tail -n 1)
[ "$last" = "FROM scratch" ] || { echo "audit: runtime image is not FROM scratch"; fail=1; }
echo "audit: base images pinned by digest; runtime image is FROM scratch"

if grep -E 'uses: ' .github/workflows/*.yml | grep -vE '@[0-9a-f]{40}( |$)'; then
    echo "audit: actions not pinned to a commit"
    fail=1
fi
echo "audit: actions pinned to commits"

[ "$fail" -eq 0 ] && echo "audit: ok"
exit "$fail"
