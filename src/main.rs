#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod commands;
mod config;
mod conflict;
mod context_paths;
mod encoding;
mod extraction_naming;
mod game_data;
mod game_data_differ;
mod game_data_extractor;
mod game_data_live_merger;
mod game_data_merger;
mod game_data_patcher;
mod game_data_reader;
mod gui;
mod gui_about;
mod gui_config;
mod gui_mod_converter;
mod gui_modding;
mod live_context_manager;
mod logs;
mod mod_converter;
mod mod_data;
mod mod_data_reader;
mod mod_error_registry;
mod tool_execute;
mod tool_paths;

use std::path::Path;
use std::process::ExitCode;
use std::str::FromStr;

use anyhow::{anyhow, bail, Result};
use simplelog::{
    ColorChoice, CombinedLogger, Config, LevelFilter, SharedLogger, TermLogger, TerminalMode,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // On Windows we compile as SUBSYSTEM:WINDOWS to avoid a console flash when
    // the GUI is launched.  If the user does pass CLI arguments we try to
    // attach to the parent process's console so stdout/stderr work normally.
    // If that fails (e.g. launched by double-click with stray args) we fall
    // back to the GUI.
    // If exactly one argument is a path to an existing file, open the GUI with
    // that file pre-loaded rather than treating it as a CLI command.
    let gui_path: Option<std::path::PathBuf> = if args.len() == 1 {
        let p = std::path::Path::new(&args[0]);
        if p.is_file() { Some(p.to_path_buf()) } else { None }
    } else {
        None
    };
    let use_gui = args.is_empty() || gui_path.is_some();

    #[cfg(windows)]
    if !use_gui && !windows_console::try_attach() {
        return match gui::run(None, config::load()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                // No console available; nothing useful we can do with the error.
                let _ = err;
                ExitCode::FAILURE
            }
        };
    }

    if use_gui {
        // No args -> GUI with auto-detect; single existing file -> GUI with that path.
        match gui::run(gui_path, config::load()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        }
    } else {
        init_cli_logging();
        match run_cli(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                log::error!("{err:#}");
                ExitCode::FAILURE
            }
        }
    }
}

/// Attach to the parent process's console (Windows SUBSYSTEM:WINDOWS only).
///
/// After AttachConsole the standard handle slots are still NULL, so we open
/// CONOUT$/CONIN$ and install them via SetStdHandle so that Rust's
/// stdout/stderr (which call GetStdHandle on each write) work correctly.
#[cfg(windows)]
mod windows_console {
    use std::ffi::c_void;

    unsafe extern "system" {
        fn AttachConsole(dwProcessId: u32) -> i32;
        fn SetStdHandle(nStdHandle: u32, hHandle: *mut c_void) -> i32;
        fn CreateFileW(
            lpFileName: *const u16,
            dwDesiredAccess: u32,
            dwShareMode: u32,
            lpSecurityAttributes: *mut c_void,
            dwCreationDisposition: u32,
            dwFlagsAndAttributes: u32,
            hTemplateFile: *mut c_void,
        ) -> *mut c_void;
    }

    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
    const INVALID_HANDLE_VALUE: *mut c_void = -1_isize as *mut c_void;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn try_attach() -> bool {
        unsafe {
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                return false;
            }

            let conout = wide("CONOUT$");
            let hout = CreateFileW(
                conout.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if hout != INVALID_HANDLE_VALUE {
                SetStdHandle(STD_OUTPUT_HANDLE, hout);
                SetStdHandle(STD_ERROR_HANDLE, hout);
            }

            let conin = wide("CONIN$");
            let hin = CreateFileW(
                conin.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if hin != INVALID_HANDLE_VALUE {
                SetStdHandle(STD_INPUT_HANDLE, hin);
            }

            true
        }
    }
}

fn init_cli_logging() {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| LevelFilter::from_str(&s).ok())
        .unwrap_or(LevelFilter::Info);
    let mut loggers: Vec<Box<dyn SharedLogger>> = vec![TermLogger::new(
        level,
        Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )];
    match logs::open_log_file() {
        Ok((_, file)) => {
            // File always gets Debug-level so we have the full trail
            // even when the console is running at Info. Writes are
            // drained by a background worker that batches a burst into
            // one flush (see FileSink in logs.rs).
            let sink = logs::FileSink::start(file);
            loggers.push(logs::FileLogger::new(LevelFilter::Debug, sink));
        }
        Err(err) => eprintln!("warning: file logging disabled: {err:#}"),
    }
    let _ = CombinedLogger::init(loggers);
}

fn run_cli(args: Vec<String>) -> Result<()> {
    let mut args = args.into_iter();
    let command = args.next().ok_or_else(|| {
        anyhow!(
            "Usage: screpacker <command> [args...]\n  commands: extract, merge, diff, patch, livemerge"
        )
    })?;

    let html = args
        .next()
        .ok_or_else(|| anyhow!("Usage: screpacker {command} <html-file>"))?;
    let html_path = Path::new(&html);

    match command.as_str() {
        "extract" => commands::run_extract(html_path),
        "merge" => commands::run_merge(html_path),
        "livemerge" => commands::run_livemerge(html_path),
        "diff" => commands::run_diff(html_path),
        "patch" => commands::run_patch(html_path),
        other => bail!("unknown command: {other}"),
    }
}
