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
    /// Hard cap on the total size of files admitted to the index. When a tree exceeds it, source
    /// files (known languages) are kept before other text, smaller files before larger ones, and
    /// the rest are skipped and reported. This is the guard against huge unignored build trees.
    pub max_total_bytes: u64,
    /// Hard cap on the number of indexed files (same priority rules as `max_total_bytes`).
    pub max_files: usize,
    /// Skip well-known build, cache and dependency directories (`node_modules`, Cargo `target/`,
    /// anything marked with `CACHEDIR.TAG`, virtualenvs, ...) without descending into them, and
    /// skip files with binary extensions without opening them.
    pub default_excludes: bool,
}

impl Config {
    pub const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;
    pub const DEFAULT_MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
    pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
    pub const DEFAULT_MAX_FILES: usize = 500_000;

    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let index_dir = root.join(INDEX_DIR);
        Config {
            root,
            index_dir,
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
            index_secrets: false,
            max_segment_bytes: Self::DEFAULT_MAX_SEGMENT_BYTES,
            max_total_bytes: Self::DEFAULT_MAX_TOTAL_BYTES,
            max_files: Self::DEFAULT_MAX_FILES,
            default_excludes: true,
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
        assert_eq!(c.max_total_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(c.max_files, 500_000);
        assert!(c.default_excludes);
    }
}
