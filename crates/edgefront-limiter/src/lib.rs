//! Rate limiting primitives for edgefront, ported from libpacer.
//!
//! Every decision takes `now_ns`, a monotonic timestamp in nanoseconds
//! chosen by the caller, so the library makes no hidden clock reads and
//! tests are exact.

#![forbid(unsafe_code)]

pub mod clock;
pub mod config;
pub mod gcra;
pub mod lint;
pub mod rate;
pub mod siphash;
pub mod table;
pub mod window;

pub use config::{
    ConnQuota, LimiterConfig, Lines, ListenerConfig, ListenerLines, LogLevel, MinRate, SourceMap,
    parse_pacerd, parse_pacerd_mapped,
};
pub use gcra::{Decision, Error, Gcra, Rate};
pub use lint::{Class, Lint, lint};
pub use rate::{Limiter, Plan, Quota, RateError, RateSpec};
pub use siphash::KeyHash;
pub use table::{EntryState, FullPolicy, Table, TableConfig, TableStats};
pub use window::Window;
