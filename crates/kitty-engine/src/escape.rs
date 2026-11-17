//! Escaping rules, ported from the Python patcher (`escape_twine_tags`,
//! `_escape_twine_region`, `_escape_script_body`, `auto_escape_passage_bodies`,
//! `escape_html_between_tags`).

use crate::pytext::{html_escape, html_unescape_basic, py_strip};
use regex::{Captures, Regex};
use std::sync::LazyLock;

static ENCODED_ENTITY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&(?:quot|lt|gt|amp|#39|#x27);").unwrap());
static TWINE_MACRO_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<<(.*?)>>").unwrap());
static TW_TAG_ESCAPED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)&lt;/?tw-passagedata(?:&gt;|[^>]*?&gt;)").unwrap());
static TW_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)(<tw-passagedata\b[^>]*>)(.*?)(</tw-passagedata>)").unwrap());

/// `_escape_script_body`
pub fn escape_script_body(body: &str) -> String {
    if ENCODED_ENTITY_RE.is_match(body) {
        return body.to_string();
    }
    html_escape(body).replace("&#x27;", "&#39;")
}

/// `_escape_twine_region`
pub fn escape_twine_region(content: &str) -> String {
    let new_content = TWINE_MACRO_RE.replace_all(content, "&lt;&lt;$1&gt;&gt;").into_owned();
    if new_content != content {
        let mut n = html_escape(&new_content);
        n = n.replace("&amp;lt;", "&lt;");
        n = n.replace("&amp;gt;", "&gt;");
        n = n.replace("&amp;quot;", "&quot;");
        n = n.replace("&amp;amp;", "&amp;");
        n = n.replace("&#x27;", "&#39;");
        let n = TW_TAG_ESCAPED_RE
            .replace_all(&n, |c: &Captures| html_unescape_basic(&c[0]))
            .into_owned();
        return n;
    }
    content.to_string()
}

/// `escape_twine_tags`
pub fn escape_twine_tags(content: &str) -> String {
    const OPEN_RAW: &str = "<<script>>";
    const CLOSE_RAW: &str = "<</script>>";
    const OPEN_ESC: &str = "&lt;&lt;script&gt;&gt;";
    const CLOSE_ESC: &str = "&lt;&lt;/script&gt;&gt;";

    let (tag_open, tag_close) = if content.contains(OPEN_RAW) {
        (OPEN_RAW, CLOSE_RAW)
    } else if content.contains(OPEN_ESC) {
        (OPEN_ESC, CLOSE_ESC)
    } else {
        return escape_twine_region(content);
    };

    let mut result = String::with_capacity(content.len() + content.len() / 8);
    for (i, part) in content.split(tag_open).enumerate() {
        if i == 0 {
            result.push_str(&escape_twine_region(part));
        } else if let Some(close_idx) = part.find(tag_close) {
            let script_body = &part[..close_idx];
            let after_close = &part[close_idx + tag_close.len()..];
            result.push_str(OPEN_ESC);
            result.push_str(&escape_script_body(script_body));
            result.push_str(CLOSE_ESC);
            result.push_str(&escape_twine_region(after_close));
        } else {
            result.push_str(OPEN_ESC);
            result.push_str(&escape_script_body(part));
        }
    }
    result
}

fn escape_non_script_body(body: &str) -> String {
    if !body.contains("<<script>>") {
        return html_escape(body);
    }
    let mut result = String::new();
    for (i, part) in body.split("<<script>>").enumerate() {
        if i == 0 {
            result.push_str(&html_escape(part));
        } else if let Some(close_idx) = part.find("<</script>>") {
            let script_body = &part[..close_idx];
            let after = &part[close_idx + "<</script>>".len()..];
            result.push_str("&lt;&lt;script&gt;&gt;");
            result.push_str(&escape_script_body(script_body));
            result.push_str("&lt;&lt;/script&gt;&gt;");
            result.push_str(&html_escape(after));
        } else {
            result.push_str("&lt;&lt;script&gt;&gt;");
            result.push_str(&escape_script_body(part));
        }
    }
    result
}

/// `auto_escape_passage_bodies`
pub fn auto_escape_passage_bodies(new_lines: &str) -> String {
    TW_BLOCK_RE
        .replace_all(new_lines, |c: &Captures| {
            let open_tag = &c[1];
            let mut body = c[2].to_string();
            let close = &c[3];
            if body.contains("&lt;&lt;") || body.contains("&lt;") {
                return c[0].to_string();
            }
            if body.contains("<<") || body.contains(">>") {
                body = escape_non_script_body(&body);
            }
            format!("{open_tag}{body}{close}")
        })
        .into_owned()
}

const CONTROL_KEYWORDS: [&str; 16] = [
    "Replace:",
    "ReplaceReg [",
    "With:",
    "With{",
    "IfExists [",
    "IfNotExists [",
    "IfRegExists [",
    "IfRegNotExists [",
    "Add Passage:",
    "Add Javascript",
    "Add CSS:",
    "Add Events:",
    "Append Array [",
    "Append Variable to Class [",
    "Append Function to Class [",
    "Add Function [",
];

fn escape_block_content(content: &str) -> String {
    if content.contains("&lt;") || content.contains("&quot;") {
        content.to_string()
    } else {
        html_escape(content)
    }
}

/// `escape_html_between_tags` with the default `<e>` / `</e>` delimiters.
pub fn escape_html_between_tags(text: &str) -> String {
    let start_tag = "<e>";
    let end_tag = "</e>";
    let mut result: Vec<String> = Vec::new();
    let mut inside = false;
    let mut buffer: Vec<&str> = Vec::new();

    for line in text.split('\n') {
        let stripped_line = py_strip(line);

        if CONTROL_KEYWORDS.iter().any(|kw| stripped_line.contains(kw)) {
            if inside {
                result.push(escape_block_content(&buffer.join("\n")));
                buffer.clear();
                inside = false;
            }
            result.push(line.to_string());
            continue;
        }
        if stripped_line == start_tag {
            inside = true;
            continue;
        }
        if stripped_line == end_tag {
            if inside {
                result.push(escape_block_content(&buffer.join("\n")));
                buffer.clear();
                inside = false;
            } else {
                result.push(line.to_string());
            }
            continue;
        }
        if inside {
            buffer.push(line);
        } else {
            result.push(line.to_string());
        }
    }

    if inside {
        let content = buffer.join("\n");
        if content.contains("&lt;") || content.contains("&quot;") || content.contains("<<") {
            result.push(content);
        } else {
            result.push(html_escape(&content));
        }
    }
    result.join("\n")
}

/// The chain of `.replace(..)` calls the Python patcher uses to turn an
/// escaped replacement text back into raw JS/CSS (for script-block targets).
pub fn unescape_for_raw(s: &str) -> String {
    s.replace("&lt;&lt;", "<<")
        .replace("&gt;&gt;", ">>")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_escapes_macros() {
        assert_eq!(escape_twine_region("a <<if $x>>b<</if>> c"), "a &lt;&lt;if $x&gt;&gt;b&lt;&lt;/if&gt;&gt; c");
    }

    #[test]
    fn region_leaves_plain_text() {
        assert_eq!(escape_twine_region("a 'b' \"c\""), "a 'b' \"c\"");
    }

    #[test]
    fn region_quotes_become_decimal_entities() {
        assert_eq!(escape_twine_region("<<set $a to \"it's\">>"), "&lt;&lt;set $a to &quot;it&#39;s&quot;&gt;&gt;");
    }

    #[test]
    fn e_blocks_are_escaped_and_delimiters_dropped() {
        assert_eq!(escape_html_between_tags("x\n<e>\n<<a>>\n</e>\ny"), "x\n&lt;&lt;a&gt;&gt;\ny");
    }
}
