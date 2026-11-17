use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use log::{error, info};

use crate::encoding::url_encode_file_name;
use crate::extraction_naming::include_hash_in_passage_file_name;
use crate::game_data::{
    GameData, ModulesOfType, Passage, UserData, CSS_MODULE_TYPE, CSS_TYPE, JS_MODULE_TYPE, JS_TYPE,
    PASSAGE_DIRECTORY, PASSAGE_EXTENSION, SYSTEM_LIBRARIES_FILE_NAME, SYSTEM_SCRIPTS_DIRECTORY,
    SYSTEM_SCRIPT_FILE_NAME,
};
use crate::game_data_reader;

pub fn extract(html_path: &Path, output_directory: &Path) -> Result<()> {
    info!(
        "Extracting {} to {}",
        html_path.display(),
        output_directory.display()
    );
    let data = game_data_reader::read_from_file(html_path)?;
    extract_from_data(&data, output_directory)
}

/// Variant for callers that already hold a parsed `GameData` (e.g. the
/// converter tab, which keeps each version's data cached on the
/// version index). Skips the file read but otherwise behaves like
/// `extract`.
pub fn extract_from_data(data: &GameData, output_directory: &Path) -> Result<()> {
    let mut tracker = FileTracker::new(output_directory);

    extract_user_data(
        &data.styles,
        &output_directory.join(CSS_TYPE.directory),
        &mut tracker,
    )?;
    info!("Extracted {} stylesheets", data.styles.entries.len());

    extract_user_data(
        &data.scripts,
        &output_directory.join(JS_TYPE.directory),
        &mut tracker,
    )?;
    info!("Extracted {} scripts", data.scripts.entries.len());

    extract_passages(data, output_directory, &mut tracker)?;

    if let Some(modules) = &data.modules {
        extract_modules(data, &modules.script_modules, output_directory, &mut tracker)?;
        info!(
            "Extracted {} {}",
            modules.script_modules.entries.len(),
            modules.script_modules.module_type.directory_name
        );
        extract_modules(data, &modules.style_modules, output_directory, &mut tracker)?;
        info!(
            "Extracted {} {}",
            modules.style_modules.entries.len(),
            modules.style_modules.module_type.directory_name
        );
    }

    let system_extracted = extract_system_components(data, output_directory, &mut tracker)?;

    for ty in [&CSS_TYPE, &JS_TYPE] {
        delete_unwritten(&output_directory.join(ty.directory), ty.naming_matcher, &tracker)?;
    }

    delete_unwritten(
        &output_directory.join(PASSAGE_DIRECTORY),
        |n| n.ends_with(&format!(".{PASSAGE_EXTENSION}")),
        &tracker,
    )?;

    if data.modules.is_some() {
        for mt in [&JS_MODULE_TYPE, &CSS_MODULE_TYPE] {
            delete_unwritten(
                &output_directory.join(mt.directory_name),
                mt.naming_matcher,
                &tracker,
            )?;
        }
    }

    if system_extracted {
        delete_unwritten(
            &output_directory.join(SYSTEM_SCRIPTS_DIRECTORY),
            |n| n.ends_with(".js"),
            &tracker,
        )?;
    }

    Ok(())
}

fn extract_system_components(
    data: &GameData,
    output_directory: &Path,
    tracker: &mut FileTracker,
) -> Result<bool> {
    let libs = data.system_components.sugarcube_libraries;
    let script = data.system_components.sugarcube_script;
    if libs.is_none() && script.is_none() {
        return Ok(false);
    }

    let dir = output_directory.join(SYSTEM_SCRIPTS_DIRECTORY);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    if let Some(ranges) = libs {
        let path = dir.join(SYSTEM_LIBRARIES_FILE_NAME);
        let contents = &data.document_raw_text[ranges.inner.start..ranges.inner.end];
        fs::write(&path, contents).with_context(|| format!("write {}", path.display()))?;
        tracker.record_written(&path, false);
        info!("Extracted SugarCube libraries");
    }
    if let Some(ranges) = script {
        let path = dir.join(SYSTEM_SCRIPT_FILE_NAME);
        let contents = &data.document_raw_text[ranges.inner.start..ranges.inner.end];
        fs::write(&path, contents).with_context(|| format!("write {}", path.display()))?;
        tracker.record_written(&path, false);
        info!("Extracted SugarCube engine script");
    }

    Ok(true)
}

fn extract_user_data(
    user_data: &UserData,
    output_directory: &Path,
    tracker: &mut FileTracker,
) -> Result<()> {
    fs::create_dir_all(output_directory)
        .with_context(|| format!("create {}", output_directory.display()))?;

    for entry in &user_data.entries {
        let encoded_file_name = url_encode_file_name(&entry.file_name);
        let contents = &user_data.user_data_raw_text
            [entry.raw_text_range.start..entry.raw_text_range.end];
        let file_path = output_directory.join(&encoded_file_name);
        fs::write(&file_path, contents)
            .with_context(|| format!("write {}", file_path.display()))?;
        tracker.record_written(&file_path, false);
    }
    Ok(())
}

fn extract_modules(
    data: &GameData,
    modules: &ModulesOfType,
    base_directory: &Path,
    tracker: &mut FileTracker,
) -> Result<()> {
    let output_directory = base_directory.join(modules.module_type.directory_name);
    let extension = modules.module_type.file_extension;
    fs::create_dir_all(&output_directory)
        .with_context(|| format!("create {}", output_directory.display()))?;

    for entry in &modules.entries {
        let encoded_file_name =
            url_encode_file_name(&format!("{}.{}", entry.module_name, extension));
        let contents = &data.document_raw_text[entry.ranges.inner.start..entry.ranges.inner.end];
        let file_path = output_directory.join(&encoded_file_name);
        fs::write(&file_path, contents)
            .with_context(|| format!("write {}", file_path.display()))?;
        tracker.record_written(&file_path, false);
    }
    Ok(())
}

fn extract_passages(
    data: &GameData,
    output_directory: &Path,
    tracker: &mut FileTracker,
) -> Result<()> {
    let passage_directory = output_directory.join(PASSAGE_DIRECTORY);
    fs::create_dir_all(&passage_directory)
        .with_context(|| format!("create {}", passage_directory.display()))?;

    let mut widget_count = 0;
    for passage in &data.passages {
        let file_path =
            prepare_passage_file_path(&passage.name, &passage_directory, tracker, false)?;

        let mut contents = passage_twee_header(passage);
        contents.push_str(&passage.decoded_text);
        fs::write(&file_path, contents)
            .with_context(|| format!("write {}", file_path.display()))?;
        tracker.record_written(&file_path, true);

        if passage.is_widget {
            widget_count += 1;
        }
    }

    info!(
        "Extracted {} passages (including {} widgets)",
        data.passages.len(),
        widget_count
    );
    Ok(())
}

fn prepare_passage_file_path(
    name: &str,
    passage_directory: &Path,
    tracker: &mut FileTracker,
    nested: bool,
) -> Result<PathBuf> {
    let file_name = url_encode_file_name(&format!("{name}.{PASSAGE_EXTENSION}"));
    let file_path = passage_directory.join(&file_name);

    if tracker.check_written(&file_path) {
        error!(
            "Extraction of {} failed, multiple passages with the same name",
            tracker.relative_path(&file_path)
        );
        bail!("multiple passages with the same name: {name}");
    }

    if let Some(existing_name) = tracker.check_written_any_case(&file_path) {
        if nested {
            error!(
                "Extraction of {} failed, hash collision",
                tracker.relative_path(&file_path)
            );
            bail!("hash collision for passage {name}");
        }

        let existing_name = existing_name.to_string();
        let first_of_name = passage_directory.join(&existing_name);
        if first_of_name.exists() {
            let stem = Path::new(&existing_name)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or(existing_name.clone());
            let new_name = include_hash_in_passage_file_name(&stem);
            let target_file = passage_directory.join(format!("{new_name}.{PASSAGE_EXTENSION}"));

            fs::rename(&first_of_name, &target_file).with_context(|| {
                format!("rename {} -> {}", first_of_name.display(), target_file.display())
            })?;

            tracker.erase_record(&first_of_name);
            tracker.record_written(&target_file, true);
        }

        let file_stem = Path::new(&file_name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(file_name.clone());
        let name_with_hash = include_hash_in_passage_file_name(&file_stem);
        return prepare_passage_file_path(&name_with_hash, passage_directory, tracker, true);
    }

    if !file_path.exists() {
        return Ok(file_path);
    }

    // On case-insensitive filesystems a file with differing case may already
    // exist at this path. Delete it before overwriting so the new case sticks.
    let canonical = fs::canonicalize(&file_path)
        .with_context(|| format!("canonicalize {}", file_path.display()))?;
    if canonical.file_name().map(|n| n.to_string_lossy().into_owned())
        == Some(file_name.clone())
    {
        return Ok(file_path);
    }
    info!(
        "Extractor deleting {} file prior to overwrite - case differs from desired {}",
        tracker.relative_path(&canonical),
        tracker.relative_path(&file_path)
    );
    fs::remove_file(&canonical)
        .with_context(|| format!("delete {}", canonical.display()))?;
    Ok(file_path)
}

fn passage_twee_header(passage: &Passage) -> String {
    if passage.tags.is_empty() {
        format!("::{}\n", passage.name)
    } else {
        format!("::{} [{}]\n", passage.name, passage.tags.join(","))
    }
}

fn delete_unwritten(
    directory: &Path,
    naming_matcher: fn(&str) -> bool,
    tracker: &FileTracker,
) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("read_dir {}", directory.display()))
        }
    };

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_path = entry.path();
        let Some(file_name) = file_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let name_to_test = strip_suffix(file_name, ".orig");
        let name_to_test = strip_suffix(name_to_test, ".rej");

        if naming_matcher(name_to_test) && !tracker.check_written(&file_path) {
            info!(
                "Extractor deleting {} file - unmodified leftover from previous state",
                tracker.relative_path(&file_path)
            );
            if let Err(err) = fs::remove_file(&file_path) {
                error!("Failed to delete {}: {err}", tracker.relative_path(&file_path));
                return Err(err)
                    .with_context(|| format!("delete {}", file_path.display()));
            }
        }
    }
    Ok(())
}

fn strip_suffix<'a>(s: &'a str, suffix: &str) -> &'a str {
    s.strip_suffix(suffix).unwrap_or(s)
}

struct FileTracker {
    base: PathBuf,
    written_files: HashSet<String>,
    written_files_lower_case: HashMap<String, String>,
}

impl FileTracker {
    fn new(base: &Path) -> Self {
        Self {
            base: base.to_path_buf(),
            written_files: HashSet::new(),
            written_files_lower_case: HashMap::new(),
        }
    }

    fn record_written(&mut self, file: &Path, track_case: bool) {
        let rel = self.relative_path(file);
        if track_case {
            let file_name = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.written_files_lower_case
                .insert(rel.to_lowercase(), file_name);
        }
        self.written_files.insert(rel);
    }

    fn erase_record(&mut self, file: &Path) {
        let rel = self.relative_path(file);
        self.written_files.remove(&rel);
    }

    fn check_written(&self, file: &Path) -> bool {
        self.written_files.contains(&self.relative_path(file))
    }

    fn check_written_any_case(&self, file: &Path) -> Option<&str> {
        self.written_files_lower_case
            .get(&self.relative_path(file).to_lowercase())
            .map(String::as_str)
    }

    fn relative_path(&self, file: &Path) -> String {
        file.strip_prefix(&self.base)
            .unwrap_or(file)
            .to_string_lossy()
            .into_owned()
    }
}
