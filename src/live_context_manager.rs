use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use indexmap::IndexSet;
use log::{debug, error, info};
use regex::Regex;

use crate::encoding::{
    html_encode, quoted_string_encode, remove_cr_newlines, url_decode_file_name,
    url_encode_file_name,
};
use crate::extraction_naming::is_valid_hash_name_match;
use crate::game_data::{
    GameData, ModuleEntry, ModuleType, Modules, ModulesOfType, Passage, UserData, UserDataType,
    CSS_MODULE_TYPE, CSS_TYPE, JS_MODULE_TYPE, JS_TYPE, PASSAGE_DIRECTORY, PASSAGE_EXTENSION,
    PASSAGE_HTML_TAG, SYSTEM_LIBRARIES_FILE_NAME, SYSTEM_SCRIPTS_DIRECTORY,
    SYSTEM_SCRIPT_FILE_NAME,
};

/// Live-render context, built once per `livemerge` invocation.
///
/// The surrounding HTML is split into fixed slabs (everything that doesn't
/// contain passages / styles / scripts / module blocks) that are computed
/// once up front. Then each dynamic region maintains a map of pre-rendered
/// per-entry bytes plus an insertion-ordered set of keys that are currently
/// active. Writing out the HTML just concatenates the slabs with the
/// currently-active renders in order.
///
/// The `extra_scripts` slab holds whatever JS the caller wants injected
/// right after `<body>` — live-merge uses this for an auto-reload stub.
pub struct LiveRenderContext {
    pub game_data: GameData,
    pub output_html_path: PathBuf,
    pub source_directory: PathBuf,

    /// Slab from byte 0 up to the start of the SugarCube libraries
    /// `<script>` body. When that script isn't present this just holds
    /// the original "everything before modules / body" prefix and
    /// `before_body_post_libs` is empty.
    pub before_body_pre_libs: Vec<u8>,
    pub before_body_post_libs: Vec<u8>,
    pub before_body_after_modules: Vec<u8>,
    pub before_styles: Vec<u8>,
    pub before_scripts: Vec<u8>,
    pub before_passages: Vec<u8>,
    /// Document tail from the last passage end up to the start of the
    /// SugarCube engine `<script>` body. When that script isn't present
    /// this holds the entire tail and `after_passages_post_script` is
    /// empty.
    pub after_passages_pre_script: Vec<u8>,
    pub after_passages_post_script: Vec<u8>,
    pub extra_scripts: Vec<u8>,

    pub passages: PassageContext,
    pub css: UserDataContext,
    pub js: UserDataContext,
    pub script_modules: Option<ModulesOfTypeContext>,
    pub style_modules: Option<ModulesOfTypeContext>,
    pub system_scripts: SystemScriptsContext,
}

/// Live state for the two SugarCube engine `<script>` slots. Each slot
/// is `Some` only when the source HTML actually had that script in its
/// expected position; at runtime the `render` bytes start out as the
/// original embedded body and get swapped for the on-disk
/// `system-scripts/<file>.js` whenever it exists / changes.
pub struct SystemScriptsContext {
    pub directory: PathBuf,
    pub libraries: Option<SystemScriptSlot>,
    pub script: Option<SystemScriptSlot>,
}

pub struct SystemScriptSlot {
    pub file_name: &'static str,
    pub original: Vec<u8>,
    pub render: Vec<u8>,
}

pub struct PassageContext {
    pub directory: PathBuf,
    pub html_mapping: HashMap<String, HtmlPassage>,
    pub renders: HashMap<String, Vec<u8>>,
    pub lookup_by_file: HashMap<String, String>,
    pub current_set: IndexSet<String>,
    pub preserved_passage_location: HashMap<String, PassageLocation>,
    pub highest_pid_seen: u64,
    pub highest_y_coordinate: i64,
}

#[derive(Clone)]
pub struct HtmlPassage {
    pub name: String,
    pub start_tag: String,
    pub tags_attr: String,
    pub pid: Option<u64>,
    pub y_coordinate: Option<i64>,
}

pub struct UserDataContext {
    pub type_: &'static UserDataType,
    pub user_data_raw_text: String,
    pub directory: PathBuf,
    pub html_mapping: HashMap<String, HtmlUserDataEntry>,
    pub renders: HashMap<String, Vec<u8>>,
    pub lookup_by_file: HashMap<String, String>,
    pub current_set: IndexSet<String>,
    pub preserved_index: HashMap<String, usize>,
    pub highest_file_index: usize,
}

#[derive(Clone)]
pub struct HtmlUserDataEntry {
    pub file_name: String,
    pub index: usize,
    pub raw_text: String,
}

pub struct ModulesOfTypeContext {
    pub type_: &'static ModuleType,
    pub directory: PathBuf,
    pub html_mapping: HashMap<String, HtmlModuleEntry>,
    pub renders: HashMap<String, Vec<u8>>,
    pub lookup_by_file: HashMap<String, String>,
    pub current_set: IndexSet<String>,
}

#[derive(Clone)]
pub struct HtmlModuleEntry {
    pub module_name: String,
    pub original_module_name: String,
    pub raw_text: String,
}

#[derive(Clone, Copy, Debug)]
pub struct PassageLocation {
    pub pid: u64,
    pub y_coordinate: i64,
}

/// One-shot builder. Loads all current source files and writes the output
/// HTML once before returning. Subsequent calls to `attempt_build` only
/// re-render the files whose paths appear in `modified_file_set`.
pub fn start_live_context(
    game_data: GameData,
    output_html_path: &Path,
    source_directory: &Path,
    extra_scripts: Vec<u8>,
) -> Result<LiveRenderContext> {
    let mut context = create_empty_context(game_data, output_html_path, source_directory, extra_scripts)?;
    reload_all_renders(&mut context)?;
    write_html(&context)?;
    Ok(context)
}

pub fn reload_all_renders(context: &mut LiveRenderContext) -> Result<()> {
    build_passage_set(context)?;
    reload_system_scripts(context);

    // Take the contexts out temporarily so we can borrow game_data at the
    // same time the per-entry render helpers need it. Restoring afterwards
    // keeps the orchestration code straightforward.
    let mut js = std::mem::replace(&mut context.js, dummy_user_data_ctx(&JS_TYPE));
    build_user_data_set(context, &mut js)?;
    context.js = js;

    let mut css = std::mem::replace(&mut context.css, dummy_user_data_ctx(&CSS_TYPE));
    build_user_data_set(context, &mut css)?;
    context.css = css;

    if let Some(mut script_modules) = context.script_modules.take() {
        build_module_set(context, &mut script_modules)?;
        context.script_modules = Some(script_modules);
    }
    if let Some(mut style_modules) = context.style_modules.take() {
        build_module_set(context, &mut style_modules)?;
        context.style_modules = Some(style_modules);
    }
    Ok(())
}

/// Apply the set of file paths touched since the last build and re-write
/// the output HTML. Each path is classified by parent directory — any
/// file outside one of the five tracked directories is ignored.
pub fn attempt_build(context: &mut LiveRenderContext, modified_files: &[PathBuf]) -> Result<()> {
    let start = std::time::Instant::now();
    for path in modified_files {
        if let Err(err) = detected_file_change(context, path) {
            error!("Handling change for {} failed: {err:#}", path.display());
        }
    }
    write_html(context)?;
    info!("Build took {}ms", start.elapsed().as_millis());
    Ok(())
}

fn create_empty_context(
    game_data: GameData,
    output_html_path: &Path,
    source_directory: &Path,
    extra_scripts: Vec<u8>,
) -> Result<LiveRenderContext> {
    let raw = game_data.document_raw_text.as_str();
    let body_start = game_data.body_start;
    let csp = game_data.csp_range;
    let modules = game_data.modules.as_ref();

    let head_endpoint = modules.map(|m| m.html_range.start).unwrap_or(body_start);
    let libs = game_data.system_components.sugarcube_libraries;
    // Splitting `before_body` around the libs script lets the live
    // renderer swap in fresh `system-scripts/libraries.js` bytes
    // without rebuilding the surrounding head every time. The libs
    // script is invariably ahead of CSP / modules / body, so the
    // pre-libs slab is a plain copy and only the post-libs slab can
    // contain the CSP swap.
    let (before_body_pre_libs, before_body_post_libs) = if let Some(libs_r) = libs {
        let mut pre = Vec::new();
        copy_range(&mut pre, raw, "beginning until libs", 0, libs_r.inner.start);
        let mut post = Vec::new();
        if let Some(csp) = csp.filter(|r| r.start < head_endpoint) {
            copy_range(&mut post, raw, "post-libs until CSP start", libs_r.inner.end, csp.start);
            write_csp_removal_comment(&mut post);
            copy_range(&mut post, raw, "end of CSP until head endpoint", csp.end, head_endpoint);
        } else {
            copy_range(&mut post, raw, "post-libs until head endpoint", libs_r.inner.end, head_endpoint);
        }
        (pre, post)
    } else {
        let mut buf = Vec::new();
        if let Some(csp) = csp.filter(|r| r.start < head_endpoint) {
            copy_range(&mut buf, raw, "beginning until CSP start", 0, csp.start);
            write_csp_removal_comment(&mut buf);
            copy_range(&mut buf, raw, "end of CSP until head endpoint", csp.end, head_endpoint);
        } else {
            copy_range(&mut buf, raw, "beginning until head endpoint", 0, head_endpoint);
        }
        (buf, Vec::new())
    };

    let before_body_after_modules = {
        let mut buf = Vec::new();
        if let Some(m) = modules {
            let modules_end = m.html_range.end;
            if let Some(csp) = csp.filter(|r| r.start >= modules_end) {
                copy_range(&mut buf, raw, "end of modules until CSP start", modules_end, csp.start);
                write_csp_removal_comment(&mut buf);
                copy_range(&mut buf, raw, "end of CSP until body start", csp.end, body_start);
            } else {
                copy_range(&mut buf, raw, "end of modules until body start", modules_end, body_start);
            }
        }
        buf
    };

    let styles_inner_start = game_data.styles.ranges.inner.start;
    let before_styles = {
        let mut buf = Vec::new();
        debug!(
            "Copying range from reference file: before style element contents ({body_start}, {styles_inner_start})"
        );
        buf.extend_from_slice(&raw.as_bytes()[body_start..styles_inner_start]);
        buf
    };

    let before_scripts = {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"</style>");
        buf.extend_from_slice(
            &raw.as_bytes()
                [game_data.scripts.ranges.outer.start..game_data.scripts.ranges.inner.start],
        );
        buf
    };

    let first_passage_start = game_data
        .passages
        .first()
        .ok_or_else(|| anyhow!("source HTML contains no passages"))?
        .ranges
        .outer
        .start;
    let last_passage_end = game_data.passages.last().unwrap().ranges.outer.end;

    let before_passages = {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"</script>");
        buf.extend_from_slice(
            &raw.as_bytes()[game_data.scripts.ranges.outer.end..first_passage_start],
        );
        buf
    };

    let script = game_data.system_components.sugarcube_script;
    let (after_passages_pre_script, after_passages_post_script) = if let Some(script_r) = script {
        (
            raw.as_bytes()[last_passage_end..script_r.inner.start].to_vec(),
            raw.as_bytes()[script_r.inner.end..].to_vec(),
        )
    } else {
        (raw.as_bytes()[last_passage_end..].to_vec(), Vec::new())
    };

    let system_scripts = SystemScriptsContext {
        directory: source_directory.join(SYSTEM_SCRIPTS_DIRECTORY),
        libraries: libs.map(|r| SystemScriptSlot {
            file_name: SYSTEM_LIBRARIES_FILE_NAME,
            original: raw.as_bytes()[r.inner.start..r.inner.end].to_vec(),
            render: raw.as_bytes()[r.inner.start..r.inner.end].to_vec(),
        }),
        script: script.map(|r| SystemScriptSlot {
            file_name: SYSTEM_SCRIPT_FILE_NAME,
            original: raw.as_bytes()[r.inner.start..r.inner.end].to_vec(),
            render: raw.as_bytes()[r.inner.start..r.inner.end].to_vec(),
        }),
    };

    let passages_ctx = build_passage_ctx(&game_data, source_directory);
    let css_ctx = build_user_data_ctx(&CSS_TYPE, &game_data.styles, source_directory);
    let js_ctx = build_user_data_ctx(&JS_TYPE, &game_data.scripts, source_directory);
    let (script_modules_ctx, style_modules_ctx) = match &game_data.modules {
        Some(Modules {
            script_modules,
            style_modules,
            ..
        }) => (
            Some(build_module_ctx(script_modules, source_directory)),
            Some(build_module_ctx(style_modules, source_directory)),
        ),
        None => (None, None),
    };

    Ok(LiveRenderContext {
        game_data,
        output_html_path: output_html_path.to_path_buf(),
        source_directory: source_directory.to_path_buf(),
        before_body_pre_libs,
        before_body_post_libs,
        before_body_after_modules,
        before_styles,
        before_scripts,
        before_passages,
        after_passages_pre_script,
        after_passages_post_script,
        extra_scripts,
        passages: passages_ctx,
        css: css_ctx,
        js: js_ctx,
        script_modules: script_modules_ctx,
        style_modules: style_modules_ctx,
        system_scripts,
    })
}

fn copy_range(buf: &mut Vec<u8>, raw: &str, description: &str, start: usize, end: usize) {
    debug!("Copying range from reference file: {description} ({start}, {end})");
    buf.extend_from_slice(&raw.as_bytes()[start..end]);
}

fn write_csp_removal_comment(buf: &mut Vec<u8>) {
    buf.extend_from_slice(b"<!-- CSP removed to allow the localhost live-reload WebSocket connection -->");
}

fn build_passage_ctx(data: &GameData, source_directory: &Path) -> PassageContext {
    let mut html_mapping = HashMap::new();
    for p in &data.passages {
        html_mapping.insert(p.name.clone(), to_html_passage(data, p));
    }
    PassageContext {
        directory: source_directory.join(PASSAGE_DIRECTORY),
        html_mapping,
        renders: HashMap::new(),
        lookup_by_file: HashMap::new(),
        current_set: IndexSet::new(),
        preserved_passage_location: HashMap::new(),
        highest_pid_seen: 0,
        highest_y_coordinate: 0,
    }
}

fn to_html_passage(data: &GameData, p: &Passage) -> HtmlPassage {
    let start_tag =
        data.document_raw_text[p.ranges.outer.start..p.ranges.inner.start].to_string();
    let tags_attr = p.attributes.tags.clone().unwrap_or_default();
    let pid = p.attributes.pid.as_deref().and_then(|s| s.parse::<u64>().ok());
    let y_coordinate = p
        .attributes
        .position
        .as_deref()
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.parse::<i64>().ok());
    HtmlPassage {
        name: p.name.clone(),
        start_tag,
        tags_attr,
        pid,
        y_coordinate,
    }
}

fn build_user_data_ctx(
    ty: &'static UserDataType,
    data: &UserData,
    source_directory: &Path,
) -> UserDataContext {
    let mut html_mapping = HashMap::new();
    for e in &data.entries {
        html_mapping.insert(
            e.file_name.clone(),
            HtmlUserDataEntry {
                file_name: e.file_name.clone(),
                index: e.index,
                raw_text: data.user_data_raw_text
                    [e.raw_text_range.start..e.raw_text_range.end]
                    .to_string(),
            },
        );
    }
    UserDataContext {
        type_: ty,
        user_data_raw_text: data.user_data_raw_text.clone(),
        directory: source_directory.join(ty.directory),
        html_mapping,
        renders: HashMap::new(),
        lookup_by_file: HashMap::new(),
        current_set: IndexSet::new(),
        preserved_index: HashMap::new(),
        highest_file_index: 0,
    }
}

fn build_module_ctx(
    modules: &ModulesOfType,
    source_directory: &Path,
) -> ModulesOfTypeContext {
    let mut html_mapping = HashMap::new();
    for e in &modules.entries {
        html_mapping.insert(
            e.module_name.clone(),
            HtmlModuleEntry {
                module_name: e.module_name.clone(),
                original_module_name: e.original_module_name.clone(),
                raw_text: String::new(), // we only need raw_text for "modified vs not" log; skip to keep ctx small
            },
        );
        let _ = e as &ModuleEntry; // silence unused without an extra import
    }
    ModulesOfTypeContext {
        type_: modules.module_type,
        directory: source_directory.join(modules.module_type.directory_name),
        html_mapping,
        renders: HashMap::new(),
        lookup_by_file: HashMap::new(),
        current_set: IndexSet::new(),
    }
}

fn dummy_user_data_ctx(ty: &'static UserDataType) -> UserDataContext {
    UserDataContext {
        type_: ty,
        user_data_raw_text: String::new(),
        directory: PathBuf::new(),
        html_mapping: HashMap::new(),
        renders: HashMap::new(),
        lookup_by_file: HashMap::new(),
        current_set: IndexSet::new(),
        preserved_index: HashMap::new(),
        highest_file_index: 0,
    }
}

// ---------- passage rendering ----------

fn build_passage_set(context: &mut LiveRenderContext) -> Result<()> {
    context.passages.current_set.clear();
    let dir = context.passages.directory.clone();
    let parent = dir.parent().unwrap_or(&dir).to_path_buf();
    let source_passages = read_passages(&dir, &parent)?;
    let source_map: HashMap<String, &PassageFile> =
        source_passages.iter().map(|p| (p.passage_name.clone(), p)).collect();

    let html_names: Vec<String> = context
        .game_data
        .passages
        .iter()
        .map(|p| p.name.clone())
        .collect();

    for name in &html_names {
        if let Some(sp) = source_map.get(name) {
            context.passages.current_set.insert(name.clone());
            context
                .passages
                .lookup_by_file
                .insert(sp.relative_path.clone(), name.clone());
            render_passage(context, sp, true);
        } else {
            info!("Passage {name}: deleted as file was removed from source");
        }
    }
    for sp in &source_passages {
        if context.passages.html_mapping.contains_key(&sp.passage_name) {
            continue;
        }
        context.passages.current_set.insert(sp.passage_name.clone());
        context
            .passages
            .lookup_by_file
            .insert(sp.relative_path.clone(), sp.passage_name.clone());
        render_passage(context, sp, false);
    }
    Ok(())
}

fn render_passage(context: &mut LiveRenderContext, source: &PassageFile, has_html_entry: bool) {
    let mut out: Vec<u8> = Vec::new();
    let html_passage = if has_html_entry {
        context.passages.html_mapping.get(&source.passage_name).cloned()
    } else {
        None
    };

    if let Some(hp) = &html_passage {
        if hp.tags_attr != source.tags {
            let needle = format!("tags=\"{}\"", hp.tags_attr);
            let replacement = format!("tags=\"{}\"", source.tags);
            out.extend_from_slice(hp.start_tag.replacen(&needle, &replacement, 1).as_bytes());
            info!(
                "Passage {}: tags modified from [{}] to [{}]",
                hp.name, hp.tags_attr, source.tags
            );
        } else {
            out.extend_from_slice(hp.start_tag.as_bytes());
        }

        if let Some(pid) = hp.pid {
            context.passages.highest_pid_seen = context.passages.highest_pid_seen.max(pid);
        }
        if let Some(y) = hp.y_coordinate {
            context.passages.highest_y_coordinate = context.passages.highest_y_coordinate.max(y);
        }
    } else {
        let location = context
            .passages
            .preserved_passage_location
            .get(&source.passage_name)
            .copied()
            .unwrap_or_else(|| {
                context.passages.highest_y_coordinate += 125;
                context.passages.highest_pid_seen += 1;
                let loc = PassageLocation {
                    pid: context.passages.highest_pid_seen,
                    y_coordinate: context.passages.highest_y_coordinate,
                };
                context
                    .passages
                    .preserved_passage_location
                    .insert(source.passage_name.clone(), loc);
                loc
            });

        out.extend_from_slice(
            format!(
                "<{PASSAGE_HTML_TAG} pid=\"{}\" name=\"{}\" tags=\"{}\" position=\"100,{}\" size=\"100,100\">",
                location.pid,
                source.passage_name,
                source.tags,
                location.y_coordinate,
            )
            .as_bytes(),
        );
    }

    out.extend_from_slice(html_encode(&source.data).as_bytes());
    out.extend_from_slice(format!("</{PASSAGE_HTML_TAG}>").as_bytes());

    context
        .passages
        .renders
        .insert(source.passage_name.clone(), out);
}

// ---------- user data rendering ----------

fn build_user_data_set(context: &LiveRenderContext, subctx: &mut UserDataContext) -> Result<()> {
    subctx.current_set.clear();
    let sources = collect_user_data(&subctx.directory, subctx.type_.naming_matcher)?;
    let source_names: HashMap<String, &UserDataFile> =
        sources.iter().map(|f| (f.file_name.clone(), f)).collect();

    // Existing HTML entries first, preserving their original order.
    let ordered_html: Vec<String> = {
        // The GameData stores entries in a Vec so iteration preserves order;
        // we just need the names.
        let data = match subctx.type_.html_id {
            "twine-user-stylesheet" => &context.game_data.styles,
            "twine-user-script" => &context.game_data.scripts,
            _ => bail!("unknown user data type"),
        };
        data.entries.iter().map(|e| e.file_name.clone()).collect()
    };

    for name in &ordered_html {
        if let Some(src) = source_names.get(name) {
            subctx.current_set.insert(name.clone());
            subctx
                .lookup_by_file
                .insert(src.relative_path.clone(), name.clone());
            render_user_data(subctx, src, true);
        } else {
            info!(
                "{} {}: deleted as file was removed from source",
                subctx.type_.log_name.to_lowercase(),
                name
            );
        }
    }

    for src in &sources {
        if subctx.html_mapping.contains_key(&src.file_name) {
            continue;
        }
        subctx.current_set.insert(src.file_name.clone());
        subctx
            .lookup_by_file
            .insert(src.relative_path.clone(), src.file_name.clone());
        render_user_data(subctx, src, false);
    }
    Ok(())
}

fn render_user_data(subctx: &mut UserDataContext, source: &UserDataFile, has_html_entry: bool) {
    let html_entry = if has_html_entry {
        subctx.html_mapping.get(&source.file_name).cloned()
    } else {
        None
    };

    let index = if let Some(e) = &html_entry {
        subctx.highest_file_index = subctx.highest_file_index.max(e.index);
        e.index
    } else if let Some(&ix) = subctx.preserved_index.get(&source.file_name) {
        ix
    } else {
        subctx.highest_file_index += 1;
        let ix = subctx.highest_file_index;
        subctx
            .preserved_index
            .insert(source.file_name.clone(), ix);
        ix
    };

    let mut out: Vec<u8> = Vec::new();
    let quoted = quoted_string_encode(&source.file_name);
    out.extend_from_slice(
        format!("/* {} #{}: \"{}\" */\n", subctx.type_.html_id, index, quoted).as_bytes(),
    );
    let text = remove_cr_newlines(
        &fs::read_to_string(subctx.directory.join(&source.disk_file_name))
            .with_context(|| format!("read {}", source.disk_file_name))
            .unwrap_or_default(),
    );
    out.extend_from_slice(text.as_bytes());

    if let Some(e) = &html_entry {
        if e.raw_text != text {
            info!(
                "{} {}: contents modified from reference HTML",
                subctx.type_.log_name, source.file_name
            );
        }
    } else {
        info!(
            "{} {}: added compared to reference HTML",
            subctx.type_.log_name, source.file_name
        );
    }

    subctx.renders.insert(source.file_name.clone(), out);
}

// ---------- module rendering ----------

fn build_module_set(_context: &LiveRenderContext, subctx: &mut ModulesOfTypeContext) -> Result<()> {
    subctx.current_set.clear();
    let sources = collect_modules_of_type(&subctx.directory, subctx.type_)?;
    let source_map: HashMap<String, &ModuleFile> =
        sources.iter().map(|f| (f.module_name.clone(), f)).collect();

    let mut html_mapping_keys: Vec<String> = subctx.html_mapping.keys().cloned().collect();
    html_mapping_keys.sort(); // stable order for non-script-module case (empty map is fine)

    // The Kotlin iterates in `modulesOfType.entries` order; we don't have
    // that sequence retained after the context is built, but the set of
    // names is the same and the final writeHtml order only follows
    // `current_set` insertion. We seed current_set with source entries that
    // also appear in html_mapping, in HTML-defined order by name, then
    // append pure-source additions.
    for name in &html_mapping_keys {
        if let Some(src) = source_map.get(name) {
            subctx.current_set.insert(name.clone());
            subctx
                .lookup_by_file
                .insert(src.relative_path.clone(), name.clone());
            render_module(subctx, src);
        } else {
            info!(
                "{} {name}: deleted as file was removed from source",
                subctx.type_.directory_name
            );
        }
    }

    for src in &sources {
        if subctx.html_mapping.contains_key(&src.module_name) {
            continue;
        }
        subctx.current_set.insert(src.module_name.clone());
        subctx
            .lookup_by_file
            .insert(src.relative_path.clone(), src.module_name.clone());
        render_module(subctx, src);
        info!(
            "{} {}: added as new",
            subctx.type_.directory_name, src.module_name
        );
    }

    Ok(())
}

fn render_module(subctx: &mut ModulesOfTypeContext, source: &ModuleFile) {
    let html_entry = subctx.html_mapping.get(&source.module_name).cloned();

    let attr_name = match &html_entry {
        Some(h) => h.original_module_name.clone(),
        None => source.module_name.clone(),
    };

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(subctx.type_.start_tag_prefix.as_bytes());
    out.extend_from_slice(html_encode(&attr_name).as_bytes());
    out.extend_from_slice(subctx.type_.start_tag_suffix.as_bytes());

    let text = remove_cr_newlines(
        &fs::read_to_string(&source.path)
            .with_context(|| format!("read {}", source.path.display()))
            .unwrap_or_default(),
    );
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(subctx.type_.end_tag.as_bytes());

    if html_entry.is_none() {
        info!(
            "{} {}: added compared to reference HTML",
            subctx.type_.directory_name, source.module_name
        );
    }

    subctx.renders.insert(source.module_name.clone(), out);
}

// ---------- system-script rendering ----------

fn reload_system_scripts(context: &mut LiveRenderContext) {
    let dir = context.system_scripts.directory.clone();
    if let Some(slot) = context.system_scripts.libraries.as_mut() {
        load_system_slot(&dir, slot);
    }
    if let Some(slot) = context.system_scripts.script.as_mut() {
        load_system_slot(&dir, slot);
    }
}

fn load_system_slot(directory: &Path, slot: &mut SystemScriptSlot) {
    let path = directory.join(slot.file_name);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let cleaned = remove_cr_newlines(&text);
            slot.render = cleaned.into_bytes();
            info!(
                "{}: loaded latest contents from disk ({} bytes)",
                slot.file_name,
                slot.render.len()
            );
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            slot.render = slot.original.clone();
            debug!(
                "{} not present in source tree, using original embedded contents",
                slot.file_name
            );
        }
        Err(err) => {
            slot.render = slot.original.clone();
            error!(
                "{}: failed to read {} — falling back to embedded contents: {err:#}",
                slot.file_name,
                path.display()
            );
        }
    }
}

fn detected_system_script_change(
    context: &mut LiveRenderContext,
    path: &Path,
    file_name: &str,
    relative_path: &str,
    deleted: bool,
) -> Result<()> {
    let slot = if file_name == SYSTEM_LIBRARIES_FILE_NAME {
        context.system_scripts.libraries.as_mut()
    } else if file_name == SYSTEM_SCRIPT_FILE_NAME {
        context.system_scripts.script.as_mut()
    } else {
        debug!("Ignoring untracked system-scripts file {file_name}");
        return Ok(());
    };
    let Some(slot) = slot else {
        debug!(
            "{file_name} change ignored: source HTML had no matching script slot to splice into"
        );
        return Ok(());
    };

    if deleted {
        slot.render = slot.original.clone();
        info!("Detected removal of system script {file_name} ({relative_path}), reverted to embedded contents");
        return Ok(());
    }

    match fs::read_to_string(path) {
        Ok(text) => {
            let cleaned = remove_cr_newlines(&text);
            slot.render = cleaned.into_bytes();
            info!("Detected modification of system script {file_name} ({relative_path})");
        }
        Err(err) => {
            error!(
                "{file_name}: failed to re-read after change — keeping previous contents: {err:#}"
            );
        }
    }
    Ok(())
}

// ---------- file-change routing ----------

fn detected_file_change(context: &mut LiveRenderContext, path: &Path) -> Result<()> {
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return Ok(()),
    };
    let parent = match path.parent() {
        Some(p) => p,
        None => return Ok(()),
    };

    let deleted = !path.is_file();
    let relative_path = path
        .strip_prefix(&context.source_directory)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.display().to_string());

    // Match against each tracked directory and matcher.
    if paths_equal(parent, &context.passages.directory) {
        if file_name.ends_with(&format!(".{PASSAGE_EXTENSION}")) {
            return detected_passage_change(context, path, &relative_path, deleted);
        }
    } else if paths_equal(parent, &context.js.directory) {
        if (JS_TYPE.naming_matcher)(file_name) {
            return detected_user_data_change(context, path, &relative_path, deleted, true);
        }
    } else if paths_equal(parent, &context.css.directory) {
        if (CSS_TYPE.naming_matcher)(file_name) {
            return detected_user_data_change(context, path, &relative_path, deleted, false);
        }
    } else if context
        .script_modules
        .as_ref()
        .is_some_and(|m| paths_equal(parent, &m.directory))
    {
        if (JS_MODULE_TYPE.naming_matcher)(file_name) {
            return detected_module_change(context, path, &relative_path, deleted, true);
        }
    } else if context
        .style_modules
        .as_ref()
        .is_some_and(|m| paths_equal(parent, &m.directory))
    {
        if (CSS_MODULE_TYPE.naming_matcher)(file_name) {
            return detected_module_change(context, path, &relative_path, deleted, false);
        }
    } else if paths_equal(parent, &context.system_scripts.directory) {
        return detected_system_script_change(context, path, file_name, &relative_path, deleted);
    }

    debug!("Detected change in untracked file {relative_path}");
    Ok(())
}

fn detected_passage_change(
    context: &mut LiveRenderContext,
    path: &Path,
    relative_path: &str,
    deleted: bool,
) -> Result<()> {
    if deleted {
        let name = context.passages.lookup_by_file.get(relative_path).cloned();
        if let Some(name) = name {
            if context.passages.current_set.shift_remove(&name) {
                info!("Detected removal of passage {name} ({relative_path})");
            } else {
                debug!("Detected removal of passage {name}, but already removed");
            }
        }
        return Ok(());
    }

    let parent_dir = context.passages.directory.parent().map(|p| p.to_path_buf());
    let parent = parent_dir.unwrap_or_else(|| context.passages.directory.clone());
    let passage_file = match read_passage_file(path, &parent) {
        Ok(pf) => pf,
        Err(err) => {
            error!(
                "Changed passage parsing from {relative_path} failed, skipping change: {err:#}"
            );
            return Ok(());
        }
    };
    let name = passage_file.passage_name.clone();
    let had_html_entry = context.passages.html_mapping.contains_key(&name);
    render_passage(context, &passage_file, had_html_entry);

    if context.passages.current_set.insert(name.clone()) {
        context
            .passages
            .lookup_by_file
            .insert(passage_file.relative_path.clone(), name.clone());
        if had_html_entry {
            info!("Detected re-adding of passage {name} ({relative_path})");
        } else {
            info!("Detected adding of new passage {name} ({relative_path})");
        }
    } else {
        info!("Detected modification of passage {name} ({relative_path})");
    }

    Ok(())
}

fn detected_user_data_change(
    context: &mut LiveRenderContext,
    path: &Path,
    relative_path: &str,
    deleted: bool,
    is_js: bool,
) -> Result<()> {
    // Do the mutation inside a swap-out so we can reuse render_user_data
    // with exclusive borrow of the subcontext.
    let mut subctx = if is_js {
        std::mem::replace(&mut context.js, dummy_user_data_ctx(&JS_TYPE))
    } else {
        std::mem::replace(&mut context.css, dummy_user_data_ctx(&CSS_TYPE))
    };

    let result = user_data_change(&mut subctx, path, relative_path, deleted);

    if is_js {
        context.js = subctx;
    } else {
        context.css = subctx;
    }
    result
}

fn user_data_change(
    subctx: &mut UserDataContext,
    path: &Path,
    relative_path: &str,
    deleted: bool,
) -> Result<()> {
    let log_name = subctx.type_.log_name.to_lowercase();

    if deleted {
        let name = subctx.lookup_by_file.get(relative_path).cloned();
        if let Some(name) = name {
            if subctx.current_set.shift_remove(&name) {
                info!("Detected removal of {log_name} file {name} ({relative_path})");
            } else {
                debug!("Detected removal of {log_name} file, but already removed");
            }
        }
        return Ok(());
    }

    let disk_file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return Ok(()),
    };
    let file_name = url_decode_file_name(&disk_file_name);
    let source_rel = path
        .strip_prefix(subctx.directory.parent().unwrap_or(&subctx.directory))
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| disk_file_name.clone());
    let source = UserDataFile {
        disk_file_name,
        file_name: file_name.clone(),
        relative_path: source_rel.clone(),
    };

    let had_html_entry = subctx.html_mapping.contains_key(&file_name);
    render_user_data(subctx, &source, had_html_entry);

    if subctx.current_set.insert(file_name.clone()) {
        subctx
            .lookup_by_file
            .insert(relative_path.to_string(), file_name.clone());
        if had_html_entry {
            info!("Detected re-adding of {log_name} file {file_name} ({relative_path})");
        } else {
            info!("Detected adding of new {log_name} file {file_name} ({relative_path})");
        }
    } else {
        info!("Detected modification of {log_name} file {file_name} ({relative_path})");
    }

    Ok(())
}

fn detected_module_change(
    context: &mut LiveRenderContext,
    path: &Path,
    relative_path: &str,
    deleted: bool,
    is_script: bool,
) -> Result<()> {
    let mut subctx = if is_script {
        context.script_modules.take()
    } else {
        context.style_modules.take()
    };
    let Some(subctx_ref) = subctx.as_mut() else {
        return Ok(());
    };

    let result = module_change(subctx_ref, path, relative_path, deleted);

    if is_script {
        context.script_modules = subctx;
    } else {
        context.style_modules = subctx;
    }
    result
}

fn module_change(
    subctx: &mut ModulesOfTypeContext,
    path: &Path,
    relative_path: &str,
    deleted: bool,
) -> Result<()> {
    let log_name = subctx.type_.directory_name;

    if deleted {
        let name = subctx.lookup_by_file.get(relative_path).cloned();
        if let Some(name) = name {
            if subctx.current_set.shift_remove(&name) {
                info!("Detected removal of {log_name} file {name} ({relative_path})");
            } else {
                debug!("Detected removal of {log_name} file, but already removed");
            }
        }
        return Ok(());
    }

    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return Ok(()),
    };
    let module_name = Path::new(&file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&file_name)
        .to_string();
    let source_rel = path
        .strip_prefix(subctx.directory.parent().unwrap_or(&subctx.directory))
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| file_name.clone());
    let source = ModuleFile {
        module_name: module_name.clone(),
        relative_path: source_rel.clone(),
        path: path.to_path_buf(),
    };

    let had_html_entry = subctx.html_mapping.contains_key(&module_name);
    render_module(subctx, &source);

    if subctx.current_set.insert(module_name.clone()) {
        subctx
            .lookup_by_file
            .insert(relative_path.to_string(), module_name.clone());
        if had_html_entry {
            info!("Detected re-adding of {log_name} file {module_name} ({relative_path})");
        } else {
            info!("Detected adding of new {log_name} file {module_name} ({relative_path})");
        }
    } else {
        info!("Detected modification of {log_name} file {module_name} ({relative_path})");
    }
    Ok(())
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    a.canonicalize().ok() == b.canonicalize().ok() || a == b
}

// ---------- HTML output ----------

fn write_html(context: &LiveRenderContext) -> Result<()> {
    info!("Writing HTML");
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&context.before_body_pre_libs);
    if let Some(slot) = &context.system_scripts.libraries {
        out.extend_from_slice(&slot.render);
    }
    out.extend_from_slice(&context.before_body_post_libs);
    if let Some(sm) = &context.script_modules {
        write_modules(&mut out, sm, 0);
    }
    if let Some(stm) = &context.style_modules {
        let base = context
            .script_modules
            .as_ref()
            .map(|m| m.current_set.len())
            .unwrap_or(0);
        write_modules(&mut out, stm, base);
    }
    out.extend_from_slice(&context.before_body_after_modules);
    out.extend_from_slice(&context.extra_scripts);
    out.extend_from_slice(&context.before_styles);
    write_user_data(&mut out, &context.css);
    out.extend_from_slice(&context.before_scripts);
    write_user_data(&mut out, &context.js);
    out.extend_from_slice(&context.before_passages);
    for name in &context.passages.current_set {
        if let Some(blob) = context.passages.renders.get(name) {
            out.extend_from_slice(blob);
        }
    }
    out.extend_from_slice(&context.after_passages_pre_script);
    if let Some(slot) = &context.system_scripts.script {
        out.extend_from_slice(&slot.render);
    }
    out.extend_from_slice(&context.after_passages_post_script);

    fs::write(&context.output_html_path, &out)
        .with_context(|| format!("write {}", context.output_html_path.display()))?;
    Ok(())
}

fn write_modules(out: &mut Vec<u8>, ctx: &ModulesOfTypeContext, base_index: usize) {
    for (index, name) in ctx.current_set.iter().enumerate() {
        if base_index + index > 0 {
            out.push(b'\n');
        }
        if let Some(blob) = ctx.renders.get(name) {
            out.extend_from_slice(blob);
        }
    }
}

fn write_user_data(out: &mut Vec<u8>, ctx: &UserDataContext) {
    for (index, name) in ctx.current_set.iter().enumerate() {
        if index > 0 {
            out.push(b'\n');
        }
        if let Some(blob) = ctx.renders.get(name) {
            out.extend_from_slice(blob);
        }
    }
}

// ---------- source-directory reads ----------

struct PassageFile {
    passage_name: String,
    relative_path: String,
    tags: String,
    data: String,
}

fn read_passages(directory: &Path, parent_for_relative: &Path) -> Result<Vec<PassageFile>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(directory) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err).with_context(|| format!("read_dir {}", directory.display())),
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
        if !name.ends_with(&format!(".{PASSAGE_EXTENSION}")) {
            continue;
        }
        match read_passage_file(&path, parent_for_relative) {
            Ok(p) => out.push(p),
            Err(err) => error!("Skipping passage {name}: {err:#}"),
        }
    }
    Ok(out)
}

fn read_passage_file(path: &Path, parent_for_relative: &Path) -> Result<PassageFile> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("invalid file name"))?;
    let text = remove_cr_newlines(&fs::read_to_string(path)?);
    let newline = text
        .find('\n')
        .ok_or_else(|| anyhow!("Passage file {file_name} does not contain any newlines"))?;

    let caps = twee_header_regex()
        .captures(&text[0..newline])
        .ok_or_else(|| anyhow!("No valid header in {file_name}"))?;
    let passage_name = caps.get(1).unwrap().as_str().to_string();

    let expected_name = url_encode_file_name(&format!("{passage_name}.{PASSAGE_EXTENSION}"));
    if expected_name != file_name && !is_valid_hash_name_match(file_name, &passage_name) {
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
    let relative_path = path
        .strip_prefix(parent_for_relative)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| file_name.to_string());

    Ok(PassageFile {
        passage_name,
        relative_path,
        tags,
        data,
    })
}

fn twee_header_regex() -> Regex {
    Regex::new(r"^::([^\[]+)(?: \[(.*)\])?$").expect("valid twee header regex")
}

struct UserDataFile {
    disk_file_name: String,
    file_name: String,
    relative_path: String,
}

fn collect_user_data(directory: &Path, matcher: fn(&str) -> bool) -> Result<Vec<UserDataFile>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(directory) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err).with_context(|| format!("read_dir {}", directory.display())),
    };
    let parent = directory.parent().unwrap_or(directory).to_path_buf();
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
        let relative_path = path
            .strip_prefix(&parent)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| name.to_string());
        out.push(UserDataFile {
            disk_file_name: name.to_string(),
            file_name: url_decode_file_name(name),
            relative_path,
        });
    }
    Ok(out)
}

struct ModuleFile {
    module_name: String,
    relative_path: String,
    path: PathBuf,
}

fn collect_modules_of_type(directory: &Path, ty: &ModuleType) -> Result<Vec<ModuleFile>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(directory) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err).with_context(|| format!("read_dir {}", directory.display())),
    };
    let parent = directory.parent().unwrap_or(directory).to_path_buf();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !(ty.naming_matcher)(name) {
            continue;
        }
        let module_name = Path::new(name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(name)
            .to_string();
        let relative_path = path
            .strip_prefix(&parent)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| name.to_string());
        out.push(ModuleFile {
            module_name,
            relative_path,
            path,
        });
    }
    Ok(out)
}
