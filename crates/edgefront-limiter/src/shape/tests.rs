use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::thread;

use super::*;
use crate::gcra::Rate;

const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn listener(edit: impl FnOnce(&mut ListenerConfig)) -> ListenerConfig {
    let a: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let b: SocketAddr = "127.0.0.1:9090".parse().unwrap();
    let mut l = ListenerConfig::new("web", a, b);
    edit(&mut l);
    l
}

fn bandwidth(edit: impl FnOnce(&mut ListenerConfig)) -> Arc<Bandwidth> {
    Arc::new(Bandwidth::new(&listener(edit)).unwrap())
}

fn key(s: &str) -> ClientKey {
    ClientKey::new(s.parse::<IpAddr>().unwrap(), 64)
}

fn shaper(bw: &Arc<Bandwidth>, s: &str) -> Shaper {
    Shaper::new(Arc::clone(bw), key(s))
}

/// 1 MiB/s with a 64 KiB burst, it_shaping.c's per-client rate.
const MIB_S: Rate = Rate::new(MIB, SEC, 64 * KIB);

/// The wait for one read quantum at 1 MiB/s: the interval is 10^9 * 64 /
/// 2^20 = 61,035.16 ticks of 1/64 ns, rounded up to 61,036, so 4096 bytes
/// take 4096 * 61,036 / 64 ns.
const QUANTUM_AT_MIB_S: u64 = 4096 * 61_036 / 64;

#[test]
fn the_quantum_wait_is_the_gcras_interval() {
    let (iv, _) = MIB_S.ticks().unwrap();
    assert_eq!(iv, 61_036);
    assert_eq!(QUANTUM_AT_MIB_S, 3_906_304);
}

#[test]
fn off_grants_everything_and_says_so() {
    let bw = bandwidth(|_| {});
    assert!(!bw.is_on());
    let mut s = shaper(&bw, "192.0.2.1");
    assert!(!s.is_on());
    for want in [1, 4096, 16 * KIB, u64::MAX] {
        assert_eq!(
            s.reserve(Dir::Up, want, SEC),
            Reserve {
                granted: want,
                retry_ns: 0
            }
        );
    }
    // Nothing is tracked: off never touches the table.
    assert_eq!(bw.client_stats().entries, 0);
    s.give_back(100, SEC);
    assert_eq!(bw.client_stats().entries, 0);
    assert!(format!("{bw:?}").contains("per_ip_bandwidth: false"));
    assert_eq!(bw.name(), "web");
}

#[test]
fn a_zero_reservation_is_granted_at_once() {
    let bw = bandwidth(|l| l.per_ip_bandwidth = Some(MIB_S));
    let mut s = shaper(&bw, "192.0.2.1");
    assert_eq!(s.reserve(Dir::Up, 0, SEC), Reserve::default());
    assert_eq!(bw.client_stats().entries, 0);
}

#[test]
fn per_ip_grants_the_burst_then_waits_for_a_quantum() {
    let bw = bandwidth(|l| l.per_ip_bandwidth = Some(MIB_S));
    let mut s = shaper(&bw, "192.0.2.1");
    assert!(s.is_on());
    // 16 KiB at a time, as a full buffer asks: four reads use the burst.
    for _ in 0..4 {
        assert_eq!(s.reserve(Dir::Up, 16 * KIB, SEC).granted, 16 * KIB);
    }
    let r = s.reserve(Dir::Up, 16 * KIB, SEC);
    assert_eq!(r.granted, 0);
    // A quantum of 4096 bytes at 1 MiB/s comes back in 4096/2^20 s,
    // 3,906,250 ns, rounded up through the interval in 1/64 ns ticks.
    assert_eq!(r.retry_ns, QUANTUM_AT_MIB_S);
    // One nanosecond early is still too early, by one nanosecond.
    assert_eq!(
        s.reserve(Dir::Up, 16 * KIB, SEC + r.retry_ns - 1).retry_ns,
        1
    );
    let r2 = s.reserve(Dir::Up, 16 * KIB, SEC + r.retry_ns);
    assert_eq!(r2.granted, 4096);
    assert_eq!(bw.client_stats().entries, 1);
}

#[test]
fn a_small_read_need_not_wait_for_a_whole_quantum() {
    let bw = bandwidth(|l| l.per_ip_bandwidth = Some(Rate::new(1000, SEC, 1000)));
    let mut s = shaper(&bw, "192.0.2.1");
    assert_eq!(s.reserve(Dir::Up, 1000, SEC).granted, 1000);
    // 100 bytes come back in 100 ms; a request of 100 needs only those.
    assert_eq!(s.reserve(Dir::Up, 100, SEC + 100 * MS).granted, 100);
    // A request of 4096 cannot be met by a burst of 1000: the minimum is
    // lowered to the burst, so it waits for the whole bucket.
    let r = s.reserve(Dir::Up, 4096, SEC + 100 * MS);
    assert_eq!(r.granted, 0);
    assert_eq!(r.retry_ns, SEC);
}

#[test]
fn clients_have_their_own_buckets() {
    let bw = bandwidth(|l| l.per_ip_bandwidth = Some(MIB_S));
    let mut a = shaper(&bw, "192.0.2.1");
    let mut b = shaper(&bw, "192.0.2.2");
    assert_eq!(a.reserve(Dir::Up, 64 * KIB, SEC).granted, 64 * KIB);
    assert_eq!(a.reserve(Dir::Up, 1, SEC).granted, 0);
    assert_eq!(b.reserve(Dir::Up, 64 * KIB, SEC).granted, 64 * KIB);
    // Two connections of one client share its bucket.
    let mut a2 = shaper(&bw, "192.0.2.1");
    assert_eq!(a2.reserve(Dir::Up, 1, SEC).granted, 0);
    assert_eq!(a.key(), a2.key());
}

#[test]
fn ipv6_clients_in_one_prefix_share_a_bucket() {
    let bw = bandwidth(|l| l.per_ip_bandwidth = Some(MIB_S));
    let k = |s: &str| ClientKey::new(s.parse::<IpAddr>().unwrap(), 64);
    let mut a = Shaper::new(Arc::clone(&bw), k("2001:db8:1:2::1"));
    let mut b = Shaper::new(Arc::clone(&bw), k("2001:db8:1:2:ffff::9"));
    assert_eq!(a.reserve(Dir::Up, 64 * KIB, SEC).granted, 64 * KIB);
    assert_eq!(b.reserve(Dir::Up, 1, SEC).granted, 0);
}

#[test]
fn the_listener_bucket_is_shared_by_every_client() {
    let bw = bandwidth(|l| l.bandwidth = Some(Rate::new(MIB, SEC, 32 * KIB)));
    let mut a = shaper(&bw, "192.0.2.1");
    let mut b = shaper(&bw, "192.0.2.2");
    assert_eq!(a.reserve(Dir::Up, 16 * KIB, SEC).granted, 16 * KIB);
    assert_eq!(b.reserve(Dir::Up, 16 * KIB, SEC).granted, 16 * KIB);
    assert_eq!(a.reserve(Dir::Up, 16 * KIB, SEC).granted, 0);
    assert_eq!(b.reserve(Dir::Up, 16 * KIB, SEC).granted, 0);
    assert_eq!(bw.listener_remaining(SEC), 0);
    // No client table is used for the listener's limit.
    assert_eq!(bw.client_stats().entries, 0);
}

#[test]
fn what_the_listener_holds_back_is_refunded_to_the_client() {
    let bw = bandwidth(|l| {
        l.per_ip_bandwidth = Some(Rate::new(MIB, SEC, 64 * KIB));
        l.bandwidth = Some(Rate::new(MIB, SEC, 16 * KIB));
    });
    let mut s = shaper(&bw, "192.0.2.1");
    // The client could have 64 KiB; the listener gives 16.
    assert_eq!(s.reserve(Dir::Up, 64 * KIB, SEC).granted, 16 * KIB);
    // So the client was charged 16 KiB, not 64: 48 KiB are left in its
    // bucket, which the listener releases as it refills.
    let r = s.reserve(Dir::Up, 64 * KIB, SEC);
    assert_eq!(r.granted, 0);
    // The listener's retry is the one that counts.
    assert_eq!(r.retry_ns, QUANTUM_AT_MIB_S);
    let st = bw.client_stats();
    assert_eq!(st.entries, 1);
    // The listener was short, so the connection booked its next quantum
    // there, and the client was not charged for it.
    assert_eq!(s.booked(Dir::Up), 4096);
    // When the booking is due the connection reads it without asking the
    // listener again, and only it: the client is charged 4096 more.
    assert_eq!(
        s.reserve(Dir::Up, 64 * KIB, SEC + QUANTUM_AT_MIB_S).granted,
        4096
    );
    assert_eq!(s.booked(Dir::Up), 0);
}

#[test]
fn a_short_listener_bucket_queues_its_waiters_by_booking() {
    // 1000 per second, burst 1000, read in quanta of 1000 here (the
    // request is smaller than the 4096-byte quantum).
    let bw = bandwidth(|l| l.bandwidth = Some(Rate::new(1000, SEC, 1000)));
    let mut a = shaper(&bw, "192.0.2.1");
    let mut b = shaper(&bw, "192.0.2.2");
    assert_eq!(a.reserve(Dir::Up, 1000, SEC).granted, 1000);
    // B finds the bucket empty and books the next 1000, due in 1 s.
    let r = b.reserve(Dir::Down, 1000, SEC);
    assert_eq!(
        r,
        Reserve {
            granted: 0,
            retry_ns: SEC
        }
    );
    assert_eq!(b.booked(Dir::Down), 1000);
    assert_eq!(b.booked(Dir::Up), 0);
    // A asks next and queues behind B: due in 2 s.
    assert_eq!(a.reserve(Dir::Up, 1000, SEC).retry_ns, 2 * SEC);
    // At 2 s B reads its booking; A, coming back, still waits for its own.
    assert_eq!(b.reserve(Dir::Down, 1000, 2 * SEC).granted, 1000);
    assert_eq!(a.reserve(Dir::Up, 1000, 3 * SEC).granted, 1000);
    // A booking larger than the read is used up over several reads.
    let mut c = shaper(&bw, "192.0.2.3");
    assert_eq!(c.reserve(Dir::Up, 1000, 3 * SEC).retry_ns, SEC);
    assert_eq!(c.reserve(Dir::Up, 400, 4 * SEC).granted, 400);
    assert_eq!(c.booked(Dir::Up), 600);
    assert_eq!(c.reserve(Dir::Up, 1000, 4 * SEC).granted, 600);
}

#[test]
fn a_booking_is_given_back_when_the_client_limit_holds_the_read() {
    let bw = bandwidth(|l| {
        l.per_ip_bandwidth = Some(Rate::new(100, SEC, 100));
        l.bandwidth = Some(Rate::new(100, SEC, 100));
    });
    let mut x = shaper(&bw, "192.0.2.9");
    assert_eq!(x.reserve(Dir::Up, 100, SEC).granted, 100);
    // Two connections of one client find the listener empty and book,
    // due at 1 s and 2 s from now.
    let mut s = shaper(&bw, "192.0.2.1");
    let mut s2 = shaper(&bw, "192.0.2.1");
    assert_eq!(s.reserve(Dir::Up, 100, SEC).retry_ns, SEC);
    assert_eq!(s2.reserve(Dir::Up, 100, SEC).retry_ns, 2 * SEC);
    // At 2 s the second reads its booking and spends the client's bucket.
    assert_eq!(s2.reserve(Dir::Up, 100, 3 * SEC).granted, 100);
    assert_eq!(bw.listener_remaining(3 * SEC), 0);
    // The first comes back now: its client's limit holds it, so its
    // booking goes back to the listener for everyone else.
    let r = s.reserve(Dir::Up, 100, 3 * SEC);
    assert_eq!(r.granted, 0);
    assert_eq!(r.retry_ns, SEC);
    assert_eq!(s.booked(Dir::Up), 0);
    assert_eq!(bw.listener_remaining(3 * SEC), 100);
}

#[test]
fn release_and_a_reload_drop_bookings() {
    let mut l = listener(|l| l.bandwidth = Some(Rate::new(1000, SEC, 1000)));
    let bw = Arc::new(Bandwidth::new(&l).unwrap());
    let mut a = shaper(&bw, "192.0.2.1");
    assert_eq!(a.reserve(Dir::Up, 1000, SEC).granted, 1000);
    let mut b = shaper(&bw, "192.0.2.2");
    assert_eq!(b.reserve(Dir::Up, 1000, SEC).granted, 0);
    assert_eq!(b.reserve(Dir::Down, 1000, SEC).granted, 0);
    assert_eq!((b.booked(Dir::Up), b.booked(Dir::Down)), (1000, 1000));
    // Closing gives both back: at 1 s the bucket is full again, as if the
    // bookings were never made.
    b.release(SEC);
    assert_eq!((b.booked(Dir::Up), b.booked(Dir::Down)), (0, 0));
    assert_eq!(bw.listener_remaining(2 * SEC), 1000);
    // A reload that turns the listener's limit off drops the booking.
    let mut c = shaper(&bw, "192.0.2.3");
    assert_eq!(c.reserve(Dir::Up, 1000, 2 * SEC).granted, 1000);
    assert_eq!(c.reserve(Dir::Up, 1000, 2 * SEC).granted, 0);
    assert_eq!(c.booked(Dir::Up), 1000);
    l.bandwidth = None;
    bw.reconfigure(&l).unwrap();
    assert_eq!(c.reserve(Dir::Up, 1000, 2 * SEC).granted, 1000);
    assert_eq!(c.booked(Dir::Up), 0);
    // Releasing with the limit off has nothing to give back.
    c.release(2 * SEC);
}

#[test]
fn an_untracked_client_gives_its_booking_back() {
    let bw = bandwidth(|l| {
        l.per_ip_bandwidth = Some(Rate::new(1_000_000, SEC, 1_000_000));
        l.bandwidth = Some(Rate::new(1_000_000, SEC, 1_000_000));
        l.table_size = 16;
        l.table_full = FullPolicy::Deny;
    });
    let mut a = shaper(&bw, "192.0.2.1");
    assert_eq!(a.reserve(Dir::Up, 1_000_000, SEC).granted, 1_000_000);
    // B finds the listener empty and books a quantum; its own bucket was
    // refunded, so its entry is idle.
    let mut late = shaper(&bw, "192.0.2.200");
    assert_eq!(late.reserve(Dir::Up, 16 * KIB, SEC).granted, 0);
    assert_eq!(late.booked(Dir::Up), 4096);
    // Later, clients in debt take every slot, reclaiming the idle ones.
    let t = 3 * SEC;
    let cap = bw.client_stats().capacity;
    let mut held = Vec::new();
    for i in 0..cap {
        let mut s = Shaper::new(
            Arc::clone(&bw),
            ClientKey::new(IpAddr::from([10, 1, 0, i as u8]), 64),
        );
        assert_eq!(s.reserve(Dir::Up, 1000, t).granted, 1000, "filler {i}");
        held.push(s);
    }
    let before = bw.listener_remaining(t);
    // B cannot be tracked now: it waits, and its booking goes back.
    assert_eq!(
        late.reserve(Dir::Up, 16 * KIB, t),
        Reserve {
            granted: 0,
            retry_ns: UNTRACKED_RETRY_NS
        }
    );
    assert_eq!(late.booked(Dir::Up), 0);
    assert_eq!(bw.listener_remaining(t), before + 4096);
}

#[test]
fn a_listener_denial_with_the_client_off_charges_nothing_else() {
    let bw = bandwidth(|l| l.bandwidth = Some(Rate::new(1000, SEC, 1000)));
    let mut s = shaper(&bw, "192.0.2.1");
    assert_eq!(s.reserve(Dir::Up, 1000, SEC).granted, 1000);
    let r = s.reserve(Dir::Up, 10, SEC);
    assert_eq!(r.granted, 0);
    // It booked those 10 bytes: due in 10 ms.
    assert_eq!(r.retry_ns, 10 * MS);
    assert_eq!(s.booked(Dir::Up), 10);
}

#[test]
fn give_back_returns_unread_bytes_to_both_buckets() {
    let bw = bandwidth(|l| {
        l.per_ip_bandwidth = Some(Rate::new(1000, SEC, 1000));
        l.bandwidth = Some(Rate::new(1000, SEC, 1000));
    });
    let mut s = shaper(&bw, "192.0.2.1");
    assert_eq!(s.reserve(Dir::Up, 1000, SEC).granted, 1000);
    assert_eq!(s.reserve(Dir::Up, 1, SEC).granted, 0);
    // The read returned 400 of the 1000: 600 go back to both.
    s.give_back(600, SEC);
    assert_eq!(bw.listener_remaining(SEC), 600);
    assert_eq!(s.reserve(Dir::Up, 600, SEC).granted, 600);
    assert_eq!(s.reserve(Dir::Up, 1, SEC).granted, 0);
    // Giving back nothing is a no-op, as is giving back with limits off.
    s.give_back(0, SEC);
    assert_eq!(bw.listener_remaining(SEC), 0);
}

#[test]
fn a_full_table_makes_new_clients_wait_and_keeps_old_ones_limited() {
    // Deny policy, every slot taken by a client in debt: a new client
    // cannot be tracked, so its bytes wait 50 ms, while tracked clients
    // keep their buckets.
    let bw = bandwidth(|l| {
        l.per_ip_bandwidth = Some(Rate::new(1000, SEC, 1000));
        l.table_size = 16;
        l.table_full = FullPolicy::Deny;
    });
    let cap = bw.client_stats().capacity;
    let mut tracked = Vec::new();
    for i in 0..cap {
        let mut s = Shaper::new(
            Arc::clone(&bw),
            ClientKey::new(IpAddr::from([10, 0, (i >> 8) as u8, i as u8]), 64),
        );
        assert_eq!(s.reserve(Dir::Up, 1000, SEC).granted, 1000);
        tracked.push(s);
    }
    assert_eq!(bw.client_stats().entries, cap);
    let mut late = shaper(&bw, "192.0.2.200");
    assert_eq!(
        late.reserve(Dir::Up, 16 * KIB, SEC),
        Reserve {
            granted: 0,
            retry_ns: UNTRACKED_RETRY_NS
        }
    );
    for s in &mut tracked {
        assert_eq!(s.reserve(Dir::Up, 1, SEC).granted, 0);
    }
    // An untracked client's give_back is ignored.
    late.give_back(100, SEC);
    assert_eq!(bw.client_stats().entries, cap);
    // Once the old clients' debts are paid they are idle and reclaimed.
    assert_eq!(late.reserve(Dir::Up, 16 * KIB, 3 * SEC).granted, 1000);
}

#[test]
fn reconfigure_switches_limits_in_place_and_keeps_debt() {
    let mut l = listener(|_| {});
    let bw = Arc::new(Bandwidth::new(&l).unwrap());
    let mut s = shaper(&bw, "192.0.2.1");
    assert_eq!(s.reserve(Dir::Up, 5000, SEC).granted, 5000);
    l.per_ip_bandwidth = Some(Rate::new(1000, SEC, 1000));
    bw.reconfigure(&l).unwrap();
    assert!(bw.is_on());
    assert_eq!(s.reserve(Dir::Up, 1000, SEC).granted, 1000);
    assert_eq!(s.reserve(Dir::Up, 1, SEC).granted, 0);
    // A faster rate keeps the debt, which is time: the bucket was spent
    // until SEC + 1 s, and a burst of 1000 at 10^6 per second is 1 ms
    // deep, so nothing comes back before SEC + 999 ms.
    l.per_ip_bandwidth = Some(Rate::new(1_000_000, SEC, 1000));
    bw.reconfigure(&l).unwrap();
    assert_eq!(s.reserve(Dir::Up, 1, SEC + 998 * MS).granted, 0);
    assert_eq!(s.reserve(Dir::Up, 1000, 2 * SEC).granted, 1000);
    l.per_ip_bandwidth = None;
    l.bandwidth = Some(Rate::new(10, SEC, 10));
    bw.reconfigure(&l).unwrap();
    assert_eq!(s.reserve(Dir::Up, 100, 2 * SEC).granted, 10);
    l.bandwidth = None;
    bw.reconfigure(&l).unwrap();
    assert!(!bw.is_on());
    assert_eq!(s.reserve(Dir::Up, u64::MAX, 2 * SEC).granted, u64::MAX);
}

#[test]
fn reconfigure_refuses_what_needs_a_restart_and_changes_nothing() {
    let mut l = listener(|l| l.per_ip_bandwidth = Some(Rate::new(1000, SEC, 1000)));
    let bw = Bandwidth::new(&l).unwrap();
    let mut size = l.clone();
    size.table_size = 1024;
    assert_eq!(
        bw.reconfigure(&size).unwrap_err(),
        "listener web: table_size cannot change without a restart"
    );
    let mut full = l.clone();
    full.table_full = FullPolicy::Evict;
    assert_eq!(
        bw.reconfigure(&full).unwrap_err(),
        "listener web: table_full cannot change without a restart"
    );
    l.per_ip_bandwidth = Some(Rate::new(5, SEC, 5));
    l.bandwidth = Some(Rate::new(0, SEC, 1));
    let e = bw.reconfigure(&l).unwrap_err();
    assert!(e.starts_with("listener web: bandwidth: "), "{e}");
    let mut bad_ip = l.clone();
    bad_ip.per_ip_bandwidth = Some(Rate::new(1, 0, 1));
    bad_ip.bandwidth = None;
    let e = bw.reconfigure(&bad_ip).unwrap_err();
    assert!(e.starts_with("listener web: per_ip_bandwidth: "), "{e}");
    // The old per-client rate is still in force, and the listener is off.
    let mut s = Shaper::new(Arc::new(bw), key("192.0.2.1"));
    assert_eq!(s.reserve(Dir::Up, 1000, SEC).granted, 1000);
    let bw = &s.bandwidth;
    assert_eq!(bw.listener_remaining(SEC), 1_000_000_000);
}

#[test]
fn new_refuses_rates_and_sizes_the_table_refuses() {
    let l = listener(|l| l.bandwidth = Some(Rate::new(0, SEC, 1)));
    assert_eq!(Bandwidth::new(&l).unwrap_err(), Error::InvalidRate);
    let l = listener(|l| l.table_size = 0);
    assert_eq!(Bandwidth::new(&l).unwrap_err(), Error::InvalidConfig);
}

#[test]
fn memory_and_sweep_cover_the_client_table() {
    let bw = bandwidth(|l| {
        l.per_ip_bandwidth = Some(Rate::new(1000, SEC, 1000));
        l.table_size = 64;
    });
    assert!(bw.memory() >= 64 * 64);
    let mut s = shaper(&bw, "192.0.2.1");
    assert_eq!(s.reserve(Dir::Up, 1000, SEC).granted, 1000);
    assert_eq!(bw.sweep(SEC, 64), 0);
    // Paid back and idle: the sweep removes it.
    assert_eq!(bw.sweep(10 * SEC, 64), 1);
    assert_eq!(bw.client_stats().entries, 0);
}

/// A simulated connection that always has data to read, read into a
/// buffer of `buffer` bytes that its destination drains at once. Each read
/// returns a pseudo-random share of what was reserved, as real reads do,
/// and the rest is given back. Returns the bytes read by `end_ns`.
fn download(s: &mut Shaper, buffer: u64, start_ns: u64, end_ns: u64, seed: u64) -> u64 {
    let mut now = start_ns;
    let mut got = 0;
    let mut x = seed | 1;
    while now < end_ns {
        let r = s.reserve(Dir::Up, buffer, now);
        if r.granted == 0 {
            assert!(r.retry_ns > 0);
            now += r.retry_ns;
            continue;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        // Most reads fill the grant; one in four is short.
        let n = if x.is_multiple_of(4) {
            1 + x % r.granted
        } else {
            r.granted
        };
        s.give_back(r.granted - n, now);
        got += n;
        // A read and a write take a few microseconds.
        now += 3_000;
    }
    got
}

/// Done-when item 2, in virtual time: one client held to `per_ip_bandwidth`
/// over 30 seconds, through 16 KiB reads with short reads mixed in. Past
/// the opening burst, the achieved rate is within 2% of the configured
/// rate, and the total never exceeds what the bucket allows.
#[test]
fn a_client_is_held_to_its_rate_within_two_percent_over_30_seconds() {
    for (rate, burst) in [(MIB, 64 * KIB), (512, 512), (10 * MIB, 256 * KIB)] {
        let bw = bandwidth(|l| l.per_ip_bandwidth = Some(Rate::new(rate, SEC, burst)));
        let mut s = shaper(&bw, "192.0.2.1");
        let got = download(&mut s, 16 * KIB, SEC, 31 * SEC, rate);
        let ideal = rate * 30 + burst;
        // Never more than the bucket allows.
        assert!(got <= ideal, "rate {rate}: {got} > {ideal}");
        let achieved = (got - burst.min(got)) as f64 / 30.0;
        assert!(
            (achieved - rate as f64).abs() / rate as f64 <= 0.02,
            "rate {rate}: {achieved} B/s past the burst"
        );
    }
}

/// The listener's rate shared by several always-readable connections from
/// different clients, each waking at its own retry time plus a scheduling
/// delay of up to 50 µs: together they get the rate within 2% over 30
/// seconds, and none is starved by the others.
#[test]
fn a_listener_rate_is_shared_without_starving_anyone() {
    let rate = MIB;
    let bw = bandwidth(|l| l.bandwidth = Some(Rate::new(rate, SEC, 32 * KIB)));
    let mut conns: Vec<Shaper> = (0..8)
        .map(|i| shaper(&bw, &format!("192.0.2.{}", i + 1)))
        .collect();
    let mut got = vec![0u64; conns.len()];
    // Each connection wakes when its retry says, earliest first, late by a
    // pseudo-random scheduling delay as a real timer is.
    let mut queue: BinaryHeap<Reverse<(u64, usize)>> =
        (0..conns.len()).map(|i| Reverse((SEC, i))).collect();
    let end = 31 * SEC;
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut jitter = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % 50_000
    };
    while let Some(Reverse((now, i))) = queue.pop() {
        if now >= end {
            break;
        }
        let r = conns[i].reserve(Dir::Up, 16 * KIB, now);
        if r.granted == 0 {
            queue.push(Reverse((now + r.retry_ns + jitter(), i)));
        } else {
            got[i] += r.granted;
            queue.push(Reverse((now + 3_000 + jitter(), i)));
        }
    }
    let total: u64 = got.iter().sum();
    let achieved = total as f64 / 30.0;
    assert!(
        (achieved - rate as f64).abs() / rate as f64 <= 0.02,
        "{achieved} B/s"
    );
    let fair = total / conns.len() as u64;
    for (i, g) in got.iter().enumerate() {
        assert!(*g >= fair / 2, "connection {i} got {g} of a fair {fair}");
    }
}

/// pacer's #238 at the shaper's level: two always-readable downloads
/// through a 1 MiB/s listener wake at exactly the same moments, A always
/// first. Bookings queue them, so each gets half the link.
#[test]
fn two_waiters_that_wake_together_take_turns() {
    let bw = bandwidth(|l| l.bandwidth = Some(Rate::new(MIB, SEC, 32 * KIB)));
    let mut a = shaper(&bw, "192.0.2.1");
    let mut b = shaper(&bw, "192.0.2.2");
    let (mut due_a, mut due_b, mut got_a, mut got_b) = (SEC, SEC, 0, 0);
    let mut now = SEC;
    while now < 3 * SEC {
        for (s, due, got) in [
            (&mut a, &mut due_a, &mut got_a),
            (&mut b, &mut due_b, &mut got_b),
        ] {
            if now >= *due {
                let r = s.reserve(Dir::Down, 16 * KIB, now);
                *got += r.granted;
                *due = if r.granted == 0 {
                    now + r.retry_ns
                } else {
                    now
                };
            }
        }
        // Both wake on one 1 ms tick.
        now += MS;
    }
    let total = got_a + got_b;
    assert!(total.abs_diff(2 * MIB + 32 * KIB) <= 8 * KIB, "{total}");
    assert!(got_a.abs_diff(got_b) <= 32 * KIB, "{got_a} against {got_b}");
}

#[test]
fn racing_reservations_never_grant_more_than_the_burst() {
    let bw = bandwidth(|l| {
        l.per_ip_bandwidth = Some(Rate::new(1000, SEC, 64 * KIB));
        l.bandwidth = Some(Rate::new(1000, SEC, 256 * KIB));
    });
    let granted: u64 = thread::scope(|sc| {
        let hs: Vec<_> = (0..8)
            .map(|t| {
                let mut s = shaper(&bw, &format!("192.0.2.{}", t % 2 + 1));
                sc.spawn(move || {
                    let mut g = 0;
                    for _ in 0..2_000 {
                        let r = s.reserve(Dir::Up, 4096, SEC);
                        g += r.granted;
                        if r.granted > 0 && g % 3 == 0 {
                            s.give_back(r.granted / 2, SEC);
                            g -= r.granted / 2;
                        }
                    }
                    g
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).sum()
    });
    // Two clients with a 64 KiB burst each, under a 256 KiB listener. A
    // refund smaller than the quantum stays in the buckets until a small
    // read asks for it.
    assert!(granted <= 128 * KIB, "{granted}");
    let mut rest = 0;
    for c in ["192.0.2.1", "192.0.2.2"] {
        let mut s = shaper(&bw, c);
        loop {
            let r = s.reserve(Dir::Up, 1, SEC);
            if r.granted == 0 {
                break;
            }
            rest += r.granted;
        }
    }
    assert_eq!(granted + rest, 128 * KIB);
}
