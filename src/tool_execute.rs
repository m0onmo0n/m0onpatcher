//! Process-launch helpers for opening files / directories with
//! either a configured external command (browser, editor, …) or the
//! platform's default handler. Centralised here so the launch
//! semantics — detached subprocess on Unix via `setsid` + null
//! stdio, vanilla `Command::spawn` on Windows, and the platform
//! "open this thing" fallback — stay consistent across every caller.

use std::path::Path;

use anyhow::{anyhow, Result};

use crate::config::Config;
use crate::tool_paths::{self, LocationSource, ToolCommand};

/// Launch `target` with the supplied command, falling back to the
/// platform's default opener when no usable command is configured.
///
/// - When `cmd` resolves to a non-empty binary, runs that binary
///   with its `arguments` followed by `target`. The child is
///   detached from our session on Unix (setsid + null stdio) so it
///   survives us exiting; on Windows `Command::spawn` already
///   produces an independent process.
/// - When `cmd` is `None` or has an empty binary path, and
///   `platform_fallback` is true, hands off to the platform default
///   handler (`ShellExecuteW` / `open` / `xdg-open`).
/// - With no command and no fallback, returns an error.
pub fn open_path_with(
    cmd: Option<&ToolCommand>,
    platform_fallback: bool,
    target: &Path,
) -> Result<()> {
    let configured = cmd.filter(|c| !c.binary_path.is_empty());
    if let Some(c) = configured {
        return spawn_detached(&c.binary_path, &c.arguments, target);
    }
    if platform_fallback {
        return platform_open(target);
    }
    Err(anyhow!(
        "no command configured and platform fallback disabled"
    ))
}

/// "Open this directory in your editor" entry point. Validates that
/// `target` is a real directory (logs and returns otherwise),
/// resolves the editor command via `tool_command_directory_edit`,
/// and dispatches through `open_path_with` with the platform
/// fallback enabled. A launch failure is logged with the specific
/// directory plus a reminder that the editor command lives in the
/// Config tab — there's no error returned because callers are GUI
/// buttons that have nowhere meaningful to surface it.
pub fn open_directory_editor(config: &Config, target: &Path) {
    if !target.is_dir() {
        log::warn!(
            "Cannot open editor: {} is not an existing directory",
            target.display()
        );
        return;
    }
    let cmd = tool_paths::tool_command_directory_edit(config);
    if let Err(err) = open_path_with(cmd.as_ref(), true, target) {
        log::warn!(
            "Failed to open directory {}: {err}. Configure the directory editor in the Config tab.",
            target.display()
        );
    }
}

/// Same shape as `open_directory_editor`, but for opening a single
/// file in a text editor. Validates that `target` is an existing
/// file, resolves the editor command via `tool_command_text_edit`,
/// and dispatches through `open_path_with` with the platform
/// fallback enabled.
pub fn open_text_editor(config: &Config, target: &Path) {
    if !target.is_file() {
        log::warn!(
            "Cannot open editor: {} is not an existing file",
            target.display()
        );
        return;
    }
    let cmd = tool_paths::tool_command_text_edit(config);
    if let Err(err) = open_path_with(cmd.as_ref(), true, target) {
        log::warn!(
            "Failed to open file {}: {err}. Configure the text editor in the Config tab.",
            target.display()
        );
    }
}

/// Open a file via the configured browser binary if `browser` is
/// non-empty; otherwise hand off to the platform default. Convenience
/// wrapper around `open_path_with` for the common "user picked a
/// custom browser path" case (see Config tab's Browser row).
pub fn open_in_browser(path: &Path, browser: &str) -> Result<()> {
    let cmd = if browser.is_empty() {
        None
    } else {
        Some(ToolCommand {
            binary_path: browser.to_string(),
            arguments: Vec::new(),
            source: LocationSource::Config,
        })
    };
    open_path_with(cmd.as_ref(), true, path)
}

#[cfg(unix)]
fn spawn_detached(binary: &str, args: &[String], target: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    unsafe extern "C" {
        fn setsid() -> i32;
    }

    let mut cmd = std::process::Command::new(binary);
    cmd.args(args)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    unsafe {
        cmd.pre_exec(|| {
            // Put the child in a new session so it no longer shares
            // our controlling terminal or process group. When
            // screpacker exits the child is reparented to init
            // rather than killed.
            let _ = setsid();
            Ok(())
        });
    }

    cmd.spawn()
        .map(|_| ())
        .map_err(|err| anyhow!("failed to launch {binary}: {err}"))
}

#[cfg(windows)]
fn spawn_detached(binary: &str, args: &[String], target: &Path) -> Result<()> {
    std::process::Command::new(binary)
        .args(args)
        .arg(target)
        .spawn()
        .map(|_| ())
        .map_err(|err| anyhow!("failed to launch {binary}: {err}"))
}

#[cfg(target_os = "windows")]
fn platform_open(path: &Path) -> Result<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            lpOperation: *const u16,
            lpFile: *const u16,
            lpParameters: *const u16,
            lpDirectory: *const u16,
            nShowCmd: i32,
        ) -> *mut c_void;
    }

    const SW_SHOWNORMAL: i32 = 1;

    let wide = |s: &std::ffi::OsStr| -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    };
    let op: Vec<u16> = "open\0".encode_utf16().collect();
    let file = wide(path.as_os_str());

    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns > 32 on success.
    if result as usize > 32 {
        Ok(())
    } else {
        Err(anyhow!("ShellExecuteW failed (code {})", result as usize))
    }
}

#[cfg(target_os = "macos")]
fn platform_open(path: &Path) -> Result<()> {
    std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| anyhow!("failed to launch opener: {err}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_open(path: &Path) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| anyhow!("failed to launch opener: {err}"))
}
