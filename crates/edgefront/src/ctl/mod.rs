//! `edgefront ctl`: clients of a running edgefront's admin API.

mod limits;

use std::process::ExitCode;

const USAGE: &str = "usage: edgefront ctl limits [-s SOCKET] [-l LISTENER] stats|ping|reload|drain|top [N]|client ADDRESS\n";

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
