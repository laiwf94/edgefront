//! Rate limiting primitives for edgefront, ported from libpacer.
//!
//! Every decision takes `now_ns`, a monotonic timestamp in nanoseconds
//! chosen by the caller, so the library makes no hidden clock reads and
//! tests are exact.

#![forbid(unsafe_code)]

pub mod gcra;
pub mod siphash;
pub mod table;

pub use gcra::{Decision, Error, Gcra, Rate};
pub use siphash::KeyHash;
pub use table::{EntryState, FullPolicy, Table, TableConfig, TableStats};
