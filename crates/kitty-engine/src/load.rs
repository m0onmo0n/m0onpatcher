//! Loading a mods folder: file discovery, load order, per-mod parse and the
//! merge into one [`ParseState`] (port of `load_mods` and the plain-alphabetical
//! part of `resolve_mod_load_order`).
//!
//! Not supported yet (the Python patcher has them): `.json` dependency
//! sidecars / `cm_dependencies.json`, `[load_order]` from `kitty_config.toml`,
//! encrypted mod files. The caller gets a warning in `LoadResult::warnings`.

use crate::fold::absorb_inner_cross_mod;
use crate::model::{LegacyEntry, ParseState};
use crate::parse::{proc_replacement_new, proc_replacement_old};
use crate::pytext::{py_strip, universal_newlines};
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const VALID_MOD_EXTS: [&str; 3] = [".mod", ".kdiff", ".patch"];

#[derive(Debug)]
pub struct LoadResult {
    pub state: ParseState,
    /// File names in load order.
    pub loaded_mods: Vec<String>,
    pub warnings: Vec<String>,
}

fn stem(filename: &str) -> String {
    let name = filename.to_lowercase();
    for ext in VALID_MOD_EXTS {
        if let Some(s) = name.strip_suffix(ext) {
            return s.to_string();
        }
    }
    name
}

static PRIORITY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^#?\s*Priority\s*:\s*(\d+)").unwrap());

/// Priority from the header (first 50 header lines), default 1000.
fn header_priority(content: &str) -> i64 {
    let mut count = 0;
    for line in content.lines() {
        let stripped = py_strip(line);
        if !stripped.is_empty() && !stripped.starts_with('#') && stripped != "[Mod]" {
            break;
        }
        if let Some(c) = PRIORITY_RE.captures(stripped) {
            return c[1].parse().unwrap_or(1000);
        }
        count += 1;
        if count >= 50 {
            break;
        }
    }
    1000
}

static LEGACY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^#?\s*Legacy\s*:\s*(\S*)").unwrap());

/// `# Legacy: all` / `# Legacy: first` in the header: how this mod's `~` / `~~` blocks are applied.
/// `Some(true)` = replace every match (legacy phase), `Some(false)` = first match only, `None` = no header
/// (the caller's default). The second value is the text of an unknown value, for a warning.
fn header_legacy(content: &str) -> (Option<bool>, Option<String>) {
    let mut count = 0;
    for line in content.lines() {
        let stripped = py_strip(line);
        if !stripped.is_empty() && !stripped.starts_with('#') && stripped != "[Mod]" {
            break;
        }
        if let Some(c) = LEGACY_RE.captures(stripped) {
            let value = c[1].to_lowercase();
            return match value.as_str() {
                "all" => (Some(true), None),
                "first" => (Some(false), None),
                _ => (None, Some(c[1].to_string())),
            };
        }
        count += 1;
        if count >= 50 {
            break;
        }
    }
    (None, None)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in &entries {
        if e.file_type()?.is_dir() {
            if e.file_name() == "logs" {
                continue;
            }
            collect_files(&e.path(), out)?;
        }
    }
    for e in &entries {
        if e.file_type()?.is_file() {
            out.push(e.path());
        }
    }
    Ok(())
}

/// A line of a mod file that starts with this (in the first column) is a comment: it is removed before the
/// mod is parsed, so it never ends up in the game. Not a KittyPatcher feature (it would inject the line), so
/// mods that use it only work with this engine. Chosen because no existing mod has a line starting this way
/// (`#` starts CSS ids, `//` and `/*` are JavaScript / CSS / SugarCube markup).
pub const COMMENT_PREFIX: &str = "##";

/// Drop the comment lines (see [`COMMENT_PREFIX`]); text without any is returned unchanged.
pub fn strip_comment_lines(text: &str) -> String {
    if !text.contains(COMMENT_PREFIX) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if !line.starts_with(COMMENT_PREFIX) {
            out.push_str(line);
        }
    }
    out
}

/// Read a mod file the way Python text mode does (strict UTF-8, universal newlines), minus comment lines.
pub fn read_mod_source(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let text = String::from_utf8(bytes).with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    Ok(strip_comment_lines(&universal_newlines(&text)))
}

/// Switches that change how mods are read. The defaults reproduce the m0on KittyPatcher 0.7.6 v2 behaviour.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadOptions {
    /// Default for the old `~` / `~~` blocks of mods that have no `# Legacy:` header. When on, those blocks are
    /// not part of the normal replace sequence: they are applied first, before everything else, in load order,
    /// and each one replaces *every* match in the whole file (KittyPatcher 0.1.5c behaviour) instead of only the
    /// first one. `Replace:` blocks and all other directives are unchanged. A mod can decide for itself with
    /// `# Legacy: all` (replace every match) or `# Legacy: first` (first match only) in its header.
    pub legacy_replace_all: bool,
}

/// `load_mods`
pub fn load_mods(mods_dir: &Path) -> Result<LoadResult> {
    load_mods_with(mods_dir, &LoadOptions::default())
}

/// [`load_mods`] with options.
pub fn load_mods_with(mods_dir: &Path, opts: &LoadOptions) -> Result<LoadResult> {
    let mut files = Vec::new();
    collect_files(mods_dir, &mut files)?;
    load_mod_files_with(&files, opts)
}

/// Same as [`load_mods`] but for an explicit list of files (the packer passes the `.mod`
/// files it found). Files with other extensions are ignored; `.json` files only produce a warning.
pub fn load_mod_files(files: &[PathBuf]) -> Result<LoadResult> {
    load_mod_files_with(files, &LoadOptions::default())
}

/// [`load_mod_files`] with options.
pub fn load_mod_files_with(files: &[PathBuf], opts: &LoadOptions) -> Result<LoadResult> {
    let mut warnings = Vec::new();

    // stem -> path (first one wins), like `all_files`
    let mut all: Vec<(String, PathBuf)> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for f in files {
        let fname = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if VALID_MOD_EXTS.iter().any(|ext| fname.ends_with(ext)) {
            let s = stem(fname);
            if !seen.contains_key(&s) {
                seen.insert(s.clone(), all.len());
                all.push((s, f.clone()));
            }
        }
        if fname.to_lowercase().ends_with(".json") {
            warnings.push(format!(
                "{fname}: dependency sidecars / cm_dependencies.json are not supported by the builtin engine yet; load order may differ from the Python patcher"
            ));
        }
    }

    // read once, priorities from headers
    let mut contents: HashMap<String, String> = HashMap::new();
    let mut prio: HashMap<String, i64> = HashMap::new();
    for (s, p) in &all {
        let c = read_mod_source(p)?;
        prio.insert(s.clone(), header_priority(&c));
        contents.insert(s.clone(), c);
    }
    all.sort_by(|a, b| (prio[&a.0], &a.0).cmp(&(prio[&b.0], &b.0)));

    let mut global = ParseState::default();
    let mut loaded = Vec::new();

    for (s, path) in &all {
        let mod_file = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let content = &contents[s];

        let (header_mode, bad_value) = header_legacy(content);
        if let Some(v) = bad_value {
            warnings.push(format!("{mod_file}: unknown value '{v}' in '# Legacy:' header (use 'all' or 'first'); ignored"));
        }
        let legacy_all = header_mode.unwrap_or(opts.legacy_replace_all);

        let mut local = ParseState::default();
        if legacy_all {
            // legacy blocks go to the legacy phase; only their bookkeeping (conflict check, index, owner) is kept
            let mut legacy = ParseState::default();
            proc_replacement_old(content, &mut legacy, &mod_file);
            for (old, new) in std::mem::take(&mut legacy.legacy_pairs) {
                global.legacy_list.push(LegacyEntry { old, new, mod_file: mod_file.clone() });
            }
            local.conflict_map = legacy.conflict_map;
            local.mod_file_indexes = legacy.mod_file_indexes;
            local.key_to_mod = legacy.key_to_mod;
        } else {
            proc_replacement_old(content, &mut local, &mod_file);
        }
        proc_replacement_new(content, &mut local, &mod_file);

        // ---- merge in load order ----
        for (k, v) in local.mod_dict {
            global.mod_dict.insert(k, v);
        }
        let mut existing: std::collections::HashSet<String> = global.mod_list.iter().map(|(k, _)| k.clone()).collect();
        for (mk, msrc) in &local.mod_list {
            if existing.contains(mk) {
                let prior: Vec<&str> = global.mod_list.iter().filter(|(k2, _)| k2 == mk).map(|(_, m)| m.as_str()).collect();
                global.log.push(format!(
                    "ORDERING WARNING: Multiple mods inject at anchor '{}' -- load-order determines injection sequence: {}, {}.",
                    py_strip(&mk.chars().take(80).collect::<String>()),
                    prior.join(", "),
                    msrc
                ));
            }
            existing.insert(mk.clone());
        }
        global.mod_list.extend(local.mod_list);
        for (k, v) in local.mod_file_indexes {
            global.mod_file_indexes.entry(k).or_default().extend(v);
        }
        global.mod_reg_list.extend(local.mod_reg_list);
        global.mod_struct_list.extend(local.mod_struct_list);
        global.mod_func_list.extend(local.mod_func_list);
        global.mod_hook_list.extend(local.mod_hook_list);
        for (k, v) in local.conflict_map {
            global.conflict_map.entry(k).or_default().extend(v);
        }
        for prec in local.passage_registry {
            let pname = prec.name.clone();
            if !pname.is_empty() && !global.passage_names_seen.contains_key(&pname) {
                global.passage_names_seen.insert(pname, global.passage_registry.len());
                global.passage_registry.push(prec);
            }
        }
        global.js_registry.extend(local.js_registry);
        global.css_registry.extend(local.css_registry);
        global.events_registry.extend(local.events_registry);
        for (k, v) in local.key_to_mod {
            global.key_to_mod.insert(k, v);
        }
        global.log.extend(local.log);
        global.unsupported.extend(local.unsupported);
        loaded.push(mod_file);
    }

    absorb_inner_cross_mod(&mut global);

    Ok(LoadResult { state: global, loaded_mods: loaded, warnings })
}

#[cfg(test)]
mod comment_tests {
    use super::*;

    #[test]
    fn strips_only_lines_that_start_with_the_prefix() {
        let text = "# Name: x\n## a comment\nReplace:\nfoo\n  ## indented, kept\nWith:\nbar ## trailing, kept\n##\n#id { color: red; }\n";
        assert_eq!(
            strip_comment_lines(text),
            "# Name: x\nReplace:\nfoo\n  ## indented, kept\nWith:\nbar ## trailing, kept\n#id { color: red; }\n"
        );
    }

    #[test]
    fn text_without_comments_is_unchanged() {
        let text = "Replace:\nfoo\nWith:\nbar\n// js comment\n/* css comment */\n";
        assert_eq!(strip_comment_lines(text), text);
        assert_eq!(strip_comment_lines(""), "");
    }

    #[test]
    fn last_line_without_newline() {
        assert_eq!(strip_comment_lines("a\n## end"), "a\n");
        assert_eq!(strip_comment_lines("a\nb"), "a\nb");
    }

    fn parse_one(dir_name: &str, body: &str) -> serde_json::Value {
        let dir = std::env::temp_dir().join(format!("kitty-comment-test-{}-{dir_name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.mod"), body).unwrap();
        let res = load_mods(&dir).unwrap();
        let json = crate::dump::to_json(&res.loaded_mods, &res.state);
        let _ = std::fs::remove_dir_all(&dir);
        json
    }

    #[test]
    fn a_commented_mod_parses_like_the_same_mod_without_comments() {
        let plain = "# Name: demo\nReplace:\nold line\nWith:\nnew line\nAdd Passage: Demo\ntags: a b\nHello there\n";
        let commented = "# Name: demo\n## what this mod does\nReplace:\n## the anchor\nold line\nWith:\n## the replacement\nnew line\n##\nAdd Passage: Demo\n## tags next\ntags: a b\nHello there\n## end\n";
        assert_eq!(parse_one("plain", plain), parse_one("commented", commented));
    }
}
