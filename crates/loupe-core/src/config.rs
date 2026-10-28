use std::path::PathBuf;

/// Name of the index directory created under the repo root.
pub const INDEX_DIR: &str = ".loupe";
/// Extra ignore file honored alongside `.gitignore`.
pub const IGNORE_FILE: &str = ".loupeignore";

#[derive(Clone, Debug)]
pub struct Config {
    /// Repository root being indexed.
    pub root: PathBuf,
    /// Where the index lives. Defaults to `<root>/.loupe`.
    pub index_dir: PathBuf,
    /// Files larger than this are skipped.
    pub max_file_bytes: u64,
    /// Index files that look like secrets (`.env`, private keys, ...). Off by default because
    /// indexed content ends up in LLM context.
    pub index_secrets: bool,
    /// Source bytes indexed per segment. Larger updates are split into several segments, which
    /// bounds the memory an update needs (postings and symbol tables are built in memory per
    /// segment) at the cost of more segments, which compaction later merges.
    pub max_segment_bytes: u64,
}

impl Config {
    pub const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;
    pub const DEFAULT_MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let index_dir = root.join(INDEX_DIR);
        Config {
            root,
            index_dir,
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
            index_secrets: false,
            max_segment_bytes: Self::DEFAULT_MAX_SEGMENT_BYTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_dir_defaults_under_root() {
        let c = Config::new("/repo");
        assert_eq!(c.index_dir, PathBuf::from("/repo/.loupe"));
        assert_eq!(c.max_file_bytes, 1024 * 1024);
        assert!(!c.index_secrets);
        assert_eq!(c.max_segment_bytes, 64 * 1024 * 1024);
    }
}
