//! Data structures the parse step fills (the Rust counterparts of the Python
//! registries: `mod_dict`, `mod_list`, `mod_func_list`, `passage_registry`, ...).

use crate::guards::{Guards, Guards4};
use indexmap::IndexMap;
use serde::Serialize;

/// Value of a `mod_dict` entry: `(replacement text, guards)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReplaceEntry(pub String, pub Guards);

/// One `mod_func_list` entry. One struct for every kind; fields that a kind
/// does not use stay `None` and are left out of the dump.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FuncEntry {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passage_name: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub guards: Guards,
    pub mod_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub once: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soft: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_anchor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_params: Option<String>,
}

/// `Hook [fn]:` entry (the Python `mod_hook_list`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HookEntry {
    pub kind: String,
    pub name: String,
    pub timing: String,
    pub body: String,
    pub guards: Guards,
    pub mod_file: String,
}

/// `Append Array` / `Append Variable to Class` / `Insert Into Array` ... entry (`mod_struct_list`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StructEntry {
    pub kind: String,
    pub name: String,
    pub scope_func: Option<String>,
    pub content: String,
    pub guards: Guards,
    pub mod_file: String,
}

/// Guards as written into a passage record: the shorthand/full extractors
/// store all 13 lists, the multi-passage split stores only 4 (Python quirk).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum PassageGuards {
    Full(Guards),
    Four(Guards4),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PassageRecord {
    pub name: String,
    pub tags: String,
    pub pid: String,
    pub body: String,
    pub guards: PassageGuards,
    pub mod_file: String,
    pub format: String,
}

/// `Add Javascript:` / `Add CSS:` / `Add Events:` record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BodyRecord {
    pub body: String,
    pub guards: Guards,
    pub mod_file: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RegEntry {
    pub pattern: String,
    pub groups: Vec<(u32, String)>,
    pub guards: Guards,
    pub mod_file: String,
}

/// One legacy `~` / `~~` block kept apart from `mod_list`, for the legacy phase (see `LoadOptions`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyEntry {
    /// anchor text, already escaped like a `mod_list` key
    pub old: String,
    /// replacement text, already escaped like a `mod_dict` value
    pub new: String,
    pub mod_file: String,
}

/// Everything the Python `load_mods` returns (the registries), plus a log
/// and the list of directives this engine does not support yet.
#[derive(Clone, Debug, Default)]
pub struct ParseState {
    /// Legacy `~` / `~~` blocks, in order, when they are collected for the legacy phase instead of `mod_list`.
    pub legacy_list: Vec<LegacyEntry>,
    /// Scratch output of `proc_replacement_old`: every legacy block of the file just parsed, `(old, new)` in file order.
    pub legacy_pairs: Vec<(String, String)>,
    pub mod_dict: IndexMap<String, ReplaceEntry>,
    pub mod_list: Vec<(String, String)>,
    pub mod_file_indexes: IndexMap<String, Vec<String>>,
    pub mod_reg_list: Vec<RegEntry>,
    pub mod_struct_list: Vec<StructEntry>,
    pub mod_func_list: Vec<FuncEntry>,
    pub mod_hook_list: Vec<HookEntry>,
    pub conflict_map: IndexMap<String, Vec<(String, String)>>,
    pub passage_registry: Vec<PassageRecord>,
    pub passage_names_seen: IndexMap<String, usize>,
    pub js_registry: Vec<BodyRecord>,
    pub css_registry: Vec<BodyRecord>,
    pub events_registry: Vec<BodyRecord>,
    pub key_to_mod: IndexMap<String, String>,
    /// Messages the Python patcher would log while parsing.
    pub log: Vec<String>,
    /// `(mod file, directive token)` for directives this engine cannot apply yet.
    pub unsupported: Vec<(String, String)>,
}

impl ParseState {
    pub fn index_push(&mut self, mod_file: &str, label: String) {
        self.mod_file_indexes.entry(mod_file.to_string()).or_default().push(label);
    }

    /// `_register_conflict`
    pub fn register_conflict(&mut self, target_name: &str, mod_file: &str, kind: &str) {
        let key = crate::pytext::py_strip(target_name).to_string();
        self.conflict_map.entry(key).or_default().push((mod_file.to_string(), kind.to_string()));
    }
}
