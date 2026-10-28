//! Term extraction: what the index records about a blob.
//!
//! Trigrams are taken over ASCII-lowercased bytes, so one posting set serves both case-sensitive
//! and case-insensitive queries (the verifier restores exactness). Trigrams containing a
//! newline are not indexed: they only matter to patterns that match a literal newline, and the
//! planner never requires them (see `plan.rs`). Non-ASCII bytes are not folded.

pub const MIN_TOKEN_LEN: usize = 2;
pub const MAX_TOKEN_LEN: usize = 64;

#[inline]
pub fn fold(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

#[inline]
pub fn trigram_key(a: u8, b: u8, c: u8) -> u32 {
    (u32::from(a) << 16) | (u32::from(b) << 8) | u32::from(c)
}

/// Everything extracted from one blob, ready to hand to the index builder.
#[derive(Clone, Debug, Default)]
pub struct FileTerms {
    /// Sorted, unique trigram keys.
    pub trigrams: Vec<u32>,
    /// Sorted, unique identifier tokens.
    pub tokens: Vec<Vec<u8>>,
}

pub fn extract_terms(content: &[u8]) -> FileTerms {
    FileTerms {
        trigrams: extract_trigrams(content),
        tokens: extract_tokens(content),
    }
}

pub fn extract_trigrams(content: &[u8]) -> Vec<u32> {
    let mut out = Vec::with_capacity(content.len().saturating_sub(2));
    for w in content.windows(3) {
        if w.contains(&b'\n') {
            continue;
        }
        out.push(trigram_key(fold(w[0]), fold(w[1]), fold(w[2])));
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Identifier tokens: maximal runs of `[A-Za-z0-9_]` that do not start with a digit, with
/// length in `MIN_TOKEN_LEN..=MAX_TOKEN_LEN`. Case is preserved.
pub fn extract_tokens(content: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut i = 0;
    while i < content.len() {
        if !is_word_byte(content[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < content.len() && is_word_byte(content[i]) {
            i += 1;
        }
        let run = &content[start..i];
        if !run[0].is_ascii_digit() && (MIN_TOKEN_LEN..=MAX_TOKEN_LEN).contains(&run.len()) {
            out.push(run.to_vec());
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigrams_are_folded_unique_and_skip_newlines() {
        let t = extract_trigrams(b"AbcABC\nxyz");
        let abc = trigram_key(b'a', b'b', b'c');
        assert!(t.contains(&abc));
        assert_eq!(t.iter().filter(|&&k| k == abc).count(), 1);
        assert!(t.contains(&trigram_key(b'x', b'y', b'z')));
        // "c\nx" and "C\nx" style windows are not indexed.
        assert!(
            t.iter()
                .all(|&k| (k >> 16) != 10 && ((k >> 8) & 0xff) != 10 && (k & 0xff) != 10)
        );
        assert!(t.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn short_inputs_have_no_trigrams() {
        assert!(extract_trigrams(b"").is_empty());
        assert!(extract_trigrams(b"ab").is_empty());
    }

    #[test]
    fn tokens() {
        let toks = extract_tokens(b"fn foo_bar(x1: u8) { 9lives Baz foo_bar a }");
        let s: Vec<&str> = toks
            .iter()
            .map(|t| std::str::from_utf8(t).unwrap())
            .collect();
        assert_eq!(s, ["Baz", "fn", "foo_bar", "u8", "x1"]);
    }

    #[test]
    fn overlong_tokens_are_dropped() {
        let long = "a".repeat(MAX_TOKEN_LEN + 1);
        assert!(extract_tokens(long.as_bytes()).is_empty());
        let ok = "a".repeat(MAX_TOKEN_LEN);
        assert_eq!(extract_tokens(ok.as_bytes()).len(), 1);
    }
}
