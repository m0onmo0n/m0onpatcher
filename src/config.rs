use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Patch engine for `.mod` files: `"builtin"` (internal engine only), `"auto"` (internal engine,
    /// KittyPatcher as fallback) or `"external"` (KittyPatcher only). See [`Config::engine_mode`].
    pub engine: String,
    /// Legacy phase of the internal engine: the old `~` / `~~` blocks of mods without a `# Legacy:` header are
    /// applied first and replace every match (KittyPatcher 0.1.5c behaviour). Off: first match only, in load order.
    pub legacy_replace_all: bool,
    pub kittypatcher_path: String,
    pub python_path: String,
    pub browser_path: String,
    /// Editor invocation as an argv list (e.g. `["code", "-n"]`).
    /// First entry is the binary, rest are leading arguments — the
    /// target directory is appended at launch time. Empty list means
    /// "use the autodetect path" (see `tool_command_directory_edit`).
    pub directory_edit_command: Vec<String>,
    /// Same shape as `directory_edit_command`, used when opening a
    /// single file (rather than a directory). Empty list falls back
    /// to the autodetected text editor (see `tool_command_text_edit`).
    pub text_edit_command: Vec<String>,
    /// Most recently selected Modding-tab HTML file. Persisted only so
    /// the "Use last" button can restore it on a fresh launch when the
    /// CWD-based auto-detect doesn't pick anything.
    pub last_html_path: String,
    /// Most recently selected Mod-converter base directory. Same role
    /// as `last_html_path` but for the converter tab.
    pub last_converter_directory: String,
}

/// Which engine applies `.mod` files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineMode {
    /// The built-in engine only; KittyPatcher is never called.
    Internal,
    /// The built-in engine; KittyPatcher is used only for mod sets the built-in engine cannot handle yet.
    InternalFallback,
    /// KittyPatcher only (the original ScRepacker behaviour).
    External,
}

impl EngineMode {
    pub const ALL: [EngineMode; 3] = [EngineMode::Internal, EngineMode::InternalFallback, EngineMode::External];

    /// Value stored in `screpacker-config.json`.
    pub fn config_value(self) -> &'static str {
        match self {
            EngineMode::Internal => "builtin",
            EngineMode::InternalFallback => "auto",
            EngineMode::External => "external",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            EngineMode::Internal => "Internal engine",
            EngineMode::InternalFallback => "Internal engine, KittyPatcher as fallback",
            EngineMode::External => "External KittyPatcher",
        }
    }
}

impl Config {
    /// The selected engine. An empty `engine` value means "not chosen yet": a config that already
    /// points at a KittyPatcher keeps using it as the fallback, a fresh one uses the internal engine only.
    pub fn engine_mode(&self) -> EngineMode {
        match self.engine.trim().to_ascii_lowercase().as_str() {
            "builtin" | "internal" => EngineMode::Internal,
            "auto" => EngineMode::InternalFallback,
            "external" => EngineMode::External,
            _ if !self.kittypatcher_path.trim().is_empty() => EngineMode::InternalFallback,
            _ => EngineMode::Internal,
        }
    }
}

const CONFIG_FILE: &str = "screpacker-config.json";

/// Directory the running binary lives in. Used as the base for the config
/// file and for "tool-shipped alongside binary" lookups elsewhere.
///
/// Special case for `cargo run` / `cargo build`: when the exe is at
/// `<project>/target/{debug,release}/screpacker`, return the project
/// root instead. That way a dev build and a shipped build both read the
/// same config file and find the same KittyPatcher sibling.
pub fn executable_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let mut comps: Vec<_> = dir.components().collect();
    let is_target_build = comps.len() >= 2
        && matches!(
            comps.last().and_then(|c| c.as_os_str().to_str()),
            Some("debug") | Some("release"),
        )
        && comps[comps.len() - 2].as_os_str() == "target";
    if is_target_build {
        comps.truncate(comps.len() - 2);
        Some(comps.iter().collect())
    } else {
        Some(dir)
    }
}

fn config_path() -> Option<PathBuf> {
    executable_dir().map(|d| d.join(CONFIG_FILE))
}

/// Read config from `<exe_dir>/screpacker-config.json`. Never fails: any IO or JSON
/// error yields a Config with empty fields.
pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

pub fn save(config: &Config) -> Result<()> {
    let path = config_path().ok_or_else(|| anyhow!("cannot determine executable directory"))?;
    let text = serde_json::to_string_pretty(config)
        .context("serialize config to JSON")?;
    fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
