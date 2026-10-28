//! Per-segment text index: builder, on-disk format, and reader.
//!
//! Files written into a segment:
//!
//! ```text
//! text.tri   header(16): "LPTR" | version u32 | n u32 | 0 u32
//!            n records (12 bytes): trigram u32 | doc_freq u32 | offset into text.post u32
//! text.post  posting lists, back to back (self-delimiting, see postings.rs)
//! text.tok   "LPTK" | version u32 | fst_len u64 | fst (token -> offset) | posting lists
//! ```
//!
//! All integers are little-endian. Posting lists hold segment-local blob ids.

use std::collections::{BTreeMap, HashMap};

use fst::automaton::{Automaton, Str};
use fst::{IntoStreamer, Map, MapBuilder, Streamer};
use loupe_core::{Error, LocalBlobId, Result};
use loupe_store::{Segment, SegmentWriter};

use crate::plan::Query;
use crate::postings::{decode_list, encode_list, intersect, union};
use crate::terms::FileTerms;

pub const TRI_FILE: &str = "text.tri";
pub const POST_FILE: &str = "text.post";
pub const TOK_FILE: &str = "text.tok";

const TRI_MAGIC: &[u8; 4] = b"LPTR";
const TOK_MAGIC: &[u8; 4] = b"LPTK";
const VERSION: u32 = 1;
const TRI_HEADER: usize = 16;
const TRI_RECORD: usize = 12;
const TOK_HEADER: usize = 16;

fn corrupt(msg: &str) -> Error {
    Error::Corrupt(format!("text index: {msg}"))
}

/// Accumulates terms for a segment. Blobs must be added in increasing local-id order.
#[derive(Debug, Default)]
pub struct TextIndexBuilder {
    trigrams: HashMap<u32, Vec<u32>>,
    tokens: BTreeMap<Vec<u8>, Vec<u32>>,
    next_id: u32,
}

/// Serialized index files, ready to write into a segment (or to query in memory).
#[derive(Debug)]
pub struct BuiltIndex {
    pub tri: Vec<u8>,
    pub post: Vec<u8>,
    pub tok: Vec<u8>,
}

impl TextIndexBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, blob: LocalBlobId, terms: FileTerms) -> Result<()> {
        if blob.0 < self.next_id {
            return Err(Error::InvalidArgument(format!(
                "blobs must be added in increasing order (got {} after {})",
                blob.0,
                self.next_id.saturating_sub(1)
            )));
        }
        self.next_id = blob.0 + 1;
        for t in terms.trigrams {
            self.trigrams.entry(t).or_default().push(blob.0);
        }
        for tok in terms.tokens {
            match self.tokens.get_mut(tok.as_slice()) {
                Some(list) => list.push(blob.0),
                None => {
                    self.tokens.insert(tok, vec![blob.0]);
                }
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Result<BuiltIndex> {
        let mut keys: Vec<u32> = self.trigrams.keys().copied().collect();
        keys.sort_unstable();

        let mut tri = Vec::with_capacity(TRI_HEADER + keys.len() * TRI_RECORD);
        tri.extend_from_slice(TRI_MAGIC);
        tri.extend_from_slice(&VERSION.to_le_bytes());
        let n = u32::try_from(keys.len()).map_err(|_| corrupt("too many trigrams"))?;
        tri.extend_from_slice(&n.to_le_bytes());
        tri.extend_from_slice(&0u32.to_le_bytes());

        let mut post = Vec::new();
        for key in keys {
            let ids = &self.trigrams[&key];
            let offset = u32::try_from(post.len())
                .map_err(|_| Error::InvalidArgument("segment postings exceed 4 GiB".into()))?;
            encode_list(&mut post, ids);
            tri.extend_from_slice(&key.to_le_bytes());
            tri.extend_from_slice(&(ids.len() as u32).to_le_bytes());
            tri.extend_from_slice(&offset.to_le_bytes());
        }

        let mut tok_post = Vec::new();
        let mut fst = MapBuilder::memory();
        for (token, ids) in &self.tokens {
            fst.insert(token, tok_post.len() as u64)
                .map_err(|e| corrupt(&format!("fst build: {e}")))?;
            encode_list(&mut tok_post, ids);
        }
        let fst_bytes = fst
            .into_inner()
            .map_err(|e| corrupt(&format!("fst build: {e}")))?;

        let mut tok = Vec::with_capacity(TOK_HEADER + fst_bytes.len() + tok_post.len());
        tok.extend_from_slice(TOK_MAGIC);
        tok.extend_from_slice(&VERSION.to_le_bytes());
        tok.extend_from_slice(&(fst_bytes.len() as u64).to_le_bytes());
        tok.extend_from_slice(&fst_bytes);
        tok.extend_from_slice(&tok_post);

        Ok(BuiltIndex { tri, post, tok })
    }
}

impl BuiltIndex {
    pub fn write_to(&self, writer: &mut SegmentWriter) -> Result<()> {
        writer.add_file(TRI_FILE, &self.tri)?;
        writer.add_file(POST_FILE, &self.post)?;
        writer.add_file(TOK_FILE, &self.tok)
    }

    pub fn view(&self) -> Result<TextIndex<'_>> {
        TextIndex::from_parts(&self.tri, &self.post, &self.tok)
    }
}

/// Read-only view over one segment's text index.
pub struct TextIndex<'a> {
    tri: &'a [u8],
    n_tri: usize,
    post: &'a [u8],
    tokens: Map<&'a [u8]>,
    tok_post: &'a [u8],
}

impl std::fmt::Debug for TextIndex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextIndex")
            .field("trigrams", &self.n_tri)
            .field("tokens", &self.tokens.len())
            .finish()
    }
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

impl<'a> TextIndex<'a> {
    pub fn from_segment(segment: &'a Segment) -> Result<TextIndex<'a>> {
        let get = |name: &str| {
            segment
                .file(name)
                .ok_or_else(|| corrupt(&format!("segment has no {name}")))
        };
        Self::from_parts(get(TRI_FILE)?, get(POST_FILE)?, get(TOK_FILE)?)
    }

    pub fn from_parts(tri: &'a [u8], post: &'a [u8], tok: &'a [u8]) -> Result<TextIndex<'a>> {
        if tri.len() < TRI_HEADER || &tri[..4] != TRI_MAGIC {
            return Err(corrupt("bad trigram table header"));
        }
        let version = read_u32(tri, 4);
        if version != VERSION {
            return Err(Error::FormatVersion {
                found: version,
                expected: VERSION,
            });
        }
        let n_tri = read_u32(tri, 8) as usize;
        if tri.len() != TRI_HEADER + n_tri * TRI_RECORD {
            return Err(corrupt("trigram table length mismatch"));
        }

        if tok.len() < TOK_HEADER || &tok[..4] != TOK_MAGIC {
            return Err(corrupt("bad token index header"));
        }
        let version = read_u32(tok, 4);
        if version != VERSION {
            return Err(Error::FormatVersion {
                found: version,
                expected: VERSION,
            });
        }
        let fst_len = usize::try_from(u64::from_le_bytes(tok[8..16].try_into().unwrap()))
            .map_err(|_| corrupt("token fst length"))?;
        let fst_end = TOK_HEADER
            .checked_add(fst_len)
            .filter(|&e| e <= tok.len())
            .ok_or_else(|| corrupt("token fst length exceeds file"))?;
        let tokens =
            Map::new(&tok[TOK_HEADER..fst_end]).map_err(|e| corrupt(&format!("token fst: {e}")))?;

        Ok(TextIndex {
            tri,
            n_tri,
            post,
            tokens,
            tok_post: &tok[fst_end..],
        })
    }

    pub fn trigram_count(&self) -> usize {
        self.n_tri
    }

    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// `(doc_freq, posting offset)` of a trigram, by binary search over the sorted table.
    fn trigram_record(&self, key: u32) -> Option<(u32, usize)> {
        let (mut lo, mut hi) = (0usize, self.n_tri);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let at = TRI_HEADER + mid * TRI_RECORD;
            match read_u32(self.tri, at).cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    return Some((
                        read_u32(self.tri, at + 4),
                        read_u32(self.tri, at + 8) as usize,
                    ));
                }
            }
        }
        None
    }

    /// Number of blobs containing the trigram (0 if absent).
    pub fn trigram_df(&self, key: u32) -> u32 {
        self.trigram_record(key).map_or(0, |(df, _)| df)
    }

    pub fn trigram_postings(&self, key: u32) -> Result<Vec<u32>> {
        match self.trigram_record(key) {
            Some((_, off)) => decode_list(self.post, off),
            None => Ok(Vec::new()),
        }
    }

    /// Blobs containing the exact identifier token.
    pub fn token_postings(&self, token: &[u8]) -> Result<Vec<u32>> {
        match self.tokens.get(token) {
            Some(off) => decode_list(
                self.tok_post,
                usize::try_from(off).map_err(|_| corrupt("token offset"))?,
            ),
            None => Ok(Vec::new()),
        }
    }

    /// Every indexed token, in byte order.
    pub fn tokens(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.tokens.len());
        let mut stream = self.tokens.stream();
        while let Some((key, _)) = stream.next() {
            out.push(String::from_utf8_lossy(key).into_owned());
        }
        out
    }

    /// Up to `limit` indexed tokens starting with `prefix`, in byte order.
    pub fn tokens_with_prefix(&self, prefix: &str, limit: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut stream = self
            .tokens
            .search(Str::new(prefix).starts_with())
            .into_stream();
        while let Some((key, _)) = stream.next() {
            if out.len() >= limit {
                break;
            }
            out.push(String::from_utf8_lossy(key).into_owned());
        }
        out
    }

    /// Local blob ids satisfying `query`. `None` means "every blob" (the query is `All`), which
    /// callers treat as a full scan.
    pub fn candidates(&self, query: &Query) -> Result<Option<Vec<u32>>> {
        Ok(match self.evaluate(query)? {
            Set::All => None,
            Set::Ids(ids) => Some(ids),
        })
    }

    fn evaluate(&self, q: &Query) -> Result<Set> {
        match q {
            Query::All => Ok(Set::All),
            Query::Never => Ok(Set::Ids(Vec::new())),
            Query::Tri(key) => Ok(Set::Ids(self.trigram_postings(*key)?)),
            Query::And(parts) => {
                // Smallest estimated list first, so later intersections touch few ids.
                let mut ordered: Vec<(u64, &Query)> =
                    parts.iter().map(|p| (self.estimate(p), p)).collect();
                ordered.sort_by_key(|&(est, _)| est);
                let mut acc: Option<Vec<u32>> = None;
                for (_, part) in ordered {
                    match self.evaluate(part)? {
                        Set::All => {}
                        Set::Ids(ids) => {
                            let next = match &acc {
                                None => ids,
                                Some(prev) => intersect(prev, &ids),
                            };
                            if next.is_empty() {
                                return Ok(Set::Ids(next));
                            }
                            acc = Some(next);
                        }
                    }
                }
                Ok(acc.map_or(Set::All, Set::Ids))
            }
            Query::Or(parts) => {
                let mut acc: Vec<u32> = Vec::new();
                for part in parts {
                    match self.evaluate(part)? {
                        Set::All => return Ok(Set::All),
                        Set::Ids(ids) => acc = union(&acc, &ids),
                    }
                }
                Ok(Set::Ids(acc))
            }
        }
    }

    /// Upper-bound-ish size of a sub-query's result, used only to order intersections.
    fn estimate(&self, q: &Query) -> u64 {
        match q {
            Query::All => u64::MAX,
            Query::Never => 0,
            Query::Tri(key) => u64::from(self.trigram_df(*key)),
            Query::And(parts) => parts
                .iter()
                .map(|p| self.estimate(p))
                .min()
                .unwrap_or(u64::MAX),
            Query::Or(parts) => parts
                .iter()
                .map(|p| self.estimate(p))
                .fold(0u64, u64::saturating_add),
        }
    }
}

enum Set {
    All,
    Ids(Vec<u32>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terms::{extract_terms, trigram_key};

    fn build(docs: &[&str]) -> BuiltIndex {
        let mut b = TextIndexBuilder::new();
        for (i, d) in docs.iter().enumerate() {
            b.add(LocalBlobId(i as u32), extract_terms(d.as_bytes()))
                .unwrap();
        }
        b.finish().unwrap()
    }

    #[test]
    fn trigram_and_token_lookup() {
        let built = build(&["hello world", "help wanted", "unrelated"]);
        let idx = built.view().unwrap();
        assert_eq!(
            idx.trigram_postings(trigram_key(b'h', b'e', b'l')).unwrap(),
            [0, 1]
        );
        assert_eq!(
            idx.trigram_postings(trigram_key(b'l', b'l', b'o')).unwrap(),
            [0]
        );
        assert_eq!(idx.trigram_df(trigram_key(b'h', b'e', b'l')), 2);
        assert!(
            idx.trigram_postings(trigram_key(b'z', b'z', b'z'))
                .unwrap()
                .is_empty()
        );
        assert_eq!(idx.token_postings(b"hello").unwrap(), [0]);
        assert_eq!(idx.token_postings(b"HELLO").unwrap(), Vec::<u32>::new());
        assert_eq!(idx.tokens_with_prefix("hel", 10), ["hello", "help"]);
        assert_eq!(idx.tokens_with_prefix("hel", 1), ["hello"]);
    }

    #[test]
    fn query_evaluation() {
        let built = build(&["alpha beta", "alpha gamma", "delta beta"]);
        let idx = built.view().unwrap();
        let t = |s: &str| {
            let b = s.as_bytes();
            Query::Tri(trigram_key(b[0], b[1], b[2]))
        };
        let c = |q: Query| idx.candidates(&q).unwrap();
        assert_eq!(c(t("alp")), Some(vec![0, 1]));
        assert_eq!(c(Query::and(vec![t("alp"), t("bet")])), Some(vec![0]));
        assert_eq!(c(Query::or(vec![t("gam"), t("del")])), Some(vec![1, 2]));
        assert_eq!(c(Query::Never), Some(vec![]));
        assert_eq!(c(Query::All), None);
        assert_eq!(c(Query::and(vec![t("alp"), t("zzz")])), Some(vec![]));
    }

    #[test]
    fn empty_index_is_valid() {
        let built = build(&[]);
        let idx = built.view().unwrap();
        assert_eq!(idx.trigram_count(), 0);
        assert!(idx.token_postings(b"x").unwrap().is_empty());
    }

    #[test]
    fn blobs_must_be_added_in_order() {
        let mut b = TextIndexBuilder::new();
        b.add(LocalBlobId(3), FileTerms::default()).unwrap();
        assert!(b.add(LocalBlobId(3), FileTerms::default()).is_err());
        assert!(b.add(LocalBlobId(2), FileTerms::default()).is_err());
        assert!(b.add(LocalBlobId(4), FileTerms::default()).is_ok());
    }

    #[test]
    fn rejects_malformed_files() {
        let built = build(&["abc def"]);
        let (tri, post, tok) = (&built.tri, &built.post, &built.tok);
        assert!(TextIndex::from_parts(&tri[..10], post, tok).is_err());
        assert!(TextIndex::from_parts(&tri[..tri.len() - 1], post, tok).is_err());
        assert!(TextIndex::from_parts(tri, post, &tok[..8]).is_err());
        let mut bad = tok.clone();
        bad[8] = 0xff;
        bad[9] = 0xff;
        assert!(TextIndex::from_parts(tri, post, &bad).is_err());
        let mut bad = tri.clone();
        bad[0] = b'X';
        assert!(TextIndex::from_parts(&bad, post, tok).is_err());
    }

    #[test]
    fn truncated_postings_error_instead_of_panicking() {
        let built = build(&["abc def", "abc xyz"]);
        let idx = TextIndex::from_parts(&built.tri, &built.post[..1], &built.tok).unwrap();
        // Either an error or a short list is acceptable; a panic is not.
        let _ = idx.trigram_postings(trigram_key(b'a', b'b', b'c'));
    }
}
