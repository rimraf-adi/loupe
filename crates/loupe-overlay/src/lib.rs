//! In-memory overlay of unsaved or just-changed files.
//!
//! Queries merge the overlay over the indexed snapshot: for any path present here the overlay
//! wins, whether it holds new bytes or a removal. The overlay is small by design, so searchers
//! scan its files directly instead of keeping a separate trigram index for it.
//!
//! Every mutation gets a monotonically increasing sequence number. A background re-index records
//! the sequence when its scan starts and afterwards calls [`Overlay::drop_through`], which
//! discards exactly the entries the scan can have observed; later edits survive.

use std::collections::HashMap;
use std::sync::Arc;

use loupe_core::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// The file's current content.
    Put(Arc<[u8]>),
    /// The file no longer exists.
    Removed,
}

#[derive(Clone, Debug)]
pub struct Overlay {
    entries: HashMap<String, (u64, Entry)>,
    seq: u64,
    bytes: usize,
    max_bytes: usize,
}

impl Default for Overlay {
    fn default() -> Self {
        Overlay::with_limit(Self::DEFAULT_MAX_BYTES)
    }
}

impl Overlay {
    pub const DEFAULT_MAX_BYTES: usize = 256 * 1024 * 1024;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limit(max_bytes: usize) -> Self {
        Overlay {
            entries: HashMap::new(),
            seq: 0,
            bytes: 0,
            max_bytes,
        }
    }

    /// Record `path`'s new content. Fails (changing nothing) if it would exceed the byte limit.
    pub fn put(&mut self, path: impl Into<String>, content: impl Into<Arc<[u8]>>) -> Result<u64> {
        let path = path.into();
        let content: Arc<[u8]> = content.into();
        let old = self.entry_bytes(&path);
        if self.bytes - old + content.len() > self.max_bytes {
            return Err(Error::InvalidArgument(format!(
                "overlay limit of {} bytes exceeded",
                self.max_bytes
            )));
        }
        self.bytes = self.bytes - old + content.len();
        Ok(self.insert(path, Entry::Put(content)))
    }

    /// Record that `path` was deleted.
    pub fn remove(&mut self, path: impl Into<String>) -> u64 {
        let path = path.into();
        self.bytes -= self.entry_bytes(&path);
        self.insert(path, Entry::Removed)
    }

    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.get(path).map(|(_, e)| e)
    }

    pub fn is_removed(&self, path: &str) -> bool {
        matches!(self.get(path), Some(Entry::Removed))
    }

    /// Files with live content, sorted by path.
    pub fn puts(&self) -> Vec<(&str, &Arc<[u8]>)> {
        let mut v: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(p, (_, e))| match e {
                Entry::Put(b) => Some((p.as_str(), b)),
                Entry::Removed => None,
            })
            .collect();
        v.sort_unstable_by_key(|(p, _)| *p);
        v
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Sequence number of the latest mutation (0 if none yet).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Discard entries last modified at or before `seq`.
    pub fn drop_through(&mut self, seq: u64) {
        self.entries.retain(|_, (s, _)| *s > seq);
        self.bytes = self.entries.values().map(|(_, e)| entry_len(e)).sum();
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn entry_bytes(&self, path: &str) -> usize {
        self.entries.get(path).map_or(0, |(_, e)| entry_len(e))
    }

    fn insert(&mut self, path: String, entry: Entry) -> u64 {
        self.seq += 1;
        self.entries.insert(path, (self.seq, entry));
        self.seq
    }
}

fn entry_len(e: &Entry) -> usize {
    match e {
        Entry::Put(b) => b.len(),
        Entry::Removed => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(s: &str) -> Arc<[u8]> {
        Arc::from(s.as_bytes())
    }

    #[test]
    fn put_overwrite_remove() {
        let mut o = Overlay::new();
        o.put("a", bytes("one")).unwrap();
        o.put("b", bytes("two")).unwrap();
        o.put("a", bytes("three")).unwrap();
        assert_eq!(o.get("a"), Some(&Entry::Put(bytes("three"))));
        assert_eq!(o.bytes(), 8);
        o.remove("b");
        assert!(o.is_removed("b"));
        assert_eq!(o.bytes(), 5);
        assert_eq!(o.puts().len(), 1);
        assert_eq!(o.len(), 2);
    }

    #[test]
    fn puts_are_sorted() {
        let mut o = Overlay::new();
        for p in ["z", "a", "m"] {
            o.put(p, bytes("x")).unwrap();
        }
        let paths: Vec<&str> = o.puts().into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths, ["a", "m", "z"]);
    }

    #[test]
    fn byte_limit_is_enforced_and_atomic() {
        let mut o = Overlay::with_limit(10);
        o.put("a", bytes("123456")).unwrap();
        assert!(o.put("b", bytes("12345")).is_err());
        assert_eq!(o.len(), 1, "failed put changes nothing");
        // Replacing an entry frees its old bytes first.
        o.put("a", bytes("1234567890")).unwrap();
        assert_eq!(o.bytes(), 10);
    }

    #[test]
    fn drop_through_keeps_later_edits() {
        let mut o = Overlay::new();
        o.put("old", bytes("x")).unwrap();
        let scan_started = o.seq();
        o.put("new", bytes("y")).unwrap();
        o.put("old", bytes("z")).unwrap(); // edited after the scan began
        o.remove("gone");
        o.drop_through(scan_started);
        assert_eq!(o.get("old"), Some(&Entry::Put(bytes("z"))));
        assert_eq!(o.get("new"), Some(&Entry::Put(bytes("y"))));
        assert!(o.is_removed("gone"));

        o.drop_through(o.seq());
        assert!(o.is_empty());
        assert_eq!(o.bytes(), 0);
    }
}
