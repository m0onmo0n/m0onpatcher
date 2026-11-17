//! The step ScRepacker does after patching: the patched HTML is read into its parts (styles, scripts, passages,
//! modules), written out as in-memory files, and merged back into the *input* HTML as the reference document.
//! The result differs from the raw patcher output (for example in how characters are encoded), and mods written
//! against ScRepacker's output (the stage 2 mods, for instance) expect that form. The Stage builder does this after
//! every stage so a chain gives what the same stages run one after the other through ScRepacker would.
//!
//! Ported from the packer's `game_data_patcher.rs` (`compute_originals` and its helpers).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::Result;

use crate::encoding::url_encode_file_name;
use crate::extraction_naming::include_hash_in_passage_file_name;
use crate::game_data::{
    GameData, ModuleType, UserDataType, CSS_TYPE, JS_TYPE, PASSAGE_DIRECTORY, PASSAGE_EXTENSION,
    SYSTEM_LIBRARIES_FILE_NAME, SYSTEM_SCRIPTS_DIRECTORY, SYSTEM_SCRIPT_FILE_NAME,
};
use crate::{game_data_merger, game_data_reader};

/// Write the normalized form of `patched_html` to `output`, using `reference` (the HTML the stage started from)
/// as the document the parts are merged into.
pub fn write_normalized(reference: &Path, patched_html: &str, output: &Path) -> Result<()> {
    let data = game_data_reader::read_from_string(patched_html.to_string())?;
    let files = compute_originals(&data)?;
    if let Some(dir) = output.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    game_data_merger::merge_from_memory(reference, output, &files)
}

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
