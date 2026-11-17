use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use log::{debug, error, info};
use regex::Regex;

use crate::encoding::{
    html_encode, quoted_string_encode, remove_cr_newlines, url_decode_file_name,
    url_encode_file_name,
};
use crate::extraction_naming::is_valid_hash_name_match;
use crate::game_data::{
    GameData, HtmlTagRanges, ModuleType, ModulesOfType, UserData, UserDataType, CSS_TYPE, JS_TYPE,
    PASSAGE_DIRECTORY, PASSAGE_EXTENSION, PASSAGE_HTML_TAG, SYSTEM_LIBRARIES_FILE_NAME,
    SYSTEM_SCRIPTS_DIRECTORY, SYSTEM_SCRIPT_FILE_NAME,
};
use crate::game_data_reader;

pub fn merge(
    source_html_path: &Path,
    output_html_path: &Path,
    extracted_directory: &Path,
) -> Result<()> {
    let data = game_data_reader::read_from_file(source_html_path)?;
    info!(
        "Merging files in {} into {} with {} as reference",
        extracted_directory.display(),
        output_html_path.display(),
        source_html_path.display()
    );
    let source = FileSource::Disk(extracted_directory);
    let mut out = String::with_capacity(data.document_raw_text.len());
    write_merged(&mut out, &data, &source)?;
    fs::write(output_html_path, out)
        .with_context(|| format!("write {}", output_html_path.display()))?;
    Ok(())
}

/// In-memory variant: takes a map of `rel_path -> content` instead of a
/// directory. Used by the patcher to produce a merged HTML without first
/// flushing the source tree to disk.
pub fn merge_from_memory(
    source_html_path: &Path,
    output_html_path: &Path,
    files: &BTreeMap<String, String>,
) -> Result<()> {
    let data = game_data_reader::read_from_file(source_html_path)?;
    info!(
        "Merging in-memory files into {} with {} as reference",
        output_html_path.display(),
        source_html_path.display()
    );
    let source = FileSource::Memory(files);
    let mut out = String::with_capacity(data.document_raw_text.len());
    write_merged(&mut out, &data, &source)?;
    fs::write(output_html_path, out)
        .with_context(|| format!("write {}", output_html_path.display()))?;
    Ok(())
}

/// Abstract the "tree of extracted files" so the merger can work against
/// either a directory on disk or a map held in memory by the patcher.
pub enum FileSource<'a> {
    Disk(&'a Path),
    Memory(&'a BTreeMap<String, String>),
}

impl<'a> FileSource<'a> {
    fn list(&self, subdir: &str, matcher: fn(&str) -> bool) -> Result<Vec<String>> {
        match self {
            FileSource::Disk(base) => {
                let directory = base.join(subdir);
                let entries = match fs::read_dir(&directory) {
                    Ok(e) => e,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                    Err(err) => {
                        return Err(err)
                            .with_context(|| format!("read_dir {}", directory.display()));
                    }
                };
                let mut names = Vec::new();
                for entry in entries {
                    let entry = entry?;
                    if !entry.file_type()?.is_file() {
                        continue;
                    }
                    if let Some(name) = entry.path().file_name().and_then(|n| n.to_str()) {
                        if matcher(name) {
                            names.push(name.to_string());
                        }
                    }
                }
                Ok(names)
            }
            FileSource::Memory(map) => {
                let prefix = format!("{subdir}/");
                let mut names = Vec::new();
                for key in map.keys() {
                    if let Some(rest) = key.strip_prefix(&prefix) {
                        if !rest.contains('/') && matcher(rest) {
                            names.push(rest.to_string());
                        }
                    }
                }
                Ok(names)
            }
        }
    }

    fn read(&self, subdir: &str, file_name: &str) -> Result<String> {
        match self {
            FileSource::Disk(base) => {
                let path = base.join(subdir).join(file_name);
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
            }
            FileSource::Memory(map) => {
                let key = format!("{subdir}/{file_name}");
                map.get(&key)
                    .cloned()
                    .ok_or_else(|| anyhow!("in-memory source missing {key}"))
            }
        }
    }

    /// Like `read`, but returns `Ok(None)` when the source has no entry
    /// for that path instead of erroring. Used by the system-scripts
    /// splice — a missing file means "leave the embedded HTML alone",
    /// not "abort the merge".
    fn read_optional(&self, subdir: &str, file_name: &str) -> Result<Option<String>> {
        match self {
            FileSource::Disk(base) => {
                let path = base.join(subdir).join(file_name);
                match fs::read_to_string(&path) {
                    Ok(s) => Ok(Some(s)),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
                }
            }
            FileSource::Memory(map) => {
                let key = format!("{subdir}/{file_name}");
                Ok(map.get(&key).cloned())
            }
        }
    }

    fn describe(&self, subdir: &str) -> String {
        match self {
            FileSource::Disk(base) => base.join(subdir).display().to_string(),
            FileSource::Memory(_) => format!("<memory>/{subdir}"),
        }
    }
}

fn write_merged(out: &mut String, data: &GameData, source: &FileSource) -> Result<()> {
    let raw = &data.document_raw_text;
    let style_start = data.styles.ranges.inner.start;
    let head_end = data
        .modules
        .as_ref()
        .map(|m| m.html_range.start)
        .unwrap_or(style_start);

    write_with_system_splice(
        out,
        raw,
        source,
        data.system_components.sugarcube_libraries,
        SYSTEM_LIBRARIES_FILE_NAME,
        "before modules / styles",
        0,
        head_end,
    )?;

    if let Some(modules) = &data.modules {
        let any_written = merge_modules_of_type(out, data, source, &modules.script_modules, false)?;
        merge_modules_of_type(out, data, source, &modules.style_modules, any_written)?;
        debug!(
            "Copying range from reference file: end of modules until user style ({}, {})",
            modules.html_range.end, style_start
        );
        out.push_str(&raw[modules.html_range.end..style_start]);
    }

    merge_user_data(out, &data.styles, source, &CSS_TYPE)?;

    out.push_str("</style>");
    out.push_str(&raw[data.scripts.ranges.outer.start..data.scripts.ranges.inner.start]);

    merge_user_data(out, &data.scripts, source, &JS_TYPE)?;

    out.push_str("</script>");

    let first_passage_start = data
        .passages
        .first()
        .ok_or_else(|| anyhow!("source HTML contains no passages"))?
        .ranges
        .outer
        .start;
    let last_passage_end = data.passages.last().unwrap().ranges.outer.end;
    out.push_str(&raw[data.scripts.ranges.outer.end..first_passage_start]);

    merge_passages(out, data, source)?;

    write_with_system_splice(
        out,
        raw,
        source,
        data.system_components.sugarcube_script,
        SYSTEM_SCRIPT_FILE_NAME,
        "after passages",
        last_passage_end,
        raw.len(),
    )?;
    Ok(())
}

/// Copy `raw[start..end]` into `out`, but if `slot` is `Some` and its
/// inner range falls inside that window, swap the inner contents for the
/// latest version of `<system-scripts>/<file_name>` from the source tree.
/// When the source tree has no entry for that file, the original raw
/// contents stay in place — which keeps round-trips lossless even when
/// the user hasn't extracted (or has deleted) the system-scripts dir.
fn write_with_system_splice(
    out: &mut String,
    raw: &str,
    source: &FileSource,
    slot: Option<HtmlTagRanges>,
    file_name: &str,
    description: &str,
    start: usize,
    end: usize,
) -> Result<()> {
    let Some(ranges) = slot.filter(|r| r.inner.start >= start && r.inner.end <= end) else {
        debug!("Copying range from reference file: {description} ({start}, {end})");
        out.push_str(&raw[start..end]);
        return Ok(());
    };
    debug!(
        "Copying range from reference file: {description} pre-{file_name} ({start}, {})",
        ranges.inner.start
    );
    out.push_str(&raw[start..ranges.inner.start]);
    match source.read_optional(SYSTEM_SCRIPTS_DIRECTORY, file_name)? {
        Some(text) => {
            let text = remove_cr_newlines(&text);
            debug!("Writing {} characters from {file_name} to target HTML", text.len());
            out.push_str(&text);
        }
        None => {
            debug!(
                "{file_name} not present in source tree, preserving original embedded contents"
            );
            out.push_str(&raw[ranges.inner.start..ranges.inner.end]);
        }
    }
    debug!(
        "Copying range from reference file: {description} post-{file_name} ({}, {end})",
        ranges.inner.end
    );
    out.push_str(&raw[ranges.inner.end..end]);
    Ok(())
}

fn merge_modules_of_type(
    out: &mut String,
    data: &GameData,
    source: &FileSource,
    modules: &ModulesOfType,
    previous_modules_written: bool,
) -> Result<bool> {
    let ty: &ModuleType = modules.module_type;
    let extracted = collect_modules_of_type(source, ty)?;
    let extracted_map: HashMap<&str, &ModuleFile> =
        extracted.iter().map(|f| (f.module_name.as_str(), f)).collect();
    let mut written_anything = previous_modules_written;
    let mut modified = 0u32;
    let mut unmodified = 0u32;
    let mut added = 0u32;
    let mut deleted = 0u32;

    for entry in &modules.entries {
        let Some(file) = extracted_map.get(entry.module_name.as_str()) else {
            info!(
                "{} {}: deleted (skipped on copy) as file was removed from source",
                ty.directory_name, entry.module_name
            );
            deleted += 1;
            continue;
        };

        if written_anything {
            out.push('\n');
        } else {
            written_anything = true;
        }

        out.push_str(ty.start_tag_prefix);
        out.push_str(&html_attribute_encode(&entry.original_module_name));
        out.push_str(ty.start_tag_suffix);

        let text = remove_cr_newlines(&source.read(ty.directory_name, &file.file_name)?);
        debug!(
            "Writing {} characters from {} to target HTML",
            text.len(),
            file.file_name
        );
        out.push_str(&text);

        let original = &data.document_raw_text[entry.ranges.inner.start..entry.ranges.inner.end];
        if original != text {
            info!("{} {}: contents modified", ty.directory_name, entry.module_name);
            modified += 1;
        } else {
            debug!(
                "{} {}: contents were not modified from reference",
                ty.directory_name, entry.module_name
            );
            unmodified += 1;
        }

        out.push_str(ty.end_tag);
    }

    let original_names: HashSet<&str> = modules
        .entries
        .iter()
        .map(|e| e.module_name.as_str())
        .collect();

    debug!(
        "Copying {} files that were added in sources",
        ty.directory_name
    );

    for file in &extracted {
        if original_names.contains(file.module_name.as_str()) {
            continue;
        }

        if written_anything {
            out.push('\n');
        } else {
            written_anything = true;
        }

        out.push_str(ty.start_tag_prefix);
        out.push_str(&html_attribute_encode(&file.module_name));
        out.push_str(ty.start_tag_suffix);

        let text = remove_cr_newlines(&source.read(ty.directory_name, &file.file_name)?);
        debug!(
            "Writing {} characters from {} to target HTML",
            text.len(),
            file.file_name
        );
        out.push_str(&text);
        info!("{} {}: added as new", ty.directory_name, file.module_name);
        added += 1;

        out.push_str(ty.end_tag);
    }

    info!(
        "{} - modified: {}, unmodified: {}, deleted {}, added {}",
        ty.directory_name, modified, unmodified, deleted, added
    );
    Ok(written_anything)
}

fn merge_user_data(
    out: &mut String,
    user_data: &UserData,
    source: &FileSource,
    data_type: &UserDataType,
) -> Result<()> {
    debug!(
        "Copying {} files from directory {}",
        data_type.log_name.to_lowercase(),
        source.describe(data_type.directory)
    );

    let extracted = collect_user_data(source, data_type.directory, data_type.naming_matcher)?;
    let extracted_names: HashSet<&str> = extracted.iter().map(|f| f.name.as_str()).collect();

    debug!(
        "Found {} {} files in directory",
        extracted.len(),
        data_type.log_name.to_lowercase()
    );
    debug!(
        "Copying {} files that exist in both reference and sources",
        data_type.log_name.to_lowercase()
    );

    let mut highest_index: usize = 0;
    let mut modified = 0u32;
    let mut unmodified = 0u32;
    let mut added = 0u32;
    let mut deleted = 0u32;

    for entry in &user_data.entries {
        if !extracted_names.contains(entry.file_name.as_str()) {
            info!(
                "{} {}: deleted (skipped on copy) as file was removed from source",
                data_type.log_name, entry.file_name
            );
            deleted += 1;
            continue;
        }

        if highest_index > 0 {
            out.push('\n');
        }

        if user_data.is_split || extracted.len() != 1 {
            debug!(
                "Writing header for {} {} with preserved index {}",
                data_type.log_name.to_lowercase(),
                entry.file_name,
                entry.index
            );
            let quoted = quoted_string_encode(&entry.file_name);
            out.push_str(&format!(
                "/* {} #{}: \"{}\" */\n",
                data_type.html_id, entry.index, quoted
            ));
            highest_index = highest_index.max(entry.index);
        } else {
            debug!(
                "Not writing comment header because {}s were not split in reference",
                data_type.log_name.to_lowercase()
            );
        }

        let text = remove_cr_newlines(&source.read(data_type.directory, &entry.disk_file_name)?);
        debug!(
            "Writing {} characters from {} to target HTML",
            text.len(),
            entry.file_name
        );
        out.push_str(&text);

        let original = &user_data.user_data_raw_text
            [entry.raw_text_range.start..entry.raw_text_range.end];
        if original != text {
            info!("{} {}: contents modified", data_type.log_name, entry.file_name);
            modified += 1;
        } else {
            debug!(
                "{} {}: contents were not modified from reference",
                data_type.log_name, entry.file_name
            );
            unmodified += 1;
        }
    }

    let original_names: HashSet<&str> = user_data
        .entries
        .iter()
        .map(|e| e.file_name.as_str())
        .collect();

    debug!(
        "Copying {} files that were added in sources",
        data_type.log_name.to_lowercase()
    );

    for file in &extracted {
        if original_names.contains(file.name.as_str()) {
            continue;
        }

        if highest_index > 0 {
            out.push('\n');
        }

        highest_index += 1;
        let quoted = quoted_string_encode(&file.name);
        out.push_str(&format!(
            "/* {} #{}: \"{}\" */\n",
            data_type.html_id, highest_index, quoted
        ));
        let text = remove_cr_newlines(&source.read(data_type.directory, &file.disk_file_name)?);
        debug!(
            "Writing {} characters from {} to target HTML with index {}",
            text.len(),
            file.name,
            highest_index
        );
        out.push_str(&text);
        info!("{} {}: added as new", data_type.log_name, file.name);
        added += 1;
    }

    info!(
        "{}s - modified: {}, unmodified: {}, deleted {}, added {}",
        data_type.log_name, modified, unmodified, deleted, added
    );
    Ok(())
}

fn merge_passages(out: &mut String, data: &GameData, source: &FileSource) -> Result<()> {
    let extracted = read_passages(source)?;
    let extracted_map: HashMap<&str, &PassageFile> =
        extracted.iter().map(|p| (p.passage_name.as_str(), p)).collect();

    let raw = &data.document_raw_text;
    let mut highest_pid: u64 = 0;
    let mut highest_y: i64 = 0;
    let mut modified = 0u32;
    let mut unmodified = 0u32;
    let mut added = 0u32;
    let mut deleted = 0u32;

    for passage in &data.passages {
        let Some(extracted_pass) = extracted_map.get(passage.name.as_str()) else {
            info!(
                "Passage {}: deleted as file was removed from source",
                passage.name
            );
            deleted += 1;
            continue;
        };

        let start_tag = &raw[passage.ranges.outer.start..passage.ranges.inner.start];
        let original_tags = passage.attributes.tags.as_deref().unwrap_or("");

        let tags_modified = if original_tags != extracted_pass.tags {
            let needle = format!("tags=\"{original_tags}\"");
            let replacement = format!("tags=\"{}\"", extracted_pass.tags);
            out.push_str(&start_tag.replacen(&needle, &replacement, 1));
            info!(
                "Passage {}: tags modified from [{}] to [{}]",
                passage.name, original_tags, extracted_pass.tags
            );
            true
        } else {
            out.push_str(start_tag);
            false
        };

        let data_modified = passage.decoded_text != extracted_pass.data;

        out.push_str(&html_encode(&extracted_pass.data));
        out.push_str(&format!("</{PASSAGE_HTML_TAG}>"));

        if let Some(pid_str) = passage.attributes.pid.as_deref() {
            if let Ok(pid) = pid_str.parse::<u64>() {
                highest_pid = highest_pid.max(pid);
            }
        }
        if let Some(pos) = passage.attributes.position.as_deref() {
            if let Some(first) = pos.split(',').next() {
                if let Ok(y) = first.parse::<i64>() {
                    highest_y = highest_y.max(y);
                }
            }
        }

        if data_modified {
            info!("Passage {}: data modified", passage.name);
        }
        if tags_modified || data_modified {
            modified += 1;
        } else {
            unmodified += 1;
        }
    }

    let original_names: HashSet<&str> = data.passages.iter().map(|p| p.name.as_str()).collect();

    for extracted_pass in &extracted {
        if original_names.contains(extracted_pass.passage_name.as_str()) {
            continue;
        }
        if extracted_pass.passage_name.contains('"') {
            error!(
                "Passage name {} contains quotes",
                extracted_pass.passage_name
            );
            bail!(
                "Passage name {} contains quotes",
                extracted_pass.passage_name
            );
        }

        highest_pid += 1;
        highest_y += 125;

        out.push_str(&format!(
            "<{PASSAGE_HTML_TAG} pid=\"{pid}\" name=\"{name}\" tags=\"{tags}\" position=\"100,{y}\" size=\"100,100\">",
            pid = highest_pid,
            name = extracted_pass.passage_name,
            tags = extracted_pass.tags,
            y = highest_y,
        ));
        out.push_str(&html_encode(&extracted_pass.data));
        out.push_str(&format!("</{PASSAGE_HTML_TAG}>"));
        info!(
            "Passage {}: added with tags {}",
            extracted_pass.passage_name, extracted_pass.tags
        );
        added += 1;
    }

    info!(
        "Passages - modified: {}, unmodified: {}, deleted: {}, added: {}",
        modified, unmodified, deleted, added
    );
    Ok(())
}

// ---- Filesystem-abstracted collection helpers ----

struct ModuleFile {
    module_name: String,
    file_name: String,
}

fn collect_modules_of_type(source: &FileSource, ty: &ModuleType) -> Result<Vec<ModuleFile>> {
    let names = source.list(ty.directory_name, ty.naming_matcher)?;
    let mut out: Vec<ModuleFile> = names
        .into_iter()
        .map(|name| {
            let module_name = Path::new(&name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&name)
                .to_string();
            ModuleFile {
                module_name,
                file_name: name,
            }
        })
        .collect();
    out.sort_by(|a, b| a.module_name.cmp(&b.module_name));
    Ok(out)
}

struct UserDataFile {
    disk_file_name: String,
    name: String,
}

fn collect_user_data(
    source: &FileSource,
    subdir: &str,
    matcher: fn(&str) -> bool,
) -> Result<Vec<UserDataFile>> {
    let names = source.list(subdir, matcher)?;
    let mut out: Vec<UserDataFile> = names
        .into_iter()
        .map(|name| UserDataFile {
            name: url_decode_file_name(&name),
            disk_file_name: name,
        })
        .collect();
    out.sort_by(|a, b| a.disk_file_name.cmp(&b.disk_file_name));
    Ok(out)
}

struct PassageFile {
    passage_name: String,
    tags: String,
    data: String,
}

fn read_passages(source: &FileSource) -> Result<Vec<PassageFile>> {
    let matcher: fn(&str) -> bool = |n| n.ends_with(".twee");
    let names = source.list(PASSAGE_DIRECTORY, matcher)?;
    let mut out = Vec::with_capacity(names.len());
    for name in &names {
        let content = source.read(PASSAGE_DIRECTORY, name)?;
        out.push(read_passage_file(name, &content)?);
    }
    out.sort_by(|a, b| a.passage_name.cmp(&b.passage_name));
    Ok(out)
}

fn read_passage_file(file_name: &str, raw_content: &str) -> Result<PassageFile> {
    let text = remove_cr_newlines(raw_content);
    let newline = text.find('\n').ok_or_else(|| {
        error!("Passage file {file_name} does not contain any newlines");
        anyhow!("Passage file {file_name} does not contain any newlines")
    })?;

    let header_re = twee_header_regex();
    let caps = header_re.captures(&text[0..newline]).ok_or_else(|| {
        error!("No valid header in {file_name}");
        anyhow!("No valid header in {file_name}")
    })?;

    let passage_name = caps
        .get(1)
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| anyhow!("header missing name in {file_name}"))?;

    let expected_name = url_encode_file_name(&format!("{passage_name}.{PASSAGE_EXTENSION}"));
    if expected_name != file_name && !is_valid_hash_name_match(file_name, &passage_name) {
        error!("Passage file {file_name} does not match its header passage name {passage_name}");
        bail!("Passage file {file_name} does not match its header passage name {passage_name}");
    }

    let tags = caps
        .get(2)
        .map(|m| {
            m.as_str()
                .split(|c| c == ',' || c == ' ')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    let data = text[newline + 1..].to_string();

    Ok(PassageFile {
        passage_name,
        tags,
        data,
    })
}

fn twee_header_regex() -> Regex {
    // `^::<name>(?: [<tags>])?$`
    Regex::new(r"^::([^\[]+)(?: \[(.*)\])?$").expect("valid twee header regex")
}

/// Minimal attribute value encoder — same entity set as `html_encode`.
/// For well-formed Twine output the tags/module names we emit never contain
/// these characters, but we encode defensively.
fn html_attribute_encode(text: &str) -> String {
    html_encode(text)
}
