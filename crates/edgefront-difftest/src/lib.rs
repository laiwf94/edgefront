//! Differential tests for edgefront's ports.
//!
//! Each legacy implementation that edgefront replaces stays in the
//! repository until its port agrees with it. A small driver in the legacy
//! code answers queries with the original logic; this crate generates
//! inputs, asks the driver and the Rust crates the same questions, and
//! reports every disagreement with the input that caused it.
//!
//! - [`steer`]: configuration acceptance and route choice, against
//!   `legacy/steer/cmd/diffdrive`.
//! - [`balancer`]: pick sequences of steer's load balancer, scenario by
//!   scenario, against the same driver's `picks` query.
//! - [`resilience`]: outlier ejection and decay, and retry decisions with
//!   their budget and backoff, on generated failure traces against the same
//!   driver's `outlier` and `retry` queries.
//! - [`pacer`]: libpacer's GCRA, keyed table and sliding window.
//!   [`pacer::Interpreter`] answers the C driver's one-line-per-operation
//!   protocol from edgefront-limiter. libpacer and its driver were deleted
//!   with legacy/pacer in week 4; what they answered is kept, and
//!   [`pacer::check_recorded`] holds the Rust side to it.
//! - [`pacer_config`]: pacerd's config reader, on every config pacer
//!   shipped, the string literals of its tests and seeded mutants of them,
//!   held to the answers pacerd's reader recorded
//!   ([`pacer_config::check_recorded`]).
//! - [`cache`]: the cache node as a client sees it, against cachet 0.1, the
//!   binary from before the workspace split. A recorded request log is
//!   replayed against both binaries on one logical clock, and status,
//!   headers, body digest and origin fetches are diffed per request.
//!
//! - [`l4`]: pacerd's data path as a client sees it: the scenarios of
//!   pacer's `tests/it_*.c` as traffic scripts under `tests/pacer_scripts`,
//!   run against `edgefront limits` and held to the transcripts legacy
//!   pacerd gave before it was deleted, outcomes and counters step by step.
//! - [`drill`]: not a diff but the tools of steer's failover drill, its
//!   fake upstream and its open-loop load generator, so the drill runs
//!   against the Rust binary without Go.
//!
//! Every run is seeded, and the seed is printed with its report, so a
//! divergence found in CI can be replayed locally with `--seed`.

pub mod balancer;
pub mod cache;
pub mod drill;
pub mod driver;
pub mod l4;
pub mod pacer;
pub mod pacer_config;
pub mod resilience;
pub mod rng;
pub mod steer;
