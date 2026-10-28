use std::collections::{BTreeMap, HashSet};

use crate::BlobId;

/// What the index knows about one file at snapshot time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    /// Modification time, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    pub blob: BlobId,
    pub lang: Option<String>,
}

/// A consistent view of the repo: `path -> FileMeta`.
///
/// Paths are relative to the repo root, `/`-separated. The map is ordered, so iteration
/// order (and therefore dense `FileId` assignment) is deterministic.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// When the scan that produced this snapshot started, nanoseconds since the Unix epoch.
    /// Used to avoid trusting `(size, mtime)` for files modified too close to the scan.
    pub taken_ns: i64,
    pub files: BTreeMap<String, FileMeta>,
}

impl Snapshot {
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn get(&self, path: &str) -> Option<&FileMeta> {
        self.files.get(path)
    }

    /// The set of live blobs: a blob is live iff some path maps to it.
    pub fn live_blobs(&self) -> HashSet<BlobId> {
        self.files.values().map(|m| m.blob).collect()
    }
}
