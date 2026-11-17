#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! The window. On Windows this is a window-subsystem program (no console flash on start), so the terminal does not
//! wait for it: use `m0onpatcher-cli` for the command line there. On Linux and macOS this program also understands
//! the commands of `m0onpatcher-cli` when it is started with arguments.

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(not(windows))]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if !args.is_empty() {
            return m0onpatcher::cli::run(args);
        }
    }
    match m0onpatcher::run_gui() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
