//! Eviction policies for the stores, kept apart from the stores' entries so
//! the simulator (`crate::sim`) runs the very code a store runs.
//!
//! Each policy holds keys and byte sizes only, and reports what it evicts
//! through a callback, so the caller drops or demotes the entries it keeps
//! elsewhere.
//!
//! - [`sieve`]: SIEVE (Zhang et al., NSDI 2024), a FIFO queue with one
//!   visited bit per object and a hand that sweeps from the oldest object
//!   to the newest. A hit sets a bit and moves nothing.
//! - [`tinylfu`]: W-TinyLFU (Einziger et al., ACM ToS 2017), a small LRU
//!   window in front of a segmented LRU whose entry is guarded by a
//!   count-min sketch of recent accesses: an object leaving the window
//!   gets in only if it is estimated more popular than the object it
//!   would displace.

pub mod sieve;
pub mod tinylfu;
