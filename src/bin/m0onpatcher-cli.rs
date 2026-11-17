//! The command line version of m0onpatcher (a console program, so the terminal waits for it and gets its exit code).
//! Run `m0onpatcher-cli help` for the commands.

use std::process::ExitCode;

fn main() -> ExitCode {
    m0onpatcher::cli::run(std::env::args().skip(1).collect())
}
