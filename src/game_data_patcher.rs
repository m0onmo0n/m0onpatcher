use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use flickzeug::Diff;
use log::{debug, error, info, warn};
use regex::Regex;

use crate::config::{Config, EngineMode};
use crate::conflict::{ConflictManager, Resolution};
use crate::context_paths;
use crate::encoding::url_encode_file_name;
use crate::extraction_naming::include_hash_in_passage_file_name;
use crate::game_data::{
    GameData, ModuleType, UserDataType, CSS_TYPE, JS_TYPE, PASSAGE_DIRECTORY, PASSAGE_EXTENSION,
    SYSTEM_LIBRARIES_FILE_NAME, SYSTEM_SCRIPTS_DIRECTORY, SYSTEM_SCRIPT_FILE_NAME,
};
use crate::game_data_merger;
use crate::game_data_reader;
use crate::mod_data::ModData;
use crate::mod_data_reader;
use crate::mod_error_registry::ModErrorRegistry;
use crate::tool_paths::{self, ToolCommand};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchSuccess {
    Clean,
    WithWarnings,
    WithErrors,
}

pub fn patch(
    html_path: &Path,
    mods_directory: &Path,
    source_patch_directory: &Path,
    conflict_manager: &mut ConflictManager,
    config: &Config,
) -> Result<PatchSuccess> {
    // Registry is created first so apply_kitty_mods can record its own
    // per-mod success/failure outcomes (mirrors Kotlin applyKittyMods).
    let mut registry = ModErrorRegistry::new();

    // Run the KittyPatcher phase first. If it succeeds it returns a
    // GameData already parsed from the KP-modified HTML — that becomes
    // the baseline for native .diff mods. Any failure is logged and we
    // fall back to the original HTML as if no .mod files were present.
    let data = match apply_kitty_mods(html_path, config, &mut registry) {
        Some(d) => d,
        None => game_data_reader::read_from_file(html_path)?,
    };
    let originals = compute_originals(&data)?;

    let raw_mods = mod_data_reader::read_mods(mods_directory, &mut registry)?;
    let sorted = resolve_load_order(deduplicate_mods(raw_mods, &mut registry), &mut registry);

    let mut current = originals.clone();
    let mut failed_mod_names: HashSet<String> = HashSet::new();

    for mod_data in &sorted {
        let dependency_failed = mod_data
            .dependencies
            .iter()
            .any(|d| failed_mod_names.contains(d));
        if dependency_failed {
            warn!(
                "Mod {} skipped because its dependencies failed to load",
                mod_data.diff_file_name
            );
            registry.record_failure(
                &mod_data.mod_name,
                &mod_data.diff_file_name,
                "dependency failed to load",
            );
            failed_mod_names.insert(mod_data.mod_name.clone());
            continue;
        }

        info!(
            "Applying native mod '{}' which modifies {} files",
            mod_data.mod_name,
            mod_data.modified_files.len()
        );
        match apply_mod(
            &originals,
            &sorted,
            &mut current,
            mod_data,
            source_patch_directory,
            conflict_manager,
            &mut registry,
        ) {
            Ok(true) => {
                info!(
                    "Mod {} patch successfully loaded",
                    mod_data.diff_file_name
                );
                registry.record_success();
            }
            Ok(false) => {
                warn!(
                    "Mod {} failed to load, its dependents will be skipped",
                    mod_data.diff_file_name
                );
                registry.record_failure(
                    &mod_data.mod_name,
                    &mod_data.diff_file_name,
                    "conflicts during patching",
                );
                failed_mod_names.insert(mod_data.mod_name.clone());
            }
            Err(err) => {
                error!(
                    "Mod {} failed to load due to unexpected internal error: {err:#}",
                    mod_data.diff_file_name
                );
                registry.record_failure(
                    &mod_data.mod_name,
                    &mod_data.diff_file_name,
                    "internal error during patching",
                );
                failed_mod_names.insert(mod_data.mod_name.clone());
            }
        }
    }

    let modded_html = derive_modded_output_path(html_path)?;
    game_data_merger::merge_from_memory(html_path, &modded_html, &current)?;

    Ok(report_results(&registry))
}

fn derive_modded_output_path(html_path: &Path) -> Result<PathBuf> {
    let parent = html_path
        .parent()
        .ok_or_else(|| anyhow!("html path has no parent"))?;
    let stem = html_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("html path has no stem"))?;
    Ok(parent.join(format!("{stem}.modded.html")))
}

fn report_results(registry: &ModErrorRegistry) -> PatchSuccess {
    let warnings = registry.files_with_warnings();
    if !warnings.is_empty() {
        let joined = warnings
            .iter()
            .map(|s| format!("    {s}"))
            .collect::<Vec<_>>()
            .join("\n");
        warn!(
            "The following files were merged optimistically and may contain errors:\n{joined}"
        );
    }

    let failures = registry.failures();
    if !failures.is_empty() {
        let successful = registry.success_count();
        let total = successful + failures.len() as u32;
        let formatted = failures
            .iter()
            .map(|f| {
                format!(
                    "\n    [{:<30}] {} (diff: {})",
                    f.mod_name, f.failure_reason, f.mod_diff_name
                )
            })
            .collect::<String>();
        error!(
            "Failed to load {} out of {}, errors:{formatted}",
            failures.len(),
            total
        );
        if successful == 0 {
            error!("ALL MODS FAILED TO LOAD");
        }
        PatchSuccess::WithErrors
    } else if !warnings.is_empty() {
        PatchSuccess::WithWarnings
    } else {
        info!("All {} mods loaded successfully", registry.success_count());
        PatchSuccess::Clean
    }
}

// ---- Mirrored from game_data_differ.rs: construct the same in-memory tree
// the extractor would have written out.

fn compute_originals(data: &GameData) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    add_user_data(&mut out, &data.styles, &CSS_TYPE);
    add_user_data(&mut out, &data.scripts, &JS_TYPE);
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
    user_data: &crate::game_data::UserData,
    ty: &UserDataType,
) {
    for entry in &user_data.entries {
        let content = user_data.user_data_raw_text
            [entry.raw_text_range.start..entry.raw_text_range.end]
            .to_string();
        out.insert(format!("{}/{}", ty.directory, entry.disk_file_name), content);
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
        out.insert(format!("{}/{}", ty.directory_name, file_name), content);
    }
}

fn add_passages(out: &mut BTreeMap<String, String>, data: &GameData) {
    let names = passage_file_names(&data.passages);
    for (passage, file_name) in data.passages.iter().zip(names) {
        let mut content = if passage.tags.is_empty() {
            format!("::{}\n", passage.name)
        } else {
            format!("::{} [{}]\n", passage.name, passage.tags.join(","))
        };
        content.push_str(&passage.decoded_text);
        out.insert(format!("{}/{}", PASSAGE_DIRECTORY, file_name), content);
    }
}

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

// ---- Mod ordering ----

fn deduplicate_mods(mods: Vec<ModData>, registry: &mut ModErrorRegistry) -> Vec<ModData> {
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for m in mods {
        if let Some(prev) = seen.get(&m.mod_name) {
            error!(
                "Mod at {} and {} both use mod name {}, only keeping the first",
                m.diff_file_name, prev, m.mod_name
            );
            registry.record_failure(&m.mod_name, &m.diff_file_name, "Duplicate mod name");
        } else {
            seen.insert(m.mod_name.clone(), m.diff_file_name.clone());
            out.push(m);
        }
    }
    out
}

fn resolve_load_order(unsorted: Vec<ModData>, registry: &mut ModErrorRegistry) -> Vec<ModData> {
    // Topological sort by dependency. Mods whose dependency names aren't in
    // the set aren't treated as a hard error here — the applier still skips
    // them later because their `dependencies` vector references unknown
    // names, which get flagged as failed.
    let name_to_index: HashMap<String, usize> = unsorted
        .iter()
        .enumerate()
        .map(|(i, m)| (m.mod_name.clone(), i))
        .collect();

    let mut reverse: Vec<Vec<usize>> = (0..unsorted.len()).map(|_| Vec::new()).collect();
    for (i, m) in unsorted.iter().enumerate() {
        for dep in &m.dependencies {
            if let Some(&di) = name_to_index.get(dep) {
                reverse[di].push(i);
            }
        }
    }

    let mut deps_left: Vec<BTreeSet<String>> = unsorted
        .iter()
        .map(|m| m.dependencies.iter().cloned().collect())
        .collect();
    let mut resolved: Vec<ModData> = Vec::new();
    let mut ready: Vec<usize> = (0..unsorted.len())
        .filter(|&i| deps_left[i].is_empty())
        .collect();
    let mut placed: HashSet<usize> = HashSet::new();

    let mut unsorted_opt: Vec<Option<ModData>> = unsorted.into_iter().map(Some).collect();

    while !ready.is_empty() {
        let mut next = Vec::new();
        for idx in std::mem::take(&mut ready) {
            if placed.contains(&idx) {
                continue;
            }
            placed.insert(idx);
            let m = unsorted_opt[idx].take().unwrap();
            let name = m.mod_name.clone();
            resolved.push(m);
            for &dependent in &reverse[idx] {
                deps_left[dependent].remove(&name);
                if deps_left[dependent].is_empty() {
                    next.push(dependent);
                }
            }
        }
        ready = next;
    }

    for (i, slot) in unsorted_opt.iter_mut().enumerate() {
        if let Some(unresolved) = slot.take() {
            let deps = deps_left[i]
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            error!(
                "Failed to load mod {}, unresolved dependencies: {deps}",
                unresolved.mod_name
            );
            registry.record_failure(
                &unresolved.mod_name,
                &unresolved.diff_file_name,
                &format!("unresolved dependencies: {deps}"),
            );
        }
    }

    resolved
}

// ---- Per-mod application ----

pub(crate) fn apply_mod(
    originals: &BTreeMap<String, String>,
    all_sorted: &[ModData],
    current: &mut BTreeMap<String, String>,
    mod_data: &ModData,
    source_patch_directory: &Path,
    conflict_manager: &mut ConflictManager,
    registry: &mut ModErrorRegistry,
) -> Result<bool> {
    let diffs = flickzeug::patch_from_str(&mod_data.diff_text)
        .map_err(|e| anyhow!("diff parse failed: {e}"))?;

    // Set the conflict-template comment ahead of any 3-way merge this mod
    // may trigger. Matches the Kotlin `setComment` call at the top of
    // attemptMergeViaThreeWay.
    conflict_manager.recorder.set_comment(
        &format!(
            "Conflict with previously applied mods when applying mod {}\n\
             Section A: block with only current mod applied, Section B: original code (plus changes from dependencies if any), Section C: block with changes from previously applied mods\n\
             Replace the placeholder in the end with a version of the code that combines both changes from B->A, and B->C",
            mod_data.mod_name
        ),
        0,
    );

    // Base for any 3-way merges this mod triggers: originals with all
    // transitive dependencies applied. Mirrors Kotlin
    // threeWayPrepareBase, which uses the accumulated post-deps state
    // as the merge's common ancestor rather than the raw originals.
    // Built lazily — most mods apply cleanly without needing 3-way
    // merge, and materialising this map eagerly would mean parsing +
    // applying every dependency's diff for every mod.
    let mut base_with_deps: Option<BTreeMap<String, String>> = None;

    // Stage in a scratch map so a single-file failure doesn't half-apply.
    let mut staged: BTreeMap<String, Option<String>> = BTreeMap::new();
    let max_fuzz = mod_data.max_fuzz.unwrap_or(0);
    // Run the whole loop even when a per-file step fails, so the
    // user sees every problematic hunk in one log pass instead of
    // one fix-and-rerun cycle per failure. We commit nothing if any
    // step bailed.
    let mut had_failure = false;

    for diff in &diffs {
        let (rel_path, mode) = classify(diff)?;
        let ours_raw = current.get(&rel_path).cloned();
        let ours = ours_raw.clone().unwrap_or_default();

        let new_content = match apply_single(&ours, diff, mode, max_fuzz) {
            SingleResult::Ok(content) => content,
            SingleResult::Delete => {
                staged.insert(rel_path, None);
                continue;
            }
            SingleResult::Err(err) => {
                debug!("Direct apply failed for {rel_path}: {err}, attempting 3-way merge");
                let base_map = base_with_deps.get_or_insert_with(|| {
                    build_base_with_deps(originals, mod_data, all_sorted)
                });
                let base = base_map.get(&rel_path).cloned().unwrap_or_default();

                let theirs = match apply_clean_base(&base, diff, mode, max_fuzz) {
                    Some(SingleResult::Ok(c)) => c,
                    Some(SingleResult::Delete) => {
                        staged.insert(rel_path, None);
                        continue;
                    }
                    Some(SingleResult::Err(err)) => {
                        error!(
                            "Mod {} diff for {rel_path} does not apply cleanly to the original: {err}",
                            mod_data.diff_file_name
                        );
                        had_failure = true;
                        continue;
                    }
                    None => String::new(),
                };

                let outcome = three_way_merge(
                    &rel_path,
                    &base,
                    &ours,
                    &theirs,
                    conflict_manager,
                    registry,
                    source_patch_directory,
                    &mod_data.mod_name,
                );
                match outcome {
                    ThreeWayOutcome::Clean(content) => content,
                    ThreeWayOutcome::Optimistic(content) => {
                        registry.record_warning_for_file(&rel_path);
                        content
                    }
                    ThreeWayOutcome::Failed => {
                        had_failure = true;
                        continue;
                    }
                }
            }
        };

        staged.insert(rel_path, Some(new_content));
    }

    if had_failure {
        // Some file in this mod couldn't be applied. Discard the
        // scratch map so the live state stays at its pre-call value.
        return Ok(false);
    }

    // Commit.
    for (path, opt) in staged {
        match opt {
            Some(content) => {
                current.insert(path, content);
            }
            None => {
                current.remove(&path);
            }
        }
    }
    Ok(true)
}

/// Build the common-ancestor baseline for a 3-way merge: start from
/// `originals`, then layer this mod's transitive dependencies on top
/// (in their topological order). Equivalent of Kotlin
/// threeWayPrepareBase + getOrderedDependencies combined.
fn build_base_with_deps(
    originals: &BTreeMap<String, String>,
    mod_data: &ModData,
    all_sorted: &[ModData],
) -> BTreeMap<String, String> {
    let mut state = originals.clone();
    for dep in ordered_dependencies(all_sorted, mod_data) {
        debug!(
            "base-with-deps: applying dependency to base: {}",
            dep.mod_name
        );
        apply_mod_to_state(dep, &mut state);
    }
    state
}

/// Apply every diff hunk in `mod_data` to `state` in-place, ignoring
/// hunks that don't apply cleanly. Used only for the base-with-deps
/// construction above — a dependency's own 3-way fallback doesn't
/// happen here (Kotlin's threeWayPrepareBase also uses plain `patch`
/// for dep application, without recursive 3-way). If a dep's hunk
/// can't be applied the base is just missing that change; the current
/// mod's 3-way merge will then either cope or fail naturally.
fn apply_mod_to_state(mod_data: &ModData, state: &mut BTreeMap<String, String>) {
    let diffs = match flickzeug::patch_from_str(&mod_data.diff_text) {
        Ok(d) => d,
        Err(err) => {
            debug!(
                "base-with-deps: failed to parse diff for dep {}: {err}",
                mod_data.mod_name
            );
            return;
        }
    };
    for diff in &diffs {
        let (rel_path, mode) = match classify(diff) {
            Ok(v) => v,
            Err(err) => {
                debug!(
                    "base-with-deps: classify failed in dep {}: {err}",
                    mod_data.mod_name
                );
                continue;
            }
        };
        let ours = state.get(&rel_path).cloned().unwrap_or_default();
        match apply_single(&ours, diff, mode, mod_data.max_fuzz.unwrap_or(0)) {
            SingleResult::Ok(content) => {
                state.insert(rel_path, content);
            }
            SingleResult::Delete => {
                state.remove(&rel_path);
            }
            SingleResult::Err(err) => {
                debug!(
                    "base-with-deps: dep {} hunk on {rel_path} didn't apply: {err}",
                    mod_data.mod_name
                );
            }
        }
    }
}

/// Topologically-ordered transitive dependencies of `target`, looked
/// up in `all_sorted`. Mirrors Kotlin getOrderedDependencies: BFS over
/// dependency names into a set, then filter sortedMods to preserve
/// the outer load order.
fn ordered_dependencies<'a>(
    all_sorted: &'a [ModData],
    target: &ModData,
) -> Vec<&'a ModData> {
    let lookup: HashMap<&str, &ModData> = all_sorted
        .iter()
        .map(|m| (m.mod_name.as_str(), m))
        .collect();
    let mut required: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = target.dependencies.clone();
    while let Some(name) = queue.pop() {
        if required.insert(name.clone()) {
            if let Some(m) = lookup.get(name.as_str()) {
                queue.extend(m.dependencies.iter().cloned());
            }
        }
    }
    all_sorted
        .iter()
        .filter(|m| required.contains(&m.mod_name))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchMode {
    Create,
    Modify,
    Delete,
}

fn classify(diff: &Diff<'_, str>) -> Result<(String, PatchMode)> {
    let original = diff.original();
    let modified = diff.modified();

    let create = matches!(original, Some("/dev/null") | None);
    let delete = matches!(modified, Some("/dev/null") | None);

    if create && delete {
        return Err(anyhow!("diff has /dev/null on both sides"));
    }

    let path_source = if create {
        modified.ok_or_else(|| anyhow!("diff has no +++ filename"))?
    } else {
        original.ok_or_else(|| anyhow!("diff has no --- filename"))?
    };

    let rel_path = strip_diff_path(path_source)?;

    let mode = if create {
        PatchMode::Create
    } else if delete {
        PatchMode::Delete
    } else {
        PatchMode::Modify
    };

    Ok((rel_path, mode))
}

fn strip_diff_path(raw: &str) -> Result<String> {
    // Expect: `"a/<path>"` or `"b/<path>"` (with literal quotes) per the
    // format our differ emits. Also accept an unquoted variant if present.
    let inner = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw);
    let rel = inner
        .strip_prefix("a/")
        .or_else(|| inner.strip_prefix("b/"))
        .ok_or_else(|| anyhow!("unexpected diff path format: {raw}"))?;
    Ok(rel.to_string())
}

enum SingleResult {
    Ok(String),
    Delete,
    Err(String),
}

fn apply_single(
    base: &str,
    diff: &Diff<'_, str>,
    mode: PatchMode,
    max_fuzz: u32,
) -> SingleResult {
    if mode == PatchMode::Delete {
        return SingleResult::Delete;
    }
    // max_fuzz defaults to 0 (strict) so drifted-context hunks route
    // through 3-way merge where `.resolve` files can match — closer
    // parity with git/GNU-patch than a fuzzy-first apply. Mods can
    // raise it to 1 or 2 via their JSON manifest when they know their
    // hunk context is weak and would rather skip the 3-way dance.
    let config = flickzeug::ApplyConfig {
        line_end_strategy: flickzeug::LineEndHandling::default(),
        fuzzy_config: flickzeug::FuzzyConfig {
            max_fuzz: max_fuzz as usize,
            ignore_whitespace: false,
            ignore_case: false,
        },
    };
    match flickzeug::apply_with_config(base, diff, &config) {
        Ok((content, _stats)) => {
            // flickzeug's Create path reconstructs content from raw `+`
            // lines, and on Windows ends up with CRLF terminators even
            // when the source diff uses LF — which then stops a later
            // mod's LF-context hunk from matching the newly-created
            // file. Force LF for Create so the in-memory state is
            // line-ending-consistent across platforms. Modify mode
            // keeps whatever line endings the existing content had.
            let normalized = if mode == PatchMode::Create {
                content.replace("\r\n", "\n")
            } else {
                content
            };
            SingleResult::Ok(normalized)
        }
        Err(err) => SingleResult::Err(err.to_string()),
    }
}

fn apply_clean_base(
    base: &str,
    diff: &Diff<'_, str>,
    mode: PatchMode,
    max_fuzz: u32,
) -> Option<SingleResult> {
    match mode {
        PatchMode::Create => Some(apply_single("", diff, PatchMode::Create, max_fuzz)),
        PatchMode::Modify => Some(apply_single(base, diff, PatchMode::Modify, max_fuzz)),
        PatchMode::Delete => Some(SingleResult::Delete),
    }
}

enum ThreeWayOutcome {
    Clean(String),
    Optimistic(String),
    Failed,
}

fn three_way_merge(
    rel_path: &str,
    base: &str,
    ours: &str,
    theirs: &str,
    conflict_manager: &mut ConflictManager,
    _registry: &mut ModErrorRegistry,
    source_patch_directory: &Path,
    mod_name: &str,
) -> ThreeWayOutcome {
    // The Kotlin invokes `git merge-file -L a -L b -L c <mod_applied> <base>
    // <current>`, so label `a` = mod-applied and `c` = accumulated current.
    // Our `theirs` param holds mod-applied and `ours` holds accumulated state
    // (matching the usual git semantics). Pass them to diffy-imara swapped so
    // diffy-imara's `ours` side is mod-applied, then the relabel to `a`/`c`
    // yields the same conflict-block orientation the Kotlin resolutions use.
    let merge = diffy_imara::MergeOptions::new().merge(base, theirs, ours);
    match merge {
        Ok(merged) => {
            info!("3-way merge on {rel_path} for mod {mod_name} successful");
            ThreeWayOutcome::Clean(merged)
        }
        Err(raw_conflict_text) => {
            let conflict_text = relabel_conflict_markers(&raw_conflict_text);
            match attempt_union(rel_path, &conflict_text, conflict_manager) {
                UnionResult::Resolved(content, had_optimistic) => {
                    if had_optimistic {
                        info!(
                            "3-way merge on {rel_path} for mod {mod_name} resolved with optimistic union"
                        );
                        ThreeWayOutcome::Optimistic(content)
                    } else {
                        info!(
                            "3-way merge on {rel_path} for mod {mod_name} resolved via known conflict resolutions"
                        );
                        ThreeWayOutcome::Clean(content)
                    }
                }
                UnionResult::Failed => {
                    if let Err(err) = write_conflict_file(
                        source_patch_directory,
                        mod_name,
                        rel_path,
                        &conflict_text,
                    ) {
                        error!("Failed to write conflict file for {rel_path}: {err:#}");
                    }
                    ThreeWayOutcome::Failed
                }
            }
        }
    }
}

/// diffy-imara hard-codes the labels `ours` / `original` / `theirs` in its
/// conflict markers. Rewrite them to `a` / `b` / `c` — matching the Kotlin's
/// `-L a -L b -L c` flags — before we either attempt union resolution or
/// write the file to disk, so the user always sees the same labels.
///
/// `\n` is intentionally absent on each match: when a conflict region
/// runs to EOF without a trailing newline (which `git merge-file` and
/// diffy-imara both do), the closing marker won't have a `\n` after it.
/// Substring-replacing on just the marker itself catches both the
/// "marker followed by newline" and "marker at EOF" cases.
fn relabel_conflict_markers(text: &str) -> String {
    text.replace("<<<<<<< ours", "<<<<<<< a")
        .replace("||||||| original", "||||||| b")
        .replace(">>>>>>> theirs", ">>>>>>> c")
}

fn write_conflict_file(
    source_patch_directory: &Path,
    mod_name: &str,
    rel_path: &str,
    conflict_text: &str,
) -> Result<()> {
    let target = source_patch_directory.join(mod_name).join(rel_path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&target, conflict_text).with_context(|| format!("write {}", target.display()))?;
    error!(
        "Unable to auto-resolve conflict, wrote merged-with-markers file to {}",
        target.display()
    );
    Ok(())
}

enum UnionResult {
    Resolved(String, bool),
    Failed,
}

fn attempt_union(
    rel_path: &str,
    text: &str,
    conflict_manager: &mut ConflictManager,
) -> UnionResult {
    // After `relabel_conflict_markers`, Diff3 blocks use the Kotlin-style
    // labels `a` / `b` / `c`. Port of the Kotlin `attemptUnion`. The
    // trailing newline after `>>>>>>> c` is optional so a conflict
    // landing at EOF without a final `\n` still parses.
    let re = Regex::new(
        r"(?s)<<<<<<< a\n(.*?)\|{7} b\n(.*?)=======\n(.*?)>>>>>>> c(?:\n|\z)",
    )
    .unwrap();

    let mut out = String::new();
    let mut end_of_last = 0;
    let mut any_optimistic = false;
    let mut failure_count = 0;
    let mut match_count = 0;

    for caps in re.captures_iter(text) {
        match_count += 1;
        let whole = caps.get(0).unwrap();
        let current_text = caps.get(1).unwrap().as_str();
        let base_text = caps.get(2).unwrap().as_str();
        let other_text = caps.get(3).unwrap().as_str();

        out.push_str(&text[end_of_last..whole.start()]);

        let resolution: Option<&Resolution> = conflict_manager
            .index
            .find_resolution(rel_path, whole.as_str());

        if let Some(res) = resolution {
            info!(
                "Conflict in {rel_path} with known resolution {} detected",
                res.resolution_name
            );
            out.push_str(&res.resolution);
        } else if !base_text.is_empty() && base_text != "\n" {
            match find_common_prefix(current_text, other_text, base_text, rel_path) {
                Some(prefix) => {
                    out.push_str(current_text);
                    out.push_str(&other_text[prefix.len()..]);
                    any_optimistic = true;
                }
                None => {
                    let file_name = Path::new(rel_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(rel_path);
                    conflict_manager
                        .recorder
                        .record(file_name, rel_path, whole.as_str());
                    error!(
                        "Unable to auto-resolve conflict where lines were changed, not just added, in {rel_path}: {}",
                        whole.as_str()
                    );
                    failure_count += 1;
                }
            }
        } else {
            // When both sides are pure additions, splice them in
            // back-to-back. If the surrounding context didn't leave a
            // newline boundary between them, insert one so the first
            // added line doesn't accidentally fuse onto the previous
            // one.
            if !out.ends_with('\n') && !current_text.starts_with('\n') {
                out.push('\n');
            }
            out.push_str(current_text);
            if !out.ends_with('\n') && !other_text.starts_with('\n') {
                out.push('\n');
            }
            out.push_str(other_text);
            any_optimistic = true;
        }

        end_of_last = whole.end();
    }

    if failure_count > 0 {
        error!("Detected {failure_count} conflicts in {rel_path}, auto-resolve failed");
        return UnionResult::Failed;
    }

    out.push_str(&text[end_of_last..]);

    if out.contains("<<<<<<< a") {
        error!("Matching error, supposedly resolved file still contains conflict markers");
        return UnionResult::Failed;
    }

    let _ = match_count; // informational only
    UnionResult::Resolved(out, any_optimistic)
}

fn find_common_prefix<'a>(
    current: &'a str,
    other: &'a str,
    base: &'a str,
    file_name: &str,
) -> Option<&'a str> {
    if current.starts_with(base) && other.starts_with(base) {
        return Some(base);
    }
    if file_name.ends_with(".js") && (base.ends_with(",\n") || base.ends_with(";\n")) {
        let trimmed = &base[..base.len() - 1];
        if current.starts_with(trimmed) && other.starts_with(trimmed) {
            return Some(trimmed);
        }
    }
    None
}

// ---- KittyPatcher phase -------------------------------------------------

/// Runs KittyPatcher on the source HTML with any `.mod` files found in
/// the context's mods directory, then parses the resulting HTML and
/// returns its `GameData` so native .diff mods can layer on top.
///
/// Returns `None` for every non-happy path — missing mods, unresolved
/// KittyPatcher command, process failure, log-file verification failure,
/// post-run parse failure, or duplicate passage names. Callers treat
/// `None` as "no KittyPatcher phase happened" and continue with the
/// untouched HTML.
///
/// The registry receives one outcome per `.mod` file: a success each if
/// the phase produced a usable GameData, otherwise a failure each with
/// the same reason. Mirrors Kotlin `applyKittyMods` /
/// `applySelectedKittyMods` bookkeeping. The "no .mod files present"
/// early-out records nothing — there's nothing to succeed or fail.
fn apply_kitty_mods(
    html_path: &Path,
    config: &Config,
    registry: &mut ModErrorRegistry,
) -> Option<GameData> {
    let mods_dir = context_paths::context_locate_mods(html_path)?;
    if !mods_dir.is_dir() {
        return None;
    }
    let mod_files = match scan_kitty_mod_files(&mods_dir) {
        Ok(v) => v,
        Err(err) => {
            error!(
                "Failed to scan mods directory {}: {err}",
                mods_dir.display()
            );
            return None;
        }
    };
    if mod_files.is_empty() {
        debug!("No .mod files found, not performing Kitty phase");
        return None;
    }
    info!(
        "Found {} .mod files",
        mod_files.len()
    );

    match perform_kitty_patch(html_path, config, &mod_files) {
        Ok(data) => {
            for _ in 0..mod_files.len() {
                registry.record_success();
            }
            Some(data)
        }
        Err(reason) => {
            error!("Applying the .mod files failed: {reason}");
            for mf in &mod_files {
                let stem = mf
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                let name = mf
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                registry.record_failure(stem, name, &reason);
            }
            None
        }
    }
}

/// IO-heavy core extracted so every failure path can return a
/// single reason string that `apply_kitty_mods` uses both for logging
/// and for the per-mod registry entries.
pub(crate) fn perform_kitty_patch(
    html_path: &Path,
    config: &Config,
    mod_files: &[PathBuf],
) -> Result<GameData, String> {
    // Built-in engine first (unless the config asks for the external KittyPatcher only).
    let mode = config.engine_mode();
    if mode != EngineMode::External {
        match perform_builtin_patch(html_path, mod_files) {
            Ok(data) => return Ok(data),
            Err(BuiltinError::Unsupported(reason)) if mode == EngineMode::InternalFallback => {
                info!("Built-in patch engine cannot handle this mod set ({reason}); falling back to the external KittyPatcher");
            }
            Err(BuiltinError::Unsupported(reason)) => {
                return Err(format!(
                    "{reason}. The internal engine is selected; pick \"Internal engine, KittyPatcher as fallback\" or \"External KittyPatcher\" on the Config tab to patch this mod set"
                ));
            }
            Err(BuiltinError::Failed(reason)) => return Err(reason),
        }
    }

    let command = tool_paths::tool_command_kittypatcher(config, Some(html_path))
        .map_err(|err| format!("KittyPatcher not found ({err:?}); set its path on the Config tab or choose the internal engine"))?;

    let temp_dir = context_paths::context_locate_temp(html_path)
        .ok_or_else(|| "no temp directory resolved for kitty".to_string())?;
    let kitty_dir = temp_dir.join("kitty");
    if kitty_dir.exists() {
        fs::remove_dir_all(&kitty_dir)
            .map_err(|err| format!("clean {}: {err}", kitty_dir.display()))?;
    }
    fs::create_dir_all(&kitty_dir)
        .map_err(|err| format!("create {}: {err}", kitty_dir.display()))?;

    // Kotlin hard-codes the filename to what KittyPatcher expects; mirror
    // it here so the same mods work identically.
    let kitty_html = kitty_dir.join("CourseOfTemptation.html");
    let modified_before = kitty_html
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok());
    fs::copy(html_path, &kitty_html)
        .map_err(|err| format!("copy HTML into kitty dir: {err}"))?;

    let kitty_mods_dir = kitty_dir.join("mods");
    fs::create_dir_all(&kitty_mods_dir)
        .map_err(|err| format!("create {}: {err}", kitty_mods_dir.display()))?;
    for mf in mod_files {
        let name = mf
            .file_name()
            .ok_or_else(|| format!("{} has no file name", mf.display()))?;
        let dst = kitty_mods_dir.join(name);
        fs::copy(mf, &dst)
            .map_err(|err| format!("copy {} into kitty mods dir: {err}", mf.display()))?;
    }

    run_kitty_patcher(&command, &kitty_dir, &kitty_html)?;

    if !verify_kitty_success(&kitty_mods_dir.join("logs")) {
        return Err("KittyPatcher reported a failure (see tmp\\kitty\\mods\\logs\\FailsPatchLog.txt)".to_string());
    }

    // Some KittyPatcher builds write the result alongside the input as
    // `CourseOfTemptation.patched.html` rather than overwriting the
    // source. Prefer that when it exists; otherwise fall back to the
    // source path — but only if its mtime actually changed, since
    // in-place patchers are the only ones we can detect that way.
    let patched_alongside = kitty_dir.join("CourseOfTemptation.patched.html");
    let kitty_modded_html = if patched_alongside.is_file() {
        patched_alongside
    } else {
        let modified_after = kitty_html
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok());
        if modified_before == modified_after {
            return Err("KittyPatcher did not change the HTML (no mod could be applied)".to_string());
        }
        kitty_html
    };

    info!("KittyPatcher finished, reading its output");

    let data = game_data_reader::read_from_file(&kitty_modded_html).map_err(|err| {
        format!(
            "read KittyPatcher output {}: {err:#}",
            kitty_modded_html.display()
        )
    })?;

    if let Some(dup) = find_duplicate_passage_name(&data) {
        report_duplicate_passage_name(&dup, mod_files);
        return Err(format!("KittyPatcher mods caused duplicate passage {dup}"));
    }

    Ok(data)
}

enum BuiltinError {
    /// The engine does not support something in this mod set (the caller may fall back).
    Unsupported(String),
    /// The engine ran and the patch failed (or the input could not be read).
    Failed(String),
}

/// Run the built-in patch engine (crate `kitty-engine`) instead of the external KittyPatcher.
/// Writes the same three log files the external patcher writes into `tmp\kitty\mods\logs`.
fn perform_builtin_patch(html_path: &Path, mod_files: &[PathBuf]) -> Result<GameData, BuiltinError> {
    let started = std::time::Instant::now();
    info!("Applying {} .mod files with the internal engine", mod_files.len());
    let loaded = kitty_engine::load::load_mod_files(mod_files)
        .map_err(|err| BuiltinError::Failed(format!("built-in engine: {err:#}")))?;
    for w in &loaded.warnings {
        warn!("Built-in engine: {w}");
    }
    let html_text = fs::read_to_string(html_path)
        .map_err(|err| BuiltinError::Failed(format!("read {}: {err}", html_path.display())))?;
    let outcome = kitty_engine::apply::patch_with_progress(&html_text, &loaded.state, &loaded.loaded_mods, &mut |m| info!("{m}"))
        .map_err(|err| BuiltinError::Unsupported(format!("{err:#}")))?;

    write_builtin_logs(html_path, &outcome);

    if outcome.replacements_failed > 0 || !outcome.fail_log.is_empty() {
        for l in &outcome.fail_log {
            error!("{l}");
        }
        return Err(BuiltinError::Failed(format!(
            "built-in engine: {} replacement(s) failed (see tmp\\kitty\\mods\\logs\\FailsPatchLog.txt)",
            outcome.replacements_failed.max(outcome.fail_log.len())
        )));
    }
    info!(
        "Internal engine applied {} mod file(s): {} replacements made in {:.1} s",
        loaded.loaded_mods.len(),
        outcome.replacements_made,
        started.elapsed().as_secs_f64()
    );

    let data = game_data_reader::read_from_string(outcome.html)
        .map_err(|err| BuiltinError::Failed(format!("read built-in engine output: {err:#}")))?;
    if let Some(dup) = find_duplicate_passage_name(&data) {
        report_duplicate_passage_name(&dup, mod_files);
        return Err(BuiltinError::Failed(format!("mods caused duplicate passage {dup}")));
    }
    Ok(data)
}

fn write_builtin_logs(html_path: &Path, outcome: &kitty_engine::apply::Outcome) {
    let Some(temp_dir) = context_paths::context_locate_temp(html_path) else { return };
    let logs = temp_dir.join("kitty").join("mods").join("logs");
    if let Err(err) = fs::create_dir_all(&logs) {
        warn!("cannot create {}: {err}", logs.display());
        return;
    }
    let join = |lines: &[String]| {
        let mut s = lines.join("\n");
        if !s.is_empty() {
            s.push('\n');
        }
        s
    };
    let main = format!(
        "Built-in patch engine (kitty-engine)\nTotal replacements made: {}\nTotal replacements failed: {}\n",
        outcome.replacements_made, outcome.replacements_failed
    );
    for (name, text) in [
        ("FailsPatchLog.txt", join(&outcome.fail_log)),
        ("ModPatchLog.txt", format!("Mod patching started...\n{}", join(&outcome.log))),
        ("MainPatchLog.txt", main),
    ] {
        if let Err(err) = fs::write(logs.join(name), text) {
            warn!("cannot write {}: {err}", logs.join(name).display());
        }
    }
}

fn scan_kitty_mod_files(mods_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(mods_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.to_ascii_lowercase().ends_with(".mod") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn run_kitty_patcher(
    command: &ToolCommand,
    working_dir: &Path,
    html_path: &Path,
) -> Result<(), String> {
    let mut cmd = Command::new(&command.binary_path);
    cmd.args(&command.arguments)
        .arg(html_path)
        .current_dir(working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Force CPython stdio to UTF-8 so Rich (and anything else
        // KittyPatcher prints) doesn't crash on the legacy Windows
        // console codepage with `UnicodeEncodeError: 'charmap'`.
        // Both vars are no-ops outside CPython / non-UTF-8 platforms.
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1");

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("launch KittyPatcher: {err}"))?;

    // KittyPatcher prompts for a final keypress; Kotlin sends a bare "\n"
    // and closes stdin to skip the prompt without hanging.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"\n");
    }

    let output = child
        .wait_with_output()
        .map_err(|err| format!("wait for KittyPatcher: {err}"))?;

    if output.status.success() {
        return Ok(());
    }

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Kotlin tolerates a specific Python EOF on stdin as the "user
    // skipped the press-enter prompt" signal — same thing our \n
    // write can trip when the process has already exited.
    if exit_code == 1 && stderr.contains("EOFError: EOF when reading a line") {
        return Ok(());
    }
    info!("Standard output: {stdout}");
    info!("Standard error: {stderr}");
    Err(format!("KittyPatcher exit code {exit_code}"))
}

fn verify_kitty_success(logs_dir: &Path) -> bool {
    let fails = logs_dir.join("FailsPatchLog.txt");
    if !fails.is_file() {
        return false;
    }
    if fs::metadata(&fails).map(|m| m.len()).unwrap_or(0) != 0 {
        return false;
    }

    let mod_log = logs_dir.join("ModPatchLog.txt");
    if !mod_log.is_file() {
        return false;
    }
    if fs::metadata(&mod_log).map(|m| m.len()).unwrap_or(0) == 0 {
        return false;
    }
    true
}

fn find_duplicate_passage_name(data: &GameData) -> Option<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    for p in &data.passages {
        if !seen.insert(p.name.as_str()) {
            return Some(p.name.clone());
        }
    }
    None
}

/// Port of Kotlin's `reportDuplicatePassageName`: list every .mod file
/// whose content mentions the offending passage name (case-insensitive)
/// so the user has a shortlist of possible culprits. Pure logging —
/// caller decides how to react.
fn report_duplicate_passage_name(passage_name: &str, mod_files: &[PathBuf]) {
    let needle = passage_name.to_ascii_lowercase();
    let candidates: Vec<String> = mod_files
        .iter()
        .filter_map(|p| {
            let content = fs::read_to_string(p).ok()?;
            if content.to_ascii_lowercase().contains(&needle) {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();
    let joined = candidates.join(",");
    error!(
        "Passage {passage_name} is present multiple times in KittyPatcher patched HTML, possible candidates of mods causing this: {joined}"
    );
}
