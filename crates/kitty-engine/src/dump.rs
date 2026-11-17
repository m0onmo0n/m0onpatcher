//! JSON dump of a [`ParseState`], shaped like the Python reference dump
//! (`dump_parse.py`) so the two can be compared.

use crate::model::ParseState;
use serde_json::{json, Map, Value};

pub fn to_json(loaded_mods: &[String], st: &ParseState) -> Value {
    let mut m = Map::new();
    m.insert("loaded_mods".into(), json!(loaded_mods));
    m.insert(
        "mod_dict".into(),
        Value::Array(st.mod_dict.iter().map(|(k, v)| json!([k, v])).collect()),
    );
    m.insert("mod_list".into(), json!(st.mod_list.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>()));
    m.insert("mod_file_indexes".into(), json!(st.mod_file_indexes));
    m.insert("mod_reg_list".into(), json!(st.mod_reg_list));
    m.insert("mod_struct_list".into(), json!(st.mod_struct_list));
    m.insert("mod_func_list".into(), json!(st.mod_func_list));
    m.insert("mod_hook_list".into(), json!(st.mod_hook_list));
    m.insert(
        "conflict_map".into(),
        Value::Object(
            st.conflict_map
                .iter()
                .map(|(k, v)| (k.clone(), Value::Array(v.iter().map(|(a, b)| json!([a, b])).collect())))
                .collect(),
        ),
    );
    m.insert("passage_registry".into(), json!(st.passage_registry));
    m.insert("passage_names_seen".into(), json!(st.passage_names_seen));
    m.insert("js_registry".into(), json!(st.js_registry));
    m.insert("css_registry".into(), json!(st.css_registry));
    m.insert("events_registry".into(), json!(st.events_registry));
    m.insert("key_to_mod".into(), json!(st.key_to_mod));
    Value::Object(m)
}
