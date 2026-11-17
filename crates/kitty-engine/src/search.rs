//! Fast substring search and in-place replacement for the 20 MB HTML (SIMD `memchr::memmem`
//! instead of `str::find`, and no whole-file copy per edit).

use memchr::memmem;

pub trait FastFind {
    /// Byte offset of the first occurrence of `needle` (same result as `str::find`).
    fn ffind(&self, needle: &str) -> Option<usize>;
}

impl FastFind for str {
    fn ffind(&self, needle: &str) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        memmem::find(self.as_bytes(), needle.as_bytes())
    }
}

/// Replace every non-overlapping occurrence of `needle` (like `str::replace`) without copying the
/// whole text when there are only a few occurrences. Returns the number of replacements.
pub fn replace_all_in_place(text: &mut String, needle: &str, repl: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let positions: Vec<usize> = memmem::find_iter(text.as_bytes(), needle.as_bytes()).collect();
    let n = positions.len();
    if n == 0 {
        return 0;
    }
    if n <= 4 {
        for &p in positions.iter().rev() {
            text.replace_range(p..p + needle.len(), repl);
        }
        return n;
    }
    // many occurrences: one pass into a new buffer
    let mut out = String::with_capacity(text.len() + n * repl.len().saturating_sub(needle.len()));
    let mut last = 0usize;
    for &p in &positions {
        out.push_str(&text[last..p]);
        out.push_str(repl);
        last = p + needle.len();
    }
    out.push_str(&text[last..]);
    *text = out;
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_matches_std() {
        assert_eq!("abcabc".ffind("ca"), "abcabc".find("ca"));
        assert_eq!("abc".ffind("zz"), None);
        assert_eq!("héllo wörld".ffind("wör"), "héllo wörld".find("wör"));
    }

    #[test]
    fn replace_all_matches_std() {
        for (t, n, r) in [("aXbXc", "X", "--"), ("XXXXXX", "X", "ab"), ("none", "z", "y"), ("aaaa", "aa", "b")] {
            let mut s = t.to_string();
            replace_all_in_place(&mut s, n, r);
            assert_eq!(s, t.replace(n, r), "{t} {n} {r}");
        }
        let mut s = "a1a2a3a4a5a6".to_string();
        replace_all_in_place(&mut s, "a", "bb");
        assert_eq!(s, "a1a2a3a4a5a6".replace('a', "bb"));
    }
}
