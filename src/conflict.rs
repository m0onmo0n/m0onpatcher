use std::fs;
use std::path::{Path, PathBuf};

use log::{debug, info, warn};

use crate::encoding::remove_cr_newlines;
use crate::extraction_naming::passage_name_truncated_hash;

pub struct ConflictManager {
    pub index: ConflictResolutionIndex,
    pub recorder: Box<dyn ConflictRecorder>,
}

impl ConflictManager {
    pub fn noop() -> Self {
        Self {
            index: ConflictResolutionIndex::empty(),
            recorder: Box::new(NoopConflictRecorder),
        }
    }
}

pub trait ConflictRecorder: Send + Sync {
    fn set_comment(&mut self, comment: &str, priority: i32);
    fn record(&mut self, file_name: &str, relative_path: &str, conflict_text: &str);
}

pub struct NoopConflictRecorder;

impl ConflictRecorder for NoopConflictRecorder {
    fn set_comment(&mut self, _comment: &str, _priority: i32) {}
    fn record(&mut self, _file_name: &str, _relative_path: &str, _conflict_text: &str) {}
}

pub struct ConflictResolutionIndex {
    resolutions: Vec<Resolution>,
}

#[derive(Debug, Clone)]
pub struct Resolution {
    pub resolution_name: String,
    pub conflict_file_name: String,
    pub conflict_text: String,
    pub resolution: String,
}

impl ConflictResolutionIndex {
    pub fn empty() -> Self {
        Self {
            resolutions: Vec::new(),
        }
    }

    pub fn load(directories: &[PathBuf]) -> Self {
        let valid: Vec<&PathBuf> = directories.iter().filter(|d| d.is_dir()).collect();
        info!(
            "Loading conflict resolutions from locations: {}",
            valid
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );

        let mut resolutions = Vec::new();
        for dir in &valid {
            let entries = match fs::read_dir(dir) {
                Ok(e) => e,
                Err(err) => {
                    warn!("Failed to read {}: {err}", dir.display());
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("resolve") {
                    continue;
                }
                match load_patch_resolution(&path) {
                    Some(r) => resolutions.push(r),
                    None => warn!(
                        "Conflict resolution file {} in incorrect format",
                        path.display()
                    ),
                }
            }
        }

        info!("Loaded {} conflict resolutions", resolutions.len());
        Self { resolutions }
    }

    pub fn find_resolution(&self, file_name: &str, conflict_text: &str) -> Option<&Resolution> {
        let normalized = file_name.replace('\\', "/");
        self.resolutions
            .iter()
            .find(|r| r.conflict_file_name == normalized && r.conflict_text == conflict_text)
    }
}

const CONFLICT_BEGIN_MARKER: &str = "<<<<<<< a";
const CONFLICT_END_MARKER: &str = ">>>>>>> c\n";

fn load_patch_resolution(path: &Path) -> Option<Resolution> {
    let raw = fs::read_to_string(path).ok()?;
    let text = remove_cr_newlines(&raw);
    let start = skip_leading_comments(&text);

    let colon = text[start..].find(':').map(|p| start + p)?;
    let conflict_begin = text[start..]
        .find(CONFLICT_BEGIN_MARKER)
        .map(|p| start + p)?;
    let conflict_end = text[start..].find(CONFLICT_END_MARKER).map(|p| start + p)?;

    if conflict_begin < colon || conflict_end < conflict_begin {
        return None;
    }

    let conflict_file_name = text[start..colon].trim().replace('\\', "/");
    let conflict_text = text[conflict_begin..conflict_end + CONFLICT_END_MARKER.len()].to_string();
    let resolution = text[conflict_end + CONFLICT_END_MARKER.len()..].to_string();

    let resolution_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    debug!("Loaded conflict resolution {resolution_name} for file {conflict_file_name}");
    Some(Resolution {
        resolution_name,
        conflict_file_name,
        conflict_text,
        resolution,
    })
}

/// Skip leading blank lines and `#` comment lines until we find an actual
/// header line. Mirrors the Kotlin `skipComments` helper.
fn skip_leading_comments(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        match bytes[pos] {
            b'\n' | b'\r' | b'#' => match text[pos..].find('\n') {
                Some(p) => pos += p + 1,
                None => return text.len(),
            },
            _ => return pos,
        }
    }
    pos
}

/// Writes a `.resolve` template file for each unresolved conflict so the
/// user can fill in a resolution and re-run `patch`. Directory is cleaned
/// lazily on the first `record()` call.
pub struct TemplateConflictRecorder {
    directory: PathBuf,
    comment: String,
    comment_priority: i32,
    initialized: bool,
}

impl TemplateConflictRecorder {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            comment: String::new(),
            comment_priority: 0,
            initialized: false,
        }
    }

    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }
        self.initialized = true;
        match clean_directory(&self.directory) {
            Ok(()) => info!(
                "Writing conflict resolution templates to {}",
                self.directory.display()
            ),
            Err(err) => warn!(
                "Failed to initialize conflict template directory {}: {err}",
                self.directory.display()
            ),
        }
    }

    fn write_template(
        &self,
        file_name: &str,
        relative_path: &str,
        conflict_text: &str,
    ) -> std::io::Result<()> {
        let hash = passage_name_truncated_hash(conflict_text);
        let path = self
            .directory
            .join(format!("conflict-{file_name}-{hash}.resolve"));
        let mut out = String::new();
        if !self.comment.trim().is_empty() {
            for line in self.comment.lines() {
                out.push_str(&format!("# {line}\n"));
            }
            out.push('\n');
        }
        out.push_str(&format!("{relative_path}: \n"));
        out.push_str(conflict_text);
        out.push_str("--- REPLACE THIS WITH RESOLUTION ---\n");
        fs::write(path, out)
    }
}

impl ConflictRecorder for TemplateConflictRecorder {
    fn set_comment(&mut self, comment: &str, priority: i32) {
        if priority >= self.comment_priority {
            self.comment = comment.to_string();
            self.comment_priority = priority;
        }
    }

    fn record(&mut self, file_name: &str, relative_path: &str, conflict_text: &str) {
        self.ensure_initialized();
        if let Err(err) = self.write_template(file_name, relative_path, conflict_text) {
            warn!("Failed to write conflict template for {relative_path}: {err}");
        }
    }
}

fn clean_directory(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::create_dir_all(dir)
}
