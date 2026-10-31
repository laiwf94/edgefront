# pacer

pacer is a rate limiter written in C17 with no dependencies beyond libc. libpacer is the library: a GCRA token bucket whose whole state is one word updated with a lock-free compare-and-swap, a sharded per-key table with fixed memory and idle eviction, and a sliding-window counter. pacerd is a TCP proxy built on it that limits each client address and each listener by connections, new connections per second and bytes per second, cuts off slowloris clients, and drains without losing bytes in flight.

The design, its trade-offs and the sources it draws on are in [docs/DESIGN.md](docs/DESIGN.md).

## Quick start

Everything builds and runs on Linux. On macOS, `scripts/dev.sh` runs any command inside the pinned toolchain image (Docker); the library and its unit tests also build natively with `make test`.

```sh
scripts/dev.sh make check          # format, lint, -Werror build, tests under ASan+UBSan and TSan, coverage, fuzzing
scripts/dev.sh examples/demo.sh    # the live demo below
scripts/dev.sh make bench          # limiter and proxy benchmarks
```

On Linux, `make` builds `build/release/pacerd` and `build/release/pacerctl`:

```sh
make
build/release/pacerd -t -c examples/pacerd.conf     # check a config
build/release/pacerd -c examples/pacerd.conf
build/release/pacerctl -s /run/pacerd/admin.sock stats
kill -HUP $(pidof pacerd)                            # reload limits
kill -TERM $(pidof pacerd)                           # drain and exit
```

The Docker image is under 1 MB: static binaries on `scratch`, running as uid 65532.

```sh
docker build -t pacerd .
docker run -v $PWD/my.conf:/etc/pacerd/pacerd.conf:ro -p 8080:8080 pacerd
scripts/docker-smoke.sh            # builds the image and checks it end to end
```

## Example

A listener that allows each client address 20 open connections, 10 new ones per second and 1 MiB/s, and closes connections that move fewer than 512 bytes in 10 seconds:

```
admin_socket /run/pacerd/admin.sock

listener web {
    listen 0.0.0.0:8080
    upstream 10.0.0.5:9000
    max_conns 10000
    per_ip_max_conns 20
    per_ip_conn_rate 10/s burst 20
    per_ip_bandwidth 1m/s burst 256k
    bandwidth 100m/s burst 1m
    min_rate 512 per 10s
    idle_timeout 5m
}
```

`examples/pacerd.conf` lists every directive. After a client opened eight connections at once against a limit of five per second, then downloaded 1 MiB at 1 MiB/s:

```
$ pacerctl -s /tmp/s.sock stats
pacerd version=0.1.0 uptime_s=2 workers=2 draining=0
listener=web open=0 accepted=6 rejected_listener_full=0 rejected_per_ip_conns=0 rejected_per_ip_rate=3 rejected_table_full=0 rejected_fd_exhausted=0 closed_done=6 closed_idle=0 closed_slow=0 closed_upstream_failed=0 closed_reset=0 closed_drain=0 closed_other=0 bytes_up=38 bytes_down=1048601 throttled=162 clients_tracked=1 client_table_capacity=16384 client_table_evictions=0
worker=0 accepted=4
worker=1 accepted=2
```

Logs are one `key=value` line per event, with the connection's ID and the reason it closed:

```
ts=...Z level=info msg="conn rejected" listener=demo peer=127.0.0.1:49502 reason=per_ip_rate
ts=...Z level=info msg="conn closed" conn=13 listener=demo peer=127.0.0.1:49536 reason=done bytes_up=14 bytes_down=12582912 ms=2941
ts=...Z level=info msg=drained
```

The library is used directly like this:

```c
pacer_gcra g;
pacer_rate r = {100, 1000000000, 20};      /* 100 per second, burst 20 */
pacer_gcra_init(&g, &r);

pacer_decision d;
if (pacer_gcra_take(&g, pacer_now_ns(), 1, &d) != 1) {
    /* denied: d.retry_after_ns says exactly when this request would fit */
}
```

## Live demo

`examples/demo.sh` runs the release pacerd against a small Python upstream and checks each outcome; it exits non-zero if any check fails. Its output:

```
pacerd 0.1.0 listening on 127.0.0.1:19001
limits: 5 new connections/s per address (burst 5), 4 MiB/s per address,
        at least 64 bytes per 2 s per connection

1. A client opens 12 connections at once
  ok    connections admitted: 5
  ok    refused for connection rate: 7
   waiting one second for the bucket to refill
  ok    connections admitted after recovery: 5

2. A slowloris client and a steady client, side by side
   slowloris: cut after 2.1s
  ok    slowloris cut off: cut
  ok    steady client: survived
  ok    closed as slow clients: 1

3. Graceful drain with a download in flight
   sending SIGTERM to pacerd one second into a ~3 s download
  ok    new connections refused while draining: yes
  ok    download complete and intact: 12582912 9516ea24820809b1c67d2ba6029a806e3850c303c0535bdb2461d0639ef8e517
  ok    pacerd exit code: 0
  ok    drained without forcing connections closed: 0

demo: all checks passed
```

## Behaviour under failure

A client that exceeds a limit is refused at accept, or has its reads paused until its bucket refills; pausing reads pushes back through TCP flow control, so the proxy never buffers more than one fixed buffer per direction per connection. An upstream that refuses, cannot be reached, times out on connect, resets or goes silent closes the client connection with the reason in the log and the counters. A client that trickles bytes to hold a slot is cut off by the minimum-rate check; one that our own shaping slowed down is not.

When the process runs out of file descriptors, pending connections are accepted and closed at once instead of hanging, and the event loop stays idle. When the client table is full of active clients, new addresses are refused and existing clients keep their limits; an attacker cycling through addresses cannot reset anyone else's. SIGTERM stops accepting, refuses new connections, and lets open ones finish until `drain_timeout`. SIGHUP applies new limits in place and refuses changes that need a restart, keeping the running config. After a SIGKILL, a restart binds the same port at once and replaces the stale admin socket. All of these have tests that run against the real binary.

## Benchmarks

Medians of four runs, inside the Docker Desktop Linux VM (arm64, 16 vCPUs) on an Apple M4 Max laptop. The VM shares the machine, so single runs move by up to a third; the shape of the results is stable.

Decisions per second, in millions (`bench/bench_limiter.c`):

| | 1 thread | 2 threads | 4 threads | 8 threads |
|---|---:|---:|---:|---:|
| one shared GCRA, lock-free | 313 | 114 | 195 | 106 |
| one shared GCRA, mutex baseline | 185 | 54 | 50 | 36 |
| keyed table, 10k keys per thread | 19 | 18 | 21 | 27 |
| keyed table, one hot key | 66 | 24 | 19 | 15 |

One shared limiter is the worst case for any design, since every decision writes the same cache line; the lock-free version is 1.7 to 4 times faster than the mutex baseline at every thread count. The keyed table pays for a hash and a shard lock per decision, and its multi-threaded rate is bound by moving lock cache lines between cores. In pacerd one decision covers a read of up to 16 KiB, so even the slowest row is far above what the proxy needs.

Proxy cost, direct to the upstream against through pacerd with no limits configured (`bench/bench_proxy.c`):

| | direct | via pacerd |
|---|---:|---:|
| bulk download, one connection | 14.2 GiB/s | 6.1 GiB/s |
| 64-byte round trip, p50 | 40 us | 94 us |
| 64-byte round trip, p99 | 110 us | 400 us |
| 64-byte round trips per second, 32 connections | 266k | 61k |

Each proxied round trip crosses two more TCP connections and wakes a pacerd worker twice. pacerd spends about 34 us of CPU per round trip here, most of it in the loopback TCP path and in waking threads inside the VM.

## Limits

Limits are per process; several instances do not share state. Addresses in the config are numeric, as pacerd does no DNS. Listen addresses, table sizes, the worker count and the admin socket path need a restart to change. Bytes are copied through user space rather than spliced. The kernel spreads connections over workers by hash, without regard to how busy each worker is. Rates above 6.4*10^10 per second are capped at one token per 1/64 ns, and the limiter's clock stops after about 4.5 years of process uptime. pacerd has no TLS or HTTP awareness: it sees bytes on TCP connections.

## Layout

```
include/pacer.h        the library API, with ownership and threading rules
src/lib/               GCRA, keyed table, SipHash, sliding window, clock
src/pacerd/            config parser, event loop, proxy, admin socket, logs
src/pacerctl/          admin client
tests/                 unit tests, and integration tests against the real binary
fuzz/                  libFuzzer targets and seed corpora
bench/                 limiter and proxy benchmarks
examples/              example config and the live demo
scripts/               dev container, coverage, audit, Docker smoke test, per-commit check
```

## License

MIT. See [LICENSE](LICENSE).
