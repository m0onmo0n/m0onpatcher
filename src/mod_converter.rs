use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::{self, Config};
use crate::conflict::{ConflictManager, ConflictResolutionIndex, TemplateConflictRecorder};
use crate::game_data::GameData;
use crate::game_data_differ;
use crate::game_data_patcher;
use crate::game_data_reader;
use crate::mod_data::ModData;
use crate::mod_data_reader;
use crate::mod_error_registry::ModErrorRegistry;

/// Path + size pair we use to detect when a version's HTML has been
/// replaced on disk between scans. Cheap to compare and doesn't need a
/// hash; if the size is identical and the path identical we treat it as
/// the same file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlFileInfo {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug)]
pub struct ConverterVersionInfo {
    pub version: String,
    pub html_file_info: Option<HtmlFileInfo>,
    /// Tri-state load slot:
    /// - `None` — never attempted to load the GameData yet.
    /// - `Some(Ok(_))` — successfully parsed; reuse it.
    /// - `Some(Err(_))` — load was attempted and failed; we don't
    ///   retry until something invalidates this entry (e.g. the
    ///   underlying HTML changes, which `scan_versions` detects and
    ///   wipes the slot back to `None`).
    ///
    /// The successful payload is wrapped in `Arc` so the conversion
    /// worker thread can take a cheap clone of the parsed data
    /// without copying / re-parsing.
    pub game_data: Option<Result<Arc<GameData>>>,
    /// True when the version came from `<base_directory>/versions`,
    /// false when it came from the executable directory's bundled
    /// `versions/` folder.
    pub from_base_directory: bool,
}

#[derive(Debug, Default)]
pub struct ConverterVersionIndex {
    pub versions: HashMap<String, ConverterVersionInfo>,
}

impl ConverterVersionIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Refresh the in-memory version map from disk.
    ///
    /// Looks at `<executable_dir>/versions/` and (if `base_directory`
    /// is provided) `<base>/versions/`. Each `vX.html` file whose
    /// `X` starts with an ASCII digit becomes/updates an entry. Files
    /// in the base-directory location override matches in the
    /// executable directory and flip `from_base_directory` to true.
    ///
    /// Diff semantics on re-scan:
    /// - If an existing entry's `html_file_info` differs from the
    ///   freshly-scanned one, replace it and clear any cached
    ///   `game_data` (the file changed so the parsed state is stale).
    /// - If the entry was missing, add it with `game_data = None`.
    /// - If a previously-known entry isn't present in the fresh scan,
    ///   drop it.
    pub fn scan_versions(&mut self, base_directory: Option<&Path>) {
        let mut fresh: HashMap<String, (HtmlFileInfo, bool)> = HashMap::new();

        if let Some(exe_dir) = config::executable_dir() {
            collect_html_versions(&exe_dir.join("versions"), false, &mut fresh);
        }
        if let Some(base) = base_directory {
            collect_html_versions(&base.join("versions"), true, &mut fresh);
        }

        for (version, (info, from_base)) in &fresh {
            match self.versions.get_mut(version) {
                Some(existing) => {
                    if existing.html_file_info.as_ref() != Some(info) {
                        existing.html_file_info = Some(info.clone());
                        existing.game_data = None;
                    }
                    existing.from_base_directory = *from_base;
                }
                None => {
                    self.versions.insert(
                        version.clone(),
                        ConverterVersionInfo {
                            version: version.clone(),
                            html_file_info: Some(info.clone()),
                            game_data: None,
                            from_base_directory: *from_base,
                        },
                    );
                }
            }
        }

        self.versions.retain(|k, _| fresh.contains_key(k));
    }

    /// Ensure `version` exists in the map and, if its HTML is
    /// available on disk, has its GameData loaded. Specifically:
    ///
    /// - If the entry doesn't exist, insert a placeholder with no
    ///   HTML file info and `from_base_directory = false`.
    /// - If the entry exists, has an HTML file, and hasn't been
    ///   loaded yet (`game_data == None`), parse the HTML and stash
    ///   the result (success or error) in `game_data`.
    /// - If a previous attempt already failed (`game_data ==
    ///   Some(Err(_))`), don't retry — the caller has to invalidate
    ///   the entry first (e.g. by fixing the file and triggering a
    ///   re-scan).
    pub fn prepare_version(&mut self, version: &str) {
        let info = self
            .versions
            .entry(version.to_string())
            .or_insert_with(|| ConverterVersionInfo {
                version: version.to_string(),
                html_file_info: None,
                game_data: None,
                from_base_directory: false,
            });

        if info.game_data.is_some() {
            // Already loaded successfully or already failed — either
            // way, nothing more to do here.
            return;
        }
        let Some(html_info) = info.html_file_info.as_ref() else {
            // No HTML on disk yet, so there's nothing to load.
            return;
        };
        info.game_data =
            Some(game_data_reader::read_from_file(&html_info.path).map(Arc::new));
    }
}

/// Walk one directory looking for `v<version>.html` files. Caller
/// passes `from_base = true` when scanning the user's base-directory
/// override; that flag flows through to `(HtmlFileInfo, bool)` and is
/// later copied onto `ConverterVersionInfo.from_base_directory`.
/// Errors (directory missing, unreadable entries, no metadata) are
/// silently skipped — the scan is best-effort.
fn collect_html_versions(
    dir: &Path,
    from_base: bool,
    out: &mut HashMap<String, (HtmlFileInfo, bool)>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        let Some(stem) = lower.strip_suffix(".html") else {
            continue;
        };
        let Some(rest) = stem.strip_prefix('v') else {
            continue;
        };
        let Some(first) = rest.chars().next() else {
            continue;
        };
        if !first.is_ascii_digit() {
            continue;
        }
        let size = match fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        out.insert(
            rest.to_string(),
            (HtmlFileInfo { path, size }, from_base),
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConverterModType {
    Diff,
    Mod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConverterModStatus {
    Unprocessed,
    Converted,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ConverterModInfo {
    pub name: String,
    pub mod_type: ConverterModType,
    pub contents: Vec<u8>,
    pub game_version: String,
    pub status: ConverterModStatus,
}

/// Inventory of every mod the converter knows about, keyed by file
/// name. `target_version` names the destination dir (`to-v<version>`)
/// the converter is producing into; mods whose filename also exists
/// inside that dir are marked Converted. The index is rebuilt
/// wholesale on each scan; the only carry-over from the previous
/// state is sticky Failed-with-unchanged-bytes statuses (so a mod
/// that failed last conversion stays flagged until it's edited).
#[derive(Debug, Default)]
pub struct ConverterModIndex {
    pub mods: HashMap<String, ConverterModInfo>,
    pub target_version: Option<String>,
}

impl ConverterModIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Walk `base_directory` and rebuild the mod inventory.
    ///
    /// Scans every immediate subdirectory whose name is `from-v<X>`
    /// (X must start with a digit) for `.mod` and `.diff` files; each
    /// becomes/replaces an entry keyed by its file name. Then picks
    /// the first `to-v<X>` directory (alphabetical) as the target,
    /// marks any mod whose filename also lives in there as
    /// Converted. Any mod that was Failed in the previous map and
    /// whose `contents` + `game_version` haven't changed retains its
    /// Failed status; everything else starts Unprocessed.
    pub fn scan_mods(&mut self, base_directory: &Path) {
        let mut from_dirs: Vec<(String, PathBuf)> = Vec::new();
        let mut to_dirs: Vec<(String, PathBuf)> = Vec::new();

        match fs::read_dir(base_directory) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let Ok(ft) = entry.file_type() else {
                        continue;
                    };
                    if !ft.is_dir() {
                        continue;
                    }
                    let path = entry.path();
                    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    if let Some(version) = parse_version_dir(name, "from-v") {
                        from_dirs.push((version, path));
                    } else if let Some(version) = parse_version_dir(name, "to-v") {
                        to_dirs.push((version, path));
                    }
                }
            }
            Err(_) => {
                // Unreadable base — empty index, no target.
                self.mods.clear();
                self.target_version = None;
                return;
            }
        }

        from_dirs.sort_by(|a, b| a.0.cmp(&b.0));
        to_dirs.sort_by(|a, b| a.0.cmp(&b.0));

        let mut new_mods: HashMap<String, ConverterModInfo> = HashMap::new();
        for (version, dir) in &from_dirs {
            let Ok(files) = fs::read_dir(dir) else {
                continue;
            };
            for entry in files.flatten() {
                let path = entry.path();
                let Some(file_name) = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
                else {
                    continue;
                };
                let lower = file_name.to_ascii_lowercase();
                let mod_type = if lower.ends_with(".diff") {
                    ConverterModType::Diff
                } else if lower.ends_with(".mod") {
                    ConverterModType::Mod
                } else {
                    continue;
                };
                let stem = Path::new(&file_name)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| file_name.clone());
                let contents = match fs::read(&path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                new_mods.insert(
                    file_name,
                    ConverterModInfo {
                        name: stem,
                        mod_type,
                        contents,
                        game_version: version.clone(),
                        status: ConverterModStatus::Unprocessed,
                    },
                );
            }
        }

        let target = to_dirs.into_iter().next();
        let target_version = target.as_ref().map(|(v, _)| v.clone());

        if let Some((_, target_dir)) = target.as_ref() {
            if let Ok(files) = fs::read_dir(target_dir) {
                for entry in files.flatten() {
                    let Some(file_name) = entry
                        .file_name()
                        .to_str()
                        .map(|s| s.to_string())
                    else {
                        continue;
                    };
                    if let Some(info) = new_mods.get_mut(&file_name) {
                        info.status = ConverterModStatus::Converted;
                    }
                }
            }
        }

        // Carry over Failed status when neither the bytes nor the
        // game version moved — sticky so the user keeps seeing the
        // failure flag until they actually change the source mod.
        for (filename, info) in new_mods.iter_mut() {
            if let Some(prev) = self.mods.get(filename) {
                if prev.status == ConverterModStatus::Failed
                    && prev.contents == info.contents
                    && prev.game_version == info.game_version
                    && info.status == ConverterModStatus::Unprocessed
                {
                    info.status = ConverterModStatus::Failed;
                }
            }
        }

        self.mods = new_mods;
        self.target_version = target_version;
    }
}

/// Match a directory name like `from-v1.0.3` (or `to-v…`) against the
/// given prefix and return the version substring iff its first char
/// is an ASCII digit. Same rule the version-HTML scanner uses.
fn parse_version_dir(name: &str, prefix: &str) -> Option<String> {
    let rest = name.strip_prefix(prefix)?;
    let first = rest.chars().next()?;
    if !first.is_ascii_digit() {
        return None;
    }
    Some(rest.to_string())
}

/// Peek inside a zip; if the archive contains exactly one `.html`
/// file, read its bytes into memory and return both the in-archive
/// entry name and the bytes. Other entries (and the case where there
/// are zero or multiple `.html` entries) yield `None` — the caller
/// decides how to surface that.
///
/// We slurp the bytes here rather than just returning the entry name
/// so callers don't have to keep the zip around: a "confirm" click in
/// a deferred UI dialog can write straight from the buffer instead of
/// re-opening the archive.
pub fn read_single_html_from_zip(zip_path: &Path) -> Option<(String, Vec<u8>)> {
    use std::io::Read;

    let file = File::open(zip_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut html_count = 0usize;
    let mut first_index: Option<usize> = None;
    let mut first_name: Option<String> = None;
    for i in 0..archive.len() {
        let entry = archive.by_index(i).ok()?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name();
        if name.to_ascii_lowercase().ends_with(".html") {
            html_count += 1;
            if first_name.is_none() {
                first_name = Some(name.to_string());
                first_index = Some(i);
            }
        }
    }
    if html_count != 1 {
        return None;
    }
    let mut entry = archive.by_index(first_index?).ok()?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf).ok()?;
    Some((first_name?, buf))
}

/// Write a pre-read HTML byte buffer to
/// `<executable_dir>/versions/v<version>.html`, creating the parent
/// directory if needed. Returns the destination path on success.
pub fn write_html_to_versions(bytes: &[u8], version: &str) -> Result<PathBuf> {
    let exe_dir =
        config::executable_dir().ok_or_else(|| anyhow!("cannot determine executable directory"))?;
    let versions_dir = exe_dir.join("versions");
    fs::create_dir_all(&versions_dir)
        .with_context(|| format!("create {}", versions_dir.display()))?;
    let out_path = versions_dir.join(format!("v{version}.html"));
    fs::write(&out_path, bytes).with_context(|| format!("write {}", out_path.display()))?;
    Ok(out_path)
}

/// Read every `.mod` and `.diff` entry out of a zip into memory.
/// Subdirectory structure is flattened — only the entry's file name is
/// kept. Errors at any point in the scan are silently skipped (they're
/// rare and the caller can detect "no mod files" from the empty
/// vector). Slurped at drop time so a later dialog confirm doesn't
/// have to reopen the archive.
pub fn read_mod_files_from_zip(zip_path: &Path) -> Vec<(String, Vec<u8>)> {
    use std::io::Read;

    let Ok(file) = File::open(zip_path) else {
        return Vec::new();
    };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        let entry_name = entry.name().to_string();
        let lower = entry_name.to_ascii_lowercase();
        if !lower.ends_with(".mod") && !lower.ends_with(".diff") {
            continue;
        }
        let file_name = Path::new(&entry_name)
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| entry_name.clone());
        let mut buf = Vec::with_capacity(entry.size() as usize);
        if entry.read_to_end(&mut buf).is_err() {
            continue;
        }
        out.push((file_name, buf));
    }
    out
}

/// Drop a batch of (filename, bytes) entries into
/// `<base_directory>/from-v<version>/`, creating the directory if it
/// doesn't exist. Each entry is written as a top-level file inside
/// that directory. Returns the destination directory on success.
pub fn write_mods_to_from_version_dir(
    base_directory: &Path,
    version: &str,
    files: &[(String, Vec<u8>)],
) -> Result<PathBuf> {
    let target_dir = base_directory.join(format!("from-v{version}"));
    fs::create_dir_all(&target_dir)
        .with_context(|| format!("create {}", target_dir.display()))?;
    for (name, bytes) in files {
        let path = target_dir.join(name);
        fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(target_dir)
}

/// Switch the converter's target version on disk:
///
/// 1. For every existing `to-v<X>` directory under `base_directory`,
///    move its contents into `tmp/inactive-to-v<X>` (creating the
///    inactive dir if needed, overwriting any existing files) and
///    then remove the original `to-v<X>` dir.
/// 2. Create an empty `to-v<new_target>` directory.
///
/// Idempotent only insofar as step 1 leaves no `to-v*` dirs behind;
/// re-running with the same target after step 2 simply re-archives
/// the (empty) `to-v<new_target>` into `tmp/inactive-to-v<new_target>`
/// and re-creates a fresh empty one. Callers should short-circuit when
/// the requested target already matches the current one.
/// Promote every "actually resolved" `.resolve` template file from
/// `<base>/tmp/resolve-templates/` to `<base>/conflicts/`. A template
/// is considered resolved when it no longer contains the placeholder
/// marker `--- REPLACE THIS WITH RESOLUTION ---` that
/// `TemplateConflictRecorder` writes for the user to fill in.
///
/// Returns the list of file names actually moved (so the caller can
/// log them). Files in the way at the destination are overwritten.
/// A missing `tmp/resolve-templates/` is treated as "nothing to do".
pub fn move_resolved_conflicts(base_directory: &Path) -> Result<Vec<String>> {
    const PLACEHOLDER: &str = "--- REPLACE THIS WITH RESOLUTION ---";

    let templates_dir = base_directory.join("tmp").join("resolve-templates");
    let conflicts_dir = base_directory.join("conflicts");
    let mut moved = Vec::new();

    let entries = match fs::read_dir(&templates_dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(moved),
        Err(err) => {
            return Err(anyhow::Error::from(err).context(format!(
                "read {}",
                templates_dir.display()
            )));
        }
    };

    let mut conflicts_dir_ready = false;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
        else {
            continue;
        };
        if !name.to_ascii_lowercase().ends_with(".resolve") {
            continue;
        }

        let contents = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(err) => {
                log::warn!("Failed to read {}: {err}", path.display());
                continue;
            }
        };
        if contents.contains(PLACEHOLDER) {
            continue;
        }

        if !conflicts_dir_ready {
            fs::create_dir_all(&conflicts_dir)
                .with_context(|| format!("create {}", conflicts_dir.display()))?;
            conflicts_dir_ready = true;
        }
        let dest = conflicts_dir.join(&name);
        // `fs::rename` on Windows refuses to overwrite, so explicitly
        // remove any existing file first.
        if dest.exists() {
            fs::remove_file(&dest)
                .with_context(|| format!("remove {}", dest.display()))?;
        }
        fs::rename(&path, &dest).with_context(|| {
            format!("rename {} -> {}", path.display(), dest.display())
        })?;
        moved.push(name);
    }

    Ok(moved)
}

pub fn switch_target_version(base_directory: &Path, new_target: &str) -> Result<()> {
    let entries = fs::read_dir(base_directory)
        .with_context(|| format!("read {}", base_directory.display()))?;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some(version) = parse_version_dir(name, "to-v") {
            let inactive = base_directory
                .join("tmp")
                .join(format!("inactive-to-v{version}"));
            move_dir_contents_overwrite(&path, &inactive).with_context(|| {
                format!("move {} into {}", path.display(), inactive.display())
            })?;
            fs::remove_dir_all(&path)
                .with_context(|| format!("remove {}", path.display()))?;
        }
    }

    let new_dir = base_directory.join(format!("to-v{new_target}"));
    fs::create_dir_all(&new_dir)
        .with_context(|| format!("create {}", new_dir.display()))?;
    Ok(())
}

/// Move a single mod file from `from-v<previous>/` into
/// `from-v<new>/` under `base_directory`. The destination directory
/// is created if missing; an existing file at the destination is
/// overwritten so the operation is idempotent if the user changes
/// their mind. Returns the new path.
pub fn move_mod_to_version(
    base_directory: &Path,
    previous_version: &str,
    new_version: &str,
    file_name: &str,
) -> Result<PathBuf> {
    let src = base_directory
        .join(format!("from-v{previous_version}"))
        .join(file_name);
    let dest_dir = base_directory.join(format!("from-v{new_version}"));
    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("create {}", dest_dir.display()))?;
    let dest = dest_dir.join(file_name);
    if dest.exists() {
        fs::remove_file(&dest)
            .with_context(|| format!("remove {}", dest.display()))?;
    }
    fs::rename(&src, &dest)
        .with_context(|| format!("rename {} -> {}", src.display(), dest.display()))?;
    Ok(dest)
}

/// "Soft delete" a mod file: move it from `from-v<previous>/` to
/// `tmp/removed-from-v<previous>/`. The file isn't actually unlinked
/// so the user can recover it manually if needed. An existing file at
/// the destination is overwritten. Returns the resting path.
pub fn delete_mod_file(
    base_directory: &Path,
    previous_version: &str,
    file_name: &str,
) -> Result<PathBuf> {
    let src = base_directory
        .join(format!("from-v{previous_version}"))
        .join(file_name);
    let dest_dir = base_directory
        .join("tmp")
        .join(format!("removed-from-v{previous_version}"));
    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("create {}", dest_dir.display()))?;
    let dest = dest_dir.join(file_name);
    if dest.exists() {
        fs::remove_file(&dest)
            .with_context(|| format!("remove {}", dest.display()))?;
    }
    fs::rename(&src, &dest)
        .with_context(|| format!("rename {} -> {}", src.display(), dest.display()))?;
    Ok(dest)
}

/// Move every file (and recursively, every directory) under `src`
/// into `dst`. Files in `dst` that share a name with one being moved
/// get overwritten. `src` itself is left in place — the caller deletes
/// it once we're done so partial-failure recovery is straightforward.
///
/// `dst` is only created when the function is actually about to move
/// something into it; a `src` that's empty (or contains only entries
/// that nothing-deeper ends up moving) leaves no trace on disk.
fn move_dir_contents_overwrite(src: &Path, dst: &Path) -> Result<()> {
    let entries = fs::read_dir(src).with_context(|| format!("read {}", src.display()))?;
    let mut dst_ready = false;
    let mut ensure_dst = |dst: &Path, ready: &mut bool| -> Result<()> {
        if !*ready {
            fs::create_dir_all(dst)
                .with_context(|| format!("create {}", dst.display()))?;
            *ready = true;
        }
        Ok(())
    };
    for entry in entries.flatten() {
        let from = entry.path();
        let Some(name) = from.file_name() else {
            continue;
        };
        let to = dst.join(name);
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_file() {
            ensure_dst(dst, &mut dst_ready)?;
            // `fs::rename` on Windows refuses to overwrite an existing
            // destination, so explicitly remove it first.
            if to.exists() {
                fs::remove_file(&to)
                    .with_context(|| format!("remove {}", to.display()))?;
            }
            fs::rename(&from, &to).with_context(|| {
                format!("rename {} -> {}", from.display(), to.display())
            })?;
        } else if ft.is_dir() {
            // Recursion creates its own `to` only if there's something
            // worth moving, which transitively creates `dst` as the
            // parent. We don't need to flip dst_ready ourselves.
            move_dir_contents_overwrite(&from, &to)?;
            fs::remove_dir_all(&from)
                .with_context(|| format!("remove {}", from.display()))?;
        }
    }
    Ok(())
}

// ---- Mod conversion -----------------------------------------------------

/// Snapshot fed into the background `convert` worker. Cloning these
/// structures up front means the worker is decoupled from any
/// concurrent mutation of the live indexes in the GUI.
///
/// GameData is shared via `Arc` so cloning the snapshot is cheap and
/// no parsing happens in the worker. Versions whose load failed (or
/// hadn't been attempted) at snapshot time are simply absent from
/// `source_data`; mods that reference them get a clean failure.
#[derive(Debug, Clone)]
pub struct ConvertInputs {
    pub base_directory: PathBuf,
    pub target_version: String,
    /// In-memory GameData for the target version. Caller pulls this
    /// out of `ConverterVersionIndex` and refuses to spawn when it
    /// isn't `Some(Ok(_))`.
    pub target_data: Arc<GameData>,
    /// Map of (game_version → loaded GameData) for every version any
    /// mod points to (and the target itself). Versions whose load
    /// failed or never happened are omitted.
    pub source_data: HashMap<String, Arc<GameData>>,
    /// Cloned mod entries to convert. Order doesn't matter here; the
    /// converter sorts internally for a deterministic log trail.
    pub mods: Vec<ConverterModInfo>,
    pub config: Config,
}

#[derive(Debug, Default)]
pub struct ConvertOutcome {
    pub successful: Vec<String>,
    pub failed: Vec<String>,
    /// True when the loop exited because the cancel flag was tripped
    /// rather than because all mods had been processed.
    pub cancelled: bool,
}

/// Convert every mod in `inputs.mods` to a diff against the target
/// version, writing each result into
/// `<base_directory>/to-v<target_version>/<mod-name>.diff`.
///
/// Cancellation is checked between mods (per the spec): a long-running
/// per-mod conversion finishes its current mod before exit, so the
/// resulting `to-v*` directory contents stay consistent with what's
/// reported in `successful`.
pub fn convert(inputs: ConvertInputs, cancel: Arc<AtomicBool>) -> ConvertOutcome {
    let mut outcome = ConvertOutcome::default();
    let to_dir = inputs
        .base_directory
        .join(format!("to-v{}", inputs.target_version));
    if let Err(err) = fs::create_dir_all(&to_dir) {
        log::error!("Failed to create {}: {err}", to_dir.display());
        return outcome;
    }

    let mut mods = inputs.mods.clone();
    mods.sort_by(|a, b| {
        a.game_version
            .cmp(&b.game_version)
            .then_with(|| a.name.cmp(&b.name))
    });

    log::info!(
        "Converting {} mods to v{} (output: {})",
        mods.len(),
        inputs.target_version,
        to_dir.display()
    );

    // One ConflictManager shared across every mod's upgrade pass —
    // matches the Kotlin runner. The resolution index is loaded once
    // from <base>/conflicts and <executable_dir>/conflicts (in the
    // same precedence order Kotlin uses), and the same recorder
    // appends every unresolved conflict to a single template
    // directory so the user can inspect them all in one place.
    let mut resolution_dirs: Vec<PathBuf> = Vec::new();
    resolution_dirs.push(inputs.base_directory.join("conflicts"));
    if let Some(exe_dir) = config::executable_dir() {
        resolution_dirs.push(exe_dir.join("conflicts"));
    }
    let template_dir = inputs
        .base_directory
        .join("tmp")
        .join("resolve-templates");
    let mut conflict_mgr = ConflictManager {
        index: ConflictResolutionIndex::load(&resolution_dirs),
        recorder: Box::new(TemplateConflictRecorder::new(template_dir)),
    };

    // Cache of `compute_originals` results keyed by game version.
    // Building one originals map walks every passage / module / user
    // data block, so we don't want to redo it once per mod when many
    // mods share the same source (or target) version.
    let mut originals_cache: HashMap<String, BTreeMap<String, String>> = HashMap::new();

    for m in &mods {
        if cancel.load(Ordering::Relaxed) {
            log::info!("Conversion cancelled before processing {}", m.name);
            outcome.cancelled = true;
            break;
        }
        match convert_one_mod(m, &inputs, &to_dir, &mut conflict_mgr, &mut originals_cache) {
            Ok(()) => {
                log::info!("Converted mod {}", m.name);
                outcome.successful.push(m.name.clone());
            }
            Err(err) => {
                log::error!("Mod {} conversion failed: {err:#}", m.name);
                outcome.failed.push(m.name.clone());
            }
        }
    }

    log::info!(
        "Conversion done: {} successful, {} failed{}",
        outcome.successful.len(),
        outcome.failed.len(),
        if outcome.cancelled { " (cancelled)" } else { "" }
    );

    outcome
}

/// Process a single mod into the target `to-v*` directory.
///
/// Two phases mirror Kotlin's flow:
///
/// 1. Produce a `source_diff_text` — for `.diff` mods that's just the
///    on-disk bytes; for `.mod` files we run KittyPatcher against the
///    source-version HTML and diff the resulting GameData back
///    against the unmodified source (`convert_kitty_mod_to_diff`).
/// 2. If `source_version == target_version`, write that diff straight
///    out. Otherwise upgrade it to the target version via a 3-way
///    merge against the version-upgrade diff (`upgrade_diff`).
fn convert_one_mod(
    m: &ConverterModInfo,
    inputs: &ConvertInputs,
    to_dir: &Path,
    conflict_mgr: &mut ConflictManager,
    originals_cache: &mut HashMap<String, BTreeMap<String, String>>,
) -> Result<()> {
    let source_diff_text = match m.mod_type {
        ConverterModType::Diff => String::from_utf8(m.contents.clone())
            .map_err(|err| anyhow!(".diff is not valid UTF-8: {err}"))?,
        ConverterModType::Mod => convert_kitty_mod_to_diff(m, inputs, originals_cache)?,
    };

    let target_diff_text = if m.game_version == inputs.target_version {
        source_diff_text
    } else {
        upgrade_diff(
            &m.name,
            &source_diff_text,
            &m.game_version,
            &inputs.target_version,
            inputs,
            conflict_mgr,
            originals_cache,
        )?
    };

    let dest = to_dir.join(format!("{}.diff", m.name));
    fs::write(&dest, target_diff_text)
        .with_context(|| format!("write {}", dest.display()))?;
    Ok(())
}

/// Cached `compute_originals` lookup. Lazily inserts on first miss
/// for a given version; subsequent calls reuse the existing
/// BTreeMap. The borrow split (contains_key check, then mutate, then
/// fetch) avoids tripping NLL even though we only carry an `&'a mut`.
fn cached_originals<'a>(
    cache: &'a mut HashMap<String, BTreeMap<String, String>>,
    version: &str,
    data: &GameData,
) -> Result<&'a BTreeMap<String, String>> {
    if !cache.contains_key(version) {
        let originals = game_data_differ::compute_originals(data)?;
        cache.insert(version.to_string(), originals);
    }
    Ok(cache
        .get(version)
        .expect("entry was just inserted or already present"))
}

/// Run KittyPatcher with the source-version HTML + a single `.mod`
/// payload, then diff the resulting GameData back against the
/// unmodified source-version GameData. Returns the resulting diff
/// text — i.e. what would have lived in `from-v<source>/<mod>.diff`
/// if the user had authored a .diff directly.
///
/// KittyPatcher needs an HTML file on disk to operate on; we
/// reconstruct it from the in-memory `document_raw_text` rather than
/// re-reading from the original location. The mod payload bytes are
/// written to the same staging area so `perform_kitty_patch` can pick
/// them up by path, then both files are cleaned up.
fn convert_kitty_mod_to_diff(
    m: &ConverterModInfo,
    inputs: &ConvertInputs,
    originals_cache: &mut HashMap<String, BTreeMap<String, String>>,
) -> Result<String> {
    let Some(source_data) = inputs.source_data.get(&m.game_version) else {
        bail!(
            "source GameData for v{} not loaded — version's HTML is missing or failed to parse",
            m.game_version
        );
    };

    let staging = inputs.base_directory.join("tmp").join("convert-source");
    fs::create_dir_all(&staging)
        .with_context(|| format!("create {}", staging.display()))?;
    let staged_html = staging.join(format!("source-v{}.html", m.game_version));
    fs::write(&staged_html, source_data.document_raw_text.as_bytes())
        .with_context(|| format!("write {}", staged_html.display()))?;
    let staged_mod = staging.join(format!("{}.mod", m.name));
    fs::write(&staged_mod, &m.contents)
        .with_context(|| format!("write {}", staged_mod.display()))?;

    let kitty_result = game_data_patcher::perform_kitty_patch(
        &staged_html,
        &inputs.config,
        &[staged_mod.clone()],
    );
    let _ = fs::remove_file(&staged_mod);

    let patched = kitty_result.map_err(|err| anyhow!("KittyPatcher failed: {err}"))?;
    // Patched output is per-mod (different mods produce different
    // patched HTMLs), so don't cache it. The unmodified source-version
    // map is shared across every mod for that version, so it does go
    // through the cache.
    let modded_state = game_data_differ::compute_originals(&patched)?;
    let source_state = cached_originals(originals_cache, &m.game_version, source_data)?;
    let diff_text = game_data_differ::build_diff_text(source_state, &modded_state);
    Ok(diff_text)
}

/// Cross-version diff upgrade: take a diff that targets
/// `source_version`, produce an equivalent diff against
/// `target_version`. The strategy:
///
/// - originals = source-version GameData (unmodified)
/// - current = target-version GameData (unmodified, mutable copy)
/// - apply the source diff to `current` via the 3-way-merge-aware
///   `apply_mod` (so changes already present in target version flow
///   through the resolver instead of producing spurious conflicts)
/// - diff(target_unmodified, current) is the new mod diff against
///   the target version.
fn upgrade_diff(
    mod_name: &str,
    source_diff_text: &str,
    source_version: &str,
    target_version: &str,
    inputs: &ConvertInputs,
    conflict_mgr: &mut ConflictManager,
    originals_cache: &mut HashMap<String, BTreeMap<String, String>>,
) -> Result<String> {
    let Some(source_data) = inputs.source_data.get(source_version) else {
        bail!(
            "source GameData for v{source_version} not loaded — version's HTML is missing or failed to parse"
        );
    };

    // `cached_originals` borrows `originals_cache` mutably; we have
    // to clone the resulting BTreeMap (instead of holding a borrow)
    // because the second cache call below needs the same mutable
    // borrow.
    let source_state = cached_originals(originals_cache, source_version, source_data)?.clone();
    let target_state =
        cached_originals(originals_cache, target_version, &inputs.target_data)?.clone();
    let mut current = target_state.clone();

    // Build a synthetic ModData carrying the upgrade payload. Using
    // mod_data_reader's validator gives the same modified-files set
    // the regular .diff loading path would, so apply_mod's logging /
    // file-list expectations match.
    let modified_files = mod_data_reader::determine_modified_files_and_validate(source_diff_text)
        .map_err(|err| anyhow!("source diff invalid: {err}"))?;
    let mod_data = ModData {
        mod_name: mod_name.to_string(),
        diff_file_name: format!("{mod_name}.diff"),
        dependencies: Vec::new(),
        diff_text: source_diff_text.to_string(),
        modified_files,
        max_fuzz: None,
    };

    conflict_mgr.recorder.set_comment(
        &format!(
            "Conflict when upgrading mod {mod_name} from v{source_version} to v{target_version}\n\
             Section A: source-version block with mod applied, Section B: source-version block, Section C: target-version block\n\
             Replace the placeholder in the end with a version of the code that combines both changes from B->A, and B->C"
        ),
        1,
    );
    let mut registry = ModErrorRegistry::new();
    let scratch = inputs.base_directory.join("tmp").join("convert-scratch");
    fs::create_dir_all(&scratch)
        .with_context(|| format!("create {}", scratch.display()))?;

    let applied = game_data_patcher::apply_mod(
        &source_state,
        &[],
        &mut current,
        &mod_data,
        &scratch,
        conflict_mgr,
        &mut registry,
    )?;
    if !applied {
        bail!(
            "3-way merge failed during v{source_version} → v{target_version} upgrade"
        );
    }

    let _ = registry; // failure counts already surfaced via apply_mod's logging.

    Ok(game_data_differ::build_diff_text(&target_state, &current))
}
