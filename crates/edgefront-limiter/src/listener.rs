//! Listener-wide rates: caps on a whole entry point, whoever the clients
//! are.
//!
//! Per-key limits stop one client; a botnet of a million clients each
//! under its limit still adds up. [`ListenerLimits`] caps the listener
//! itself: new connections a second, requests a second and requests in
//! flight, each a single lock-free bucket or counter, checked before any
//! key is looked at so a flood is turned away at the cost of one CAS.
//! A request admitted holds an [`InFlight`] guard that gives its place
//! back when dropped.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::gcra::{Error, Gcra, Rate};

/// Why the listener refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerRefusal {
    /// Over `conn_rate`; retry after this many ns.
    ConnRate(u64),
    /// Over `request_rate`; retry after this many ns.
    RequestRate(u64),
    /// `max_in_flight` requests are running.
    InFlight,
}

/// The listener's caps; any may be absent.
#[derive(Debug, Default)]
pub struct ListenerLimits {
    conn_rate: Option<Gcra>,
    request_rate: Option<Gcra>,
    max_in_flight: Option<u64>,
    in_flight: Arc<AtomicU64>,
    refused: [AtomicU64; 3],
}

/// A request's place in flight.
#[derive(Debug)]
pub struct InFlight {
    count: Option<Arc<AtomicU64>>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(c) = &self.count {
            c.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl ListenerLimits {
    pub fn new(
        conn_rate: Option<Rate>,
        request_rate: Option<Rate>,
        max_in_flight: Option<u64>,
    ) -> Result<Self, Error> {
        Ok(ListenerLimits {
            conn_rate: conn_rate.map(|r| Gcra::new(&r)).transpose()?,
            request_rate: request_rate.map(|r| Gcra::new(&r)).transpose()?,
            max_in_flight,
            ..ListenerLimits::default()
        })
    }

    /// A new connection at `now_ns`.
    pub fn connection(&self, now_ns: u64) -> Result<(), ListenerRefusal> {
        if let Some(g) = &self.conn_rate {
            let d = g
                .take(now_ns, 1)
                .map_err(|_| ListenerRefusal::ConnRate(0))?;
            if !d.allowed {
                self.refused[0].fetch_add(1, Ordering::Relaxed);
                return Err(ListenerRefusal::ConnRate(d.retry_after_ns));
            }
        }
        Ok(())
    }

    /// A new request at `now_ns`: its place in flight, or the refusal.
    pub fn request(&self, now_ns: u64) -> Result<InFlight, ListenerRefusal> {
        let guard = match self.max_in_flight {
            None => InFlight { count: None },
            Some(max) => {
                let prev = self.in_flight.fetch_add(1, Ordering::AcqRel);
                let g = InFlight {
                    count: Some(Arc::clone(&self.in_flight)),
                };
                if prev >= max {
                    self.refused[2].fetch_add(1, Ordering::Relaxed);
                    return Err(ListenerRefusal::InFlight);
                }
                g
            }
        };
        if let Some(r) = &self.request_rate {
            let d = r
                .take(now_ns, 1)
                .map_err(|_| ListenerRefusal::RequestRate(0))?;
            if !d.allowed {
                self.refused[1].fetch_add(1, Ordering::Relaxed);
                return Err(ListenerRefusal::RequestRate(d.retry_after_ns));
            }
        }
        Ok(guard)
    }

    /// Requests in flight now.
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Refusals for connection rate, request rate and in flight.
    pub fn refused(&self) -> [u64; 3] {
        self.refused.each_ref().map(|a| a.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000_000;

    #[test]
    fn rates_and_in_flight_cap_the_listener() {
        let l = ListenerLimits::new(
            Some(Rate::new(10, SEC, 2)),
            Some(Rate::new(100, SEC, 3)),
            Some(2),
        )
        .unwrap();
        assert!(l.connection(SEC).is_ok() && l.connection(SEC).is_ok());
        assert_eq!(l.connection(SEC), Err(ListenerRefusal::ConnRate(SEC / 10)));
        let a = l.request(SEC).unwrap();
        let b = l.request(SEC).unwrap();
        assert_eq!(l.in_flight(), 2);
        assert_eq!(l.request(SEC).unwrap_err(), ListenerRefusal::InFlight);
        assert_eq!(l.in_flight(), 2, "a refused request gives its place back");
        drop(a);
        let c = l.request(SEC).unwrap();
        drop((b, c));
        assert_eq!(
            l.request(SEC).unwrap_err(),
            ListenerRefusal::RequestRate(SEC / 100)
        );
        assert_eq!(l.in_flight(), 0);
        assert_eq!(l.refused(), [1, 1, 1]);
        // Nothing set: everything passes.
        let open = ListenerLimits::new(None, None, None).unwrap();
        for _ in 0..1000 {
            assert!(open.connection(0).is_ok());
            drop(open.request(0).unwrap());
        }
        assert!(ListenerLimits::new(Some(Rate::new(0, SEC, 1)), None, None).is_err());
    }

    #[test]
    fn sixteen_threads_never_pass_the_in_flight_cap() {
        let l = ListenerLimits::new(None, None, Some(4)).unwrap();
        let peak = AtomicU64::new(0);
        std::thread::scope(|s| {
            for _ in 0..16 {
                s.spawn(|| {
                    for _ in 0..20_000 {
                        if let Ok(g) = l.request(0) {
                            peak.fetch_max(l.in_flight(), Ordering::Relaxed);
                            drop(g);
                        }
                    }
                });
            }
        });
        assert!(
            peak.load(Ordering::Relaxed) <= 4 + 16,
            "the count may briefly include refused ones"
        );
        assert_eq!(l.in_flight(), 0);
    }
}
