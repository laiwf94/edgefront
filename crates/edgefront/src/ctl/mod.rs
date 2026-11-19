//! `edgefront ctl`: clients of a running edgefront's admin API.

mod limits;

use std::process::ExitCode;

const USAGE: &str = "usage: edgefront ctl limits [-s SOCKET] stats|ping|reload|drain\n";

/// Runs `edgefront ctl` with the arguments after `ctl`.
pub fn run(args: &[String]) -> ExitCode {
    match args {
        [what, rest @ ..] if what == "limits" => limits::run(rest),
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
