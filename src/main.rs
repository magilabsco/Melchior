//! `melchior`: the agent harness binary. See [`melchior::cli`] for the
//! command line.

use std::process::ExitCode;

fn main() -> ExitCode {
    melchior::cli::main(melchior::Distribution::melchior())
}
