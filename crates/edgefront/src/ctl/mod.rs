//! `edgefront ctl`: clients of a running edgefront's admin API.

mod limits;
mod replay;

use std::process::ExitCode;

const USAGE: &str = "usage: edgefront ctl limits [-s SOCKET] [-l LISTENER] stats|ping|reload|drain|top [N]|client ADDRESS\n       edgefront ctl limits replay --recording PATH --policy FILE [--top N] [--json FILE]\n";

/// Runs `edgefront ctl` with the arguments after `ctl`.
pub fn run(args: &[String]) -> ExitCode {
    match args {
        [what, sub, rest @ ..] if what == "limits" && sub == "replay" => replay::run(rest),
        [what, rest @ ..] if what == "limits" => limits::run(rest),
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
