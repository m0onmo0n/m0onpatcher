use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use log::{debug, error, info};
use regex::Regex;

use crate::game_data::{
    CSS_MODULE_TYPE, CSS_TYPE, JS_MODULE_TYPE, JS_TYPE, PASSAGE_DIRECTORY, PASSAGE_EXTENSION,
    SYSTEM_LIBRARIES_FILE_NAME, SYSTEM_SCRIPTS_DIRECTORY, SYSTEM_SCRIPT_FILE_NAME,
};
use crate::mod_data::ModData;
use crate::mod_error_registry::ModErrorRegistry;

/// Reads mods from `mods_directory`. `.diff` files are paired with an
/// optional sibling `<stem>.json` metadata file; `.zip` files are
/// treated as a self-contained bundle containing exactly one `.diff`
/// and zero-or-one `.json`. Any other extensions are ignored. Each
/// produced `ModData` is validated so that each path it modifies sits
/// in one of the five allowed subdirectories with a matching extension.
pub fn read_mods(mods_directory: &Path, registry: &mut ModErrorRegistry) -> Result<Vec<ModData>> {
    let mut mods = Vec::new();

    let entries = match fs::read_dir(mods_directory) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(mods);
        }
        Err(err) => return Err(err.into()),
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

        if name.ends_with(".diff") {
            match read_mod_from_diff(&path, name, registry) {
                Ok(Some(data)) => mods.push(data),
                Ok(None) => {}
                Err(err) => {
                    error!("Failed to read mod {}: {err}", path.display());
                    registry.record_failure(
                        stem(name).unwrap_or(name),
                        name,
                        &format!("Failed to read or parse diff: {err}"),
                    );
                }
            }
        } else if name.ends_with(".zip") {
            if let Some(data) = read_mod_from_zip(&path, name, registry) {
                mods.push(data);
            }
        }
    }

    mods.sort_by(|a, b| a.mod_name.to_lowercase().cmp(&b.mod_name.to_lowercase()));
    Ok(mods)
}

fn read_mod_from_diff(
    diff_path: &Path,
    file_name: &str,
    registry: &mut ModErrorRegistry,
) -> Result<Option<ModData>> {
    let json = match load_json_companion(diff_path) {
        Ok(j) => j,
        Err(err) => {
            error!(
                "Failed to parse JSON file for {}: {err:#}",
                diff_path.display()
            );
            registry.record_failure(
                stem(file_name).unwrap_or(file_name),
                file_name,
                "JSON file in invalid format",
            );
            return Ok(None);
        }
    };
    let diff_text = fs::read_to_string(diff_path)?;
    Ok(parse_mod_data(file_name, diff_text, &json, registry))
}

fn read_mod_from_zip(
    zip_path: &Path,
    file_name: &str,
    registry: &mut ModErrorRegistry,
) -> Option<ModData> {
    let zip_data = match read_mod_zip_contents(zip_path) {
        Ok(d) => d,
        Err(err) => {
            error!("Failed to parse ZIP file {}: {err:#}", zip_path.display());
            registry.record_failure(
                stem(file_name).unwrap_or(file_name),
                file_name,
                "Zip file parsing failed",
            );
            return None;
        }
    };

    // A "mod zip" is defined as exactly one .diff plus at most one
    // .json. Anything else is assumed to be some other archive dropped
    // into mods/ — e.g. the user's dump of their mod folder — and
    // silently skipped. No registry entry for that case, matching
    // Kotlin's `logger.info` + return null.
    if zip_data.diff_count != 1 || zip_data.json_count > 1 || zip_data.first_diff.is_none() {
        info!(
            "Skipping zip {file_name} - contains {} diffs and {} jsons (1 and 0-1 expected), assuming not intended as a mod zip",
            zip_data.diff_count, zip_data.json_count
        );
        return None;
    }

    let (_diff_entry_name, diff_bytes) = zip_data.first_diff.unwrap();
    let diff_text = match String::from_utf8(diff_bytes) {
        Ok(t) => t,
        Err(err) => {
            error!("Diff inside zip {file_name} is not valid UTF-8: {err}");
            registry.record_failure(
                stem(file_name).unwrap_or(file_name),
                file_name,
                "Failed to read or parse diff",
            );
            return None;
        }
    };

    let json = match zip_data.first_json {
        Some((json_name, json_bytes)) => {
            let combined = format!("{file_name}:{json_name}");
            match parse_mod_json(&json_bytes, &combined) {
                Ok(j) => j,
                Err(err) => {
                    error!("Failed to parse JSON file for {combined}: {err:#}");
                    registry.record_failure(
                        stem(file_name).unwrap_or(file_name),
                        file_name,
                        "JSON file in zip in invalid format",
                    );
                    return None;
                }
            }
        }
        None => ModJsonData::empty(),
    };

    parse_mod_data(file_name, diff_text, &json, registry)
}

/// Validates the diff text and, on success, wires the json-sourced
/// name + dependencies into a `ModData`. On validation failure records
/// a registry entry and returns None. Shared final step for both the
/// .diff and .zip paths so they produce the same shape of result.
fn parse_mod_data(
    main_file_name: &str,
    diff_text: String,
    json: &ModJsonData,
    registry: &mut ModErrorRegistry,
) -> Option<ModData> {
    let mod_name = json
        .mod_name
        .clone()
        .unwrap_or_else(|| stem(main_file_name).unwrap_or(main_file_name).to_string());

    match determine_modified_files_and_validate(&diff_text) {
        Ok(modified_files) => {
            debug!("Mod diff {main_file_name} name resolved as {mod_name}");
            Some(ModData {
                mod_name,
                diff_file_name: main_file_name.to_string(),
                dependencies: json.dependencies.clone(),
                diff_text,
                modified_files,
                max_fuzz: json.max_fuzz,
            })
        }
        Err(err) => {
            error!("Validation error in diff file {main_file_name}: {err}");
            registry.record_failure(
                &mod_name,
                main_file_name,
                &format!("diff validation failed: {err}"),
            );
            None
        }
    }
}

fn load_json_companion(diff_path: &Path) -> Result<ModJsonData> {
    let stem_str = diff_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("diff path has no stem"))?;
    let Some(parent) = diff_path.parent() else {
        return Ok(ModJsonData::empty());
    };
    let json_path = parent.join(format!("{stem_str}.json"));
    if !json_path.is_file() {
        debug!(
            "Mod diff {} has no accompanying JSON file, assuming no dependencies",
            diff_path.display()
        );
        return Ok(ModJsonData::empty());
    }
    debug!(
        "Mod diff {} has accompanying JSON file",
        json_path.display()
    );
    let bytes = fs::read(&json_path)
        .with_context(|| format!("read {}", json_path.display()))?;
    let label = json_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?");
    parse_mod_json(&bytes, label)
}

fn parse_mod_json(bytes: &[u8], label: &str) -> Result<ModJsonData> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .with_context(|| format!("JSON {label} parse failed"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("JSON {label} root is not an object"))?;

    let mod_name = match obj.get("name") {
        Some(v) if v.is_string() => v.as_str().map(|s| s.to_string()),
        Some(_) => bail!("JSON {label} node 'name' must be a string"),
        None => None,
    };

    let dependencies = match obj.get("dependencies") {
        Some(v) if v.is_array() => {
            let arr = v.as_array().unwrap();
            let mut deps = Vec::with_capacity(arr.len());
            for item in arr {
                match item.as_str() {
                    Some(s) => deps.push(s.to_string()),
                    None => bail!("JSON {label} node 'dependencies' contains non-strings"),
                }
            }
            deps
        }
        Some(_) => bail!("JSON {label} node 'dependencies' must be an array"),
        None => Vec::new(),
    };

    // Allow only integers 0..=2 — matches the GNU `patch` semantics
    // we forward to flickzeug. Anything else is likely a typo and we
    // want it caught at load time, not silently ignored.
    let max_fuzz = match obj.get("max_fuzz") {
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| anyhow!("JSON {label} node 'max_fuzz' must be an integer 0-2"))?;
            if n > 2 {
                bail!("JSON {label} node 'max_fuzz' must be between 0 and 2, got {n}");
            }
            Some(n as u32)
        }
        None => None,
    };

    Ok(ModJsonData {
        mod_name,
        dependencies,
        max_fuzz,
    })
}

fn read_mod_zip_contents(zip_path: &Path) -> Result<ModZipData> {
    let file = fs::File::open(zip_path)
        .with_context(|| format!("open {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("read zip {}", zip_path.display()))?;

    let mut data = ModZipData::default();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .with_context(|| format!("read zip entry #{i}"))?;
        if entry.is_dir() {
            continue;
        }
        let entry_name = entry.name().to_string();
        if entry_name.ends_with(".diff") {
            if data.first_diff.is_none() {
                let mut buf = Vec::new();
                entry
                    .read_to_end(&mut buf)
                    .with_context(|| format!("read zip entry {entry_name}"))?;
                data.first_diff = Some((entry_name, buf));
            }
            data.diff_count += 1;
        } else if entry_name.ends_with(".json") {
            if data.first_json.is_none() {
                let mut buf = Vec::new();
                entry
                    .read_to_end(&mut buf)
                    .with_context(|| format!("read zip entry {entry_name}"))?;
                data.first_json = Some((entry_name, buf));
            }
            data.json_count += 1;
        }
    }
    Ok(data)
}

#[derive(Default)]
struct ModZipData {
    first_diff: Option<(String, Vec<u8>)>,
    first_json: Option<(String, Vec<u8>)>,
    diff_count: usize,
    json_count: usize,
}

struct ModJsonData {
    mod_name: Option<String>,
    dependencies: Vec<String>,
    max_fuzz: Option<u32>,
}

impl ModJsonData {
    fn empty() -> Self {
        Self {
            mod_name: None,
            dependencies: Vec::new(),
            max_fuzz: None,
        }
    }
}

fn stem(file_name: &str) -> Option<&str> {
    Path::new(file_name).file_stem().and_then(|s| s.to_str())
}

pub(crate) fn determine_modified_files_and_validate(diff_text: &str) -> Result<BTreeSet<String>> {
    let double_path_re = Regex::new(r#"^diff --git "a/([^"]+)" "b/([^"]+)"$"#)?;
    let minus_path_re = Regex::new(r#"^--- "a/([^"]+)"$"#)?;
    let plus_path_re = Regex::new(r#"^\+\+\+ "b/([^"]+)"$"#)?;

    let mut modified_files = BTreeSet::new();
    let mut in_meta = false;
    let mut first_path = String::new();
    let mut second_path = String::new();

    for line in diff_text.split('\n') {
        if !in_meta {
            if line.starts_with("diff --git ") {
                in_meta = true;
                let caps = double_path_re
                    .captures(line)
                    .ok_or_else(|| anyhow!("diff --git line regex not matching: {line}"))?;
                first_path = caps[1].to_string();
                second_path = caps[2].to_string();
                if first_path != second_path {
                    bail!("rename/copy not supported on line: {line}");
                }
                validate_relative_path(&first_path)?;
                modified_files.insert(first_path.clone());
            } else if line.starts_with("+++") || line.starts_with("---") {
                bail!("diff contains +++ or --- line without a diff --git preceding it");
            }
        } else {
            if line.starts_with("---") {
                if line != "--- /dev/null" {
                    let caps = minus_path_re
                        .captures(line)
                        .ok_or_else(|| anyhow!("--- line regex not matching: {line}"))?;
                    if &caps[1] != first_path {
                        bail!("--- filename does not match 'a' on diff type line");
                    }
                }
            } else if line.starts_with("+++") {
                if line != "+++ /dev/null" {
                    let caps = plus_path_re
                        .captures(line)
                        .ok_or_else(|| anyhow!("+++ line regex not matching: {line}"))?;
                    if &caps[1] != second_path {
                        bail!("+++ filename does not match 'b' on diff type line");
                    }
                }
                in_meta = false;
            } else if line.starts_with("rename ") || line.starts_with("copy ") {
                bail!("diff header specifies rename or copy: {line}");
            }
        }
    }

    Ok(modified_files)
}

fn validate_relative_path(path: &str) -> Result<()> {
    let path_lc = path.to_lowercase();
    let slash = path_lc
        .find('/')
        .ok_or_else(|| anyhow!("Path {path} is not a file in a subdirectory"))?;
    let directory = &path_lc[..slash];
    let file_name = &path_lc[slash + 1..];

    if file_name.contains('/') {
        bail!("Path {path} depth is over 1");
    }
    if !is_valid_file_name(directory) || !is_valid_file_name(file_name) {
        bail!("Path {path} contains an illegal name");
    }
    if is_reserved(directory) || is_reserved(file_name) {
        bail!("Directory or filename in {path} has reserved name");
    }

    if directory == CSS_TYPE.directory {
        if !(CSS_TYPE.naming_matcher)(file_name) {
            bail!("Path {path} in {}/ but not with expected extension", CSS_TYPE.directory);
        }
    } else if directory == JS_TYPE.directory {
        if !(JS_TYPE.naming_matcher)(file_name) {
            bail!("Path {path} in {}/ but not with expected extension", JS_TYPE.directory);
        }
    } else if directory == PASSAGE_DIRECTORY {
        if !file_name.ends_with(&format!(".{PASSAGE_EXTENSION}")) {
            bail!("Path {path} in {PASSAGE_DIRECTORY}/ but not with expected extension");
        }
    } else if directory == CSS_MODULE_TYPE.directory_name {
        if !(CSS_MODULE_TYPE.naming_matcher)(file_name) {
            bail!(
                "Path {path} in {}/ but not with expected extension",
                CSS_MODULE_TYPE.directory_name
            );
        }
    } else if directory == JS_MODULE_TYPE.directory_name {
        if !(JS_MODULE_TYPE.naming_matcher)(file_name) {
            bail!(
                "Path {path} in {}/ but not with expected extension",
                JS_MODULE_TYPE.directory_name
            );
        }
    } else if directory == SYSTEM_SCRIPTS_DIRECTORY {
        if file_name != SYSTEM_LIBRARIES_FILE_NAME && file_name != SYSTEM_SCRIPT_FILE_NAME {
            bail!(
                "Path {path} in {SYSTEM_SCRIPTS_DIRECTORY}/ but only {SYSTEM_LIBRARIES_FILE_NAME} and {SYSTEM_SCRIPT_FILE_NAME} are recognised"
            );
        }
    } else {
        bail!("Path {path} references unexpected subdirectory");
    }

    Ok(())
}

fn is_valid_file_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    !s.chars()
        .any(|c| matches!(c, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\0'))
}

fn is_reserved(name: &str) -> bool {
    matches!(
        name,
        "." | ".."
            | "con"
            | "prn"
            | "aux"
            | "nul"
            | "com1"
            | "com2"
            | "com3"
            | "com4"
            | "com5"
            | "com6"
            | "com7"
            | "com8"
            | "com9"
            | "lpt1"
            | "lpt2"
            | "lpt3"
            | "lpt4"
            | "lpt5"
            | "lpt6"
            | "lpt7"
            | "lpt8"
            | "lpt9"
    )
}
