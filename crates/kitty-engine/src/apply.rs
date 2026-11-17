//! Apply phase: port of `patch_html_file` for the directives the current mod
//! packs use: classic `Replace:` / `~~` blocks (including passage-tag anchors),
//! `ReplaceReg`, `Replace In Passage`, Append/Prepend To Passage, Insert
//! Before/After, Add Function, Insert/Replace In Function, Add Javascript/CSS/
//! Events and Add Passage injection, guards and pid renumbering.
//!
//! Anything else returns an explicit "not supported yet" error; nothing is
//! skipped silently.

use crate::escape::{auto_escape_passage_bodies, escape_html_between_tags, escape_twine_tags, unescape_for_raw};
use crate::guards::Guards;
use crate::html::{back_chars, bracket_match, char_to_byte, common_prefix_chars, renormalize_pids, skip_chars, split_html, window, SplitHtml};
use crate::model::{BodyRecord, FuncEntry, ParseState, PassageGuards, PassageRecord, StructEntry};
use crate::pytext::{html_unescape_basic, py_strip};
use crate::search::{replace_all_in_place, FastFind};
use anyhow::{bail, Result};
use indexmap::IndexMap;
use regex::Regex;
use std::collections::HashSet;

mod more;

const SCRIPT_MARK: &str = "</style><script";
const PASSAGE_OPEN: &str = "<tw-passagedata";
const PASSAGE_CLOSE: &str = "</tw-passagedata>";
const SCRIPT_ANCHOR: &str = "</script><tw-passagedata";

static PASSAGE_NAME_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r#"<tw-passagedata[^>]*name="([^"]+)""#).unwrap());

#[derive(Debug, Default)]
pub struct Outcome {
    pub html: String,
    pub replacements_made: usize,
    pub replacements_failed: usize,
    /// Lines the Python patcher would write to FailsPatchLog.txt.
    pub fail_log: Vec<String>,
    /// Lines for ModPatchLog.txt.
    pub log: Vec<String>,
}

#[derive(PartialEq, Clone, Copy)]
enum Ctx {
    Script,
    Passage,
    Other,
}

/// Where a match at byte `pos` sits: raw script block, inside a passage body, or neither.
/// Equivalent to the three `rfind` calls of the Python code, without scanning
/// megabytes backwards for every block.
fn context_at(html: &str, pos: usize) -> Ctx {
    let pos = pos.min(html.len());
    let head = &html[..pos];
    let lpo = head.rfind(PASSAGE_OPEN).map(|p| p as i64).unwrap_or(-1);
    let lpc = head.rfind(PASSAGE_CLOSE).map(|p| p as i64).unwrap_or(-1);
    let mx = lpo.max(lpc);
    let from = if mx < 0 { 0 } else { (mx + 1) as usize };
    let script_after = from <= head.len() && head[from..].contains(SCRIPT_MARK);
    if script_after {
        Ctx::Script
    } else if lpo != -1 && lpo > lpc {
        Ctx::Passage
    } else {
        Ctx::Other
    }
}

fn short(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python `html_content.find(anchor)` with the `_is_in_script_block` test.
fn is_in_script_block(html: &str, position: usize) -> bool {
    match html.ffind(SCRIPT_ANCHOR) {
        None => true,
        Some(end) => position < end,
    }
}

/// `update_old_lines_from_html` (the `[to]` marker).
fn update_old_lines_from_html(old_lines: &str, html: &str) -> String {
    let Some(i) = old_lines.find("[to]") else { return old_lines.to_string() };
    let prefix = &old_lines[..i];
    let suffix = &old_lines[i + 4..];
    let mut from = 0usize;
    while let Some(p) = html[from..].find(prefix) {
        let start = from + p;
        let after = start + prefix.len();
        if let Some(s) = html[after..].find(suffix) {
            let middle = &html[after..after + s];
            return format!("{prefix}{middle}{suffix}");
        }
        from = start + html[start..].chars().next().map_or(1, |c| c.len_utf8());
        if from > html.len() {
            break;
        }
    }
    old_lines.to_string()
}

/// `_resolve_pid_auto_in_anchor`
fn resolve_pid_auto_in_anchor(anchor: &str, html: &str) -> String {
    let low = anchor.to_lowercase();
    let has_pid = low.contains("pid=\"auto\"") || low.contains("pid=&quot;auto&quot;");
    let has_pos = low.contains("position=\"auto\"") || low.contains("position=&quot;auto&quot;");
    if !has_pid && !has_pos {
        return anchor.to_string();
    }
    let name_re = Regex::new(r#"name="([^"]+)""#).unwrap();
    let name_re2 = Regex::new(r"name=&quot;([^&]+)&quot;").unwrap();
    let Some(pname) = name_re.captures(anchor).or_else(|| name_re2.captures(anchor)).map(|c| c[1].to_string()) else {
        return anchor.to_string();
    };
    let tag_re = Regex::new(&format!(r#"<tw-passagedata[^>]*name="{}"[^>]*>"#, regex::escape(&pname))).unwrap();
    let Some(tag) = tag_re.find(html).map(|m| m.as_str().to_string()) else { return anchor.to_string() };
    let mut anchor = anchor.to_string();
    let sub_ci = |text: &str, pat: &str, rep: &str| -> String {
        Regex::new(&format!("(?i){}", regex::escape(pat))).unwrap().replace(text, regex::NoExpand(rep)).into_owned()
    };
    if has_pid {
        if let Some(c) = Regex::new(r#"pid="([^"]+)""#).unwrap().captures(&tag) {
            let rep = format!("pid=\"{}\"", &c[1]);
            anchor = sub_ci(&anchor, "pid=&quot;auto&quot;", &rep);
            anchor = sub_ci(&anchor, "pid=\"auto\"", &rep);
        }
    }
    if has_pos {
        let pos = Regex::new(r#"position="([^"]+)""#).unwrap().captures(&tag).map(|c| c[1].to_string()).unwrap_or_else(|| "0,0".to_string());
        let rep = format!("position=\"{pos}\"");
        anchor = sub_ci(&anchor, "position=&quot;auto&quot;", &rep);
        anchor = sub_ci(&anchor, "position=\"auto\"", &rep);
    }
    anchor
}

fn check_literal_anchor(anchor: &str) -> Result<()> {
    if anchor.contains("\\n") {
        bail!("anchors containing a backslash followed by 'n' use the Python newline-flex pattern quirk, which is not ported yet");
    }
    Ok(())
}

/// Location of a passage found with the live regex
/// `(<tw-passagedata[^>]*name="X"[^>]*>)(.*?)(</tw-passagedata>)`.
#[derive(Clone, Copy, Debug)]
struct Span {
    open_start: usize,
    body_start: usize,
    body_end: usize,
    close_end: usize,
}

/// First passage whose opening tag contains `name="<name>"`, found without a
/// regex scan of the whole file (same result as the Python regex).
fn find_passage(html: &str, name: &str) -> Option<Span> {
    let needle = format!("name=\"{name}\"");
    let mut from = 0usize;
    while let Some(rel) = html[from..].ffind(&needle) {
        let p = from + rel;
        if let Some(ts) = html[..p].rfind(PASSAGE_OPEN) {
            if !html[ts..p].contains('>') {
                let after = p + needle.len();
                if let Some(g) = html[after..].find('>') {
                    let body_start = after + g + 1;
                    if let Some(c) = html[body_start..].find(PASSAGE_CLOSE) {
                        return Some(Span { open_start: ts, body_start, body_end: body_start + c, close_end: body_start + c + PASSAGE_CLOSE.len() });
                    }
                }
            }
        }
        from = p + 1;
    }
    None
}

fn decode_body(b: &str) -> String {
    b.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&").replace("&#39;", "'")
}

/// `_resolve_auto_pids`: every `pid="auto"` (any case) becomes a fresh sequential numeric pid.
fn resolve_auto_pids(text: &str, next_pid: &mut i64) -> String {
    let re = Regex::new(r#"(?i)pid="auto""#).unwrap();
    re.replace_all(text, |_: &regex::Captures| {
        let s = format!("pid=\"{}\"", *next_pid);
        *next_pid += 1;
        s
    })
    .into_owned()
}

/// Highest numeric pid currently in the HTML (`_get_max_pid`).
fn max_pid_in(html: &str) -> i64 {
    let re = Regex::new(r#"<tw-passagedata[^>]+pid="([^"]+)""#).unwrap();
    re.captures_iter(html).filter_map(|c| c[1].parse::<i64>().ok()).max().unwrap_or(0)
}

/// `_resolve_auto_passage_attrs`: `pid="auto"` / `position="auto"` in a passage replacement take the live
/// values of the named passage (a fresh pid and `0,0` when the passage is not in the HTML yet).
fn resolve_auto_passage_attrs(apply_lines: &str, html: &str, pname: &str) -> String {
    if !apply_lines.contains("pid=\"auto\"") && !apply_lines.contains("position=\"auto\"") {
        return apply_lines.to_string();
    }
    let tag_re = Regex::new(&format!(r#"<tw-passagedata[^>]*name="{}"[^>]*>"#, regex::escape(pname))).unwrap();
    let (live_pid, live_pos) = match tag_re.find(html) {
        Some(m) => {
            let tag = m.as_str();
            let pid = Regex::new(r#"pid="([^"]+)""#).unwrap().captures(tag).map(|c| c[1].to_string());
            let pos = Regex::new(r#"position="([^"]+)""#).unwrap().captures(tag).map(|c| c[1].to_string()).unwrap_or_else(|| "0,0".to_string());
            (pid, pos)
        }
        None => (Some((max_pid_in(html) + 1).to_string()), "0,0".to_string()),
    };
    let mut out = apply_lines.to_string();
    if out.contains("pid=\"auto\"") {
        if let Some(pid) = &live_pid {
            out = out.replacen("pid=\"auto\"", &format!("pid=\"{pid}\""), 1);
        }
    }
    if out.contains("position=\"auto\"") {
        out = out.replacen("position=\"auto\"", &format!("position=\"{live_pos}\""), 1);
    }
    out
}

struct P<'a> {
    st: &'a ParseState,
    loaded: Vec<String>,
    html: String,
    snap: SplitHtml,
    registry: Vec<PassageRecord>,
    /// passage name -> index into `registry` (grows when a passage is cloned)
    names_seen: IndexMap<String, usize>,
    out: Outcome,
    once: HashSet<String>,
    failed_keys: Vec<String>,
    /// Insert After/Before entries whose anchor was not found in the HTML (kind, anchor, body, soft)
    failed_ia_ib: Vec<(String, String, String, bool)>,
    /// next numeric pid for `pid="auto"` in Replace With: texts
    next_pid: i64,
    /// Add Javascript snippets added during the run (Add StoryVar)
    js_extra: Vec<BodyRecord>,
    /// optional progress messages for the caller
    progress: Option<&'a mut dyn FnMut(&str)>,
    /// names of passages present in the vanilla HTML (a deleted block is removed from it)
    original_names: HashSet<String>,
    /// registry indices already injected by Delete Block
    early_injected: HashSet<usize>,
}

impl<'a> P<'a> {
    fn report(&mut self, msg: &str) {
        if let Some(p) = self.progress.as_mut() {
            p(msg);
        }
    }

    fn fail(&mut self, msg: String, also_log: bool) {
        if also_log {
            self.out.log.push(msg.clone());
        }
        self.out.fail_log.push(msg);
        self.out.replacements_failed += 1;
    }

    /// `_check_guards`
    fn guards_ok(&self, g: &Guards) -> Result<bool> {
        for v in &g.if_exists {
            if !self.html.contains(&escape_twine_tags(v)) {
                return Ok(false);
            }
        }
        for v in &g.if_not_exists {
            if self.html.contains(&escape_twine_tags(v)) {
                return Ok(false);
            }
        }
        for v in &g.if_reg_exists {
            let re = Regex::new(&format!("(?s){v}")).map_err(|e| anyhow::anyhow!("IfRegExists pattern '{v}': {e}"))?;
            if !re.is_match(&self.html) {
                return Ok(false);
            }
        }
        for v in &g.if_reg_not_exists {
            let re = Regex::new(&format!("(?s){v}")).map_err(|e| anyhow::anyhow!("IfRegNotExists pattern '{v}': {e}"))?;
            if re.is_match(&self.html) {
                return Ok(false);
            }
        }
        for v in &g.if_mod_loaded {
            if !self.loaded.contains(&v.to_lowercase()) {
                return Ok(false);
            }
        }
        for v in &g.if_mod_not_loaded {
            if self.loaded.contains(&v.to_lowercase()) {
                return Ok(false);
            }
        }
        let reg_names: HashSet<&str> = self.registry.iter().map(|r| r.name.as_str()).collect();
        for v in &g.if_passage_exists {
            if !self.snap.passage_dict.contains_key(v) && !reg_names.contains(v.as_str()) {
                return Ok(false);
            }
        }
        for v in &g.if_passage_not_exists {
            if self.snap.passage_dict.contains_key(v) || reg_names.contains(v.as_str()) {
                return Ok(false);
            }
        }
        for v in &g.if_passage_contains {
            let Some((name, text)) = v.split_once('|') else { continue };
            let Some(body) = self.snap.passage_dict.get(name) else { return Ok(false) };
            if !decode_body(body).contains(text) {
                return Ok(false);
            }
        }
        for v in &g.if_passage_has_tag {
            let Some((name, tag)) = v.split_once('|') else { continue };
            if let Some(meta) = self.snap.passage_meta.get(name) {
                let tags = Regex::new(r#"tags="([^"]*)"#).unwrap().captures(&meta.open_tag).map(|c| c[1].to_string());
                match tags {
                    Some(t) if t.split_whitespace().any(|x| x == tag) => {}
                    _ => return Ok(false),
                }
            } else {
                let re = Regex::new(&format!(r#"<tw-passagedata[^>]*name="{}"[^>]*tags="([^"]*)""#, regex::escape(name))).unwrap();
                match re.captures(&self.html) {
                    Some(c) if c[1].split_whitespace().any(|x| x == tag) => {}
                    _ => return Ok(false),
                }
            }
        }
        if !g.if_version_at_least.is_empty() {
            bail!("IfVersionAtLeast is not supported by the builtin engine yet");
        }
        for v in &g.if_function_exists {
            let n = regex::escape(v);
            let re = Regex::new(&format!(r"(?m)(?:^|[\s;]){n}\s*[=:]\s*function|function\s+{n}\s*\(")).unwrap();
            if !re.is_match(&self.snap.script_block) {
                return Ok(false);
            }
        }
        for v in &g.if_function_not_exists {
            let n = regex::escape(v);
            let re = Regex::new(&format!(r"(?m)(?:^|[\s;]){n}\s*[=:]\s*function|function\s+{n}\s*\(")).unwrap();
            if re.is_match(&self.html) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    // ---------------------------------------------------------------------
    // Phase 1: classic Replace blocks, in mod_list order, on the live HTML
    // ---------------------------------------------------------------------
    fn phase1(&mut self) -> Result<()> {
        let st = self.st;
        let total = st.mod_list.len();
        for (n, (key, _src_mod)) in st.mod_list.iter().enumerate() {
            if n > 0 && n % 250 == 0 {
                self.report(&format!("Applying replacements: {n}/{total}"));
            }
            let Some(entry) = st.mod_dict.get(key) else { continue };
            let new_lines = entry.0.clone();
            if !self.guards_ok(&entry.1)? {
                self.out.log.push(format!("Guard condition not met for '{key}', skipping."));
                continue;
            }

            let old_expanded = update_old_lines_from_html(key, &self.html);
            let old_raw0 = old_expanded.clone();
            let mut old_lines = escape_twine_tags(&old_expanded);
            old_lines = resolve_pid_auto_in_anchor(&old_lines, &self.html);
            let old_raw = resolve_pid_auto_in_anchor(&old_raw0, &self.html);
            check_literal_anchor(&old_lines)?;
            check_literal_anchor(&old_raw)?;

            let passage_name_m: Option<(String, usize)> =
                PASSAGE_NAME_RE.captures(&old_lines).map(|c| (c[1].to_string(), c.get(0).unwrap().start()));
            let mut match_start_b: Option<usize> = None; // byte offsets into the live html
            let mut match_end_b: usize = 0;

            // ---- locate the anchor ----
            let mut used_slice = false;
            if let Some(pn) = &passage_name_m {
                let pname = pn.0.clone();
                let boundary_cross = !py_strip(&old_lines[..pn.1]).is_empty();
                if self.snap.passage_dict.contains_key(&pname) && !boundary_cross {
                    used_slice = true;
                    if let Some(meta) = self.snap.passage_meta.get(&pname).cloned() {
                        let slice = format!("{}{}{}", meta.open_tag, self.snap.passage_dict[&pname], PASSAGE_CLOSE);
                        if let Some(rel) = slice.find(&old_lines) {
                            // positions remapped with the *snapshot* offset, in characters (stale, like Python)
                            let s_chars = meta.start_chars + slice[..rel].chars().count();
                            let e_chars = s_chars + old_lines.chars().count();
                            match_start_b = Some(char_to_byte(&self.html, s_chars));
                            match_end_b = char_to_byte(&self.html, e_chars);
                        }
                    } else {
                        // no meta: full search
                        if let Some(p) = self.html.ffind(&old_lines) {
                            match_start_b = Some(p);
                            match_end_b = p + old_lines.len();
                        }
                    }
                } else if let Some(p) = self.html.ffind(&old_lines) {
                    match_start_b = Some(p);
                    match_end_b = p + old_lines.len();
                }
            } else if let Some(p) = self.html.ffind(&old_lines) {
                match_start_b = Some(p);
                match_end_b = p + old_lines.len();
            }
            let _ = used_slice;

            // raw anchor fallback (script-block targets)
            if match_start_b.is_none() && passage_name_m.is_none() && old_raw != old_lines {
                if let Some(p) = self.html.ffind(&old_raw) {
                    self.out.log.push(format!("Replace: encoded anchor missed -- raw script-block fallback succeeded for '{}...' (v0.7.4)", short(&old_lines, 80).trim()));
                    old_lines = old_raw.clone();
                    match_start_b = Some(p);
                    match_end_b = p + old_lines.len();
                }
            }

            if let Some(start) = match_start_b {
                let ctx = context_at(&self.html, start);

                let mut new_lines = new_lines.clone();
                if new_lines.contains("pid=\"auto\"") || new_lines.contains("pid=\"AUTO\"") {
                    new_lines = resolve_auto_pids(&new_lines, &mut self.next_pid);
                }
                let mut apply_lines = new_lines;
                if let Some(pn) = &passage_name_m {
                    if apply_lines.contains("<tw-passagedata") && (apply_lines.contains("pid=\"auto\"") || apply_lines.contains("position=\"auto\"")) {
                        apply_lines = resolve_auto_passage_attrs(&apply_lines, &self.html, &pn.0);
                    }
                }
                if ctx == Ctx::Script && !old_lines.contains("&lt;") && !old_lines.contains("&gt;") {
                    apply_lines = unescape_for_raw(&apply_lines);
                }

                // duplicate-probe skip
                let new_stripped = py_strip(&apply_lines);
                let old_stripped = py_strip(&old_lines);
                let overlap = common_prefix_chars(old_stripped, new_stripped);
                let probe = skip_chars(new_stripped, overlap);
                if !probe.is_empty() {
                    let n = probe.chars().count() + 200;
                    let from = match_end_b.min(self.html.len());
                    if window(&self.html, from, n).contains(probe) {
                        self.out.log.push(format!("Patch already applied at match site for '{old_lines}', skipping."));
                        continue;
                    }
                }

                if ctx == Ctx::Passage && (apply_lines.contains("<<") || apply_lines.contains(">>")) {
                    apply_lines = escape_twine_tags(&apply_lines);
                }
                self.out.log.push(format!("Replaced: '{}'", short(&old_lines, 80).replace('\n', " ")));

                if ctx == Ctx::Script {
                    replace_all_in_place(&mut self.html, &old_lines, &apply_lines);
                } else if let Some(p) = self.html.ffind(&old_lines) {
                    self.html.replace_range(p..p + old_lines.len(), &apply_lines);
                }
                self.out.replacements_made += 1;
                continue;
            }

            // ---- no match: name-based fallback for passage-tag anchors ----
            if let Some(pn) = &passage_name_m {
                let pname = pn.0.clone();
                if let Some(sp) = find_passage(&self.html, &pname) {
                    let apply_lines = resolve_auto_passage_attrs(&new_lines, &self.html, &pname);
                    if apply_lines.contains(PASSAGE_CLOSE) {
                        self.html.replace_range(sp.open_start..sp.close_end, &apply_lines);
                        self.out.log.push(format!("Replaced passage '{pname}' by name (full replacement)"));
                    } else {
                        self.html.replace_range(sp.open_start..sp.body_start, py_strip(&apply_lines));
                        self.out.log.push(format!("Replaced opening tag of '{pname}' by name (tag-only update)"));
                    }
                    self.out.replacements_made += 1;
                    continue;
                }
            }
            let msg = format!("No match found for '{old_lines}'");
            self.out.log.push(msg.clone());
            self.out.fail_log.push(msg);
            self.failed_keys.push(old_lines.clone());
            self.out.replacements_failed += 1;
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Legacy phase: `~` / `~~` blocks, applied first, every match replaced
    // ---------------------------------------------------------------------
    /// The legacy `~` / `~~` blocks (collected by `LoadOptions::legacy_replace_all`), in load order, each one
    /// replacing every occurrence of its anchor in the whole file, like KittyPatcher 0.1.5c did. The anchor and
    /// replacement are handled like a classic `Replace:` (escaping, `[to]`, raw script-block fallback); the
    /// context (passage text or script block) of the first match decides how the replacement is escaped.
    /// A block without a match is a failure, but the others still run.
    fn phase0_legacy(&mut self) -> Result<()> {
        let st = self.st;
        let total = st.legacy_list.len();
        for (n, entry) in st.legacy_list.iter().enumerate() {
            if n > 0 && n % 250 == 0 {
                self.report(&format!("Applying legacy blocks: {n}/{total}"));
            }
            let old_expanded = update_old_lines_from_html(&entry.old, &self.html);
            let mut old_lines = resolve_pid_auto_in_anchor(&escape_twine_tags(&old_expanded), &self.html);
            let old_raw = resolve_pid_auto_in_anchor(&old_expanded, &self.html);
            check_literal_anchor(&old_lines)?;
            check_literal_anchor(&old_raw)?;

            let mut first = self.html.ffind(&old_lines);
            if first.is_none() && old_raw != old_lines {
                if let Some(p) = self.html.ffind(&old_raw) {
                    old_lines = old_raw;
                    first = Some(p);
                }
            }
            let Some(start) = first else {
                // name the earlier legacy blocks that already rewrote part of this text (replace-all side effect)
                let culprits: Vec<String> = st.legacy_list[..n]
                    .iter()
                    .filter(|e| e.old.len() >= 8 && e.old != entry.old && old_lines.contains(e.old.as_str()))
                    .take(3)
                    .map(|e| format!("'{}' ({})", short(&e.old, 70).replace('\n', " "), e.mod_file))
                    .collect();
                let mut msg = format!("No match found for '{old_lines}'");
                if !culprits.is_empty() {
                    msg.push_str(&format!(
                        "
  (an earlier legacy block replaced text inside this anchor in every place it occurs: {})",
                        culprits.join(", ")
                    ));
                }
                self.out.log.push(msg.clone());
                self.out.fail_log.push(msg);
                self.out.replacements_failed += 1;
                continue;
            };
            let ctx = context_at(&self.html, start);

            let mut apply_lines = entry.new.clone();
            if apply_lines.contains("pid=\"auto\"") || apply_lines.contains("pid=\"AUTO\"") {
                apply_lines = resolve_auto_pids(&apply_lines, &mut self.next_pid);
            }
            if ctx == Ctx::Script && !old_lines.contains("&lt;") && !old_lines.contains("&gt;") {
                apply_lines = unescape_for_raw(&apply_lines);
            }
            if ctx == Ctx::Passage && (apply_lines.contains("<<") || apply_lines.contains(">>")) {
                apply_lines = escape_twine_tags(&apply_lines);
            }
            let count = replace_all_in_place(&mut self.html, &old_lines, &apply_lines);
            self.out.log.push(format!(
                "Legacy: replaced {count} occurrence(s) of '{}' ({})",
                short(&old_lines, 80).replace('\n', " "),
                entry.mod_file
            ));
            self.out.replacements_made += 1;
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // ReplaceReg
    // ---------------------------------------------------------------------
    fn phase_replace_reg(&mut self) -> Result<()> {
        let st = self.st;
        for entry in &st.mod_reg_list {
            let key = format!("ReplaceReg [{}]", entry.pattern);
            if !self.guards_ok(&entry.guards)? {
                self.out.log.push(format!("Guard condition not met for '{key}', skipping."));
                continue;
            }
            let re = match Regex::new(&format!("(?s){}", entry.pattern)) {
                Ok(r) => r,
                Err(e) => {
                    self.fail(format!("Invalid regex in '{key}': {e}"), true);
                    continue;
                }
            };
            let Some(caps) = re.captures(&self.html) else {
                self.fail(format!("No match found for '{key}'"), true);
                continue;
            };
            let mut ops: Vec<(usize, usize, String)> = Vec::new();
            let mut valid = true;
            for (gidx, text) in &entry.groups {
                match caps.get(*gidx as usize) {
                    Some(m) => ops.push((m.start(), m.end(), text.clone())),
                    None => {
                        self.fail(format!("Group {gidx} does not exist in pattern '{}'", entry.pattern), true);
                        valid = false;
                        break;
                    }
                }
            }
            if !valid {
                continue;
            }
            let already = |h: &str, gs: usize, ge: usize, rt: &str| -> bool {
                let n = rt.chars().count();
                // window [gstart - n chars, gend + n chars]
                let start = back_chars(h, gs, n);
                let end = ge + window(h, ge, n).len();
                h[start..end].contains(rt)
            };
            if ops.iter().all(|(gs, ge, rt)| already(&self.html, *gs, *ge, rt)) {
                self.out.log.push("Patch already applied, skipping.".to_string());
                continue;
            }
            ops.sort_by(|a, b| b.0.cmp(&a.0));
            for (gs, ge, rt) in ops {
                self.html.replace_range(gs..ge, &rt);
            }
            self.out.replacements_made += 1;
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Phase 4: function / passage scoped directives
    // ---------------------------------------------------------------------
    fn phase4(&mut self) -> Result<()> {
        let st = self.st;
        let mut funcs: Vec<&FuncEntry> = st.mod_func_list.iter().collect();
        funcs.sort_by_key(|e| match e.kind.as_str() {
            "add_func" | "clone_passage" => 0,
            "insert_into_func" => 1,
            "replace_in_func" | "replace_func" => 2,
            "wrap_passage" | "replace_in_all_passages" | "add_storyvar" => 3,
            _ => 0,
        });
        for e in &funcs {
            match e.kind.as_str() {
                "add_func" | "insert_into_func" | "replace_in_func" | "replace_in_passage" | "append_to_passage" | "prepend_to_passage"
                | "insert_before" | "insert_after" | "replace_in_all_passages" | "add_storyvar" | "clone_passage" | "replace_func" | "replace_func_sig" | "delete_block" | "delete_span"
                | "delete_in_passage" | "add_tag_to_passage" | "remove_tag_from_passage" | "rename_passage" | "add_variable" => {}
                k => bail!("{}: directive kind '{k}' is not supported by the builtin engine yet", e.mod_file),
            }
        }

        // function-name index of the script block (for Add Function)
        let fname_re = Regex::new(r"(?m)(?:^|[\s;])([\w$.]+)\s*[=:]\s*function|function\s+([\w$.]+)\s*\(").unwrap();
        let mut func_names: HashSet<String> = HashSet::new();
        for c in fname_re.captures_iter(&self.snap.script_block) {
            if let Some(m) = c.get(1).or_else(|| c.get(2)) {
                func_names.insert(m.as_str().to_string());
            }
        }
        let script_anchor_found = self.html.contains(SCRIPT_ANCHOR);

        // ---- batch Append / Prepend To Passage ----
        let mut atp: IndexMap<&str, Vec<&FuncEntry>> = IndexMap::new();
        for e in &funcs {
            if e.kind == "append_to_passage" || e.kind == "prepend_to_passage" {
                atp.entry(e.passage_name.as_deref().unwrap_or("")).or_default().push(e);
            }
        }
        for (pname, entries) in &atp {
            match find_passage(&self.html, pname) {
                None => {
                    let reg_idx = self.names_seen.get(*pname).copied();
                    if let Some(idx) = reg_idx {
                        let mut rb = self.registry[idx].body.clone();
                        for e in entries {
                            if !self.guards_ok(&e.guards)? {
                                self.out.log.push(format!("Guard condition not met for 'Append To Passage [{pname}]', skipping."));
                                continue;
                            }
                            let body = e.body.as_deref().unwrap_or("");
                            let is_pre = e.kind == "prepend_to_passage";
                            if rb.contains(py_strip(body)) {
                                self.out.log.push(format!("Append To Passage [{pname}]: already applied (registry), skipping."));
                                continue;
                            }
                            rb = if is_pre { format!("{body}\n{rb}") } else { format!("{rb}\n{body}\n") };
                            self.out.replacements_made += 1;
                        }
                        self.registry[idx].body = rb;
                    } else {
                        for e in entries {
                            if !self.guards_ok(&e.guards)? {
                                continue;
                            }
                            self.fail(format!("Append To Passage: passage '{pname}' not found"), true);
                        }
                    }
                }
                Some(sp) => {
                    let cur = self.html[sp.body_start..sp.body_end].to_string();
                    let mut newb = cur.clone();
                    for e in entries {
                        if !self.guards_ok(&e.guards)? {
                            self.out.log.push(format!("Guard condition not met for 'Append To Passage [{pname}]', skipping."));
                            continue;
                        }
                        let body = e.body.as_deref().unwrap_or("");
                        let is_pre = e.kind == "prepend_to_passage";
                        if newb.contains(py_strip(body)) {
                            self.out.log.push(format!("Append To Passage [{pname}]: already applied, skipping."));
                            continue;
                        }
                        newb = if is_pre { format!("{body}\n{newb}") } else { format!("{newb}\n{body}\n") };
                        self.out.replacements_made += 1;
                    }
                    if newb != cur {
                        self.html.replace_range(sp.body_start..sp.body_end, &newb);
                    }
                }
            }
        }

        // ---- batch Insert After ----
        let mut ia: IndexMap<&str, Vec<&FuncEntry>> = IndexMap::new();
        for e in &funcs {
            if e.kind != "insert_after" {
                continue;
            }
            if e.once == Some(true) && self.once.contains(&e.name) {
                self.out.log.push(format!("Insert After [ONCE]: '{}' already injected, skipping.", e.name));
                continue;
            }
            ia.entry(e.name.as_str()).or_default().push(e);
        }
        for (anchor, entries) in &ia {
            let norm = escape_twine_tags(anchor);
            let mut resolved = resolve_pid_auto_in_anchor(&norm, &self.html);
            let mut idx = self.html.ffind(&resolved);
            if idx.is_none() && resolved != norm {
                idx = self.html.ffind(&norm);
                resolved = norm.clone();
            }
            let Some(idx) = idx else {
                for e in entries {
                    if !self.guards_ok(&e.guards)? {
                        self.out.log.push(format!("Guard condition not met for 'Insert After [{anchor}]', skipping."));
                        continue;
                    }
                    self.record_failed_insert("insert_after", anchor, e);
                }
                continue;
            };
            let insert_at = idx + resolved.len();
            let in_script = is_in_script_block(&self.html, insert_at);
            let mut combined = String::new();
            for e in entries {
                if !self.guards_ok(&e.guards)? {
                    self.out.log.push(format!("Guard condition not met for 'Insert After [{anchor}]', skipping."));
                    continue;
                }
                let mut body = e.body.clone().unwrap_or_default();
                if !in_script {
                    body = escape_twine_tags(&body);
                }
                let n = body.chars().count() + 2;
                if window(&self.html, insert_at, n).contains(&body) || combined.contains(&body) {
                    self.out.log.push(format!("Insert After: already applied for '{anchor}', skipping."));
                    continue;
                }
                combined.push('\n');
                combined.push_str(&body);
                if e.once == Some(true) {
                    self.once.insert(anchor.to_string());
                }
                self.out.replacements_made += 1;
            }
            if !combined.is_empty() {
                self.html.insert_str(insert_at, &combined);
            }
        }

        // ---- main loop ----
        for e in &funcs {
            match e.kind.as_str() {
                "append_to_passage" | "prepend_to_passage" | "insert_after" => continue,
                "insert_before" => self.insert_before(e)?,
                "replace_in_passage" => self.replace_in_passage(e)?,
                "add_func" => self.add_func(e, &mut func_names, script_anchor_found)?,
                "insert_into_func" => self.insert_into_func(e)?,
                "replace_in_func" => self.replace_in_func(e)?,
                "replace_func" => self.replace_func(e)?,
                "replace_func_sig" => self.replace_func_sig(e)?,
                "delete_block" => self.delete_block(e)?,
                "delete_span" => self.delete_span(e)?,
                "delete_in_passage" => self.delete_in_passage(e)?,
                "add_tag_to_passage" => self.tag_op(e, true)?,
                "remove_tag_from_passage" => self.tag_op(e, false)?,
                "rename_passage" => self.rename_passage(e)?,
                "add_variable" => self.add_variable(e)?,
                "replace_in_all_passages" => self.replace_in_all_passages(e)?,
                "add_storyvar" => self.add_storyvar(e)?,
                "clone_passage" => self.clone_passage(e)?,
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    fn insert_before(&mut self, e: &FuncEntry) -> Result<()> {
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Insert Before [{}]', skipping.", e.name));
            return Ok(());
        }
        if e.once == Some(true) && self.once.contains(&e.name) {
            self.out.log.push(format!("Insert Before [ONCE]: anchor '{}' already injected, skipping.", e.name));
            return Ok(());
        }
        let norm = escape_twine_tags(&e.name);
        let resolved = resolve_pid_auto_in_anchor(&norm, &self.html);
        let mut idx = self.html.ffind(&resolved);
        if idx.is_none() && resolved != norm {
            idx = self.html.ffind(&norm);
        }
        let Some(idx) = idx else {
            self.record_failed_insert("insert_before", &e.name, e);
            return Ok(());
        };
        let mut body = e.body.clone().unwrap_or_default();
        if !is_in_script_block(&self.html, idx) {
            body = escape_twine_tags(&body);
        }
        // html[max(0, idx - len(body) - 2) : idx + 2]
        let n = body.chars().count() + 2;
        let start = back_chars(&self.html, idx, n);
        let end = idx + window(&self.html, idx, 2).len();
        if self.html[start..end].contains(&body) {
            self.out.log.push(format!("Insert Before: already applied for '{}', skipping.", e.name));
            return Ok(());
        }
        self.html.insert_str(idx, &format!("{body}\n"));
        if e.once == Some(true) {
            self.once.insert(e.name.clone());
        }
        self.out.replacements_made += 1;
        Ok(())
    }

    fn replace_in_passage(&mut self, e: &FuncEntry) -> Result<()> {
        let pname = e.passage_name.clone().unwrap_or_default();
        let anchor = escape_twine_tags(&e.name);
        let new_body = e.body.clone().unwrap_or_default();
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Replace In Passage [{pname}]', skipping."));
            return Ok(());
        }
        check_literal_anchor(&anchor)?;

        match find_passage(&self.html, &pname) {
            Some(sp) => {
                let passage_body = &self.html[sp.body_start..sp.body_end];
                let Some(rel) = passage_body.find(&anchor) else {
                    self.out.log.push(format!("Replace In Passage [{pname}]: anchor not found in passage body -- falling back to full-HTML Replace:"));
                    return self.rip_full_html_fallback(&pname, &anchor, &new_body, false);
                };
                if passage_body.contains(&new_body) {
                    self.out.log.push(format!("Replace In Passage [{pname}]: already applied, skipping."));
                    return Ok(());
                }
                let abs = sp.body_start + rel;
                self.html.replace_range(abs..abs + anchor.len(), &new_body);
                self.out.log.push(format!("Replace In Passage [{pname}]: replaced '{}'", short(&anchor, 60)));
                self.out.replacements_made += 1;
                Ok(())
            }
            None => {
                if let Some(&idx) = self.names_seen.get(&pname) {
                    let reg_body = self.registry[idx].body.clone();
                    let raw_anchor = html_unescape_basic(&anchor);
                    let raw_new = html_unescape_basic(&new_body);
                    let hit = if reg_body.contains(&anchor) {
                        Some((anchor.clone(), new_body.clone(), "encoded"))
                    } else if reg_body.contains(&raw_anchor) {
                        Some((raw_anchor.clone(), raw_new.clone(), "unescaped"))
                    } else {
                        None
                    };
                    if let Some((a, n, kind)) = hit {
                        self.registry[idx].body = reg_body.replacen(&a, &n, 1);
                        self.out.log.push(format!("Replace In Passage [{pname}]: passage found in registry (Phase 7 pending) -- applied replacement to registry body ({kind})"));
                        self.out.replacements_made += 1;
                        return Ok(());
                    }
                }
                self.out.log.push(format!("Replace In Passage [{pname}]: passage not found -- falling back to full-HTML Replace:"));
                self.rip_full_html_fallback(&pname, &anchor, &new_body, true)
            }
        }
    }

    fn rip_full_html_fallback(&mut self, pname: &str, anchor: &str, new_body: &str, passage_missing: bool) -> Result<()> {
        let Some(idx) = self.html.ffind(anchor) else {
            self.fail(format!("Replace In Passage [{pname}]: no match in full HTML either"), false);
            return Ok(());
        };
        let n = anchor.chars().count() + new_body.chars().count() + 10;
        if window(&self.html, idx, n).contains(new_body) {
            let how = if passage_missing { "full HTML fallback" } else { "full HTML" };
            self.out.log.push(format!("Replace In Passage [{pname}]: already applied ({how}), skipping."));
            return Ok(());
        }
        self.html.replace_range(idx..idx + anchor.len(), new_body);
        self.out.log.push(format!("Replace In Passage [{pname}]: applied via full-HTML fallback"));
        self.out.replacements_made += 1;
        Ok(())
    }

    fn add_func(&mut self, e: &FuncEntry, names: &mut HashSet<String>, anchor_found: bool) -> Result<()> {
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Add Function [{}]', skipping.", e.name));
            return Ok(());
        }
        if !anchor_found {
            self.fail(format!("Add Function: script anchor not found for '{}'", e.name), true);
            return Ok(());
        }
        if names.contains(&e.name) {
            self.out.log.push(format!("Add Function: '{}' already exists, skipping.", e.name));
            return Ok(());
        }
        let Some(idx) = self.html.ffind(SCRIPT_ANCHOR) else {
            self.fail(format!("Add Function: script anchor lost after prior injection for '{}'", e.name), false);
            return Ok(());
        };
        let body = e.body.clone().unwrap_or_default();
        self.html.insert_str(idx, &format!("\n\n{body}\n"));
        let end = self.html.ffind(SCRIPT_ANCHOR).unwrap_or(0);
        self.snap.script_block = self.html[..end].to_string();
        names.insert(e.name.clone());
        self.out.log.push(format!("Add Function: injected '{}'", e.name));
        self.out.replacements_made += 1;
        Ok(())
    }

    /// Locate `func_name`'s body: `(open brace, close brace)` byte offsets.
    fn locate_function(&mut self, func_name: &str, what: &str) -> Option<(usize, usize)> {
        let name = regex::escape(func_name);
        let re = Regex::new(&format!(r"(?s)({name}\s*[=:]\s*function\s*\(.*?\)\s*\{{|function\s+{name}\s*\(.*?\)\s*\{{)")).unwrap();
        let Some(fm) = re.find(&self.html) else {
            self.fail(format!("{what}: function '{func_name}' not found"), true);
            return None;
        };
        let fb_open = fm.end() - 1;
        match bracket_match(&self.html, fb_open, b'{', b'}') {
            Some(c) => Some((fb_open, c)),
            None => {
                self.out.log.push(format!("{what}: could not find body end for '{func_name}'"));
                self.out.replacements_failed += 1;
                None
            }
        }
    }

    fn insert_into_func(&mut self, e: &FuncEntry) -> Result<()> {
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Insert Into Function [{}]', skipping.", e.name));
            return Ok(());
        }
        let anchor_raw = e.anchor.clone().unwrap_or_default();
        let body_raw = e.body.clone().unwrap_or_default();
        let Some((open, close)) = self.locate_function(&e.name, "Insert Into Function") else { return Ok(()) };
        let func_body = &self.html[open..=close];
        let Some(ai) = func_body.find(&anchor_raw) else {
            self.fail(format!("Insert Into Function: anchor not found in '{}': '{}'", e.name, short(&anchor_raw, 60)), true);
            return Ok(());
        };
        if func_body.contains(&body_raw) {
            self.out.log.push(format!("Insert Into Function: already applied in '{}', skipping.", e.name));
            return Ok(());
        }
        let at = open + ai + anchor_raw.len();
        self.html.insert_str(at, &format!("\n{body_raw}"));
        self.out.replacements_made += 1;
        Ok(())
    }

    fn replace_in_func(&mut self, e: &FuncEntry) -> Result<()> {
        if !self.guards_ok(&e.guards)? {
            self.out.log.push(format!("Guard condition not met for 'Replace In Function [{}]', skipping.", e.name));
            return Ok(());
        }
        let anchor_raw = e.anchor.clone().unwrap_or_default();
        let body_raw = e.body.clone().unwrap_or_default();
        let Some((open, close)) = self.locate_function(&e.name, "Replace In Function") else { return Ok(()) };
        let func_body = &self.html[open..=close];
        let Some(ai) = func_body.find(&anchor_raw) else {
            self.fail(format!("Replace In Function: anchor not found in '{}': '{}'", e.name, short(&anchor_raw, 60)), true);
            return Ok(());
        };
        if func_body.contains(&body_raw) {
            self.out.log.push(format!("Replace In Function: already applied in '{}', skipping.", e.name));
            return Ok(());
        }
        let start = open + ai;
        self.html.replace_range(start..start + anchor_raw.len(), &body_raw);
        self.out.log.push(format!("Replace In Function: replaced '{}' in '{}'", short(&anchor_raw, 40), e.name));
        self.out.replacements_made += 1;
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Phase 6 sweep and Phase 7 injection
    // ---------------------------------------------------------------------
    /// A missing Insert anchor: counted as a failure unless the directive is `[soft]`; the entry is kept
    /// for the registry sweep.
    fn record_failed_insert(&mut self, kind: &str, anchor: &str, e: &FuncEntry) {
        let label = if kind == "insert_after" { "Insert After" } else { "Insert Before" };
        let msg = format!("{label}: no match for '{anchor}'");
        let soft = e.soft == Some(true);
        if soft {
            self.out.log.push(format!("[soft] {msg}"));
        } else {
            self.out.log.push(msg.clone());
            self.out.fail_log.push(msg);
            self.out.replacements_failed += 1;
        }
        self.failed_ia_ib.push((kind.to_string(), anchor.to_string(), e.body.clone().unwrap_or_default(), soft));
    }

    /// Failed Insert After/Before entries against Add Passage bodies (passages that are not in the HTML yet).
    fn sweep_failed_inserts(&mut self) {
        if self.registry.is_empty() || self.failed_ia_ib.is_empty() {
            return;
        }
        let entries = self.failed_ia_ib.clone();
        let mut resolved: Vec<(String, String)> = Vec::new(); // (kind, anchor)
        for (kind, anchor, body, _soft) in &entries {
            let unescaped = unescape_for_raw(anchor);
            for (ri, rec) in self.registry.iter_mut().enumerate() {
                if self.early_injected.contains(&ri) {
                    continue;
                }
                let psg_body = rec.body.clone();
                let (matched, inject) = if psg_body.contains(anchor.as_str()) {
                    (Some(anchor.clone()), body.clone())
                } else if unescaped != *anchor && psg_body.contains(&unescaped) {
                    (Some(unescaped.clone()), unescape_for_raw(body))
                } else {
                    (None, String::new())
                };
                let Some(m) = matched else { continue };
                let pos = psg_body.find(&m).unwrap();
                if kind == "insert_after" {
                    let check_start = pos + m.len();
                    let n = inject.chars().count() + 2;
                    if window(&psg_body, check_start, n).contains(&inject) {
                        self.out.log.push(format!("Registry IA/IB sweep: already applied '{}' in '{}', skipping.", short(anchor, 50), rec.name));
                        break;
                    }
                    rec.body = format!("{}\n{}{}", &psg_body[..check_start], inject, &psg_body[check_start..]);
                } else {
                    let n = inject.chars().count() + 2;
                    let start = back_chars(&psg_body, pos, n);
                    let end = pos + window(&psg_body, pos, 2).len();
                    if psg_body[start..end].contains(&inject) {
                        self.out.log.push(format!("Registry IA/IB sweep: already applied '{}' in '{}', skipping.", short(anchor, 50), rec.name));
                        break;
                    }
                    rec.body = format!("{}{}\n{}", &psg_body[..pos], inject, &psg_body[pos..]);
                }
                self.out.log.push(format!("Registry IA/IB sweep: applied {} '{}' into passage '{}'", if kind == "insert_after" { "Insert After" } else { "Insert Before" }, short(anchor, 50), rec.name));
                resolved.push((kind.clone(), anchor.clone()));
                break;
            }
        }
        if resolved.is_empty() {
            return;
        }
        self.out.replacements_failed = self.out.replacements_failed.saturating_sub(resolved.len());
        for (kind, anchor) in &resolved {
            let label = if kind == "insert_after" { "Insert After" } else { "Insert Before" };
            let line = format!("{label}: no match for '{anchor}'");
            if let Some(i) = self.out.fail_log.iter().position(|l| *l == line) {
                self.out.fail_log.remove(i);
            }
        }
    }

    fn phase6_sweep(&mut self) {
        if self.registry.is_empty() || self.failed_keys.is_empty() {
            return;
        }
        let st = self.st;
        let rank: std::collections::HashMap<&String, usize> = st.mod_dict.keys().enumerate().map(|(i, k)| (k, i)).collect();
        let mut keys = self.failed_keys.clone();
        keys.sort_by_key(|k| rank.get(k).copied().unwrap_or(usize::MAX));
        let mut resolved: Vec<String> = Vec::new();
        for search_key in &keys {
            let Some(entry) = st.mod_dict.get(search_key) else { continue };
            let unescaped_key = unescape_for_raw(search_key);
            for (ri, rec) in self.registry.iter_mut().enumerate() {
                if self.early_injected.contains(&ri) {
                    continue;
                }
                let (matched, inject) = if rec.body.contains(search_key.as_str()) {
                    (Some(search_key.clone()), entry.0.clone())
                } else if unescaped_key != *search_key && rec.body.contains(&unescaped_key) {
                    (Some(unescaped_key.clone()), unescape_for_raw(&entry.0))
                } else {
                    (None, String::new())
                };
                if let Some(m) = matched {
                    rec.body = rec.body.replacen(&m, &inject, 1);
                    self.out.log.push(format!("Passage registry inner replacement: applied '{}' into passage '{}' body", short(search_key, 60), rec.name));
                    resolved.push(search_key.clone());
                    break;
                }
            }
        }
        self.out.replacements_failed = self.out.replacements_failed.saturating_sub(resolved.len());
        let resolved_set: HashSet<String> = resolved.iter().map(|k| format!("No match found for '{k}'")).collect();
        self.out.fail_log.retain(|l| !resolved_set.contains(l));
    }

    fn inject_before(&mut self, anchor: &str, regs: &[BodyRecord], label: &str) -> Result<()> {
        if regs.is_empty() {
            return Ok(());
        }
        let mut parts: Vec<&str> = Vec::new();
        for r in regs {
            if !self.guards_ok(&r.guards)? {
                self.out.log.push(format!("{label}: skipped block from {} -- guard condition not met", r.mod_file));
                continue;
            }
            parts.push(&r.body);
        }
        if parts.is_empty() {
            return Ok(());
        }
        let Some(idx) = self.html.ffind(anchor) else {
            self.out.log.push(format!("{label}: anchor not found in HTML"));
            return Ok(());
        };
        let block = format!("\n{}\n", parts.join("\n"));
        self.html.insert_str(idx, &block);
        self.out.replacements_made += 1;
        Ok(())
    }

    fn inject_events(&mut self) -> Result<()> {
        let st = self.st;
        if st.events_registry.is_empty() {
            return Ok(());
        }
        let anchor = "setup.Events.db =\n[";
        let mut parts: Vec<&str> = Vec::new();
        for r in &st.events_registry {
            if !self.guards_ok(&r.guards)? {
                continue;
            }
            parts.push(&r.body);
        }
        if parts.is_empty() {
            return Ok(());
        }
        let Some(idx) = self.html.ffind(anchor) else {
            self.out.log.push("Add Events: events anchor not found in HTML".to_string());
            return Ok(());
        };
        let block = format!("\n{}", parts.join("\n"));
        self.html.insert_str(idx + anchor.len(), &block);
        self.out.replacements_made += 1;
        Ok(())
    }

    fn passage_guards_ok(&self, rec: &PassageRecord) -> Result<bool> {
        match &rec.guards {
            PassageGuards::Full(g) => self.guards_ok(g),
            PassageGuards::Four(g) => {
                let full = Guards {
                    if_exists: g.if_exists.clone(),
                    if_not_exists: g.if_not_exists.clone(),
                    if_reg_exists: g.if_reg_exists.clone(),
                    if_reg_not_exists: g.if_reg_not_exists.clone(),
                    ..Default::default()
                };
                self.guards_ok(&full)
            }
        }
    }
}

/// Run the apply phase over `input` with the parsed registries. `loaded_mods` are the mod file
/// names in load order (for `IfModLoaded` guards).
pub fn patch(input: &str, st: &ParseState, loaded_mods: &[String]) -> Result<Outcome> {
    patch_with_progress(input, st, loaded_mods, &mut |_| {})
}

/// Like [`patch`], reporting coarse progress ("Applying replacements: 500/1392") through `progress`.
pub fn patch_with_progress(input: &str, st: &ParseState, loaded_mods: &[String], progress: &mut dyn FnMut(&str)) -> Result<Outcome> {
    if let Some((m, tok)) = st.unsupported.first() {
        bail!("{m}: directive '{tok}' is not supported by the builtin engine yet");
    }
    let html = input.replace("\r\n", "\n").replace('\r', "\n");
    let snap = split_html(&html);
    let first_next_pid = snap.max_pid + 1;
    let original_names: HashSet<String> = snap.passage_dict.keys().cloned().collect();

    let mut p = P {
        st,
        loaded: loaded_mods.iter().map(|m| m.to_lowercase()).collect(),
        html,
        snap,
        registry: st.passage_registry.clone(),
        names_seen: st.passage_names_seen.clone(),
        out: Outcome::default(),
        once: HashSet::new(),
        failed_keys: Vec::new(),
        failed_ia_ib: Vec::new(),
        next_pid: first_next_pid,
        js_extra: Vec::new(),
        progress: Some(progress),
        original_names,
        early_injected: HashSet::new(),
    };

    if !st.legacy_list.is_empty() {
        p.report("Applying legacy blocks");
        p.phase0_legacy()?;
        // the passage snapshot used by phase 1 must see the text the legacy phase produced
        p.snap = split_html(&p.html);
    }
    p.report("Applying replacements");
    p.phase1()?;
    p.report("Applying passage and function directives");
    p.snap = split_html(&p.html);
    p.phase_replace_reg()?;
    p.structs()?;
    p.phase4()?;
    p.snap = split_html(&p.html);
    p.hooks()?;
    p.phase6_sweep();
    p.sweep_failed_inserts();

    p.report("Adding passages, scripts and styles");
    // ---- Phase 7: registry injection ----
    let mut js_all = st.js_registry.clone();
    js_all.extend(p.js_extra.clone());
    p.inject_before(SCRIPT_ANCHOR, &js_all, "Add Javascript")?;
    p.inject_before(SCRIPT_MARK, &st.css_registry, "Add CSS")?;
    p.inject_events()?;
    // refresh the script block before the passage guards (Python does this too)
    p.snap = split_html(&p.html);

    if !p.registry.is_empty() {
        let mut parts: Vec<String> = Vec::new();
        let mut injected = 0usize;
        let registry = p.registry.clone();
        for (ri, rec) in registry.iter().enumerate() {
            if p.early_injected.contains(&ri) {
                p.out.log.push(format!("Add Passage: skipped '{}' -- already early-injected by Delete Block", rec.name));
                continue;
            }
            if !p.passage_guards_ok(rec)? {
                p.out.log.push(format!("Add Passage: skipped '{}' -- guard condition not met", rec.name));
                continue;
            }
            if p.original_names.contains(&rec.name) {
                p.out.log.push(format!("Add Passage: skipped '{}' -- already exists in vanilla HTML", rec.name));
                continue;
            }
            if p.snap.passage_dict.contains_key(&rec.name) {
                p.out.log.push(format!("Add Passage: skipped '{}' -- already injected via accumulator path", rec.name));
                continue;
            }
            let body = escape_html_between_tags(&rec.body);
            let body = escape_twine_tags(&body);
            let body = auto_escape_passage_bodies(&body);
            parts.push(format!(
                "<tw-passagedata pid=\"auto\" name=\"{}\" tags=\"{}\" position=\"100,100\" size=\"100,100\">{}</tw-passagedata>",
                rec.name, rec.tags, body
            ));
            injected += 1;
        }
        if !parts.is_empty() {
            let block = format!("\n{}\n", parts.join("\n"));
            match p.html.ffind("</tw-storydata>") {
                Some(idx) => {
                    p.html.insert_str(idx, &block);
                    p.out.replacements_made += 1;
                }
                None => p.out.log.push("Add Passage: </tw-storydata> anchor not found in HTML".to_string()),
            }
        }
        p.out.log.push(format!("Add Passage: injected {injected} passages"));
    }

    // ---- conflict report: several blocks aimed at the same target make the run a failure ----
    for (target, entries) in &st.conflict_map {
        if entries.len() > 1 {
            let kinds: Vec<String> = entries.iter().map(|(m, k)| format!("\t{m} ({k})")).collect();
            let msg = format!("CONFLICT DETECTED: Multiple mods target '{target}':\n{}", kinds.join("\n"));
            p.out.log.push(msg.clone());
            p.out.fail_log.push(msg);
        }
    }

    p.report("Renumbering passage ids");
    // ---- pid renumbering ----
    let (renumbered, pidless) = renormalize_pids(&p.html);
    p.html = renumbered;
    if pidless > 0 {
        p.out.log.push(format!("PID WARNING: {pidless} passage(s) had no pid= attribute -- assigned placeholder pid=\"0\" before renormalization."));
    }
    let mut out = p.out;
    out.html = p.html;
    Ok(out)
}
