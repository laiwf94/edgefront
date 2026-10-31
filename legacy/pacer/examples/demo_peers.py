"""Upstream and clients for examples/demo.sh. Standard library only.

  upstream PORT            echo server; a first line "SEND n" makes it send
                           n bytes of a fixed pseudo-random stream instead
  burst PORT N             open N connections at once, report how many work
  slowloris PORT SECS      send one byte every 300 ms, report when cut off
  steady PORT SECS         send 256 bytes every 250 ms, report if cut off
  download PORT N FILE     fetch N bytes, write their SHA-256 to FILE
  expected N               print the SHA-256 the upstream's N bytes must have
"""
import hashlib
import random
import socket
import sys
import threading
import time


def stream(n):
    rng = random.Random(7)
    left = n
    while left:
        chunk = rng.randbytes(min(left, 65536))
        left -= len(chunk)
        yield chunk


def serve(conn):
    with conn:
        first = b""
        while not first.endswith(b"\n"):
            b = conn.recv(1)
            if not b:
                return
            first += b
        if first.startswith(b"SEND "):
            for chunk in stream(int(first.split()[1])):
                conn.sendall(chunk)
            return
        conn.sendall(first)
        while True:
            data = conn.recv(65536)
            if not data:
                return
            conn.sendall(data)


def upstream(port):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    s.listen(512)
    while True:
        conn, _ = s.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()


def works(port):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=2) as c:
            c.sendall(b"ping\n")
            return c.recv(5) == b"ping\n"
    except OSError:
        return False


def burst(port, n):
    results = [False] * n
    def one(i):
        results[i] = works(port)
    threads = [threading.Thread(target=one, args=(i,)) for i in range(n)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    print(sum(results))


def trickle(port, secs, chunk, interval):
    c = socket.create_connection(("127.0.0.1", port))
    c.sendall(b"hello\n")
    c.recv(6)
    start = time.monotonic()
    c.settimeout(interval)
    while time.monotonic() - start < secs:
        try:
            c.sendall(b"x" * chunk)
            got = c.recv(65536)
            if not got:
                return time.monotonic() - start
        except socket.timeout:
            pass
        except OSError:
            return time.monotonic() - start
        time.sleep(interval)
    return None


def main():
    cmd, args = sys.argv[1], sys.argv[2:]
    if cmd == "upstream":
        upstream(int(args[0]))
    elif cmd == "burst":
        burst(int(args[0]), int(args[1]))
    elif cmd == "slowloris":
        cut = trickle(int(args[0]), float(args[1]), 1, 0.3)
        print("survived" if cut is None else f"cut after {cut:.1f}s")
    elif cmd == "steady":
        cut = trickle(int(args[0]), float(args[1]), 256, 0.25)
        print("survived" if cut is None else f"cut after {cut:.1f}s")
    elif cmd == "download":
        port, n, out = int(args[0]), int(args[1]), args[2]
        h, got = hashlib.sha256(), 0
        with socket.create_connection(("127.0.0.1", port)) as c:
            c.sendall(b"SEND %d\n" % n)
            while True:
                data = c.recv(65536)
                if not data:
                    break
                h.update(data)
                got += len(data)
        with open(out, "w") as f:
            f.write(f"{got} {h.hexdigest()}\n")
    elif cmd == "expected":
        n = int(args[0])
        h = hashlib.sha256()
        for chunk in stream(n):
            h.update(chunk)
        print(f"{n} {h.hexdigest()}")


if __name__ == "__main__":
    main()
