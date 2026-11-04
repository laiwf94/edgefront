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
//! - [`pacer`]: libpacer's GCRA, keyed table and sliding window, against
//!   `legacy/pacer/tests/diffdrive.c`. The driver reads one operation per
//!   line on stdin and prints one line per operation; this crate's build
//!   script compiles it with the system C compiler, and
//!   [`PACER_DIFFDRIVE`] is the path of the binary. [`driver::run`] feeds
//!   it a script and collects the transcript, and [`pacer::Interpreter`]
//!   answers the same script from edgefront-limiter.
//!
//! Every run is seeded, and the seed is printed with its report, so a
//! divergence found in CI can be replayed locally with `--seed`.

pub mod driver;
pub mod pacer;
pub mod rng;
pub mod steer;

/// Path of the compiled `legacy/pacer/tests/diffdrive.c` driver.
pub const PACER_DIFFDRIVE: &str = env!("EDGEFRONT_PACER_DIFFDRIVE");
