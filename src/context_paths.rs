use std::path::{Path, PathBuf};

/// Resolve the directory that a given context path "lives in". A file's
/// parent directory is used as the base; a directory is used as-is.
/// Returns None if `context_path` is neither a file nor a directory (it
/// doesn't exist) or if a file has no parent component.
pub fn context_locate_base(context_path: &Path) -> Option<PathBuf> {
    if context_path.is_dir() {
        Some(context_path.to_path_buf())
    } else if context_path.is_file() {
        context_path.parent().map(|p| p.to_path_buf())
    } else {
        None
    }
}

pub fn context_locate_mods(context_path: &Path) -> Option<PathBuf> {
    context_locate_base(context_path).map(|b| b.join("mods"))
}

pub fn context_locate_source(context_path: &Path) -> Option<PathBuf> {
    context_locate_base(context_path).map(|b| b.join("src"))
}

pub fn context_locate_temp(context_path: &Path) -> Option<PathBuf> {
    context_locate_base(context_path).map(|b| b.join("tmp"))
}
