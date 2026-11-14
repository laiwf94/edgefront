//! edgefront-bench: the benchmark harness every track measures with.
//!
//! - [`load`]: the open-loop load generator. It sends on a fixed schedule
//!   of constant or stepped arrival rates over HTTP/1.1 or HTTP/2, and
//!   measures each request from the time it was due, so a stalled server
//!   cannot hide its stall by slowing the generator down (coordinated
//!   omission).
//! - [`histogram`]: the high dynamic range histogram each phase records
//!   its latencies in, three significant digits from a nanosecond to an
//!   hour.

pub mod histogram;
pub mod load;
