use std::fs;
use std::path::Path;

use loupe_core::{Error, Result, SegmentId};
use serde::{Deserialize, Serialize};

use crate::fsio::{atomic_write, io};

pub const MANIFEST_FILE: &str = "manifest.json";
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// Pointer to the current snapshot and the live segment set.
///
/// `generation` doubles as the snapshot id: generation `g > 0` has its snapshot in
/// `snapshots/<g>.snap`, and generation 0 is the empty index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub generation: u64,
    /// First segment id that has never been handed out.
    pub next_segment_id: u64,
    /// Live segments, in commit order.
    pub segments: Vec<u64>,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            generation: 0,
            next_segment_id: 1,
            segments: Vec::new(),
        }
    }
}

impl Manifest {
    pub fn segment_ids(&self) -> Vec<SegmentId> {
        self.segments.iter().map(|&s| SegmentId(s)).collect()
    }

    /// A missing manifest is the empty index; an unparseable one is corruption.
    pub(crate) fn load(index_dir: &Path) -> Result<Manifest> {
        let path = index_dir.join(MANIFEST_FILE);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Manifest::default()),
            Err(e) => return Err(Error::io(&path, e)),
        };
        let m: Manifest = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Corrupt(format!("{}: {e}", path.display())))?;
        if m.format_version != MANIFEST_FORMAT_VERSION {
            return Err(Error::FormatVersion {
                found: m.format_version,
                expected: MANIFEST_FORMAT_VERSION,
            });
        }
        Ok(m)
    }

    pub(crate) fn save(&self, index_dir: &Path) -> Result<()> {
        let path = index_dir.join(MANIFEST_FILE);
        let bytes = io(&path, serde_json::to_vec_pretty(self).map_err(Into::into))?;
        atomic_write(&path, &bytes)
    }
}
