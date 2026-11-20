//! When to issue: the renewal scheduler.
//!
//! A [`Renewer`] holds the current certificate's validity and decides,
//! on an injected [`Clock`] and the [`Wall`] time that moves with it,
//! when to ask its [`Issuer`] for the next one:
//!
//! - with no certificate, at once;
//! - otherwise at two thirds of the certificate's lifetime
//!   (`not_before + 2/3 × (not_after − not_before)`), so a 90-day
//!   certificate renews 30 days before it expires, as Let's Encrypt
//!   advises;
//! - after a failed attempt, again after a backoff that starts at five
//!   minutes and doubles to six hours, each with up to 10% jitter, but
//!   never later than an hour before the certificate expires once that
//!   is near, so a CA outage shorter than the remaining third of the
//!   lifetime never lets the certificate lapse.
//!
//! Each success is handed to the issuer's `install` (the listener writes
//! the files and swaps its TLS store) and the next renewal is planned from
//! the new certificate. The renewer keeps the numbers an operator alerts
//! on in [`Status`]: seconds until expiry, renewals and failures, and the
//! last error, and [`Status::write_metrics`] renders them in the
//! Prometheus text format.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use super::challenges::BoxFuture;
use super::client::{AcmeError, Issued};
use crate::clock::Clock;
use crate::tls::ocsp::Wall;

/// The first retry after a failure, doubled to [`MAX_RETRY`].
pub const FIRST_RETRY: Duration = Duration::from_secs(300);
pub const MAX_RETRY: Duration = Duration::from_secs(6 * 3600);
/// How close to expiry a retry is still allowed to be scheduled.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(3600);

/// Gets certificates and puts them in place.
pub trait Issuer: Send + Sync {
    /// Runs one ACME order.
    fn issue(&self) -> BoxFuture<'_, Result<Issued, AcmeError>>;
    /// Installs an issued certificate.
    fn install(&self, issued: &Issued) -> Result<(), AcmeError>;
}

/// What the renewer has done, for logs, metrics and tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub not_before: Option<SystemTime>,
    pub not_after: Option<SystemTime>,
    pub renewals: u64,
    pub failures: u64,
    pub last_error: Option<String>,
    /// When the next attempt is due.
    pub next: Option<SystemTime>,
}

impl Status {
    /// Seconds from `now` until the certificate expires; negative once it
    /// has, `None` without one.
    pub fn expires_in(&self, now: SystemTime) -> Option<f64> {
        self.not_after.map(|t| match t.duration_since(now) {
            Ok(d) => d.as_secs_f64(),
            Err(e) => -e.duration().as_secs_f64(),
        })
    }

    /// The Prometheus exposition of the status.
    pub fn write_metrics(&self, now: SystemTime, out: &mut String) {
        let _ = writeln!(
            out,
            "# HELP edgefront_acme_certificate_expiry_seconds Seconds until the ACME certificate expires."
        );
        let _ = writeln!(
            out,
            "# TYPE edgefront_acme_certificate_expiry_seconds gauge"
        );
        let _ = writeln!(
            out,
            "edgefront_acme_certificate_expiry_seconds {}",
            self.expires_in(now).map_or(0.0, f64::floor)
        );
        let _ = writeln!(
            out,
            "# HELP edgefront_acme_renewals_total ACME issuance attempts by result."
        );
        let _ = writeln!(out, "# TYPE edgefront_acme_renewals_total counter");
        let _ = writeln!(
            out,
            "edgefront_acme_renewals_total{{result=\"success\"}} {}",
            self.renewals
        );
        let _ = writeln!(
            out,
            "edgefront_acme_renewals_total{{result=\"failure\"}} {}",
            self.failures
        );
    }
}

/// The renewal scheduler; see the module documentation.
pub struct Renewer<I> {
    issuer: I,
    clock: Arc<dyn Clock>,
    wall: Wall,
    status: Arc<Mutex<Status>>,
    rng: Mutex<u64>,
}

/// When a certificate valid from `not_before` to `not_after` is renewed.
pub fn renew_at(not_before: SystemTime, not_after: SystemTime) -> SystemTime {
    let life = not_after.duration_since(not_before).unwrap_or_default();
    not_before + life * 2 / 3
}

impl<I: Issuer> Renewer<I> {
    /// A renewer for `issuer`, starting from the certificate valid over
    /// `current`, if there is one.
    pub fn new(
        issuer: I,
        clock: Arc<dyn Clock>,
        wall: Wall,
        current: Option<(SystemTime, SystemTime)>,
    ) -> Renewer<I> {
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64)
            | 1;
        let status = Status {
            not_before: current.map(|c| c.0),
            not_after: current.map(|c| c.1),
            ..Status::default()
        };
        Renewer {
            issuer,
            clock,
            wall,
            status: Arc::new(Mutex::new(status)),
            rng: Mutex::new(seed),
        }
    }

    /// The status, shared: it stays readable while [`run`](Self::run) runs.
    pub fn status(&self) -> Arc<Mutex<Status>> {
        Arc::clone(&self.status)
    }

    fn jitter(&self, d: Duration) -> Duration {
        let mut s = self.rng.lock().unwrap_or_else(|p| p.into_inner());
        *s ^= *s >> 12;
        *s ^= *s << 25;
        *s ^= *s >> 27;
        let r = s.wrapping_mul(0x2545_f491_4f6c_dd1d);
        d + d.mul_f64((r >> 11) as f64 / (1u64 << 53) as f64 * 0.1)
    }

    /// When the next attempt is due, from the status.
    fn due(&self, status: &Status, failures_in_a_row: u32) -> SystemTime {
        let now = self.wall.now();
        let planned = match (status.not_before, status.not_after) {
            (Some(b), Some(a)) => renew_at(b, a),
            _ => now,
        };
        if failures_in_a_row == 0 {
            return planned;
        }
        let backoff = self.jitter(
            FIRST_RETRY
                .saturating_mul(1 << (failures_in_a_row - 1).min(16))
                .min(MAX_RETRY),
        );
        let mut retry = now + backoff;
        if let Some(expiry) = status.not_after {
            // Near expiry, keep trying at least an hour before the end.
            let latest = expiry.checked_sub(EXPIRY_MARGIN).unwrap_or(expiry);
            if retry > latest && now < latest {
                retry = latest;
            }
        }
        retry
    }

    /// One attempt now: issue, install, and record the result. Returns
    /// whether it succeeded.
    pub async fn attempt(&self) -> bool {
        match self
            .issuer
            .issue()
            .await
            .and_then(|i| self.issuer.install(&i).map(|()| i))
        {
            Ok(issued) => {
                let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
                s.not_before = Some(issued.not_before);
                s.not_after = Some(issued.not_after);
                s.renewals += 1;
                s.last_error = None;
                tracing::info!(
                    renewals = s.renewals,
                    expires_in_s = s.expires_in(self.wall.now()).unwrap_or(0.0) as i64,
                    "acme certificate issued"
                );
                true
            }
            Err(e) => {
                let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
                s.failures += 1;
                s.last_error = Some(e.to_string());
                tracing::warn!(error = %e, failures = s.failures, "acme issuance failed");
                false
            }
        }
    }

    /// Issues and renews until `stop` resolves.
    pub async fn run(self, stop: impl Future<Output = ()>) {
        let mut stop = std::pin::pin!(stop);
        let mut failures_in_a_row = 0u32;
        loop {
            let due = {
                let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
                let due = self.due(&s, failures_in_a_row);
                s.next = Some(due);
                due
            };
            let wait = due.duration_since(self.wall.now()).unwrap_or_default();
            if !wait.is_zero() {
                let timer: Pin<Box<dyn Future<Output = _> + Send>> =
                    Box::pin(self.clock.after(wait));
                tokio::select! {
                    () = &mut stop => return,
                    _ = timer => {}
                }
                // The clock may have moved further than asked; plan again.
                if self.wall.now() < due {
                    continue;
                }
            }
            let attempt = self.attempt();
            tokio::select! {
                () = &mut stop => return,
                ok = attempt => {
                    failures_in_a_row = if ok { 0 } else { failures_in_a_row + 1 };
                }
            }
        }
    }
}
