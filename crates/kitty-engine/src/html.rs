//! HTML-level helpers used by the apply phase: passage index (`_split_html`),
//! the JavaScript bracket matcher (`_bracket_match`), character-window slicing
//! (Python counts characters, Rust strings are byte indexed) and pid
//! renumbering (`_renormalize_pids`).

use indexmap::IndexMap;
use regex::{Captures, Regex};
use std::sync::LazyLock;

static NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"name="([^"]+)""#).unwrap());
static PID_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"pid="([^"]+)"#).unwrap());

#[derive(Clone, Debug)]
pub struct PassageMeta {
    pub open_tag: String,
    /// Byte offsets of the whole `<tw-passagedata ...>...</tw-passagedata>` in the HTML that was split.
    pub start: usize,
    pub end: usize,
    /// The same start offset counted in characters (what the Python code stores).
    pub start_chars: usize,
}

#[derive(Clone, Debug, Default)]
pub struct SplitHtml {
    /// name -> body (as stored in the HTML, i.e. entity-encoded). Later duplicates win, like the Python dict.
    pub passage_dict: IndexMap<String, String>,
    pub passage_meta: IndexMap<String, PassageMeta>,
    pub script_block: String,
    pub max_pid: i64,
}

/// `_split_html` (hand-written scan: the regex version was the slowest part of a run on a 20 MB file).
pub fn split_html(html: &str) -> SplitHtml {
    let b = html.as_bytes();
    let open_f = memchr::memmem::Finder::new(b"<tw-passagedata");
    let close_f = memchr::memmem::Finder::new(b"</tw-passagedata>");
    let mut out = SplitHtml::default();
    let mut pos = 0usize;
    let mut last_byte = 0usize;
    let mut last_chars = 0usize;
    while let Some(rel) = open_f.find(&b[pos..]) {
        let start = pos + rel;
        let after = start + b"<tw-passagedata".len();
        let Some(g) = memchr::memchr(b'>', &b[after..]) else { break };
        let tag_end = after + g + 1;
        let Some(c) = close_f.find(&b[tag_end..]) else { break };
        let body_end = tag_end + c;
        let end = body_end + b"</tw-passagedata>".len();
        // running character count (matches come in document order)
        last_chars += html[last_byte..start].chars().count();
        last_byte = start;
        let attrs = &html[after..tag_end - 1];
        if let Some(nm) = NAME_RE.captures(attrs) {
            let name = nm[1].to_string();
            out.passage_dict.insert(name.clone(), html[tag_end..body_end].to_string());
            out.passage_meta.insert(
                name,
                PassageMeta { open_tag: html[start..tag_end].to_string(), start, end, start_chars: last_chars },
            );
        }
        pos = end;
    }
    // script blocks: <script ...> ... </script>
    let mut blocks: Vec<&str> = Vec::new();
    let sopen = memchr::memmem::Finder::new(b"<script");
    let sclose = memchr::memmem::Finder::new(b"</script>");
    let mut sp = 0usize;
    while let Some(rel) = sopen.find(&b[sp..]) {
        let s = sp + rel + b"<script".len();
        let Some(g) = memchr::memchr(b'>', &b[s..]) else { break };
        let body_start = s + g + 1;
        let Some(c) = sclose.find(&b[body_start..]) else { break };
        blocks.push(&html[body_start..body_start + c]);
        sp = body_start + c + b"</script>".len();
    }
    out.script_block = blocks.join("
");
    for meta in out.passage_meta.values() {
        if let Some(c) = PID_ATTR_RE.captures(&meta.open_tag) {
            if let Ok(n) = c[1].parse::<i64>() {
                out.max_pid = out.max_pid.max(n);
            }
        }
    }
    out
}

/// `_bracket_match`: index of the bracket that closes the one at `open_pos`,
/// skipping strings, template literals and comments. Works on bytes: every
/// character that matters is ASCII.
pub fn bracket_match(text: &str, open_pos: usize, open_ch: u8, close_ch: u8) -> Option<usize> {
    let b = text.as_bytes();
    let n = b.len();
    let mut depth: i64 = 0;
    let mut i = open_pos;
    while i < n {
        let ch = b[i];
        if ch == b'/' && i + 1 < n && b[i + 1] == b'/' {
            match find_byte(b, b'\n', i) {
                Some(p) => {
                    i = p;
                    continue;
                }
                None => return None,
            }
        }
        if ch == b'/' && i + 1 < n && b[i + 1] == b'*' {
            match text[i + 2..].find("*/") {
                Some(p) => {
                    i = i + 2 + p + 2;
                    continue;
                }
                None => return None,
            }
        }
        if ch == b'"' || ch == b'\'' || ch == b'`' {
            let quote = ch;
            i += 1;
            while i < n {
                let c = b[i];
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                if c == quote {
                    break;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if ch == open_ch {
            depth += 1;
        } else if ch == close_ch {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn find_byte(b: &[u8], needle: u8, from: usize) -> Option<usize> {
    b[from..].iter().position(|&x| x == needle).map(|p| p + from)
}

/// Number of leading characters two strings share.
pub fn common_prefix_chars(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// The suffix of `s` that starts after `n` characters.
pub fn skip_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[i..],
        None => "",
    }
}

/// Byte offset of character index `char_idx` (clamped to the end of the text).
pub fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(s.len())
}

/// Byte offset `n` characters before byte offset `pos` (clamped to 0).
pub fn back_chars(s: &str, pos: usize, n: usize) -> usize {
    let mut i = pos;
    for (k, (idx, _)) in s[..pos].char_indices().rev().enumerate() {
        if k == n {
            break;
        }
        i = idx;
    }
    i
}

/// Up to `n_chars` characters of `s` starting at byte offset `start`.
pub fn window(s: &str, start: usize, n_chars: usize) -> &str {
    let tail = &s[start..];
    match tail.char_indices().nth(n_chars) {
        Some((i, _)) => &tail[..i],
        None => tail,
    }
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Count of `<tw-passagedata ...>` opening tags that have no `pid=` (the
/// lookahead `(?![^>]*\bpid=)` of the Python pattern), and the text with
/// `pid="0"` inserted into them.
fn add_missing_pids(html: &str) -> (String, usize) {
    let mut out = String::with_capacity(html.len());
    let mut count = 0usize;
    let mut pos = 0usize;
    while let Some(rel) = html[pos..].find("<tw-passagedata") {
        let start = pos + rel;
        let after = start + "<tw-passagedata".len();
        // `\b` after the tag name: next char must not be a word character
        let boundary_ok = html.as_bytes().get(after).is_none_or(|&c| !is_word_byte(c));
        let Some(gt) = html[after..].find('>') else { break };
        let tag_end = after + gt + 1;
        if boundary_ok {
            let inner = &html[after..tag_end - 1];
            let ib = inner.as_bytes();
            let has_pid = inner.match_indices("pid=").any(|(i, _)| i == 0 || !is_word_byte(ib[i - 1]));
            if !has_pid {
                out.push_str(&html[pos..start]);
                out.push_str("<tw-passagedata pid=\"0\"");
                out.push_str(&html[after..tag_end]);
                count += 1;
                pos = tag_end;
                continue;
            }
        }
        out.push_str(&html[pos..tag_end]);
        pos = tag_end;
    }
    out.push_str(&html[pos..]);
    (out, count)
}

static RENUMBER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(<tw-passagedata[^>]+pid=")([^"]+)(")"#).unwrap());
static STARTNODE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(<tw-storydata[^>]+startnode=")([^"]+)(")"#).unwrap());

/// `_renormalize_pids`: renumber every passage pid 1, 2, 3... in document order
/// and update `startnode`. Returns the new text and the number of passages
/// that had no pid.
pub fn renormalize_pids(html: &str) -> (String, usize) {
    let (html, pidless) = add_missing_pids(html);
    let old_startnode = STARTNODE_RE.captures(&html).map(|c| c[2].to_string());
    let mut counter = 0usize;
    let mut pid_map: IndexMap<String, String> = IndexMap::new();
    let result = RENUMBER_RE
        .replace_all(&html, |c: &Captures| {
            counter += 1;
            let new_pid = counter.to_string();
            pid_map.insert(c[2].to_string(), new_pid.clone());
            format!("{}{}{}", &c[1], new_pid, &c[3])
        })
        .into_owned();
    if let Some(old) = old_startnode {
        if let Some(new) = pid_map.get(&old) {
            let new = new.clone();
            return (
                STARTNODE_RE.replace(&result, |c: &Captures| format!("{}{}{}", &c[1], new, &c[3])).into_owned(),
                pidless,
            );
        }
    }
    (result, pidless)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracket_match_skips_strings_and_comments() {
        let s = "{ a: '}', b: \"{\", /* } */ c: `}` } tail";
        assert_eq!(bracket_match(s, 0, b'{', b'}'), Some(s.find(" tail").unwrap() - 1));
    }

    #[test]
    fn window_counts_characters() {
        assert_eq!(window("aéb€cd", 0, 4), "aéb€");
        assert_eq!(skip_chars("aéb€cd", 3), "€cd");
        assert_eq!(common_prefix_chars("aéb", "aéc"), 2);
    }

    #[test]
    fn missing_pid_is_added() {
        let (t, n) = add_missing_pids("<tw-passagedata name=\"a\">x</tw-passagedata><tw-passagedata pid=\"3\" name=\"b\">");
        assert_eq!(n, 1);
        assert!(t.starts_with("<tw-passagedata pid=\"0\" name=\"a\">"));
    }

    #[test]
    fn renumber_updates_startnode() {
        let (t, _) = renormalize_pids("<tw-storydata startnode=\"7\"><tw-passagedata pid=\"9\" name=\"a\"></tw-passagedata><tw-passagedata pid=\"7\" name=\"b\"></tw-passagedata></tw-storydata>");
        assert!(t.contains("startnode=\"2\""));
        assert!(t.contains("pid=\"1\" name=\"a\""));
    }
}
