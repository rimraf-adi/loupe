//! Vector cache keyed by chunk hash (which already includes the model id and chunker version).
//!
//! Unchanged chunks are never re-embedded, even across segments, file moves, and renames.
//! Backed by `redb`, with an in-memory fallback if another process holds the file.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::{Database, ReadableDatabase, TableDefinition};

/// Cap on redb's in-memory page cache (its default is 1 GiB).
const CACHE_BYTES: usize = 32 * 1024 * 1024;

const VECTORS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("vectors");

pub type ChunkHash = [u8; 16];

/// A cached vector: int8 values plus the scale that restores them.
#[derive(Clone, Debug, PartialEq)]
pub struct QuantVec {
    pub scale: f32,
    pub values: Vec<i8>,
}

impl QuantVec {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.values.len());
        out.extend_from_slice(&self.scale.to_le_bytes());
        out.extend(self.values.iter().map(|&v| v as u8));
        out
    }

    fn decode(bytes: &[u8], dim: usize) -> Option<QuantVec> {
        if bytes.len() != 4 + dim {
            return None; // wrong model dimension or corrupt entry: treat as a miss
        }
        Some(QuantVec {
            scale: f32::from_le_bytes(bytes[..4].try_into().ok()?),
            values: bytes[4..].iter().map(|&b| b as i8).collect(),
        })
    }
}

enum Backend {
    Disk(Database),
    Memory(Mutex<HashMap<ChunkHash, Vec<u8>>>),
}

pub struct VectorCache {
    backend: Backend,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl std::fmt::Debug for VectorCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorCache")
            .field("persistent", &self.is_persistent())
            .field("hits", &self.hits())
            .field("misses", &self.misses())
            .finish()
    }
}

impl VectorCache {
    pub fn in_memory() -> VectorCache {
        VectorCache {
            backend: Backend::Memory(Mutex::new(HashMap::new())),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn open(path: &Path) -> VectorCache {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match Database::builder().set_cache_size(CACHE_BYTES).create(path) {
            Ok(db) => VectorCache {
                backend: Backend::Disk(db),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
            },
            Err(_) => VectorCache::in_memory(),
        }
    }

    pub fn is_persistent(&self) -> bool {
        matches!(self.backend, Backend::Disk(_))
    }

    /// `dim` guards against a stale entry written by a model with a different dimension.
    pub fn get(&self, hash: &ChunkHash, dim: usize) -> Option<QuantVec> {
        let found = match &self.backend {
            Backend::Memory(m) => m
                .lock()
                .unwrap()
                .get(hash)
                .and_then(|b| QuantVec::decode(b, dim)),
            Backend::Disk(db) => (|| {
                let tx = db.begin_read().ok()?;
                let table = tx.open_table(VECTORS).ok()?;
                let guard = table.get(hash.as_slice()).ok()??;
                QuantVec::decode(guard.value(), dim)
            })(),
        };
        let counter = if found.is_some() {
            &self.hits
        } else {
            &self.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        found
    }

    /// Store many vectors in one transaction. Best effort: a failure only means re-embedding.
    pub fn put_many(&self, entries: &[(ChunkHash, QuantVec)]) {
        match &self.backend {
            Backend::Memory(m) => {
                let mut m = m.lock().unwrap();
                for (h, v) in entries {
                    m.insert(*h, v.encode());
                }
            }
            Backend::Disk(db) => {
                let _ = (|| -> Result<(), redb::Error> {
                    let tx = db.begin_write()?;
                    {
                        let mut table = tx.open_table(VECTORS)?;
                        for (h, v) in entries {
                            table.insert(h.as_slice(), v.encode().as_slice())?;
                        }
                    }
                    tx.commit()?;
                    Ok(())
                })();
            }
        }
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qv(seed: i8) -> QuantVec {
        QuantVec {
            scale: 0.5,
            values: (0..8).map(|i| seed.wrapping_add(i)).collect(),
        }
    }

    fn exercise(c: &VectorCache) {
        let (a, b) = ([1u8; 16], [2u8; 16]);
        assert_eq!(c.get(&a, 8), None);
        c.put_many(&[(a, qv(1)), (b, qv(50))]);
        assert_eq!(c.get(&a, 8), Some(qv(1)));
        assert_eq!(c.get(&b, 8), Some(qv(50)));
        assert_eq!(
            c.get(&a, 16),
            None,
            "a different dimension is a miss, not a bad vector"
        );
        c.put_many(&[(a, qv(9))]);
        assert_eq!(c.get(&a, 8), Some(qv(9)));
        assert_eq!((c.hits(), c.misses()), (3, 2));
        let neg = QuantVec {
            scale: 1.0,
            values: vec![-127, 0, 127, -1, 1, 2, 3, 4],
        };
        c.put_many(&[([3u8; 16], neg.clone())]);
        assert_eq!(c.get(&[3u8; 16], 8), Some(neg), "negative values survive");
    }

    #[test]
    fn memory_and_disk_behave_the_same_and_disk_persists() {
        exercise(&VectorCache::in_memory());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache/embeddings.redb");
        let disk = VectorCache::open(&path);
        assert!(disk.is_persistent());
        exercise(&disk);
        drop(disk);
        let again = VectorCache::open(&path);
        assert_eq!(again.get(&[2u8; 16], 8), Some(qv(50)));
    }

    #[test]
    fn a_locked_file_falls_back_to_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.redb");
        let first = VectorCache::open(&path);
        let second = VectorCache::open(&path);
        assert!(first.is_persistent() && !second.is_persistent());
        exercise(&second);
    }
}
