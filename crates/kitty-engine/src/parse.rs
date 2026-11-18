//! Parse one mod file into a [`ParseState`] (port of `proc_replacement_old`
//! and `proc_replacement_new`). Only the directives the current mod packs use
//! are parsed; any other directive token found at column 0 is recorded in
//! `ParseState::unsupported` so the caller can refuse or fall back.

use crate::escape::{auto_escape_passage_bodies, escape_html_between_tags, escape_twine_tags};
use crate::guards::{hoist_guards_to_with_bodies, parse_guards, Guards, Guards4, ALL_DIRECTIVE_TOKENS};
use crate::model::*;
use crate::pytext::{py_strip, rstrip_crlf, splitlines_keepends};
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

/// Targets whose replacement text is raw JS/CSS (never twine-escaped).
pub const RAW_TARGETS: [&str; 3] = ["</script><tw-passagedata", "</style><script", "setup.Events.db =\n["];

/// Directive tokens this engine does not parse or apply yet. (`Move Passage` rewrites references with
/// stale per-passage offsets in the Python patcher, so it is deliberately not ported; `Wrap Passage` is
/// parsed by Python although it is not in its token list, duplicates Prepend/Append To Passage and is unused.)
const UNSUPPORTED_TOKENS: [&str; 2] = ["Move Passage [", "Wrap Passage ["];

static TYPEOF_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[typeof:([^\]]+)\]").unwrap());
static MODIFIER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\[(?:ONCE|soft|typeof:[^\]]*)\]").unwrap());
static STRUCT_SCOPE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+?)\s+In Function \[(.+?)\]$").unwrap());
static TIMING_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(before|after|around):\s*$").unwrap());
static CLONE_AS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^As\s+\[(.+?)\]").unwrap());
static WITH_LINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^With:\s*$").unwrap());
static WITH_GROUP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^With\{(\d+)\}:\s*$").unwrap());
static COMMENT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)/\*.*?\*/").unwrap());
static FULL_PASSAGE_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?ms)^Add Passage:\s*\n((?:<tw-passagedata[^>]*>.*?</tw-passagedata>\s*)+)").unwrap());
static PASSAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)(<tw-passagedata[^>]*>)(.*?)(</tw-passagedata>)").unwrap());
static SHORTHAND_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^Add Passage:[^\S\n]+(\S.+?)\s*$").unwrap());
static SHORT_TAGS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^tags:\s*(.*?)\s*\n").unwrap());
static NAME_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"name="([^"]+)""#).unwrap());
static TAGS_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"tags="([^"]*)""#).unwrap());
static TWPD_NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<tw-passagedata[^>]*name="([^"]+)""#).unwrap());
static PID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(<tw-passagedata[^>]*)pid="[^"]*""#).unwrap());
static PASSAGE_BODY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<tw-passagedata[^>]*>(.*?)</tw-passagedata>").unwrap());

// ---------------------------------------------------------------------------
// text helpers
// ---------------------------------------------------------------------------

/// Byte offsets of all line starts of `text` (offset 0 and after every `\n`).
fn line_starts(text: &str) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0usize).chain(text.match_indices('\n').map(|(i, _)| i + 1)).filter(move |&p| p <= text.len())
}

/// `_find_next_directive`: start of the next top-level directive, or `len`.
pub fn find_next_directive(text: &str) -> usize {
    for p in line_starts(text) {
        let rest = &text[p..];
        if ALL_DIRECTIVE_TOKENS.iter().any(|tok| rest.starts_with(tok)) {
            return p;
        }
    }
    text.len()
}

/// `re.split(r'(?m)^<token>', content)[1:]`
fn split_chunks<'a>(content: &'a str, token: &str) -> Vec<&'a str> {
    let mut matches: Vec<(usize, usize)> = Vec::new();
    let mut min_pos = 0usize;
    for p in line_starts(content) {
        if p >= min_pos && content[p..].starts_with(token) {
            matches.push((p, p + token.len()));
            min_pos = p + token.len();
        }
    }
    let mut out = Vec::with_capacity(matches.len());
    for (i, &(_, end)) in matches.iter().enumerate() {
        let stop = matches.get(i + 1).map(|m| m.0).unwrap_or(content.len());
        out.push(&content[end..stop]);
    }
    out
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn bracket_end(chunk: &str) -> Option<usize> {
    chunk.find("]:")
}

fn blank_guards() -> Guards {
    Guards::default()
}

// ---------------------------------------------------------------------------
// old `~~` / `~` format
// ---------------------------------------------------------------------------

/// `proc_replacement_old`
pub fn proc_replacement_old(mod_content: &str, st: &mut ParseState, mod_file: &str) {
    const TILDE_PH: &str = "\u{0}TILDE\u{0}";
    let mut in_mod_section = false;
    let mut header_done = false;
    let mut sanitized = String::with_capacity(mod_content.len());
    for line in splitlines_keepends(mod_content) {
        let stripped = py_strip(line);
        if !header_done {
            if stripped == "[Mod]" {
                in_mod_section = true;
                continue;
            }
            if stripped == "[/Mod]" {
                in_mod_section = false;
                continue;
            }
            if in_mod_section {
                continue;
            }
            if stripped.starts_with('#') || stripped.is_empty() {
                continue;
            }
            header_done = true;
        }
        sanitized.push_str(line);
    }
    let sanitized = COMMENT_RE.replace_all(&sanitized, |c: &regex::Captures| c[0].replace('~', TILDE_PH)).into_owned();

    for replacement in sanitized.split("~~") {
        let Some(t) = replacement.find('~') else { continue };
        let old_lines = replacement[..t].replace(TILDE_PH, "~");
        let new_lines = replacement[t + 1..].replace(TILDE_PH, "~");
        let old_lines = escape_twine_tags(&escape_html_between_tags(&old_lines));
        let new_lines = escape_twine_tags(&escape_html_between_tags(&new_lines));
        let old_stripped = py_strip(&old_lines).to_string();
        let new_stripped = auto_escape_passage_bodies(py_strip(&new_lines));

        st.legacy_pairs.push((old_stripped.clone(), new_stripped.clone()));
        st.mod_dict.insert(old_stripped.clone(), ReplaceEntry(new_stripped, blank_guards()));
        st.mod_list.push((old_stripped.clone(), mod_file.to_string()));
        st.key_to_mod.insert(old_stripped.clone(), mod_file.to_string());
        st.register_conflict(&old_stripped, mod_file, "Replace (legacy ~~)");
        st.index_push(mod_file, old_stripped);
    }
}

// ---------------------------------------------------------------------------
// new format
// ---------------------------------------------------------------------------

#[derive(PartialEq)]
enum BlockState {
    Idle,
    InReplace,
    InWith,
}

/// `_split_replace_blocks`: `(old text, with text)` pairs of classic `Replace:` blocks.
fn split_replace_blocks(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut state = BlockState::Idle;
    let mut old_buf: Vec<&str> = Vec::new();
    let mut with_buf: Vec<&str> = Vec::new();
    for line in splitlines_keepends(text) {
        let s = rstrip_crlf(line);
        match state {
            BlockState::Idle => {
                if py_strip(s) == "Replace:" {
                    state = BlockState::InReplace;
                    old_buf.clear();
                    with_buf.clear();
                }
            }
            BlockState::InReplace => {
                if py_strip(s) == "With:" {
                    state = BlockState::InWith;
                } else {
                    old_buf.push(line);
                }
            }
            BlockState::InWith => {
                let ss = py_strip(s);
                let absorbed = ALL_DIRECTIVE_TOKENS
                    .iter()
                    .any(|tok| *tok != "With:" && *tok != "~~" && *tok != "Replace:" && (ss == *tok || ss.starts_with(tok)));
                if ss == "Replace:" && !old_buf.is_empty() {
                    out.push((old_buf.concat(), with_buf.concat()));
                    state = BlockState::InReplace;
                    old_buf.clear();
                    with_buf.clear();
                } else if ss == "~~" && !old_buf.is_empty() {
                    out.push((old_buf.concat(), with_buf.concat()));
                    state = BlockState::Idle;
                    old_buf.clear();
                    with_buf.clear();
                } else if absorbed && !old_buf.is_empty() {
                    out.push((old_buf.concat(), with_buf.concat()));
                    state = BlockState::Idle;
                    old_buf.clear();
                    with_buf.clear();
                } else {
                    with_buf.push(line);
                }
            }
        }
    }
    if state == BlockState::InWith && !old_buf.is_empty() {
        out.push((old_buf.concat(), with_buf.concat()));
    }
    out
}

/// `_split_multi_passage_with_body`
fn split_multi_passage_with_body(old_lines: &str, new_line_stripped: &str, mod_file: &str, st: &mut ParseState) -> String {
    if new_line_stripped.matches("<tw-passagedata").count() <= 1 {
        return new_line_stripped.to_string();
    }
    let target_name = TWPD_NAME_RE.captures(old_lines).map(|c| c[1].to_string());

    // re.split(r'(?=<tw-passagedata)', s)
    let mut cuts: Vec<usize> = new_line_stripped.match_indices("<tw-passagedata").map(|(i, _)| i).collect();
    let mut pieces: Vec<&str> = Vec::new();
    let mut prev = 0usize;
    for c in cuts.drain(..) {
        pieces.push(&new_line_stripped[prev..c]);
        prev = c;
    }
    pieces.push(&new_line_stripped[prev..]);

    let mut target_block: Option<String> = None;
    let mut extra_blocks: Vec<(Option<String>, String)> = Vec::new();
    for block in pieces {
        let block = py_strip(block);
        if block.is_empty() {
            continue;
        }
        if !block.starts_with("<tw-passagedata") {
            if target_block.is_none() {
                target_block = Some(block.to_string());
            }
            continue;
        }
        let block_name = TWPD_NAME_RE.captures(block).map(|c| c[1].to_string());
        if block_name == target_name || target_block.is_none() {
            target_block = Some(block.to_string());
        } else if block.contains("</tw-passagedata>") {
            extra_blocks.push((block_name, block.to_string()));
        }
    }
    if extra_blocks.is_empty() {
        return new_line_stripped.to_string();
    }

    for (block_name, block) in &extra_blocks {
        let mut block = PID_RE.replace(block, "${1}pid=\"auto\"").into_owned();
        if !block.trim_end().ends_with("</tw-passagedata>") {
            block = format!("{}\n</tw-passagedata>", block.trim_end());
        }
        let body = PASSAGE_BODY_RE.captures(&block).map(|c| py_strip(&c[1]).to_string()).unwrap_or_default();
        let tags = TAGS_ATTR_RE.captures(&block).map(|c| c[1].to_string()).unwrap_or_default();
        let name = block_name.clone().unwrap_or_default();
        let record = PassageRecord {
            name: name.clone(),
            tags,
            pid: "auto".to_string(),
            body,
            guards: PassageGuards::Four(Guards4::default()),
            mod_file: mod_file.to_string(),
            format: "full".to_string(),
        };
        if let Some(&first_idx) = st.passage_names_seen.get(&name) {
            let first_mod = st.passage_registry[first_idx].mod_file.clone();
            st.log.push(format!(
                "DUPLICATE PASSAGE '{name}': already defined by {first_mod}, skipping copy from {mod_file} (multi-passage split)."
            ));
        } else {
            st.passage_names_seen.insert(name.clone(), st.passage_registry.len());
            st.passage_registry.push(record);
        }
        st.index_push(mod_file, format!("Add Passage: [{name}] (auto-split from multi-passage Replace:)"));
    }
    st.log.push(format!(
        "MULTI-PASSAGE REPLACE: With: body contained {} passages. Auto-split: '{}' stays as Replace:, {} extra passage(s) routed to passage registry.",
        extra_blocks.len() + 1,
        target_name.clone().unwrap_or_default(),
        extra_blocks.len()
    ));
    target_block.unwrap_or_else(|| new_line_stripped.to_string())
}

/// `_extract_add_passages`: returns `mod_content` without the extracted blocks.
fn extract_add_passages(mod_content: &str, st: &mut ParseState, mod_file: &str) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();

    for m in FULL_PASSAGE_BLOCK_RE.captures_iter(mod_content) {
        let block = m.get(1).unwrap().as_str();
        for pm in PASSAGE_RE.captures_iter(block) {
            let open_tag = &pm[1];
            let body = py_strip(&pm[2]);
            let Some(name_m) = NAME_ATTR_RE.captures(open_tag) else {
                st.log.push(format!("Add Passage: skipped malformed block (missing name attribute) in {mod_file}"));
                continue;
            };
            let name = name_m[1].to_string();
            if body.is_empty() {
                st.log.push(format!("Add Passage: skipped '{name}' -- empty body in {mod_file}"));
                continue;
            }
            let tags = TAGS_ATTR_RE.captures(open_tag).map(|c| c[1].to_string()).unwrap_or_default();
            let (body, guards) = parse_guards(body);
            let record = PassageRecord {
                name: name.clone(),
                tags,
                pid: "auto".to_string(),
                body: py_strip(&body).to_string(),
                guards: PassageGuards::Full(guards),
                mod_file: mod_file.to_string(),
                format: "full".to_string(),
            };
            add_passage_record(st, record, mod_file);
            st.index_push(mod_file, format!("Add Passage: [{name}]"));
        }
        let whole = m.get(0).unwrap();
        ranges.push((whole.start(), whole.end()));
    }

    for m in SHORTHAND_RE.captures_iter(mod_content) {
        let whole = m.get(0).unwrap();
        if ranges.iter().any(|&(s, e)| s <= whole.start() && whole.start() < e) {
            continue;
        }
        let pname = py_strip(&m[1]).to_string();
        let rest_start = if whole.end() < mod_content.len() && mod_content.as_bytes()[whole.end()] == b'\n' {
            whole.end() + 1
        } else {
            whole.end()
        };
        let rest = &mod_content[rest_start..];
        let (tags, body_start) = if let Some(t) = SHORT_TAGS_RE.captures(rest) {
            (py_strip(&t[1]).to_string(), rest_start + t.get(0).unwrap().end())
        } else {
            (String::new(), rest_start)
        };
        let body_rest = &mod_content[body_start..];
        let body_end_offset = find_next_directive(body_rest);
        let body = py_strip(&body_rest[..body_end_offset]);

        if pname.is_empty() {
            st.log.push(format!("Add Passage: skipped malformed shorthand block (empty name) in {mod_file}"));
            ranges.push((whole.start(), body_start + body_end_offset));
            continue;
        }
        if body.is_empty() {
            st.log.push(format!("Add Passage: skipped '{pname}' -- empty body in {mod_file}"));
            ranges.push((whole.start(), body_start + body_end_offset));
            continue;
        }
        let (body, guards) = parse_guards(body);
        let record = PassageRecord {
            name: pname.clone(),
            tags,
            pid: "auto".to_string(),
            body: py_strip(&body).to_string(),
            guards: PassageGuards::Full(guards),
            mod_file: mod_file.to_string(),
            format: "shorthand".to_string(),
        };
        add_passage_record(st, record, mod_file);
        st.index_push(mod_file, format!("Add Passage: [{pname}]"));
        ranges.push((whole.start(), body_start + body_end_offset));
    }

    remove_ranges(mod_content, ranges)
}

fn add_passage_record(st: &mut ParseState, record: PassageRecord, mod_file: &str) {
    let name = record.name.clone();
    if let Some(&first_idx) = st.passage_names_seen.get(&name) {
        let first_mod = st.passage_registry[first_idx].mod_file.clone();
        st.log.push(format!("DUPLICATE PASSAGE '{name}': already defined by {first_mod}, skipping copy from {mod_file}."));
    } else {
        st.passage_names_seen.insert(name, st.passage_registry.len());
        st.passage_registry.push(record);
    }
}

fn remove_ranges(text: &str, mut ranges: Vec<(usize, usize)>) -> String {
    ranges.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = text.to_string();
    for (s, e) in ranges {
        out.replace_range(s..e, "");
    }
    out
}

/// `_extract_add_js_css_events`
fn extract_add_js_css_events(mod_content: &str, st: &mut ParseState, mod_file: &str) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let which = [("Add Javascript:", 0usize), ("Add CSS:", 1), ("Add Events:", 2)];
    for (token, reg_idx) in which {
        let header = Regex::new(&format!(r"(?m)^{}\s*\n", regex::escape(token))).unwrap();
        let mut search_from = 0usize;
        while let Some(h) = header.find_at(mod_content, search_from) {
            let content_start = h.end();
            // lazy `.*?` followed by `(?=^(?:tokens|~~|\Z))`
            let mut content_end: Option<usize> = None;
            let tail = &mod_content[content_start..];
            for rel in line_starts(tail) {
                let p = content_start + rel;
                let rest = &mod_content[p..];
                if p == mod_content.len() || rest.starts_with("~~") || ALL_DIRECTIVE_TOKENS.iter().any(|t| rest.starts_with(t)) {
                    content_end = Some(p);
                    break;
                }
            }
            let Some(end) = content_end else {
                search_from = h.start() + 1;
                continue;
            };
            search_from = end.max(h.end());
            if ranges.iter().any(|&(s, e)| s <= h.start() && h.start() < e) {
                continue;
            }
            let body = py_strip(&mod_content[content_start..end]);
            if body.is_empty() {
                continue;
            }
            let (body, guards) = parse_guards(body);
            let body = py_strip(&body).to_string();
            if body.is_empty() {
                continue;
            }
            let record = BodyRecord { body, guards, mod_file: mod_file.to_string() };
            match reg_idx {
                0 => st.js_registry.push(record),
                1 => st.css_registry.push(record),
                _ => st.events_registry.push(record),
            }
            st.index_push(mod_file, format!("{token} block"));
            ranges.push((h.start(), end));
        }
    }
    remove_ranges(mod_content, ranges)
}

/// `_parse_with_groups`
fn parse_with_groups(text: &str) -> Vec<(u32, String)> {
    let toks: Vec<_> = WITH_GROUP_RE.captures_iter(text).collect();
    let mut results = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        let start = tok.get(0).unwrap().end();
        let end = toks.get(i + 1).map(|t| t.get(0).unwrap().start()).unwrap_or(text.len());
        let (body, _) = parse_guards(&text[start..end]);
        results.push((tok[1].parse::<u32>().unwrap_or(0), py_strip(&body).to_string()));
    }
    results
}

fn dedupe_tail_after_first_line(rest: &str) -> &str {
    match rest.find('\n') {
        Some(i) => &rest[i + 1..],
        None => "",
    }
}

/// `proc_replacement_new`
pub fn proc_replacement_new(mod_content: &str, st: &mut ParseState, mod_file: &str) {
    let mod_content = hoist_guards_to_with_bodies(mod_content);
    let mod_content = extract_add_passages(&mod_content, st, mod_file);
    let mod_content = extract_add_js_css_events(&mod_content, st, mod_file);

    // ---- classic Replace: blocks ----
    for (old_lines, new_lines) in split_replace_blocks(&mod_content) {
        let old_lines = escape_html_between_tags(&old_lines);
        let new_lines = escape_html_between_tags(&new_lines);
        let old_line_stripped = py_strip(&old_lines).to_string();

        let (new_lines, guards) = parse_guards(&new_lines);
        let is_raw = RAW_TARGETS.contains(&old_line_stripped.as_str());
        let mut new_line_stripped;
        if is_raw {
            new_line_stripped = py_strip(&new_lines).to_string();
        } else {
            let escaped = escape_twine_tags(&new_lines);
            new_line_stripped = auto_escape_passage_bodies(py_strip(&escaped));
        }
        if !is_raw {
            new_line_stripped = split_multi_passage_with_body(&old_lines, &new_line_stripped, mod_file, st);
        }

        st.mod_dict.insert(old_line_stripped.clone(), ReplaceEntry(new_line_stripped, guards));
        st.mod_list.push((old_line_stripped.clone(), mod_file.to_string()));
        if !is_raw {
            st.register_conflict(&old_line_stripped, mod_file, "Replace");
        }
        st.key_to_mod.insert(old_line_stripped.clone(), mod_file.to_string());

        st.mod_file_indexes.entry(mod_file.to_string()).or_default();
        st.mod_file_indexes.entry(old_lines.clone()).or_default();
        st.mod_file_indexes.get_mut(mod_file).unwrap().push(old_line_stripped);
    }

    // ---- per-mod inner replacement ----
    crate::fold::absorb_inner_per_mod(st, mod_file);

    // ---- ReplaceReg [pattern]: ----
    for chunk in split_chunks(&mod_content, "ReplaceReg [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let pattern_str = &chunk[..be];
        let rest = dedupe_tail_after_first_line(&chunk[be + 2..]);
        let (rest, top_guards) = parse_guards(rest);
        let groups = parse_with_groups(&rest);
        if groups.is_empty() {
            continue;
        }
        st.mod_reg_list.push(RegEntry { pattern: pattern_str.to_string(), groups, guards: top_guards, mod_file: mod_file.to_string() });
        st.index_push(mod_file, format!("ReplaceReg [{pattern_str}]"));
    }

    // ---- Append Array / Append Variable to Class / Append Function to Class / Insert Into Array / Object / Merge Into Object ----
    for (token, kind) in [
        ("Append Array [", "append_array"),
        ("Append Variable to Class [", "append_var"),
        ("Append Function to Class [", "append_func"),
        ("Insert Into Array [", "insert_into_array"),
        ("Insert Into Object [", "insert_into_object"),
        ("Merge Into Object [", "merge_into_object"),
    ] {
        for chunk in split_chunks(&mod_content, token) {
            let Some(be) = bracket_end(chunk) else { continue };
            let name_raw = py_strip(&chunk[..be]);
            let (name, scope_func) = match STRUCT_SCOPE_RE.captures(name_raw) {
                Some(c) => (py_strip(&c[1]).to_string(), Some(py_strip(&c[2]).to_string())),
                None => (name_raw.to_string(), None),
            };
            let rest = dedupe_tail_after_first_line(&chunk[be + 2..]);
            let next = find_next_directive(rest);
            let body = py_strip(&rest[..next]);
            if body.is_empty() {
                continue;
            }
            let (body, guards) = parse_guards(body);
            st.mod_struct_list.push(StructEntry { kind: kind.to_string(), name: name.clone(), scope_func, content: body, guards, mod_file: mod_file.to_string() });
            st.index_push(mod_file, format!("{token}{name}]"));
        }
    }

    // ---- Replace Function [name]: ----
    for chunk in split_chunks(&mod_content, "Replace Function [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let name = py_strip(&chunk[..be]).to_string();
        let rest = dedupe_tail_after_first_line(&chunk[be + 2..]);
        let next = find_next_directive(rest);
        let body = py_strip(&rest[..next]);
        if body.is_empty() {
            continue;
        }
        let (body, guards) = parse_guards(body);
        st.mod_func_list.push(FuncEntry { kind: "replace_func".to_string(), name: name.clone(), body: Some(body), guards, mod_file: mod_file.to_string(), ..Default::default() });
        st.register_conflict(&name, mod_file, "Replace Function");
        st.index_push(mod_file, format!("Replace Function [{name}]"));
    }

    // ---- Insert Before / Insert After ----
    for (kind, token) in [("insert_before", "Insert Before ["), ("insert_after", "Insert After [")] {
        for chunk in split_chunks(&mod_content, token) {
            let Some(be) = bracket_end(chunk) else { continue };
            let name_raw = py_strip(&chunk[..be]);
            let once = name_raw.contains("[ONCE]");
            let soft = name_raw.contains("[soft]");
            let typeof_func = TYPEOF_RE.captures(name_raw).map(|c| py_strip(&c[1]).to_string());
            let name = py_strip(&MODIFIER_RE.replace_all(name_raw, "")).to_string();
            let rest = &chunk[be + 2..];
            let Some(with_m) = WITH_LINE_RE.find(rest) else { continue };
            let rest_after = &rest[with_m.end()..];
            let next = find_next_directive(rest_after);
            let body = py_strip(&rest_after[..next]);
            if body.is_empty() {
                continue;
            }
            let (mut body, guards) = parse_guards(body);
            if let Some(f) = typeof_func {
                body = format!("<<if typeof {f} === \"function\">>\n{body}\n<</if>>");
            }
            st.mod_func_list.push(FuncEntry {
                kind: kind.to_string(),
                name: name.clone(),
                body: Some(body),
                guards,
                mod_file: mod_file.to_string(),
                once: Some(once),
                soft: Some(soft),
                ..Default::default()
            });
            let kind_label = token.trim_end_matches('[').trim().to_string();
            st.register_conflict(&name, mod_file, &kind_label);
            st.index_push(mod_file, format!("{token}{name}]"));
        }
    }

    // ---- Delete Block [name]: ----
    for chunk in split_chunks(&mod_content, "Delete Block [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let name = py_strip(&chunk[..be]).to_string();
        let rest = &chunk[be + 2..];
        let guard_text = rest.split('\n').next().unwrap_or(rest);
        let (_, guards) = parse_guards(guard_text);
        st.mod_func_list.push(FuncEntry { kind: "delete_block".to_string(), name: name.clone(), guards, mod_file: mod_file.to_string(), ..Default::default() });
        st.register_conflict(&name, mod_file, "Delete Block");
        st.index_push(mod_file, format!("Delete Block [{name}]"));
    }

    // ---- Replace In Passage ----
    for chunk in split_chunks(&mod_content, "Replace In Passage [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let passage_name = py_strip(&chunk[..be]).to_string();
        let rest = &chunk[be + 2..];
        let Some(with_m) = WITH_LINE_RE.find(rest) else { continue };
        let anchor_raw = py_strip(&rest[..with_m.start()]);
        if anchor_raw.is_empty() {
            continue;
        }
        let rest_after = &rest[with_m.end()..];
        let next = find_next_directive(rest_after);
        let body_raw = py_strip(&rest_after[..next]);
        let anchor_e = escape_twine_tags(&escape_html_between_tags(anchor_raw));
        let body_e = escape_twine_tags(&escape_html_between_tags(body_raw));
        let body_e = auto_escape_passage_bodies(&body_e);
        let body_e = py_strip(&body_e).to_string();
        let (body_e, guards) = parse_guards(&body_e);
        st.mod_func_list.push(FuncEntry {
            kind: "replace_in_passage".to_string(),
            passage_name: Some(passage_name.clone()),
            name: anchor_e.clone(),
            body: Some(body_e),
            guards,
            mod_file: mod_file.to_string(),
            ..Default::default()
        });
        let label = format!("Replace In Passage [{passage_name}]: {}", first_chars(&anchor_e, 40));
        st.register_conflict(&label, mod_file, "Replace In Passage");
        st.index_push(mod_file, label);
    }

    // ---- Delete Span [start] To [end]: ----
    for chunk in split_chunks(&mod_content, "Delete Span [") {
        let Some(sbe) = chunk.find(']') else { continue };
        let start_anchor = py_strip(&chunk[..sbe]);
        let rest_after_start = chunk[sbe + 1..].trim_start_matches(crate::pytext::is_py_space);
        let Some(rest_after_to) = rest_after_start.strip_prefix("To [") else { continue };
        let Some(ebe) = rest_after_to.find("]:") else { continue };
        let end_anchor = py_strip(&rest_after_to[..ebe]);
        if start_anchor.is_empty() || end_anchor.is_empty() {
            continue;
        }
        let start_enc = escape_twine_tags(&escape_html_between_tags(start_anchor));
        let end_enc = escape_twine_tags(&escape_html_between_tags(end_anchor));
        let rest_after_end = &rest_after_to[ebe + 2..];
        let guard_text = rest_after_end.split('\n').next().unwrap_or(rest_after_end);
        let (_, guards) = parse_guards(guard_text);
        st.mod_func_list.push(FuncEntry { kind: "delete_span".to_string(), name: start_enc, end_anchor: Some(end_enc), guards, mod_file: mod_file.to_string(), ..Default::default() });
        let label = format!("Delete Span [{}] To [{}]", first_chars(start_anchor, 40), first_chars(end_anchor, 40));
        st.register_conflict(&label, mod_file, "Delete Span");
        st.index_push(mod_file, label);
    }

    // ---- Prepend To Passage / Append To Passage ----
    for (kind, token) in [("prepend_to_passage", "Prepend To Passage ["), ("append_to_passage", "Append To Passage [")] {
        for chunk in split_chunks(&mod_content, token) {
            let Some(be) = bracket_end(chunk) else { continue };
            let passage_name = py_strip(&chunk[..be]).to_string();
            let rest = &chunk[be + 2..];
            let Some(with_m) = WITH_LINE_RE.find(rest) else { continue };
            let rest_after = &rest[with_m.end()..];
            let next = find_next_directive(rest_after);
            let body_raw = py_strip(&rest_after[..next]);
            if body_raw.is_empty() {
                continue;
            }
            let body_e = escape_twine_tags(&escape_html_between_tags(body_raw));
            let body_e = auto_escape_passage_bodies(py_strip(&body_e));
            let (body_e, guards) = parse_guards(&body_e);
            st.mod_func_list.push(FuncEntry {
                kind: kind.to_string(),
                passage_name: Some(passage_name.clone()),
                name: passage_name.clone(),
                body: Some(body_e),
                guards,
                mod_file: mod_file.to_string(),
                ..Default::default()
            });
            let label = format!("{token}{passage_name}]");
            let kind_label = token.trim_end_matches('[').trim().to_string();
            st.register_conflict(&label, mod_file, &kind_label);
            st.index_push(mod_file, label);
        }
    }

    // ---- Add Tag To Passage / Remove Tag From Passage ----
    for (kind, token) in [("add_tag_to_passage", "Add Tag To Passage ["), ("remove_tag_from_passage", "Remove Tag From Passage [")] {
        for chunk in split_chunks(&mod_content, token) {
            let Some(be) = bracket_end(chunk) else { continue };
            let passage_name = py_strip(&chunk[..be]).to_string();
            let rest = &chunk[be + 2..];
            let tag_line = py_strip(rest.split('\n').next().unwrap_or(rest));
            if tag_line.is_empty() {
                continue;
            }
            let (tag_text, guards) = parse_guards(tag_line);
            let tag_text = py_strip(&tag_text).to_string();
            if tag_text.is_empty() {
                continue;
            }
            st.mod_func_list.push(FuncEntry {
                kind: kind.to_string(),
                passage_name: Some(passage_name.clone()),
                name: passage_name.clone(),
                tags: Some(tag_text.clone()),
                guards,
                mod_file: mod_file.to_string(),
                ..Default::default()
            });
            let label = format!("{token}{passage_name}]: {}", first_chars(&tag_text, 40));
            let kind_label = token.trim_end_matches('[').trim().to_string();
            st.register_conflict(&label, mod_file, &kind_label);
            st.index_push(mod_file, label);
        }
    }

    // ---- Hook [fn]: ----
    for chunk in split_chunks(&mod_content, "Hook [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let name = py_strip(&chunk[..be]).to_string();
        let rest = dedupe_tail_after_first_line(&chunk[be + 2..]);
        let Some(tm) = TIMING_RE.captures(rest) else { continue };
        let timing = tm[1].to_string();
        let hook_body = py_strip(&rest[tm.get(0).unwrap().end()..]);
        let next = find_next_directive(hook_body);
        let hook_body = py_strip(&hook_body[..next]);
        if hook_body.is_empty() {
            continue;
        }
        let (hook_body, guards) = parse_guards(hook_body);
        st.mod_hook_list.push(HookEntry { kind: "hook".to_string(), name: name.clone(), timing: timing.clone(), body: hook_body, guards, mod_file: mod_file.to_string() });
        st.register_conflict(&name, mod_file, &format!("Hook ({timing})"));
        st.index_push(mod_file, format!("Hook [{name}]"));
    }

    // ---- Rename Passage [Old]: New ----
    for chunk in split_chunks(&mod_content, "Rename Passage [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let old_name = py_strip(&chunk[..be]).to_string();
        let rest = &chunk[be + 2..];
        let new_name_raw = py_strip(rest.split('\n').next().unwrap_or(rest));
        if new_name_raw.is_empty() {
            continue;
        }
        let (new_name_raw, guards) = parse_guards(new_name_raw);
        let new_name = py_strip(&new_name_raw).to_string();
        if new_name.is_empty() {
            continue;
        }
        st.mod_func_list.push(FuncEntry { kind: "rename_passage".to_string(), name: old_name.clone(), new_name: Some(new_name.clone()), guards, mod_file: mod_file.to_string(), ..Default::default() });
        let label = format!("Rename Passage [{old_name}]: {new_name}");
        st.register_conflict(&label, mod_file, "Rename Passage");
        st.index_push(mod_file, label);
    }

    // ---- Add Variable [name]: value ----
    for chunk in split_chunks(&mod_content, "Add Variable [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let varname = py_strip(&chunk[..be]).to_string();
        let rest = &chunk[be + 2..];
        let value_raw = py_strip(rest.split('\n').next().unwrap_or(rest));
        if varname.is_empty() {
            continue;
        }
        let (value_raw, guards) = parse_guards(value_raw);
        let value = py_strip(&value_raw).to_string();
        st.mod_func_list.push(FuncEntry { kind: "add_variable".to_string(), name: varname.clone(), value: Some(value), guards, mod_file: mod_file.to_string(), ..Default::default() });
        st.index_push(mod_file, format!("Add Variable [{varname}]"));
    }

    // ---- Insert Into Function ----
    for chunk in split_chunks(&mod_content, "Insert Into Function [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let func_name = py_strip(&chunk[..be]).to_string();
        let rest = &chunk[be + 2..];
        let Some(with_m) = WITH_LINE_RE.find(rest) else { continue };
        let anchor_raw = py_strip(&rest[..with_m.start()]);
        if anchor_raw.is_empty() {
            continue;
        }
        let rest_after = &rest[with_m.end()..];
        let next = find_next_directive(rest_after);
        let body_raw = py_strip(&rest_after[..next]);
        if body_raw.is_empty() {
            continue;
        }
        let (body_raw, guards) = parse_guards(body_raw);
        st.mod_func_list.push(FuncEntry {
            kind: "insert_into_func".to_string(),
            name: func_name.clone(),
            anchor: Some(anchor_raw.to_string()),
            body: Some(body_raw),
            guards,
            mod_file: mod_file.to_string(),
            ..Default::default()
        });
        let label = format!("Insert Into Function [{func_name}]: {}", first_chars(anchor_raw, 40));
        st.register_conflict(&label, mod_file, "Insert Into Function");
        st.index_push(mod_file, label);
    }

    // ---- Replace Function Signature [fn]: params ----
    for chunk in split_chunks(&mod_content, "Replace Function Signature [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let func_name = py_strip(&chunk[..be]).to_string();
        let rest = &chunk[be + 2..];
        let new_params_raw = py_strip(rest.split('\n').next().unwrap_or(rest));
        if new_params_raw.is_empty() {
            continue;
        }
        let (new_params_raw, guards) = parse_guards(new_params_raw);
        let new_params = py_strip(&new_params_raw).to_string();
        if new_params.is_empty() {
            continue;
        }
        st.mod_func_list.push(FuncEntry { kind: "replace_func_sig".to_string(), name: func_name.clone(), new_params: Some(new_params), guards, mod_file: mod_file.to_string(), ..Default::default() });
        let label = format!("Replace Function Signature [{func_name}]");
        st.register_conflict(&label, mod_file, "Replace Function Signature");
        st.index_push(mod_file, label);
    }

    // ---- Clone Passage [Source] As [Dest]: ----
    for chunk in split_chunks(&mod_content, "Clone Passage [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let after = &chunk[be + 2..];
        let rest_line = py_strip(after.split('\n').next().unwrap_or(after));
        let src_name = py_strip(&chunk[..be]).to_string();
        let Some(c) = CLONE_AS_RE.captures(rest_line) else { continue };
        let dst_name = py_strip(&c[1]).to_string();
        // Python: chunk[bracket_end + 2 + len(rest_line):] with len() counted in characters
        let chars_before = chunk[..be + 2].chars().count() + rest_line.chars().count();
        let rest_after = &chunk[crate::html::char_to_byte(chunk, chars_before)..];
        let next = find_next_directive(rest_after);
        let body_raw = py_strip(&rest_after[..next]);
        let (_, guards) = parse_guards(body_raw);
        st.mod_func_list.push(FuncEntry { kind: "clone_passage".to_string(), name: src_name.clone(), new_name: Some(dst_name.clone()), guards, mod_file: mod_file.to_string(), ..Default::default() });
        st.index_push(mod_file, format!("Clone Passage [{src_name}] As [{dst_name}]"));
    }

    // ---- Replace In All Passages [tag]: ----
    for chunk in split_chunks(&mod_content, "Replace In All Passages [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let tag_name = py_strip(&chunk[..be]).to_string();
        let rest = &chunk[be + 2..];
        let Some(with_m) = WITH_LINE_RE.find(rest) else { continue };
        let anchor_raw = py_strip(&rest[..with_m.start()]);
        if anchor_raw.is_empty() {
            continue;
        }
        let rest_after = &rest[with_m.end()..];
        let next = find_next_directive(rest_after);
        let body_raw = py_strip(&rest_after[..next]);
        if body_raw.is_empty() {
            continue;
        }
        let (body_raw, guards) = parse_guards(body_raw);
        st.mod_func_list.push(FuncEntry {
            kind: "replace_in_all_passages".to_string(),
            name: tag_name.clone(),
            anchor: Some(anchor_raw.to_string()),
            body: Some(body_raw),
            guards,
            mod_file: mod_file.to_string(),
            ..Default::default()
        });
        st.index_push(mod_file, format!("Replace In All Passages [{tag_name}]: {}", first_chars(anchor_raw, 40)));
    }

    // ---- Add StoryVar [name]: value ----
    for chunk in split_chunks(&mod_content, "Add StoryVar [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let varname = py_strip(&chunk[..be]).to_string();
        let rest = &chunk[be + 2..];
        let value_raw = py_strip(rest.split('\n').next().unwrap_or(rest));
        let (value_raw, guards) = parse_guards(value_raw);
        let value = py_strip(&value_raw).to_string();
        if varname.is_empty() {
            continue;
        }
        st.mod_func_list.push(FuncEntry { kind: "add_storyvar".to_string(), name: varname.clone(), value: Some(value), guards, mod_file: mod_file.to_string(), ..Default::default() });
        st.index_push(mod_file, format!("Add StoryVar [{varname}]"));
    }

    // ---- Add Function ----
    for chunk in split_chunks(&mod_content, "Add Function [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let name = py_strip(&chunk[..be]).to_string();
        let rest = dedupe_tail_after_first_line(&chunk[be + 2..]);
        let next = find_next_directive(rest);
        let body = py_strip(&rest[..next]);
        if body.is_empty() {
            continue;
        }
        let (body, guards) = parse_guards(body);
        st.mod_func_list.push(FuncEntry {
            kind: "add_func".to_string(),
            name: name.clone(),
            body: Some(body),
            guards,
            mod_file: mod_file.to_string(),
            ..Default::default()
        });
        st.index_push(mod_file, format!("Add Function [{name}]"));
    }

    // ---- Replace In Function ----
    for chunk in split_chunks(&mod_content, "Replace In Function [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let func_name = py_strip(&chunk[..be]).to_string();
        let rest = &chunk[be + 2..];
        let Some(with_m) = WITH_LINE_RE.find(rest) else { continue };
        let anchor_raw = py_strip(&rest[..with_m.start()]);
        if anchor_raw.is_empty() {
            continue;
        }
        let rest_after = &rest[with_m.end()..];
        let next = find_next_directive(rest_after);
        let body_raw = py_strip(&rest_after[..next]);
        if body_raw.is_empty() {
            continue;
        }
        let (body_raw, guards) = parse_guards(body_raw);
        st.mod_func_list.push(FuncEntry {
            kind: "replace_in_func".to_string(),
            name: func_name.clone(),
            anchor: Some(anchor_raw.to_string()),
            body: Some(body_raw),
            guards,
            mod_file: mod_file.to_string(),
            ..Default::default()
        });
        let label = format!("Replace In Function [{func_name}]: {}", first_chars(anchor_raw, 40));
        st.register_conflict(&label, mod_file, "Replace In Function");
        st.index_push(mod_file, label);
    }

    // ---- Delete In Passage [name]: anchor ----
    for chunk in split_chunks(&mod_content, "Delete In Passage [") {
        let Some(be) = bracket_end(chunk) else { continue };
        let passage_name = py_strip(&chunk[..be]).to_string();
        let rest = &chunk[be + 2..];
        let next = find_next_directive(rest);
        let anchor_start = rest.find('\n').map(|i| i + 1).unwrap_or(0);
        // Python slices rest[anchor_start:next]; when next < anchor_start the slice is empty
        let anchor_raw = if next >= anchor_start { py_strip(&rest[anchor_start..next]) } else { "" };
        if anchor_raw.is_empty() {
            continue;
        }
        let (anchor_raw, guards) = parse_guards(anchor_raw);
        let anchor_e = escape_twine_tags(&escape_html_between_tags(&anchor_raw));
        let anchor_e = auto_escape_passage_bodies(py_strip(&anchor_e));
        st.mod_func_list.push(FuncEntry {
            kind: "delete_in_passage".to_string(),
            passage_name: Some(passage_name.clone()),
            name: anchor_e.clone(),
            guards,
            mod_file: mod_file.to_string(),
            ..Default::default()
        });
        let label = format!("Delete In Passage [{passage_name}]: {}", first_chars(&anchor_e, 40));
        st.register_conflict(&label, mod_file, "Delete In Passage");
        st.index_push(mod_file, label);
    }

    // ---- directives we do not support yet ----
    let mut seen: HashSet<&str> = HashSet::new();
    for p in line_starts(&mod_content) {
        let rest = &mod_content[p..];
        for tok in UNSUPPORTED_TOKENS {
            if rest.starts_with(tok) && seen.insert(tok) {
                st.unsupported.push((mod_file.to_string(), tok.to_string()));
            }
        }
    }
}
