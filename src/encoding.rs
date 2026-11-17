/// Decode a quoted string: `\n` → newline, `\X` → `X` for any other char.
pub fn quoted_string_decode(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_string();
    }

    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some(other) => result.push(other),
                None => {}
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Encode a string for placement inside `"..."`: escape `\`, `\n`, and `"`.
pub fn quoted_string_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// Inverse of `url_encode_file_name`. All percent-encoded bytes are ASCII,
/// so byte-wise decoding preserves UTF-8 for any raw (non-encoded) content.
pub fn url_decode_file_name(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(h) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(v) = u8::from_str_radix(h, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).expect("non-encoded bytes retain source UTF-8 validity")
}

/// Minimal HTML entity encoder matching the JSoup base escape: the five
/// HTML-significant characters, with `'` written as `&#39;` (matching the
/// original Kotlin's post-processing of `&apos;`).
pub fn html_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Strip `\r` characters (CRLF → LF, stray CR → removed).
pub fn remove_cr_newlines(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    text.chars().filter(|&c| c != '\r').collect()
}

/// Percent-encode characters that are unsafe for filenames.
pub fn url_encode_file_name(name: &str) -> String {
    if !name.contains(|c: char| matches!(c, '/' | '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*' | '%'))
    {
        return name.to_string();
    }

    name.replace('%', "%25")
        .replace('"', "%22")
        .replace('*', "%2A")
        .replace('/', "%2F")
        .replace(':', "%3A")
        .replace('<', "%3C")
        .replace('>', "%3E")
        .replace('?', "%3F")
        .replace('\\', "%5C")
        .replace('|', "%7C")
}
