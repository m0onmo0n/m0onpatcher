//! Guard clauses (`IfExists [..]:` and friends): parsing, and the hoisting
//! of guard lines that precede a directive into the directive's body.

use crate::pytext::{py_strip, rstrip_crlf, splitlines_keepends};
use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;

/// The 13 guard lists, in the same order as the Python dict.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Guards {
    pub if_exists: Vec<String>,
    pub if_not_exists: Vec<String>,
    pub if_reg_exists: Vec<String>,
    pub if_reg_not_exists: Vec<String>,
    pub if_mod_loaded: Vec<String>,
    pub if_mod_not_loaded: Vec<String>,
    pub if_passage_exists: Vec<String>,
    pub if_passage_not_exists: Vec<String>,
    pub if_function_exists: Vec<String>,
    pub if_function_not_exists: Vec<String>,
    pub if_passage_contains: Vec<String>,
    pub if_passage_has_tag: Vec<String>,
    pub if_version_at_least: Vec<String>,
}

/// The 4-key guard dict the Python multi-passage split writes (a quirk we
/// keep so the dump compares equal).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Guards4 {
    pub if_exists: Vec<String>,
    pub if_not_exists: Vec<String>,
    pub if_reg_exists: Vec<String>,
    pub if_reg_not_exists: Vec<String>,
}

static GUARD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^(?P<kind>IfExists|IfNotExists|IfRegExists|IfRegNotExists|IfModLoaded|IfModNotLoaded|IfPassageExists|IfPassageNotExists|IfFunctionExists|IfFunctionNotExists|IfPassageContains|IfPassageHasTag|IfVersionAtLeast)\s*\[(?P<value>[^\]]*)\]:\s*$",
    )
    .unwrap()
});

fn unescape_guard_value(v: &str) -> String {
    let chars: Vec<char> = v.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() && (chars[i + 1] == ']' || chars[i + 1] == '\\') {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `_parse_guards`: strip guard lines from `text`, return the stripped
/// remainder and the collected guards.
pub fn parse_guards(text: &str) -> (String, Guards) {
    let mut g = Guards::default();
    let mut cleaned = String::with_capacity(text.len());
    let mut last = 0usize;
    for caps in GUARD_RE.captures_iter(text) {
        let m = caps.get(0).unwrap();
        cleaned.push_str(&text[last..m.start()]);
        last = m.end();
        let kind = &caps["kind"];
        let mut val = caps["value"].to_string();
        if kind == "IfPassageContains" || kind == "IfPassageHasTag" {
            val = unescape_guard_value(&val);
        }
        match kind {
            "IfExists" => g.if_exists.push(val),
            "IfNotExists" => g.if_not_exists.push(val),
            "IfRegExists" => g.if_reg_exists.push(val),
            "IfRegNotExists" => g.if_reg_not_exists.push(val),
            "IfModLoaded" => g.if_mod_loaded.push(val),
            "IfModNotLoaded" => g.if_mod_not_loaded.push(val),
            "IfPassageExists" => g.if_passage_exists.push(val),
            "IfPassageNotExists" => g.if_passage_not_exists.push(val),
            "IfFunctionExists" => g.if_function_exists.push(val),
            "IfFunctionNotExists" => g.if_function_not_exists.push(val),
            "IfPassageContains" => g.if_passage_contains.push(val),
            "IfPassageHasTag" => g.if_passage_has_tag.push(val),
            "IfVersionAtLeast" => g.if_version_at_least.push(val),
            _ => unreachable!(),
        }
    }
    cleaned.push_str(&text[last..]);
    (py_strip(&cleaned).to_string(), g)
}

/// The directive tokens that start a block at column 0 as the Python patcher
/// effectively sees them. Note: v2 rebinds `_ALL_DIRECTIVE_TOKENS` to this
/// shorter list (no `Merge Into Object`, `Clone Passage`, `Wrap Passage`,
/// `Replace In All Passages`, `Add StoryVar`) because `KittyDiffAssist` is not
/// installed. Verified by importing the module (2026-11-16).
pub const ALL_DIRECTIVE_TOKENS: [&str; 30] = [
    "Replace:",
    "ReplaceReg [",
    "Add Passage:",
    "Add Javascript:",
    "Add CSS:",
    "Add Events:",
    "Add Function [",
    "Add Variable [",
    "Append Array [",
    "Append Variable to Class [",
    "Append Function to Class [",
    "Replace Function [",
    "Replace Function Signature [",
    "Insert Before [",
    "Insert After [",
    "Insert Into Function [",
    "Delete Block [",
    "Rename Passage [",
    "Hook [",
    "Insert Into Array [",
    "Insert Into Object [",
    "Replace In Passage [",
    "Delete Span [",
    "Prepend To Passage [",
    "Append To Passage [",
    "Add Tag To Passage [",
    "Remove Tag From Passage [",
    "Replace In Function [",
    "Delete In Passage [",
    "Move Passage [",
];

const HAS_ANCHOR_BEFORE_WITH: [&str; 8] = [
    "Insert Before [",
    "Insert After [",
    "Insert Into Function [",
    "Replace In Passage [",
    "Replace In Function [",
    "Hook [",
    "Prepend To Passage [",
    "Append To Passage [",
];

static GUARD_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:IfExists|IfNotExists|IfRegExists|IfRegNotExists|IfModLoaded|IfModNotLoaded|IfPassageExists|IfPassageNotExists|IfFunctionExists|IfFunctionNotExists|IfPassageContains|IfPassageHasTag)\s*\[(?:\\.|[^\]])*\]:\s*$",
    )
    .unwrap()
});

fn is_directive_line(s: &str) -> bool {
    ALL_DIRECTIVE_TOKENS.iter().any(|tok| s == *tok || s.starts_with(tok))
}

/// `_hoist_guards_to_with_bodies`
pub fn hoist_guards_to_with_bodies(mod_content: &str) -> String {
    let lines = splitlines_keepends(mod_content);
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0usize;
    while i < lines.len() {
        let s = rstrip_crlf(lines[i]);
        if !GUARD_LINE_RE.is_match(s) {
            out.push(lines[i]);
            i += 1;
            continue;
        }

        let mut guard_buf: Vec<&str> = Vec::new();
        while i < lines.len() && GUARD_LINE_RE.is_match(rstrip_crlf(lines[i])) {
            guard_buf.push(lines[i]);
            i += 1;
        }

        let mut blank_buf: Vec<&str> = Vec::new();
        while i < lines.len() && py_strip(lines[i]).is_empty() {
            blank_buf.push(lines[i]);
            i += 1;
        }

        if i >= lines.len() {
            out.extend(guard_buf);
            out.extend(blank_buf);
            continue;
        }

        let directive_s = rstrip_crlf(lines[i]);
        if !is_directive_line(directive_s) {
            out.extend(guard_buf);
            out.extend(blank_buf);
            continue;
        }

        out.push(lines[i]);
        i += 1;

        let needs_with_scan =
            directive_s == "Replace:" || HAS_ANCHOR_BEFORE_WITH.iter().any(|tok| directive_s == *tok || directive_s.starts_with(tok));

        if needs_with_scan {
            let mut found_with = false;
            while i < lines.len() {
                out.push(lines[i]);
                if py_strip(lines[i]) == "With:" {
                    out.extend(guard_buf.iter().copied());
                    found_with = true;
                    i += 1;
                    break;
                }
                i += 1;
            }
            if !found_with {
                out.extend(guard_buf.iter().copied());
            }
        } else {
            out.extend(guard_buf.iter().copied());
        }
    }
    out.concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_guards_strips_lines() {
        let (t, g) = parse_guards("IfExists [foo]:\nbody");
        assert_eq!(t, "body");
        assert_eq!(g.if_exists, vec!["foo".to_string()]);
    }

    #[test]
    fn guard_line_then_blank_keeps_newline() {
        // `]:\s*$` backtracks so that exactly one newline is consumed.
        let (t, _) = parse_guards("IfExists [a]:\n\nbody");
        assert_eq!(t, "body");
    }

    #[test]
    fn hoist_moves_guard_after_with() {
        let src = "IfExists [x]:\nReplace:\nold\nWith:\nnew\n";
        let out = hoist_guards_to_with_bodies(src);
        assert_eq!(out, "Replace:\nold\nWith:\nIfExists [x]:\nnew\n");
    }
}
