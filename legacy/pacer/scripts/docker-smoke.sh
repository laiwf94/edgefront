#!/bin/sh
# Build the runtime image and check it end to end: it proxies a connection,
# runs as uid 65532, answers on the admin socket, and drains on docker stop.
# Everything it creates is removed on exit.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
image=${PACERD_IMAGE:-pacerd:smoke}
helper=alpine:3.20
net=pacer-smoke-$$
subnet=172.31.250
tmp=$(mktemp -d)

cleanup() {
    docker rm -f -v "$net-up" "$net-pacerd" >/dev/null 2>&1 || true
    docker network rm "$net" >/dev/null 2>&1 || true
    rm -rf "$tmp"
}
trap cleanup EXIT INT TERM

fail() {
    echo "docker-smoke: FAIL: $*" >&2
    docker logs "$net-pacerd" >&2 2>&1 || true
    exit 1
}

docker build -q -t "$image" "$root" >/dev/null
docker network create --subnet "$subnet.0/24" "$net" >/dev/null

docker run -d --name "$net-up" --network "$net" --ip "$subnet.10" "$helper" \
    nc -lk -p 9000 -e /bin/cat >/dev/null

cat > "$tmp/pacerd.conf" <<CONF
admin_socket /run/pacerd/admin.sock
listener smoke {
    listen 0.0.0.0:8080
    upstream $subnet.10:9000
    per_ip_max_conns 4
}
CONF
chmod 644 "$tmp/pacerd.conf"
docker run -d --name "$net-pacerd" --network "$net" --ip "$subnet.20" \
    -v "$tmp/pacerd.conf:/etc/pacerd/pacerd.conf:ro" "$image" >/dev/null

for _ in $(seq 50); do
    docker logs "$net-pacerd" 2>&1 | grep -q 'msg=ready' && break
    sleep 0.1
done
docker logs "$net-pacerd" 2>&1 | grep -q 'msg=ready' || fail "pacerd did not start"

reply=$(docker run --rm --network "$net" "$helper" sh -c \
    "echo hello-through-pacerd | nc -w 2 $subnet.20 8080")
[ "$reply" = "hello-through-pacerd" ] || fail "unexpected reply: $reply"
echo "docker-smoke: proxied a connection"

uid=$(docker top "$net-pacerd" -o pid,uid | awk 'NR == 2 { print $2 }')
[ "$uid" = "65532" ] || fail "pacerd runs as uid $uid, not 65532"
echo "docker-smoke: pacerd runs as uid 65532"

stats=$(docker exec "$net-pacerd" /usr/local/bin/pacerctl -s /run/pacerd/admin.sock stats)
echo "$stats" | grep -q 'listener=smoke open=0 accepted=1 ' || fail "unexpected stats: $stats"
echo "docker-smoke: admin socket answers"

docker stop -t 10 "$net-pacerd" >/dev/null
code=$(docker inspect -f '{{.State.ExitCode}}' "$net-pacerd")
[ "$code" = "0" ] || fail "exit code $code after docker stop"
docker logs "$net-pacerd" 2>&1 | grep -q 'msg=drained' || fail "no drain on docker stop"
echo "docker-smoke: drained on docker stop, exit 0"
echo "docker-smoke: ok"
