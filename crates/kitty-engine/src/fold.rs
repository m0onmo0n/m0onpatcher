//! Inner replacement ("folding"): a `Replace:` block whose anchor sits inside
//! another block's `With:` text is applied to that text and dropped from the
//! block list. Port of the per-mod pass in `proc_replacement_new` and of the
//! cross-mod pass in `load_mods`, with the fixed v2 order (no hash-seed
//! dependence): per mod the keys go shortest first, hosts in file order; across
//! mods hosts are tried in load order.

use crate::escape::escape_twine_tags;
use crate::model::{ParseState, ReplaceEntry};
use crate::parse::RAW_TARGETS;
use indexmap::{IndexMap, IndexSet};
use std::collections::HashSet;

const MAX_PASSES: usize = 20;

use crate::escape::unescape_for_raw as unescape_for_raw_host;

fn is_raw(key: &str) -> bool {
    RAW_TARGETS.contains(&key)
}

/// What to search for in `host_key`'s body when looking for `search_key`.
fn needle<'a>(search_key: &'a str, escaped: &'a str, host_key: &str) -> &'a str {
    if is_raw(host_key) { search_key } else { escaped }
}

/// Shared core of both passes. `search_order` is the order in which anchors
/// are examined when the candidate index is built; `host_order` the order in
/// which hosts are tried.
fn run_passes(
    st: &mut ParseState,
    search_order: &[String],
    host_order: &[String],
    only_active_hosts: bool,
    source_mod_for: &dyn Fn(&ParseState, &str) -> Option<String>,
) {
    let mut inner_candidates: IndexMap<String, IndexSet<String>> = IndexMap::new();
    for search_key in search_order {
        let escaped_key = escape_twine_tags(search_key);
        let mut candidates: IndexSet<String> = IndexSet::new();
        for host_key in host_order {
            if host_key == search_key || (only_active_hosts && !st.mod_dict.contains_key(host_key)) {
                continue;
            }
            let Some(host) = st.mod_dict.get(host_key) else { continue };
            if host.0.contains(needle(search_key, &escaped_key, host_key)) {
                candidates.insert(host_key.clone());
            }
        }
        if !candidates.is_empty() {
            inner_candidates.insert(search_key.clone(), candidates);
        }
    }

    let mut all_inner_keys: HashSet<String> = HashSet::new();
    for _pass in 0..MAX_PASSES {
        let mut inner_keys: Vec<String> = Vec::new();
        let keys: Vec<String> = inner_candidates.keys().cloned().collect();
        for search_key in keys {
            if !st.mod_dict.contains_key(&search_key) || all_inner_keys.contains(&search_key) {
                continue;
            }
            let host_keys: Vec<String> =
                inner_candidates.get(&search_key).map(|s| s.iter().cloned().collect()).unwrap_or_default();
            let search_new = st.mod_dict[&search_key].0.clone();
            let escaped_key = escape_twine_tags(&search_key);

            for host_key in host_keys {
                if host_key == search_key || !st.mod_dict.contains_key(&host_key) {
                    continue;
                }
                let host_entry = st.mod_dict[&host_key].clone();
                let host_is_raw = is_raw(&host_key);
                let actual_search = needle(&search_key, &escaped_key, &host_key).to_string();
                let inject_new = if host_is_raw { unescape_for_raw_host(&search_new) } else { search_new.clone() };

                if host_entry.0.contains(&actual_search) {
                    let new_body = host_entry.0.replacen(&actual_search, &inject_new, 1);
                    st.mod_dict.insert(host_key.clone(), ReplaceEntry(new_body.clone(), host_entry.1.clone()));
                    inner_keys.push(search_key.clone());

                    if let Some(source_mod) = source_mod_for(st, &search_key) {
                        if let Some(list) = st.mod_file_indexes.get_mut(&source_mod) {
                            if let Some(pos) = list.iter().position(|x| x == &search_key) {
                                list.remove(pos);
                            }
                        }
                    }

                    let other_keys: Vec<String> = inner_candidates.keys().cloned().collect();
                    for other_key in other_keys {
                        if other_key != search_key && !all_inner_keys.contains(&other_key) {
                            let ek = escape_twine_tags(&other_key);
                            let ak = if host_is_raw { other_key.clone() } else { ek };
                            if new_body.contains(&ak) {
                                inner_candidates.get_mut(&other_key).unwrap().insert(host_key.clone());
                            }
                        }
                    }
                    break;
                }
            }
        }

        if inner_keys.is_empty() {
            break;
        }
        for k in &inner_keys {
            all_inner_keys.insert(k.clone());
        }
        for k in &inner_keys {
            st.mod_dict.shift_remove(k);
            inner_candidates.shift_remove(k);
        }
    }
}

/// Per-mod pass (inside `proc_replacement_new`). `st` is the scratch state of
/// the mod being parsed.
pub fn absorb_inner_per_mod(st: &mut ParseState, mod_file: &str) {
    let this_mod_keys: Vec<String> = st.mod_file_indexes.get(mod_file).cloned().unwrap_or_default();
    let mut active: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for k in &this_mod_keys {
        if st.mod_dict.contains_key(k) && seen.insert(k.as_str()) {
            active.push(k.clone());
        }
    }
    // shortest anchor first (length in characters), file order for equal length
    let mut order: Vec<usize> = (0..active.len()).collect();
    order.sort_by_key(|&i| (active[i].chars().count(), i));
    let search_order: Vec<String> = order.into_iter().map(|i| active[i].clone()).collect();

    run_passes(st, &search_order, &active, true, &|st, k| st.key_to_mod.get(k).cloned());
}

/// Cross-mod pass at the end of `load_mods`: keys and hosts both in load order.
pub fn absorb_inner_cross_mod(st: &mut ParseState) {
    let all_keys: Vec<String> = st.mod_dict.keys().cloned().collect();
    run_passes(st, &all_keys, &all_keys, false, &|st, k| st.key_to_mod.get(k).cloned());
}
