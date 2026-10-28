//! Summary cache keyed by `(level, merkle hash, summarizer version)`.
//!
//! Backed by `redb` on disk, with an in-memory fallback if the file cannot be opened (for
//! example when a daemon already holds it), so callers never fail because of the cache.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::{Database, ReadableDatabase, TableDefinition};

/// Cap on redb's in-memory page cache (its default is 1 GiB).
const CACHE_BYTES: usize = 32 * 1024 * 1024;

use crate::merkle::Hash;

const SUMMARIES: TableDefinition<&[u8], &str> = TableDefinition::new("summaries");

enum Backend {
    Disk(Database),
    Memory(Mutex<HashMap<Vec<u8>, String>>),
}

pub struct SummaryCache {
    backend: Backend,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl std::fmt::Debug for SummaryCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SummaryCache")
            .field("persistent", &self.is_persistent())
            .field("hits", &self.hits())
            .field("misses", &self.misses())
            .finish()
    }
}

fn key(level: u8, hash: &Hash, version: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + 32 + version.len());
    k.push(level);
    k.extend_from_slice(hash);
    k.extend_from_slice(version.as_bytes());
    k
}

impl SummaryCache {
    pub fn in_memory() -> SummaryCache {
        SummaryCache {
            backend: Backend::Memory(Mutex::new(HashMap::new())),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Open (creating if needed) the on-disk cache; fall back to memory if that fails.
    pub fn open(path: &Path) -> SummaryCache {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match Database::builder().set_cache_size(CACHE_BYTES).create(path) {
            Ok(db) => SummaryCache {
                backend: Backend::Disk(db),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
            },
            Err(_) => SummaryCache::in_memory(),
        }
    }

    pub fn is_persistent(&self) -> bool {
        matches!(self.backend, Backend::Disk(_))
    }

    pub fn get(&self, level: u8, hash: &Hash, version: &str) -> Option<String> {
        let k = key(level, hash, version);
        let found = match &self.backend {
            Backend::Memory(m) => m.lock().unwrap().get(&k).cloned(),
            Backend::Disk(db) => (|| {
                let tx = db.begin_read().ok()?;
                let table = tx.open_table(SUMMARIES).ok()?;
                table.get(k.as_slice()).ok()?.map(|v| v.value().to_string())
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

    /// Best effort: a failed write only means the summary is recomputed next time.
    pub fn put(&self, level: u8, hash: &Hash, version: &str, text: &str) {
        let k = key(level, hash, version);
        match &self.backend {
            Backend::Memory(m) => {
                m.lock().unwrap().insert(k, text.to_string());
            }
            Backend::Disk(db) => {
                let _ = (|| -> Result<(), redb::Error> {
                    let tx = db.begin_write()?;
                    {
                        let mut table = tx.open_table(SUMMARIES)?;
                        table.insert(k.as_slice(), text)?;
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

    fn exercise(c: &SummaryCache) {
        let h1 = [1u8; 32];
        let h2 = [2u8; 32];
        assert_eq!(c.get(1, &h1, "v1"), None);
        c.put(1, &h1, "v1", "summary one");
        assert_eq!(c.get(1, &h1, "v1").as_deref(), Some("summary one"));
        // Level, hash, and version are all part of the key.
        assert_eq!(c.get(2, &h1, "v1"), None);
        assert_eq!(c.get(1, &h2, "v1"), None);
        assert_eq!(c.get(1, &h1, "v2"), None);
        c.put(1, &h1, "v1", "replaced");
        assert_eq!(c.get(1, &h1, "v1").as_deref(), Some("replaced"));
        assert_eq!((c.hits(), c.misses()), (2, 4));
    }

    #[test]
    fn memory_cache() {
        exercise(&SummaryCache::in_memory());
    }

    #[test]
    fn disk_cache_persists_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache/summaries.redb");
        let c = SummaryCache::open(&path);
        assert!(c.is_persistent());
        exercise(&c);
        drop(c);
        let again = SummaryCache::open(&path);
        assert_eq!(again.get(1, &[1u8; 32], "v1").as_deref(), Some("replaced"));
    }

    #[test]
    fn unopenable_path_falls_back_to_memory() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the database file should be.
        let c = SummaryCache::open(dir.path());
        assert!(!c.is_persistent());
        exercise(&c);
    }

    #[test]
    fn a_second_handle_on_a_locked_file_falls_back_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.redb");
        let first = SummaryCache::open(&path);
        assert!(first.is_persistent());
        let second = SummaryCache::open(&path);
        assert!(
            !second.is_persistent(),
            "redb allows one open handle per file"
        );
        second.put(1, &[9u8; 32], "v", "x");
        assert_eq!(second.get(1, &[9u8; 32], "v").as_deref(), Some("x"));
    }
}
