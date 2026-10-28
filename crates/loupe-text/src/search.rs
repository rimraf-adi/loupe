//! Query execution: plan -> candidate files from the index -> verify with the real regex.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use loupe_core::{BlobId, Error, FileMeta, LocalBlobId, Result, Snapshot};
use loupe_overlay::Overlay;
use loupe_store::{Segment, Store};
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};

use crate::index::TextIndex;
use crate::plan::{Query, plan};
use crate::terms::{MAX_TOKEN_LEN, MIN_TOKEN_LEN};

/// Snippet lines longer than this are cut (on a character boundary) and marked with `…`.
const MAX_LINE_BYTES: usize = 240;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchOptions {
    pub case_insensitive: bool,
    /// Treat the pattern as a literal string instead of a regex.
    pub literal: bool,
    /// Keep at most this many files (best first); `total_files` still counts all.
    pub max_files: usize,
    /// Snippets kept per file; `match_count` still counts every match.
    pub max_matches_per_file: usize,
    /// Lines of context around each match.
    pub context_lines: usize,
    /// Stop verifying after this long and report `timed_out`. Guards patterns with no usable
    /// literals, which scan every file.
    pub timeout: Option<Duration>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            case_insensitive: false,
            literal: false,
            max_files: 50,
            max_matches_per_file: 5,
            context_lines: 1,
            timeout: Some(Duration::from_secs(10)),
        }
    }
}

/// A run of lines around one or more matches; 1-based, inclusive.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snippet {
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileHit {
    pub path: String,
    pub match_count: usize,
    pub snippets: Vec<Snippet>,
    /// The file on disk differs from the snapshot the index was built from. Hits come from the
    /// current content, but files the index never saw as matching cannot be found.
    pub stale: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SearchStats {
    /// Human-readable trigram plan (or the token searched).
    pub plan: String,
    /// Files handed to the verifier.
    pub candidates: usize,
    pub scanned: usize,
    /// The plan had no usable literals, so every file was a candidate.
    pub full_scan: bool,
    pub timed_out: bool,
    /// More files matched than `max_files`.
    pub truncated: bool,
    pub stale_files: usize,
    /// Candidates that could not be read (deleted since the snapshot).
    pub unreadable: usize,
    /// Snapshot files with no index entry; always scanned.
    pub unindexed_files: usize,
    /// Files served from the overlay (unsaved or just-changed), scanned from memory.
    pub overlay_files: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    /// Snapshot the answer was computed against (0 = nothing indexed).
    pub snapshot_id: u64,
    pub hits: Vec<FileHit>,
    /// Files with at least one match, before `max_files` truncation.
    pub total_files: usize,
    pub total_matches: usize,
    pub stats: SearchStats,
}

enum Plan {
    Trigram(Query),
    Token(Vec<u8>),
}

/// Searches the current snapshot of a store.
pub struct TextSearcher {
    root: PathBuf,
    snapshot_id: u64,
    snapshot: Snapshot,
    segments: Vec<Segment>,
    /// Live blobs -> the paths holding them (sorted).
    blob_paths: HashMap<BlobId, Vec<String>>,
    /// Paths whose blob is in no segment.
    unindexed: Vec<String>,
}

impl std::fmt::Debug for TextSearcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextSearcher")
            .field("snapshot_id", &self.snapshot_id)
            .field("files", &self.snapshot.len())
            .field("segments", &self.segments.len())
            .finish()
    }
}

impl TextSearcher {
    /// Load the store's current snapshot and verify/open every live segment. A corrupt segment
    /// is an error here; recovery (dropping and re-indexing it) is the caller's decision.
    pub fn open(root: impl Into<PathBuf>, store: &Store) -> Result<TextSearcher> {
        let snapshot = store.load_snapshot()?;
        let segments = store
            .segment_ids()
            .into_iter()
            .map(|id| store.open_segment(id))
            .collect::<Result<Vec<_>>>()?;

        let indexed: HashSet<BlobId> = segments
            .iter()
            .flat_map(|s| s.blobs().iter().copied())
            .collect();
        let mut blob_paths: HashMap<BlobId, Vec<String>> = HashMap::new();
        let mut unindexed = Vec::new();
        for (path, meta) in &snapshot.files {
            blob_paths.entry(meta.blob).or_default().push(path.clone());
            if !indexed.contains(&meta.blob) {
                unindexed.push(path.clone());
            }
        }
        Ok(TextSearcher {
            root: root.into(),
            snapshot_id: store.snapshot_id(),
            snapshot,
            segments,
            blob_paths,
            unindexed,
        })
    }

    pub fn snapshot_id(&self) -> u64 {
        self.snapshot_id
    }

    /// Repository root the paths are relative to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `path`'s on-disk content differs from the snapshot (exact, by content hash).
    /// Files the overlay owns are never stale (the overlay is the freshest truth); unknown paths
    /// are not stale (there is nothing to be out of date with).
    pub fn is_stale(&self, path: &str, overlay: Option<&Overlay>) -> bool {
        if overlay.is_some_and(|o| o.get(path).is_some()) {
            return false;
        }
        self.snapshot
            .get(path)
            .is_some_and(|meta| loupe_walk::is_stale(&self.root, path, meta))
    }

    /// What the snapshot recorded about `path`.
    pub fn file_meta(&self, path: &str) -> Option<&FileMeta> {
        self.snapshot.get(path)
    }

    /// Snapshot paths in sorted order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.snapshot.files.keys().map(String::as_str)
    }

    pub(crate) fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    pub fn file_count(&self) -> usize {
        self.snapshot.len()
    }

    /// Regex (or literal) search.
    pub fn search(&self, pattern: &str, opts: &SearchOptions) -> Result<SearchResult> {
        self.search_with(pattern, opts, None)
    }

    /// Like [`TextSearcher::search`], with `overlay` merged over the snapshot: overlay files are
    /// searched from memory, and overlay-removed files disappear.
    pub fn search_with(
        &self,
        pattern: &str,
        opts: &SearchOptions,
        overlay: Option<&Overlay>,
    ) -> Result<SearchResult> {
        let pattern = if opts.literal {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        let re = compile(&pattern, opts.case_insensitive)?;
        let query = plan(&pattern, opts.case_insensitive)?;
        self.execute(&re, Plan::Trigram(query), opts, overlay)
    }

    /// Whole-identifier search, answered from the token index. Words the token index does not
    /// cover (shorter than 2 or longer than 64 bytes) fall back to a `\b` regex search.
    pub fn search_word(&self, word: &str, opts: &SearchOptions) -> Result<SearchResult> {
        self.search_word_with(word, opts, None)
    }

    pub fn search_word_with(
        &self,
        word: &str,
        opts: &SearchOptions,
        overlay: Option<&Overlay>,
    ) -> Result<SearchResult> {
        let valid = !word.is_empty()
            && !word.as_bytes()[0].is_ascii_digit()
            && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !valid {
            return Err(Error::InvalidArgument(format!(
                "not an identifier: {word:?}"
            )));
        }
        let pattern = format!(r"\b{word}\b");
        // Identifiers are case-sensitive here; the token index stores them as written.
        let re = compile(&pattern, false)?;
        let plan = if (MIN_TOKEN_LEN..=MAX_TOKEN_LEN).contains(&word.len()) {
            Plan::Token(word.as_bytes().to_vec())
        } else {
            Plan::Trigram(plan_for(&pattern)?)
        };
        self.execute(&re, plan, opts, overlay)
    }

    /// Indexed identifiers starting with `prefix` (symbol-name completion), sorted and deduped.
    pub fn tokens_with_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        let mut all = std::collections::BTreeSet::new();
        for seg in &self.segments {
            let idx = TextIndex::from_segment(seg)?;
            all.extend(idx.tokens_with_prefix(prefix, limit));
        }
        Ok(all.into_iter().take(limit).collect())
    }

    fn execute(
        &self,
        re: &Regex,
        plan: Plan,
        opts: &SearchOptions,
        overlay: Option<&Overlay>,
    ) -> Result<SearchResult> {
        let mut stats = SearchStats {
            unindexed_files: self.unindexed.len(),
            ..SearchStats::default()
        };
        let mut candidates = self.candidate_paths(&plan, &mut stats)?;
        // The overlay wins for every path it mentions: drop the snapshot's version, and search
        // the overlay's own content from memory.
        if let Some(o) = overlay {
            candidates.retain(|p| o.get(p).is_none());
        }
        let mem: Vec<(&str, &Arc<[u8]>)> = overlay.map(Overlay::puts).unwrap_or_default();
        stats.overlay_files = mem.len();
        stats.candidates = candidates.len() + mem.len();

        let deadline = opts.timeout.map(|t| Instant::now() + t);
        let timed_out = AtomicBool::new(false);
        let outcomes: Vec<Outcome> = candidates
            .par_iter()
            .map(|path| {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    timed_out.store(true, Ordering::Relaxed);
                    return Outcome::Skipped;
                }
                let meta = &self.snapshot.files[*path];
                verify_file(&self.root, path, meta, re, opts)
            })
            .collect();
        let mem_outcomes: Vec<Outcome> = mem
            .par_iter()
            .map(|(path, bytes)| {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    timed_out.store(true, Ordering::Relaxed);
                    return Outcome::Skipped;
                }
                verify_bytes(path, bytes, false, re, opts)
            })
            .collect();
        let outcomes: Vec<Outcome> = outcomes.into_iter().chain(mem_outcomes).collect();
        stats.timed_out = timed_out.load(Ordering::Relaxed);

        let mut hits = Vec::new();
        for outcome in outcomes {
            match outcome {
                Outcome::Skipped => {}
                Outcome::Unreadable => stats.unreadable += 1,
                Outcome::Scanned(hit) => {
                    stats.scanned += 1;
                    if let Some(hit) = hit {
                        stats.stale_files += usize::from(hit.stale);
                        hits.push(hit);
                    }
                }
            }
        }

        // Most matches first; ties by path so output is deterministic.
        hits.sort_by(|a, b| {
            b.match_count
                .cmp(&a.match_count)
                .then_with(|| a.path.cmp(&b.path))
        });
        let total_files = hits.len();
        let total_matches = hits.iter().map(|h| h.match_count).sum();
        if hits.len() > opts.max_files {
            hits.truncate(opts.max_files);
            stats.truncated = true;
        }
        Ok(SearchResult {
            snapshot_id: self.snapshot_id,
            hits,
            total_files,
            total_matches,
            stats,
        })
    }

    /// Paths worth verifying, sorted. Always includes unindexed files.
    fn candidate_paths(&self, plan: &Plan, stats: &mut SearchStats) -> Result<Vec<&String>> {
        let query = match plan {
            Plan::Trigram(q) => {
                stats.plan = q.describe();
                Some(q)
            }
            Plan::Token(t) => {
                stats.plan = format!("token {:?}", String::from_utf8_lossy(t));
                None
            }
        };
        if matches!(query, Some(Query::All)) {
            stats.full_scan = true;
            return Ok(self.snapshot.files.keys().collect());
        }

        let mut blobs: HashSet<BlobId> = HashSet::new();
        for seg in &self.segments {
            let idx = TextIndex::from_segment(seg)?;
            let ids: Vec<u32> = match plan {
                Plan::Trigram(q) => match idx.candidates(q)? {
                    Some(ids) => ids,
                    None => (0..seg.blobs().len() as u32).collect(),
                },
                Plan::Token(t) => idx.token_postings(t)?,
            };
            blobs.extend(ids.into_iter().filter_map(|id| seg.blob(LocalBlobId(id))));
        }

        let mut paths: Vec<&String> = blobs
            .iter()
            .filter_map(|b| self.blob_paths.get(b)) // dead blobs have no paths
            .flatten()
            .collect();
        paths.extend(self.unindexed.iter());
        paths.sort_unstable();
        paths.dedup();
        Ok(paths)
    }
}

fn plan_for(pattern: &str) -> Result<Query> {
    plan(pattern, false)
}

fn compile(pattern: &str, case_insensitive: bool) -> Result<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .build()
        .map_err(|e| Error::InvalidArgument(format!("invalid regex: {e}")))
}

enum Outcome {
    /// Not examined because the deadline passed.
    Skipped,
    Unreadable,
    Scanned(Option<FileHit>),
}

fn verify_file(
    root: &Path,
    path: &str,
    meta: &FileMeta,
    re: &Regex,
    opts: &SearchOptions,
) -> Outcome {
    let abs = root.join(path);
    let Ok(bytes) = fs::read(&abs) else {
        return Outcome::Unreadable;
    };
    // Exact: compare the content hash, so a same-size, same-mtime edit is still caught.
    let stale = BlobId::of(&bytes) != meta.blob;
    verify_bytes(path, &bytes, stale, re, opts)
}

fn verify_bytes(
    path: &str,
    bytes: &[u8],
    stale: bool,
    re: &Regex,
    opts: &SearchOptions,
) -> Outcome {
    let (match_count, snippets) = find_matches(re, bytes, opts);
    Outcome::Scanned((match_count > 0).then(|| FileHit {
        path: path.to_string(),
        match_count,
        snippets,
        stale,
    }))
}

/// Count every non-empty match; build snippets for the first `max_matches_per_file`.
pub(crate) fn find_matches(
    re: &Regex,
    bytes: &[u8],
    opts: &SearchOptions,
) -> (usize, Vec<Snippet>) {
    let mut count = 0usize;
    let mut kept: Vec<(usize, usize)> = Vec::new();
    for m in re.find_iter(bytes) {
        if m.is_empty() {
            continue;
        }
        count += 1;
        if kept.len() < opts.max_matches_per_file {
            kept.push((m.start(), m.end()));
        }
    }
    if kept.is_empty() {
        return (count, Vec::new());
    }

    // line_starts[i] = byte offset of line i (0-based); a trailing newline does not open a line.
    let mut line_starts = vec![0usize];
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' && i + 1 < bytes.len() {
            line_starts.push(i + 1);
        }
    }
    let line_of = |offset: usize| line_starts.partition_point(|&s| s <= offset) - 1;
    let last_line = line_starts.len() - 1;

    let mut ranges: Vec<(usize, usize)> = Vec::new(); // 0-based inclusive line ranges
    for (start, end) in kept {
        let first = line_of(start);
        // A match that ends with a newline does not extend onto the next line.
        let tail = if end > start && bytes[end - 1] == b'\n' {
            end - 1
        } else {
            end
        };
        let last = line_of(tail.max(start));
        let lo = first.saturating_sub(opts.context_lines);
        let hi = (last + opts.context_lines).min(last_line);
        match ranges.last_mut() {
            Some(prev) if lo <= prev.1 + 1 => prev.1 = prev.1.max(hi),
            _ => ranges.push((lo, hi)),
        }
    }

    let snippets = ranges
        .into_iter()
        .map(|(lo, hi)| {
            let lines: Vec<String> = (lo..=hi)
                .map(|i| {
                    let from = line_starts[i];
                    let to = line_starts.get(i + 1).copied().unwrap_or(bytes.len());
                    render_line(&bytes[from..to])
                })
                .collect();
            Snippet {
                start_line: lo + 1,
                end_line: hi + 1,
                text: lines.join("\n"),
            }
        })
        .collect();
    (count, snippets)
}

fn render_line(raw: &[u8]) -> String {
    let raw = raw.strip_suffix(b"\n").unwrap_or(raw);
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let text = String::from_utf8_lossy(raw);
    if text.len() <= MAX_LINE_BYTES {
        return text.into_owned();
    }
    let mut cut = MAX_LINE_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(ctx: usize, max: usize) -> SearchOptions {
        SearchOptions {
            context_lines: ctx,
            max_matches_per_file: max,
            ..SearchOptions::default()
        }
    }

    fn matches(pattern: &str, text: &str, o: &SearchOptions) -> (usize, Vec<Snippet>) {
        find_matches(&compile(pattern, false).unwrap(), text.as_bytes(), o)
    }

    #[test]
    fn snippet_lines_and_context() {
        let (n, s) = matches("needle", "one\ntwo\nneedle here\nfour\nfive\n", &opts(1, 5));
        assert_eq!(n, 1);
        assert_eq!(
            s,
            [Snippet {
                start_line: 2,
                end_line: 4,
                text: "two\nneedle here\nfour".into()
            }]
        );
    }

    #[test]
    fn context_is_clipped_at_file_edges() {
        let (_, s) = matches("a", "a\nb\n", &opts(5, 5));
        assert_eq!((s[0].start_line, s[0].end_line), (1, 2));
    }

    #[test]
    fn nearby_matches_merge_and_distant_ones_do_not() {
        let text = "x\nhit\nx\nhit\nx\nx\nx\nx\nx\nhit\n";
        let (n, s) = matches("hit", text, &opts(1, 5));
        assert_eq!(n, 3);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].start_line, s[0].end_line), (1, 5));
        assert_eq!((s[1].start_line, s[1].end_line), (9, 10));
    }

    #[test]
    fn count_is_full_even_when_snippets_are_capped() {
        let text = "hit\n".repeat(20);
        let (n, s) = matches("hit", &text, &opts(0, 3));
        assert_eq!(n, 20);
        assert_eq!(
            s.iter()
                .map(|x| x.end_line - x.start_line + 1)
                .sum::<usize>(),
            3
        );
    }

    #[test]
    fn multiline_match_spans_lines() {
        let (n, s) = matches(r"foo\nbar", "a\nfoo\nbar\nb\n", &opts(0, 5));
        assert_eq!(n, 1);
        assert_eq!((s[0].start_line, s[0].end_line), (2, 3));
    }

    #[test]
    fn empty_matches_are_ignored() {
        let (n, s) = matches("x*", "abc\n", &opts(0, 5));
        assert_eq!(n, 0);
        assert!(s.is_empty());
    }

    #[test]
    fn crlf_and_long_lines_render_cleanly() {
        let long = format!("hit {}\r\n", "é".repeat(400));
        let (_, s) = matches("hit", &long, &opts(0, 5));
        assert!(s[0].text.ends_with('…'));
        assert!(!s[0].text.contains('\r'));
        assert!(s[0].text.len() <= MAX_LINE_BYTES + '…'.len_utf8());
    }

    #[test]
    fn non_utf8_content_is_searchable() {
        let mut bytes = b"head \xff\xfe needle tail\n".to_vec();
        bytes.extend_from_slice(b"more\n");
        let re = compile("needle", false).unwrap();
        let (n, s) = find_matches(&re, &bytes, &opts(0, 5));
        assert_eq!(n, 1);
        assert!(s[0].text.contains("needle"));
    }
}
