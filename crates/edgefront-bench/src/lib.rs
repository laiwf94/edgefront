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
//! - [`scenario`]: benchmark scenarios read from TOML under
//!   `bench/scenarios/`: the schedule, the object size mix, Zipf key
//!   popularity and a hit ratio target, turned into a schedule and a
//!   deterministic request source for [`load`].
//! - [`measure`]: CPU sets, and the target's peak RSS, peak memory and CPU
//!   time read from its cgroup.
//! - [`report`]: the result of one scenario against one target as JSON,
//!   held to `bench/schema/result.schema.json` ([`schema`]), and the
//!   Markdown report made from many.

pub mod histogram;
pub mod load;
pub mod measure;
pub mod report;
pub mod scenario;
pub mod schema;
