use std::collections::HashMap;

use loupe_core::{BlobId, LocalBlobId, Result, Snapshot};

use crate::segment::Segment;
use crate::store::Store;

/// A consistent, read-only view of one generation: the snapshot, its open (verified) segments,
/// and the maps every searcher needs to turn segment hits into paths.
pub struct SnapshotView {
    snapshot_id: u64,
    snapshot: Snapshot,
    segments: Vec<Segment>,
    /// Live blob -> the paths holding it, sorted.
    blob_paths: HashMap<BlobId, Vec<String>>,
    /// Blob -> first segment (index into `segments`) and local id where it is indexed.
    blob_home: HashMap<BlobId, (usize, LocalBlobId)>,
}

impl std::fmt::Debug for SnapshotView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotView")
            .field("snapshot_id", &self.snapshot_id)
            .field("files", &self.snapshot.len())
            .field("segments", &self.segments.len())
            .finish()
    }
}

impl SnapshotView {
    /// Load the store's current snapshot and verify/open every live segment. A corrupt segment
    /// is an error; recovery (dropping and re-indexing it) is the writer's decision.
    pub fn open(store: &Store) -> Result<SnapshotView> {
        let snapshot = store.load_snapshot()?;
        let segments = store
            .segment_ids()
            .into_iter()
            .map(|id| store.open_segment(id))
            .collect::<Result<Vec<_>>>()?;

        let mut blob_home = HashMap::new();
        for (i, seg) in segments.iter().enumerate() {
            for (local, blob) in seg.blobs().iter().enumerate() {
                blob_home
                    .entry(*blob)
                    .or_insert((i, LocalBlobId(local as u32)));
            }
        }
        let mut blob_paths: HashMap<BlobId, Vec<String>> = HashMap::new();
        for (path, meta) in &snapshot.files {
            blob_paths.entry(meta.blob).or_default().push(path.clone());
        }
        Ok(SnapshotView {
            snapshot_id: store.snapshot_id(),
            snapshot,
            segments,
            blob_paths,
            blob_home,
        })
    }

    pub fn snapshot_id(&self) -> u64 {
        self.snapshot_id
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Paths of a live blob (empty for dead or unknown blobs).
    pub fn paths_of(&self, blob: &BlobId) -> &[String] {
        self.blob_paths.get(blob).map_or(&[], Vec::as_slice)
    }

    /// Where a blob is indexed, if anywhere.
    pub fn home_of(&self, blob: &BlobId) -> Option<(usize, LocalBlobId)> {
        self.blob_home.get(blob).copied()
    }
}
