//! Single entry point per subcommand, shared between the CLI dispatcher in
//! `main.rs` and the button handlers in `gui.rs`. All path derivations and
//! ConflictManager construction live here so both front-ends stay in sync.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::config;
use crate::conflict::{ConflictManager, ConflictResolutionIndex, TemplateConflictRecorder};
use crate::{
    game_data_differ, game_data_extractor, game_data_live_merger, game_data_merger,
    game_data_patcher,
};

pub fn run_extract(html_path: &Path) -> Result<()> {
    let output_dir = html_parent(html_path)?.join("src");
    game_data_extractor::extract(html_path, &output_dir)
}

pub fn run_merge(html_path: &Path) -> Result<()> {
    let parent = html_parent(html_path)?;
    let extracted = parent.join("src");
    let output = derive_output(html_path, "merged")?;
    game_data_merger::merge(html_path, &output, &extracted)
}

pub fn run_diff(html_path: &Path) -> Result<()> {
    let parent = html_parent(html_path)?;
    let modified = parent.join("src");
    let diff_file = parent.join("mods").join("default.diff");
    game_data_differ::build_diff(html_path, &modified, &diff_file)
}

pub fn run_patch(html_path: &Path) -> Result<()> {
    let parent = html_parent(html_path)?;
    let mods_dir = parent.join("mods");
    let source_patch_dir = parent.join("tmp").join("src-patch");
    let template_dir = parent.join("tmp").join("resolve-templates");

    let mut resolution_dirs: Vec<PathBuf> = Vec::new();
    if let Some(p) = tool_conflicts_directory() {
        resolution_dirs.push(p);
    }
    resolution_dirs.push(mods_dir.join("conflicts"));

    let mut conflict_manager = ConflictManager {
        index: ConflictResolutionIndex::load(&resolution_dirs),
        recorder: Box::new(TemplateConflictRecorder::new(template_dir)),
    };

    let config = config::load();
    game_data_patcher::patch(
        html_path,
        &mods_dir,
        &source_patch_dir,
        &mut conflict_manager,
        &config,
    )?;
    Ok(())
}

pub fn run_livemerge(html_path: &Path) -> Result<()> {
    // CLI usage: cancellation is via the user Ctrl+C-ing the process,
    // so the flag is constructed and never flipped.
    run_livemerge_with_cancel(html_path, Arc::new(AtomicBool::new(false)))
}

/// Same as `run_livemerge` but takes an external cancel flag. The GUI owns
/// one of these so its "Stop" button can unblock the watch loop.
pub fn run_livemerge_with_cancel(html_path: &Path, cancel: Arc<AtomicBool>) -> Result<()> {
    let parent = html_parent(html_path)?;
    let extracted = parent.join("src");
    let output = derive_output(html_path, "livemerged")?;
    game_data_live_merger::live_merge(html_path, &output, &extracted, cancel)
}

pub fn html_parent(html_path: &Path) -> Result<PathBuf> {
    Ok(html_path
        .parent()
        .ok_or_else(|| anyhow!("html path has no parent directory"))?
        .to_path_buf())
}

pub fn derive_output(html_path: &Path, label: &str) -> Result<PathBuf> {
    let parent = html_parent(html_path)?;
    let stem = html_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("html path has no file stem"))?;
    Ok(parent.join(format!("{stem}.{label}.html")))
}

/// Kotlin's `PathTools.findToolDirectory().resolve("conflicts")` — shared
/// resolutions installed alongside the binary. Returns None if the
/// executable path can't be determined.
fn tool_conflicts_directory() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("conflicts")))
}
