//! `edgefront-bench`: runs benchmark scenarios against edgefront and its
//! competitors, serves the benchmark origin, and renders reports. See
//! [`edgefront_bench::cli`].

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match edgefront_bench::cli::run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            match &failure {
                edgefront_bench::cli::Failure::Usage(message) => {
                    eprint!(
                        "edgefront-bench: {message}\n{}",
                        edgefront_bench::cli::USAGE
                    )
                }
                edgefront_bench::cli::Failure::Failed(message) => {
                    eprintln!("edgefront-bench: {message}")
                }
            }
            failure.code()
        }
    }
}
