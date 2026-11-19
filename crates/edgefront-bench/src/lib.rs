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
//! - [`origin`]: the origin every target fetches from, serving the
//!   scenarios' objects with the same cacheable headers for all of them.
//! - [`targets`]: the competitor runner. Each target under
//!   `bench/targets/` is a pinned container image with its config; a run
//!   starts the origin, the target and the load generator on a private
//!   network, each on its own CPUs.
//! - [`measure`]: CPU sets, and the target's peak RSS, peak memory and CPU
//!   time read from its cgroup.
//! - [`report`]: the result of one scenario against one target as JSON,
//!   held to `bench/schema/result.schema.json` ([`schema`]), and the
//!   Markdown report made from many.
//! - [`tls`]: the TLS client a scenario with `[tls]` loads its target
//!   through.
//! - [`cli`]: the `edgefront-bench` command line.

pub mod cli;
pub mod client;
pub mod histogram;
pub mod load;
pub mod measure;
pub mod origin;
pub mod report;
pub mod scenario;
pub mod schema;
pub mod targets;
pub mod tls;
