use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum CacheStatus {
    Unchanged { hash: String },
    ModifiedOrNew { hash: String },
}

#[derive(Default)]
pub struct SessionCache {
    entries: HashMap<PathBuf, String>,
}

impl SessionCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Computes the blake3 hash of content, checks if it was already returned in this session.
    /// If unchanged, returns `CacheStatus::Unchanged`.
    /// Otherwise records the new hash and returns `CacheStatus::ModifiedOrNew`.
    pub fn check_and_update(&mut self, path: &Path, content: &str) -> CacheStatus {
        let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
        let canonical_or_raw = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        if let Some(existing_hash) = self.entries.get(&canonical_or_raw) {
            if existing_hash == &hash {
                return CacheStatus::Unchanged { hash };
            }
        }

        self.entries.insert(canonical_or_raw, hash.clone());
        CacheStatus::ModifiedOrNew { hash }
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_dedup() {
        let mut cache = SessionCache::new();
        let p = Path::new("dummy.rs");
        let status1 = cache.check_and_update(p, "fn main() {}");
        assert!(matches!(status1, CacheStatus::ModifiedOrNew { .. }));

        let status2 = cache.check_and_update(p, "fn main() {}");
        assert!(matches!(status2, CacheStatus::Unchanged { .. }));

        let status3 = cache.check_and_update(p, "fn main() { println!(\"hi\"); }");
        assert!(matches!(status3, CacheStatus::ModifiedOrNew { .. }));
    }
}
