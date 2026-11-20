//! HTTP/2 flow-control windows: fixed, or adaptive to the link's
//! bandwidth-delay product.
//!
//! A receiver's window caps the data in flight towards it, so one stream
//! can carry at most `window / RTT`: 64 KiB over a 100 ms path is 655
//! KB/s, whatever the link. A window at least the bandwidth-delay product
//! ([`bdp`]) keeps such a link full.
//!
//! - [`Flow::Fixed`] advertises the configured stream and connection
//!   windows and never changes them; right when the path is known, and
//!   what the listener does unless `http2.adaptive_window` is set.
//! - [`Flow::Adaptive`] measures the BDP while data arrives and grows the
//!   windows to it. The upstream pool does this itself ([`Bdp`]), starting
//!   from its configured windows: on response data with no measurement
//!   running it sends a `PING`, counts the bytes read before the `ACK`,
//!   and when they reach two thirds of the stream window, sets the window
//!   to twice what it saw (at least double), up to 16 MiB. The listener,
//!   on hyper, gets hyper's version of the same estimator, which starts
//!   from the protocol's 65,535 bytes. Both follow gRPC's BDP estimator.

use std::time::Duration;

/// The window an adaptive connection grows to at most (hyper's limit).
pub const ADAPTIVE_MAX_WINDOW: u32 = 16 << 20;

/// How a connection sizes its receive windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// These windows, always.
    Fixed { stream: u32, connection: u32 },
    /// Grown to the measured bandwidth-delay product.
    Adaptive,
}

impl Flow {
    /// The `(adaptive, stream window, connection window)` a hyper builder
    /// is given: hyper turns adaptation off when a window is set, so the
    /// windows are only set for [`Flow::Fixed`].
    pub fn hyper(self) -> (bool, Option<u32>, Option<u32>) {
        match self {
            Flow::Fixed { stream, connection } => (false, Some(stream), Some(connection)),
            Flow::Adaptive => (true, None, None),
        }
    }
}

/// A connection's receive windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub stream: u32,
    pub connection: u32,
}

/// The bandwidth-delay estimator: fed one sample per `PING` round trip,
/// it says when and how far to grow the windows.
#[derive(Debug, Clone)]
pub struct Bdp {
    window: Window,
    max: u32,
    rtt: Option<Duration>,
}

impl Bdp {
    /// An estimator starting from `window`, growing to at most `max`.
    pub fn new(window: Window, max: u32) -> Bdp {
        Bdp {
            window,
            max,
            rtt: None,
        }
    }

    /// The windows in force.
    pub fn window(&self) -> Window {
        self.window
    }

    /// The smoothed round trip, once one sample has come in.
    pub fn rtt(&self) -> Option<Duration> {
        self.rtt
    }

    /// One measurement: `bytes` read during a `PING` round trip of `rtt`.
    /// Returns the new windows when they should grow: when the bytes
    /// reach two thirds of the stream window, the stream window becomes
    /// the larger of twice the bytes and twice the window, capped at the
    /// maximum, and the connection window at least that.
    pub fn sample(&mut self, bytes: u64, rtt: Duration) -> Option<Window> {
        self.rtt = Some(match self.rtt {
            None => rtt,
            // An eighth of each new sample, as TCP smooths its RTT.
            Some(old) => (old * 7 + rtt) / 8,
        });
        let w = u64::from(self.window.stream);
        if self.window.stream >= self.max || bytes * 3 < w * 2 {
            return None;
        }
        let stream = (bytes * 2).max(w * 2).min(u64::from(self.max)) as u32;
        self.window = Window {
            stream,
            connection: self.window.connection.max(stream),
        };
        Some(self.window)
    }
}

/// The bandwidth-delay product of a link of `bytes_per_second` and round
/// trip `rtt`, in bytes: the least window that keeps it full.
pub fn bdp(bytes_per_second: u64, rtt: Duration) -> u64 {
    (u128::from(bytes_per_second) * rtt.as_nanos() / 1_000_000_000) as u64
}

/// The most one stream can carry per second through `window` over `rtt`.
pub fn window_limit(window: u32, rtt: Duration) -> u64 {
    if rtt.is_zero() {
        return u64::MAX;
    }
    (u128::from(window) * 1_000_000_000 / rtt.as_nanos()) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bandwidth_delay_products() {
        // 25 MB/s over 100 ms needs 2.5 MB in flight.
        assert_eq!(bdp(25_000_000, Duration::from_millis(100)), 2_500_000);
        assert_eq!(bdp(1, Duration::from_secs(1)), 1);
        assert_eq!(bdp(u64::MAX, Duration::ZERO), 0);
        // The protocol's default window over 100 ms: 655 KB/s.
        assert_eq!(window_limit(65_535, Duration::from_millis(100)), 655_350);
        assert_eq!(window_limit(1, Duration::ZERO), u64::MAX);
    }

    #[test]
    fn the_estimator_grows_when_the_window_is_the_limit() {
        let start = Window {
            stream: 1 << 20,
            connection: 4 << 20,
        };
        let mut b = Bdp::new(start, ADAPTIVE_MAX_WINDOW);
        let rtt = Duration::from_millis(50);
        // Under two thirds of the window: the link, not the window, limits.
        assert_eq!(b.sample(600_000, rtt), None);
        assert_eq!(b.rtt(), Some(rtt));
        // A full window in one round trip: double it.
        assert_eq!(
            b.sample(1 << 20, rtt),
            Some(Window {
                stream: 2 << 20,
                connection: 4 << 20
            })
        );
        // Twice the bytes when that is more than double.
        assert_eq!(
            b.sample(5 << 20, Duration::from_millis(130)),
            Some(Window {
                stream: 10 << 20,
                connection: 10 << 20
            })
        );
        assert_eq!(b.rtt(), Some(Duration::from_millis(60)));
        // Capped at the maximum, and no growth past it.
        assert_eq!(b.sample(10 << 20, rtt).unwrap().stream, ADAPTIVE_MAX_WINDOW);
        assert_eq!(b.sample(64 << 20, rtt), None);
        assert_eq!(b.window().stream, ADAPTIVE_MAX_WINDOW);
    }

    #[test]
    fn hyper_gets_windows_only_when_fixed() {
        assert_eq!(
            Flow::Fixed {
                stream: 1,
                connection: 2
            }
            .hyper(),
            (false, Some(1), Some(2))
        );
        assert_eq!(Flow::Adaptive.hyper(), (true, None, None));
        assert!(window_limit(ADAPTIVE_MAX_WINDOW, Duration::from_millis(100)) > 167_000_000);
    }
}
