use std::path::{Path, PathBuf};

use crate::config::{self, Config};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationSource {
    Config,
    Autodetected,
}

/// Where a tool's path came from and whether that file actually exists on
/// disk at the moment we looked. Used by the GUI to decorate the display
/// ("(autodetected) …", "(missing) …", …) and could later be used to gate
/// features that need the tool.
#[derive(Debug, Clone)]
pub struct ToolPath {
    pub source: LocationSource,
    pub path: Option<PathBuf>,
    pub exists: bool,
}

impl ToolPath {
    /// Render the "(autodetected) …" / "(missing) …" / "(unset)" / raw-path
    /// string that the GUI shows in the Config panel's path fields.
    pub fn display_label(&self) -> String {
        match self.source {
            LocationSource::Autodetected => match self.path.as_deref() {
                Some(p) => format!("(autodetected) {}", p.display()),
                None => "(unset)".to_string(),
            },
            LocationSource::Config => match self.path.as_deref() {
                Some(p) if self.exists => p.display().to_string(),
                Some(p) => format!("(missing) {}", p.display()),
                None => "(unset)".to_string(),
            },
        }
    }
}

/// Look up the Python interpreter to use. Config takes precedence: if
/// `config.python_path` is non-empty it's returned verbatim with
/// `source = Config` (and `exists` reflecting whether it's actually a
/// file). Otherwise scan `PATH` for the right Python launcher:
/// `pythonw` on Windows (so launching KittyPatcher doesn't flash a
/// console window), `python3` first then `python` elsewhere. Returns
/// the first hit with `source = Autodetected`. `path: None` + a
/// `Autodetected` source means nothing was found.
pub fn tool_locate_python(config: &Config) -> ToolPath {
    if !config.python_path.is_empty() {
        let path = PathBuf::from(&config.python_path);
        let exists = path.is_file();
        return ToolPath {
            source: LocationSource::Config,
            path: Some(path),
            exists,
        };
    }

    #[cfg(windows)]
    let candidates: &[&str] = &["pythonw"];
    #[cfg(not(windows))]
    let candidates: &[&str] = &["python3", "python"];

    for candidate in candidates {
        if let Some(p) = find_in_path(candidate) {
            return ToolPath {
                source: LocationSource::Autodetected,
                path: Some(p),
                exists: true,
            };
        }
    }

    ToolPath {
        source: LocationSource::Autodetected,
        path: None,
        exists: false,
    }
}

/// Look up KittyPatcher. Config takes precedence (same semantics as
/// `tool_locate_python`). Otherwise search first the context directory,
/// then the executable directory, for a KittyPatcher binary:
///
/// - On Windows, first check for any `*.exe` whose name contains
///   "kittypatcher" (case-insensitive).
/// - Then (also on non-Windows) check for any `*.py` whose name contains
///   "kittypatcher" but NOT "modder" — that's what sets the runner apart
///   from the mod-authoring helper script shipped alongside it.
///
/// `context_path` can be a file (its parent directory is used) or a
/// directory (used as-is). `None` skips the context-dir search and goes
/// straight to the executable directory.
pub fn tool_locate_kittypatcher(config: &Config, context_path: Option<&Path>) -> ToolPath {
    if !config.kittypatcher_path.is_empty() {
        let path = PathBuf::from(&config.kittypatcher_path);
        let exists = path.is_file();
        return ToolPath {
            source: LocationSource::Config,
            path: Some(path),
            exists,
        };
    }

    let context_dir = context_path.and_then(|p| {
        if p.is_dir() {
            Some(p.to_path_buf())
        } else {
            p.parent().map(|p| p.to_path_buf())
        }
    });

    if let Some(dir) = context_dir.as_deref() {
        if let Some(found) = search_dir_for_kittypatcher(dir) {
            return ToolPath {
                source: LocationSource::Autodetected,
                path: Some(found),
                exists: true,
            };
        }
    }

    if let Some(exe_dir) = config::executable_dir() {
        if let Some(found) = search_dir_for_kittypatcher(&exe_dir) {
            return ToolPath {
                source: LocationSource::Autodetected,
                path: Some(found),
                exists: true,
            };
        }
    }

    ToolPath {
        source: LocationSource::Autodetected,
        path: None,
        exists: false,
    }
}

/// Concrete invocation for an external tool: which binary to exec
/// and which arguments to prepend before caller-supplied ones. For a
/// native KittyPatcher build `arguments` is empty; for a `.py` script
/// it's `[script_path]` and `binary_path` is the Python interpreter.
/// Editor commands (`tool_command_directory_edit`) carry whatever
/// argv list the user specified in config.
///
/// `source` records whether the resolution came from the user's
/// config or from auto-detection — useful for the GUI to label the
/// origin in its config display.
#[derive(Debug, Clone)]
pub struct ToolCommand {
    pub binary_path: String,
    pub arguments: Vec<String>,
    pub source: LocationSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittypatcherCommandError {
    MissingPatcherPath,
    MissingPythonPath,
}

/// Resolve KittyPatcher into a runnable command. Uses
/// `tool_locate_kittypatcher` to find the patcher itself; if that's a
/// `.py` file it then uses `tool_locate_python` to find an interpreter to
/// front it with. Errors signal which of the two lookups came up empty so
/// the caller can show a targeted message.
pub fn tool_command_kittypatcher(
    config: &Config,
    context_path: Option<&Path>,
) -> Result<ToolCommand, KittypatcherCommandError> {
    let kp = tool_locate_kittypatcher(config, context_path);
    let kp_path = match kp.path {
        Some(p) if kp.exists => p,
        _ => return Err(KittypatcherCommandError::MissingPatcherPath),
    };

    let is_py = kp_path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("py"))
        .unwrap_or(false);

    if !is_py {
        return Ok(ToolCommand {
            binary_path: kp_path.to_string_lossy().into_owned(),
            arguments: Vec::new(),
            source: kp.source,
        });
    }

    let py = tool_locate_python(config);
    let py_path = match py.path {
        Some(p) if py.exists => p,
        _ => return Err(KittypatcherCommandError::MissingPythonPath),
    };

    Ok(ToolCommand {
        binary_path: py_path.to_string_lossy().into_owned(),
        arguments: vec![kp_path.to_string_lossy().into_owned()],
        source: kp.source,
    })
}

/// Resolve the editor-launching command used to "open this directory
/// in your editor". Config wins: if `directory_edit_command` is
/// non-empty the first entry is the binary and the rest are leading
/// arguments. Otherwise tries to find VS Code via the platform's
/// canonical install paths (Windows) or via PATH lookup for `code`
/// (everywhere else). Returns `None` when nothing was found.
pub fn tool_command_directory_edit(config: &Config) -> Option<ToolCommand> {
    if !config.directory_edit_command.is_empty() {
        let mut iter = config.directory_edit_command.iter();
        let binary_path = iter.next().cloned().unwrap_or_default();
        let arguments: Vec<String> = iter.cloned().collect();
        return Some(ToolCommand {
            binary_path,
            arguments,
            source: LocationSource::Config,
        });
    }
    let path = autodetect_vscode()?;
    Some(ToolCommand {
        binary_path: path.to_string_lossy().into_owned(),
        arguments: Vec::new(),
        source: LocationSource::Autodetected,
    })
}

/// Same shape as `tool_command_directory_edit`, but for the
/// "open this file in your text editor" entry point. Reads
/// `text_edit_command` from config; falls back to the same VS Code
/// autodetect since `code` happily opens files too.
pub fn tool_command_text_edit(config: &Config) -> Option<ToolCommand> {
    if !config.text_edit_command.is_empty() {
        let mut iter = config.text_edit_command.iter();
        let binary_path = iter.next().cloned().unwrap_or_default();
        let arguments: Vec<String> = iter.cloned().collect();
        return Some(ToolCommand {
            binary_path,
            arguments,
            source: LocationSource::Config,
        });
    }
    let path = autodetect_vscode()?;
    Some(ToolCommand {
        binary_path: path.to_string_lossy().into_owned(),
        arguments: Vec::new(),
        source: LocationSource::Autodetected,
    })
}

#[cfg(target_os = "windows")]
fn autodetect_vscode() -> Option<PathBuf> {
    // The standard install slots for "VS Code" on Windows.
    // %LOCALAPPDATA% is the user-scoped installer (the default
    // download experience); the Program Files variants cover the
    // system-wide installer.
    let candidates: [Option<PathBuf>; 3] = [
        std::env::var_os("LOCALAPPDATA").map(|p| {
            PathBuf::from(p)
                .join("Programs")
                .join("Microsoft VS Code")
                .join("Code.exe")
        }),
        std::env::var_os("ProgramFiles").map(|p| {
            PathBuf::from(p)
                .join("Microsoft VS Code")
                .join("Code.exe")
        }),
        std::env::var_os("ProgramFiles(x86)").map(|p| {
            PathBuf::from(p)
                .join("Microsoft VS Code")
                .join("Code.exe")
        }),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn autodetect_vscode() -> Option<PathBuf> {
    find_in_path("code")
}

fn search_dir_for_kittypatcher(dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(p) = find_first_entry(dir, |lower| {
            lower.ends_with(".exe") && lower.contains("kittypatcher")
        }) {
            return Some(p);
        }
    }
    find_first_entry(dir, |lower| {
        lower.ends_with(".py") && lower.contains("kittypatcher") && !lower.contains("modder")
    })
}

fn find_first_entry(dir: &Path, predicate: impl Fn(&str) -> bool) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if predicate(&lower) {
            let path = entry.path();
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// Walk `PATH` looking for a file called `name`. On Windows also tries
/// `name.exe`, which is how executables are usually stored. Returns the
/// first hit, or None if no PATH entry contains the binary.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        #[cfg(windows)]
        {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
