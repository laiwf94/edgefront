//! Outlier detection, steer's `internal/outlier` tests translated:
//! consecutive gateway failures, the ejection time cap, multiplier decay,
//! `max_ejection_percent`, success-rate ejection and its volume and host
//! minimums, ticking on the clock, and the reference model it must agree
//! with ejection by ejection. Added here: concurrent reports never break
//! the cap, and an interval with no attempts never counts towards the
//! success rate.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use edgefront_core::clock::{Clock, ManualClock};
use edgefront_core::config::OutlierDetection;
use edgefront_proxy::balancer::rng::SplitMix64;
use edgefront_proxy::endpoint::Endpoint;
use edgefront_proxy::outlier::{
    Detector, EjectFn, REASON_CONSECUTIVE_GATEWAY_FAILURE, REASON_SUCCESS_RATE,
};
use tokio::sync::oneshot;

fn base_config() -> OutlierDetection {
    OutlierDetection {
        consecutive_gateway_failures: 5,
        interval: Duration::from_secs(10),
        base_ejection_time: Duration::from_secs(30),
        max_ejection_time: Duration::from_secs(300),
        max_ejection_percent: 100,
        success_rate_minimum_hosts: 5,
        success_rate_request_volume: 100,
        success_rate_stdev_factor: 1.9,
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Ejection {
    addr: String,
    reason: String,
    until: Instant,
}

struct Harness {
    d: Detector,
    clk: Arc<ManualClock>,
    eps: Vec<Endpoint>,
    log: Arc<Mutex<Vec<Ejection>>>,
}

impl Harness {
    fn new(cfg: OutlierDetection, n: usize) -> Harness {
        let clk = Arc::new(ManualClock::new());
        let eps: Vec<Endpoint> = (0..n)
            .map(|i| Endpoint::new(&format!("10.0.0.{}:80", i + 1), 1, 0, clk.start()))
            .collect();
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        let on_eject: EjectFn = Box::new(move |ep, reason, until| {
            sink.lock().unwrap().push(Ejection {
                addr: ep.address.to_string(),
                reason: reason.to_owned(),
                until,
            });
        });
        let d = Detector::new(cfg, eps.clone(), clk.clone(), Some(on_eject));
        Harness { d, clk, eps, log }
    }

    fn now(&self) -> Instant {
        self.clk.now()
    }

    fn ejected(&self, i: usize) -> bool {
        self.eps[i].state.ejected(self.now())
    }

    fn report(&self, i: usize, gateway: bool, failure: bool) {
        self.d.report(&self.eps[i], gateway, failure);
    }

    fn log(&self) -> Vec<Ejection> {
        self.log.lock().unwrap().clone()
    }
}

#[test]
fn consecutive_gateway_failures() {
    let h = Harness::new(base_config(), 3);
    for _ in 0..4 {
        h.report(0, true, true);
    }
    h.report(0, false, false); // a success resets the run
    for _ in 0..4 {
        h.report(0, true, true);
    }
    assert!(!h.ejected(0), "ejected before 5 in a row");
    h.report(0, false, true); // a plain 500 is not a gateway failure and resets the run
    for _ in 0..5 {
        h.report(0, true, true);
    }
    let log = h.log();
    assert!(h.ejected(0));
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].reason, REASON_CONSECUTIVE_GATEWAY_FAILURE);
    assert_eq!(log[0].addr, "10.0.0.1:80");
    assert_eq!(log[0].until, h.clk.start() + Duration::from_secs(30));
    h.clk.advance(Duration::from_secs(30));
    assert!(!h.ejected(0), "still ejected after base ejection time");
    // The second ejection lasts twice as long.
    for _ in 0..5 {
        h.report(0, true, true);
    }
    assert_eq!(h.log()[1].until, h.now() + Duration::from_secs(60));
}

#[test]
fn ejection_time_is_capped() {
    let mut cfg = base_config();
    cfg.max_ejection_time = Duration::from_secs(70);
    let h = Harness::new(cfg, 1);
    for _ in 0..5 {
        for _ in 0..5 {
            h.report(0, true, true);
        }
        h.clk.advance(Duration::from_secs(300));
    }
    let want = [30, 60, 70, 70, 70];
    let log = h.log();
    assert_eq!(log.len(), want.len());
    for (i, e) in log.iter().enumerate() {
        let start = h.clk.start() + Duration::from_secs(300 * i as u64);
        assert_eq!(
            e.until - start,
            Duration::from_secs(want[i]),
            "ejection {i}"
        );
    }
}

#[test]
fn multiplier_decays_while_healthy() {
    let h = Harness::new(base_config(), 1);
    let eject = || {
        for _ in 0..5 {
            h.report(0, true, true);
        }
    };
    eject();
    h.clk.advance(Duration::from_secs(30));
    eject();
    assert_eq!(h.eps[0].state.times_ejected(), 2);
    h.clk.advance(Duration::from_secs(60 + 29)); // ejection over, 29s healthy
    h.d.tick();
    assert_eq!(
        h.eps[0].state.times_ejected(),
        2,
        "decayed before a full base period"
    );
    h.clk.advance(Duration::from_secs(1));
    h.d.tick();
    assert_eq!(h.eps[0].state.times_ejected(), 1);
    h.clk.advance(Duration::from_secs(600));
    h.d.tick();
    assert_eq!(h.eps[0].state.times_ejected(), 0);
}

#[test]
fn max_ejection_percent() {
    let mut cfg = base_config();
    cfg.max_ejection_percent = 40;
    let h = Harness::new(cfg.clone(), 5); // at most 2 of 5
    for i in 0..5 {
        for _ in 0..5 {
            h.report(i, true, true);
        }
    }
    assert_eq!((0..5).filter(|&i| h.ejected(i)).count(), 2);

    // A low percent still allows one ejection.
    cfg.max_ejection_percent = 1;
    let h = Harness::new(cfg.clone(), 3);
    for i in 0..3 {
        for _ in 0..5 {
            h.report(i, true, true);
        }
    }
    assert!(h.ejected(0) && !h.ejected(1) && !h.ejected(2));

    // Zero turns ejection off.
    cfg.max_ejection_percent = 0;
    let h = Harness::new(cfg, 3);
    for _ in 0..10 {
        h.report(0, true, true);
    }
    assert!(!h.ejected(0), "ejected with max_ejection_percent 0");
}

#[test]
fn consecutive_disabled() {
    let mut cfg = base_config();
    cfg.consecutive_gateway_failures = 0;
    let h = Harness::new(cfg, 2);
    for _ in 0..100 {
        h.report(0, true, true);
    }
    assert!(!h.ejected(0), "ejected with the detector disabled");
}

/// Five hosts, one failing half its requests: rates 1, 1, 1, 1, 0.5 give
/// mean 0.9, stdev 0.2, threshold 0.9 - 1.9 x 0.2 = 0.52.
#[test]
fn success_rate_ejection() {
    let mut cfg = base_config();
    cfg.consecutive_gateway_failures = 0;
    let h = Harness::new(cfg, 5);
    for i in 0..5 {
        for k in 0..200 {
            h.report(i, false, i == 4 && k % 2 == 0);
        }
    }
    h.d.tick();
    let log = h.log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].addr, "10.0.0.5:80");
    assert_eq!(log[0].reason, REASON_SUCCESS_RATE);
    assert!(h.ejected(4));
    // The counters were reset: a quiet interval ejects nobody new.
    h.d.tick();
    assert_eq!(h.log().len(), 1);
}

#[test]
fn success_rate_needs_volume_and_hosts() {
    let mut cfg = base_config();
    cfg.consecutive_gateway_failures = 0;
    let h = Harness::new(cfg.clone(), 5);
    for i in 0..5 {
        // Host 0 is below the volume: four eligible hosts < a minimum of five.
        let n = if i == 0 { 50 } else { 200 };
        for k in 0..n {
            h.report(i, false, i == 4 && k % 2 == 0);
        }
    }
    h.d.tick();
    assert!(h.log().is_empty(), "ejected with too few eligible hosts");
    // Uniformly bad hosts are not outliers.
    for i in 0..5 {
        for k in 0..200 {
            h.report(i, false, k % 2 == 0);
        }
    }
    h.d.tick();
    assert!(h.log().is_empty(), "ejected with zero variance");
    // Turned off by a minimum of zero hosts.
    cfg.success_rate_minimum_hosts = 0;
    let h = Harness::new(cfg, 5);
    for i in 0..5 {
        for _ in 0..200 {
            h.report(i, false, i == 4);
        }
    }
    h.d.tick();
    assert!(h.log().is_empty(), "ejected with success rate disabled");
}

/// The cap removes the clearest outliers first: two bad hosts of six with
/// room for one ejection, and the worse one goes.
#[test]
fn success_rate_ejects_the_worst_first() {
    let mut cfg = base_config();
    cfg.consecutive_gateway_failures = 0;
    cfg.max_ejection_percent = 20; // one of six
    cfg.success_rate_stdev_factor = 0.5;
    let h = Harness::new(cfg, 6);
    for i in 0..6 {
        for k in 0..100 {
            let fail = (i == 1 && k % 4 == 0) || (i == 3 && k % 2 == 0);
            h.report(i, false, fail);
        }
    }
    h.d.tick();
    let log = h.log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].addr, "10.0.0.4:80");
}

/// An endpoint with no attempts never counts, whatever the volume: with a
/// volume of zero, idle hosts would otherwise be 0/0 and eject at random.
#[test]
fn idle_endpoints_never_count_towards_the_success_rate() {
    let mut cfg = base_config();
    cfg.consecutive_gateway_failures = 0;
    cfg.success_rate_minimum_hosts = 2;
    cfg.success_rate_request_volume = 0;
    cfg.success_rate_stdev_factor = 0.5;
    let h = Harness::new(cfg, 4);
    // Hosts 0 and 1 serve; 2 and 3 are idle. Two equal samples: no outlier.
    for i in 0..2 {
        for _ in 0..10 {
            h.report(i, false, false);
        }
    }
    h.d.tick();
    assert!(h.log().is_empty(), "{:?}", h.log());
    // With a volume of zero, one attempt is enough to count: rates 1, 0, 1
    // give mean 0.67, stdev 0.47 and threshold 0.43.
    h.report(0, false, false);
    h.report(1, false, true);
    h.report(2, false, false);
    h.d.tick();
    let log = h.log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].addr, "10.0.0.2:80");
}

#[test]
fn works_without_an_eject_callback() {
    let clk = Arc::new(ManualClock::new());
    let eps = vec![Endpoint::new("10.0.0.1:80", 1, 0, clk.start())];
    let d = Detector::new(base_config(), eps, clk.clone(), None);
    assert_eq!(d.config(), &base_config());
    assert_eq!(d.endpoints().len(), 1);
    assert!(format!("{d:?}").contains("endpoints: 1"));
    for _ in 0..5 {
        d.report(&d.endpoints()[0], true, true);
    }
    assert!(d.endpoints()[0].state.ejected(clk.now()));
}

#[tokio::test]
async fn run_ticks_on_the_clock() {
    let mut cfg = base_config();
    cfg.consecutive_gateway_failures = 0;
    let h = Arc::new(Harness::new(cfg.clone(), 5));
    let (stop, stopped) = oneshot::channel::<()>();
    let runner = Arc::clone(&h);
    let task = tokio::spawn(async move {
        runner
            .d
            .run(async move {
                let _ = stopped.await;
            })
            .await;
    });
    for i in 0..5 {
        for k in 0..200 {
            h.report(i, false, i == 2 && k % 2 == 0);
        }
    }
    assert!(
        h.clk.wait_for_waiters(1, Duration::from_secs(5)).await,
        "run never waited on the clock"
    );
    // Just short of the interval nothing happens.
    h.clk.advance(cfg.interval - Duration::from_millis(1));
    tokio::task::yield_now().await;
    assert!(h.log().is_empty());
    h.clk.advance(Duration::from_millis(1));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !h.ejected(2) {
        assert!(Instant::now() < deadline, "no ejection after an interval");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // It waits for the next interval again, then stops when told.
    assert!(h.clk.wait_for_waiters(1, Duration::from_secs(5)).await);
    drop(stop);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("run did not stop")
        .unwrap();
}

/// Many threads reporting gateway failures at once never eject more than
/// the cap allows: 10 hosts at 30% give exactly three ejections.
#[test]
fn concurrent_reports_respect_the_cap() {
    let mut cfg = base_config();
    cfg.max_ejection_percent = 30;
    cfg.consecutive_gateway_failures = 3;
    let h = Arc::new(Harness::new(cfg, 10));
    let count = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|s| {
        for t in 0..8 {
            let h = Arc::clone(&h);
            let count = Arc::clone(&count);
            s.spawn(move || {
                for k in 0..2_000 {
                    let i = (t + k) % 10;
                    h.report(i, true, true);
                    count.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    assert_eq!(count.load(Ordering::Relaxed), 16_000);
    let log = h.log();
    assert_eq!(log.len(), 3, "{log:?}");
    assert_eq!((0..10).filter(|&i| h.ejected(i)).count(), 3);
}

/// The rules restated in the plainest form: per-host arrays recomputed
/// from scratch, sharing no code with the detector or endpoint state.
struct RefModel {
    cfg: OutlierDetection,
    now: Instant,
    run: Vec<u32>,
    ok: Vec<u64>,
    bad: Vec<u64>,
    times: Vec<u64>,
    until: Vec<Instant>,
    decay: Vec<Instant>,
}

impl RefModel {
    fn new(cfg: OutlierDetection, n: usize, epoch: Instant) -> RefModel {
        RefModel {
            cfg,
            now: epoch,
            run: vec![0; n],
            ok: vec![0; n],
            bad: vec![0; n],
            times: vec![0; n],
            until: vec![epoch; n],
            decay: vec![epoch; n],
        }
    }

    fn ejected(&self, i: usize) -> bool {
        self.now < self.until[i]
    }

    fn may_eject(&self) -> bool {
        let n = self.run.len();
        let pct = usize::from(self.cfg.max_ejection_percent);
        let mut limit = n * pct / 100;
        if pct > 0 && limit == 0 {
            limit = 1;
        }
        (0..n).filter(|&i| self.ejected(i)).count() < limit
    }

    fn eject(&mut self, i: usize) {
        self.times[i] += 1;
        let d =
            (self.cfg.base_ejection_time * self.times[i] as u32).min(self.cfg.max_ejection_time);
        self.until[i] = self.now + d;
        self.decay[i] = self.until[i];
        self.run[i] = 0;
    }

    fn report(&mut self, i: usize, gw: bool, fail: bool) {
        if gw {
            self.run[i] += 1;
        } else {
            self.run[i] = 0;
        }
        if gw || fail {
            self.bad[i] += 1;
        } else {
            self.ok[i] += 1;
        }
        let needed = self.cfg.consecutive_gateway_failures;
        if needed > 0 && self.run[i] >= needed && !self.ejected(i) && self.may_eject() {
            self.eject(i);
        }
    }

    fn tick(&mut self) {
        let mut cands: Vec<(usize, f64)> = Vec::new();
        let base = self.cfg.base_ejection_time;
        for i in 0..self.run.len() {
            if self.times[i] > 0 && !self.ejected(i) {
                while self.times[i] > 0 && self.now.saturating_duration_since(self.decay[i]) >= base
                {
                    self.times[i] -= 1;
                    self.decay[i] += base;
                }
            }
            let total = self.ok[i] + self.bad[i];
            if self.cfg.success_rate_minimum_hosts > 0
                && !self.ejected(i)
                && total >= u64::from(self.cfg.success_rate_request_volume)
            {
                cands.push((i, self.ok[i] as f64 / total as f64));
            }
            self.ok[i] = 0;
            self.bad[i] = 0;
        }
        if self.cfg.success_rate_minimum_hosts == 0
            || cands.len() < self.cfg.success_rate_minimum_hosts as usize
        {
            return;
        }
        let mut mean = 0.0;
        for c in &cands {
            mean += c.1;
        }
        mean /= cands.len() as f64;
        let mut v = 0.0;
        for c in &cands {
            v += (c.1 - mean) * (c.1 - mean);
        }
        let sd = (v / cands.len() as f64).sqrt();
        if sd < 1e-9 {
            return;
        }
        cands.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        for (i, rate) in cands {
            if rate < mean - self.cfg.success_rate_stdev_factor * sd {
                if !self.may_eject() {
                    return;
                }
                self.eject(i);
            }
        }
    }
}

/// A uniform draw in `0..n`.
fn below(rng: &SplitMix64, n: u64) -> u64 {
    ((u128::from(rng.next_u64()) * u128::from(n)) >> 64) as u64
}

/// A uniform draw in [0, 1).
fn unit(rng: &SplitMix64) -> f64 {
    (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

/// The detector must agree with the reference model, ejection by
/// ejection, over long random sequences of results, clock advances and
/// interval ticks.
#[test]
fn matches_reference_model() {
    let rng = SplitMix64::new(7 << 32 | 11);
    let mut consecutive = 0;
    let mut success_rate = 0;
    for iter in 0..400 {
        let n = 1 + below(&rng, 7) as usize;
        let base = Duration::from_secs(1 + below(&rng, 20));
        let cfg = OutlierDetection {
            consecutive_gateway_failures: below(&rng, 5) as u32,
            interval: Duration::from_secs(1),
            base_ejection_time: base,
            max_ejection_time: base * (1 + below(&rng, 5) as u32),
            max_ejection_percent: [0, 10, 34, 50, 100][below(&rng, 5) as usize],
            success_rate_minimum_hosts: below(&rng, 4) as u32,
            success_rate_request_volume: 1 + below(&rng, 10) as u32,
            success_rate_stdev_factor: 0.5 + unit(&rng) * 1.5,
        };
        let h = Harness::new(cfg.clone(), n);
        let mut model = RefModel::new(cfg.clone(), n, h.clk.start());
        let badness: Vec<f64> = (0..n)
            .map(|_| [0.0, 0.05, 0.5, 0.95][below(&rng, 4) as usize])
            .collect();
        for step in 0..400 {
            match below(&rng, 20) {
                0..16 => {
                    let i = below(&rng, n as u64) as usize;
                    let fail = unit(&rng) < badness[i];
                    let gw = fail && below(&rng, 4) != 0;
                    h.report(i, gw, fail);
                    model.report(i, gw, fail);
                }
                16..18 => {
                    let dt = Duration::from_millis(below(&rng, 15_000));
                    h.clk.advance(dt);
                    model.now += dt;
                }
                _ => {
                    h.d.tick();
                    model.tick();
                }
            }
            for i in 0..n {
                assert_eq!(
                    h.ejected(i),
                    model.ejected(i),
                    "iter {iter} step {step} host {i}: ejected (cfg {cfg:?})"
                );
                assert_eq!(
                    h.eps[i].state.times_ejected(),
                    model.times[i],
                    "iter {iter} step {step} host {i}: times ejected (cfg {cfg:?})"
                );
            }
        }
        for e in h.log() {
            match e.reason.as_str() {
                REASON_CONSECUTIVE_GATEWAY_FAILURE => consecutive += 1,
                REASON_SUCCESS_RATE => success_rate += 1,
                other => panic!("unknown reason {other}"),
            }
        }
    }
    // The sequences must actually exercise both detectors.
    assert!(
        consecutive >= 100 && success_rate >= 100,
        "too few ejections to be meaningful: {consecutive} consecutive, {success_rate} success rate"
    );
}
