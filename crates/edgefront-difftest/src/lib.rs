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
//!
//! Every run is seeded, and the seed is printed with its report, so a
//! divergence found in CI can be replayed locally with `--seed`.

pub mod rng;
pub mod steer;
