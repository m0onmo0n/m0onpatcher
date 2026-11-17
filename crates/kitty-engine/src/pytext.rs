//! Small helpers that reproduce Python string behaviour the Python patcher
//! relies on (what counts as whitespace, `splitlines`, `html.escape`, ...).

/// Python `str.isspace()` for a single character.
pub fn is_py_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{0b}'
            | '\u{0c}'
            | '\r'
            | '\u{1c}'..='\u{1f}'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// Python `str.strip()`.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Python `str.rstrip('\r\n')`.
pub fn rstrip_crlf(s: &str) -> &str {
    s.trim_end_matches(['\r', '\n'])
}

/// Python `str.splitlines(keepends=True)`.
pub fn splitlines_keepends(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut iter = s.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        let is_break = matches!(
            c,
            '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        );
        if is_break {
            let mut end = i + c.len_utf8();
            if c == '\r' && bytes.get(end) == Some(&b'\n') {
                end += 1;
                iter.next();
            }
            out.push(&s[start..end]);
            start = end;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Python `html.escape(s)` with `quote=True` (note: `'` becomes `&#x27;`).
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// Subset of Python `html.unescape`: the named entities the patcher produces
/// (`amp`, `lt`, `gt`, `quot`, `apos`) and numeric references. The Python
/// version also knows ~2200 other names and legacy names without `;`; they
/// never occur in the tags this is applied to.
pub fn html_unescape_basic(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let tail = &rest[p..];
        if let Some(semi) = tail.find(';') {
            let name = &tail[1..semi];
            let rep: Option<char> = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => {
                    if let Some(num) = name.strip_prefix('#') {
                        let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                            u32::from_str_radix(hex, 16).ok()
                        } else {
                            num.parse::<u32>().ok()
                        };
                        code.and_then(char::from_u32)
                    } else {
                        None
                    }
                }
            };
            if let Some(ch) = rep {
                out.push(ch);
                rest = &tail[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// What Python's text-mode `open()` gives for a UTF-8 file: universal
/// newlines (`\r\n` and lone `\r` become `\n`).
pub fn universal_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Python `str.startswith` on the first line break of `s`: true when
/// `pos` is the start of a line (start of text or right after `\n`).
pub fn is_line_start(s: &str, pos: usize) -> bool {
    pos == 0 || s.as_bytes().get(pos - 1) == Some(&b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_matches_python() {
        assert_eq!(py_strip("\u{a0} a b\u{2028}"), "a b");
        assert_eq!(py_strip("\u{1c}x\u{1f}"), "x");
    }

    #[test]
    fn splitlines_keeps_ends() {
        assert_eq!(splitlines_keepends("a\nb\r\nc"), vec!["a\n", "b\r\n", "c"]);
        assert_eq!(splitlines_keepends("a\n"), vec!["a\n"]);
        assert_eq!(splitlines_keepends(""), Vec::<&str>::new());
    }

    #[test]
    fn escape_like_python() {
        assert_eq!(html_escape("a&b<c>\"d'"), "a&amp;b&lt;c&gt;&quot;d&#x27;");
    }

    #[test]
    fn unescape_basic() {
        assert_eq!(html_unescape_basic("&lt;a&gt; &quot;x&quot; &#39;y&#39; &amp; &foo"), "<a> \"x\" 'y' & &foo");
    }
}
