use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use log::{error, info};

use crate::encoding::url_encode_file_name;
use crate::extraction_naming::include_hash_in_passage_file_name;
use crate::game_data::{
    GameData, ModuleType, UserDataType, CSS_MODULE_TYPE, CSS_TYPE, JS_MODULE_TYPE, JS_TYPE,
    PASSAGE_DIRECTORY, PASSAGE_EXTENSION, SYSTEM_LIBRARIES_FILE_NAME, SYSTEM_SCRIPTS_DIRECTORY,
    SYSTEM_SCRIPT_FILE_NAME,
};
use crate::game_data_reader;

pub fn build_diff(
    html_path: &Path,
    modified_sources_directory: &Path,
    diff_file: &Path,
) -> Result<()> {
    if !modified_sources_directory.is_dir() {
        error!(
            "Modified sources directory {} does not exist, cannot diff",
            modified_sources_directory.display()
        );
        anyhow::bail!("modified sources directory not found");
    }

    let data = game_data_reader::read_from_file(html_path)?;
    let originals = compute_originals(&data)?;
    let modified = read_modified(modified_sources_directory)?;

    if let Some(parent) = diff_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }

    let diff_text = build_diff_text(&originals, &modified);
    fs::write(diff_file, diff_text)
        .with_context(|| format!("write {}", diff_file.display()))?;
    info!("Wrote diff to {}", diff_file.display());
    Ok(())
}

/// Build the map of files that `game_data_extractor` would have written, but
/// computed entirely in-memory from the parsed `GameData`. Keys are
/// forward-slash relative paths (e.g. `css/style.css`).
pub(crate) fn compute_originals(data: &GameData) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();

    add_user_data(&mut out, data, &data.styles, &CSS_TYPE);
    add_user_data(&mut out, data, &data.scripts, &JS_TYPE);
    add_passages(&mut out, data);

    if let Some(modules) = &data.modules {
        add_modules(&mut out, data, &modules.script_modules);
        add_modules(&mut out, data, &modules.style_modules);
    }

    add_system_components(&mut out, data);

    Ok(out)
}

fn add_system_components(out: &mut BTreeMap<String, String>, data: &GameData) {
    if let Some(ranges) = data.system_components.sugarcube_libraries {
        let content =
            data.document_raw_text[ranges.inner.start..ranges.inner.end].to_string();
        out.insert(
            format!("{SYSTEM_SCRIPTS_DIRECTORY}/{SYSTEM_LIBRARIES_FILE_NAME}"),
            content,
        );
    }
    if let Some(ranges) = data.system_components.sugarcube_script {
        let content =
            data.document_raw_text[ranges.inner.start..ranges.inner.end].to_string();
        out.insert(
            format!("{SYSTEM_SCRIPTS_DIRECTORY}/{SYSTEM_SCRIPT_FILE_NAME}"),
            content,
        );
    }
}

fn add_user_data(
    out: &mut BTreeMap<String, String>,
    _data: &GameData,
    user_data: &crate::game_data::UserData,
    ty: &UserDataType,
) {
    for entry in &user_data.entries {
        let content = user_data.user_data_raw_text
            [entry.raw_text_range.start..entry.raw_text_range.end]
            .to_string();
        let rel = format!("{}/{}", ty.directory, entry.disk_file_name);
        out.insert(rel, content);
    }
}

fn add_modules(
    out: &mut BTreeMap<String, String>,
    data: &GameData,
    modules: &crate::game_data::ModulesOfType,
) {
    let ty: &ModuleType = modules.module_type;
    for entry in &modules.entries {
        let file_name =
            url_encode_file_name(&format!("{}.{}", entry.module_name, ty.file_extension));
        let content =
            data.document_raw_text[entry.ranges.inner.start..entry.ranges.inner.end].to_string();
        let rel = format!("{}/{}", ty.directory_name, file_name);
        out.insert(rel, content);
    }
}

fn add_passages(out: &mut BTreeMap<String, String>, data: &GameData) {
    let file_names = passage_file_names(&data.passages);
    for (passage, file_name) in data.passages.iter().zip(file_names) {
        let mut content = if passage.tags.is_empty() {
            format!("::{}\n", passage.name)
        } else {
            format!("::{} [{}]\n", passage.name, passage.tags.join(","))
        };
        content.push_str(&passage.decoded_text);
        let rel = format!("{}/{}", PASSAGE_DIRECTORY, file_name);
        out.insert(rel, content);
    }
}

/// Assign each passage its on-disk file name, mirroring the extractor's
/// case-collision handling: if two names collide case-insensitively, BOTH
/// get the SHA-1 hash suffix.
fn passage_file_names(passages: &[crate::game_data::Passage]) -> Vec<String> {
    let initial: Vec<String> = passages
        .iter()
        .map(|p| url_encode_file_name(&format!("{}.{}", p.name, PASSAGE_EXTENSION)))
        .collect();

    let mut lc_counts: HashMap<String, usize> = HashMap::new();
    for name in &initial {
        *lc_counts.entry(name.to_lowercase()).or_insert(0) += 1;
    }

    let suffix = format!(".{PASSAGE_EXTENSION}");
    initial
        .into_iter()
        .map(|name| {
            if *lc_counts.get(&name.to_lowercase()).unwrap_or(&0) > 1 {
                let stem = name.strip_suffix(&suffix).unwrap_or(&name);
                format!("{}{}", include_hash_in_passage_file_name(stem), suffix)
            } else {
                name
            }
        })
        .collect()
}

/// Walk the modified-sources directory and collect the same set of tracked
/// files by naming matcher that the extractor writes.
fn read_modified(dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();

    read_directory_with_matcher(&mut out, dir, CSS_TYPE.directory, CSS_TYPE.naming_matcher)?;
    read_directory_with_matcher(&mut out, dir, JS_TYPE.directory, JS_TYPE.naming_matcher)?;
    read_directory_with_matcher(&mut out, dir, PASSAGE_DIRECTORY, |n| {
        n.ends_with(&format!(".{PASSAGE_EXTENSION}"))
    })?;
    read_directory_with_matcher(
        &mut out,
        dir,
        CSS_MODULE_TYPE.directory_name,
        CSS_MODULE_TYPE.naming_matcher,
    )?;
    read_directory_with_matcher(
        &mut out,
        dir,
        JS_MODULE_TYPE.directory_name,
        JS_MODULE_TYPE.naming_matcher,
    )?;
    // Only the two known system-script slot files are recognised — any
    // other .js dropped in here is silently ignored, matching what the
    // merger can actually splice back into the HTML.
    read_directory_with_matcher(&mut out, dir, SYSTEM_SCRIPTS_DIRECTORY, |n| {
        n == SYSTEM_LIBRARIES_FILE_NAME || n == SYSTEM_SCRIPT_FILE_NAME
    })?;

    Ok(out)
}

fn read_directory_with_matcher(
    out: &mut BTreeMap<String, String>,
    base: &Path,
    subdir: &str,
    matcher: fn(&str) -> bool,
) -> Result<()> {
    let directory = base.join(subdir);
    let entries = match fs::read_dir(&directory) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("read_dir {}", directory.display()));
        }
    };

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !matcher(name) {
            continue;
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let content = crate::encoding::remove_cr_newlines(&content);
        let rel = format!("{subdir}/{name}");
        out.insert(rel, content);
    }
    Ok(())
}

pub(crate) fn build_diff_text(
    originals: &BTreeMap<String, String>,
    modified: &BTreeMap<String, String>,
) -> String {
    let mut keys: Vec<&str> = originals
        .keys()
        .chain(modified.keys())
        .map(String::as_str)
        .collect();
    keys.sort();
    keys.dedup();

    let mut out = String::new();
    let mut unchanged = 0u32;
    let mut modified_count = 0u32;
    let mut added = 0u32;
    let mut deleted = 0u32;

    for rel in keys {
        let orig = originals.get(rel);
        let modif = modified.get(rel);

        match (orig, modif) {
            (Some(o), Some(m)) if o == m => {
                unchanged += 1;
            }
            (Some(o), Some(m)) => {
                write_diff_block(&mut out, rel, Some(o), Some(m));
                modified_count += 1;
            }
            (Some(o), None) => {
                write_diff_block(&mut out, rel, Some(o), None);
                deleted += 1;
            }
            (None, Some(m)) => {
                write_diff_block(&mut out, rel, None, Some(m));
                added += 1;
            }
            (None, None) => unreachable!("key from union must be in at least one map"),
        }
    }

    info!(
        "Diff: modified: {}, added: {}, deleted: {}, unchanged: {}",
        modified_count, added, deleted, unchanged
    );
    out
}

fn write_diff_block(out: &mut String, rel: &str, original: Option<&str>, modified: Option<&str>) {
    let _ = writeln!(out, "diff --git \"a/{rel}\" \"b/{rel}\"");

    if original.is_none() {
        out.push_str("new file mode 100644\n");
    } else if modified.is_none() {
        out.push_str("deleted file mode 100644\n");
    }

    let orig_header = match original {
        Some(_) => format!("\"a/{rel}\""),
        None => "/dev/null".to_string(),
    };
    let mod_header = match modified {
        Some(_) => format!("\"b/{rel}\""),
        None => "/dev/null".to_string(),
    };

    // Emit the `---`/`+++` headers ourselves so we can always quote paths,
    // matching git's `core.quotePath=on` format that the Kotlin patcher expects.
    // flickzeug's filename support re-escapes our quotes, so we only feed it
    // the bodies and pull out just the hunks.
    let _ = writeln!(out, "--- {orig_header}");
    let _ = writeln!(out, "+++ {mod_header}");

    let patch = flickzeug::create_patch(original.unwrap_or(""), modified.unwrap_or(""));
    for hunk in patch.hunks() {
        let _ = write!(out, "{hunk}");
    }
}
