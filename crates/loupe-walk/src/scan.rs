use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ignore::WalkBuilder;
use loupe_core::config::IGNORE_FILE;
use loupe_core::{BlobId, Config, Error, FileMeta, Result, Snapshot};
use rayon::prelude::*;

use crate::diff::{Diff, diff};
use crate::lang::detect_language;
use crate::secrets::is_secret_path;

/// Bytes read first to sniff for NUL (binary) and a shebang.
const SNIFF_BYTES: usize = 8192;
/// A file modified within this window of the previous scan is re-hashed even if `(size, mtime)`
/// match: a second write inside the same timestamp tick would otherwise go unnoticed.
const RACY_MARGIN_NS: i64 = 2_000_000_000;
const MINIFIED_MIN_BYTES: usize = 2048;
const MINIFIED_AVG_LINE_BYTES: usize = 1000;

#[derive(Clone, Debug, Default)]
pub struct WalkOptions {
    /// Ignore `(size, mtime)` and re-hash every file. The daemon runs this periodically.
    pub force_rehash: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// Regular files that passed ignore rules.
    pub seen: usize,
    /// Files whose hash came from the previous snapshot without reading them.
    pub reused: usize,
    /// Files that were read and hashed.
    pub hashed: usize,
    pub skipped_large: usize,
    pub skipped_binary: usize,
    pub skipped_minified: usize,
    pub skipped_secret: usize,
    pub skipped_symlink: usize,
    /// Unreadable entries, non-UTF-8 paths, and files that vanished mid-scan.
    pub errors: usize,
}

#[derive(Clone, Debug)]
pub struct Scan {
    pub snapshot: Snapshot,
    /// Changes relative to the `prev` snapshot passed to [`scan`].
    pub diff: Diff,
    pub stats: WalkStats,
}

struct Candidate {
    abs: PathBuf,
    rel: String,
    size: u64,
    mtime_ns: i64,
}

enum Outcome {
    Reused(String, FileMeta),
    Hashed(String, FileMeta),
    Binary,
    Minified,
    Failed,
}

/// Walk the repo, hash changed files, and diff against `prev`.
///
/// Honors `.gitignore` (without requiring a `.git` directory), `.ignore`, and `.loupeignore`.
/// Hidden files are included; `.git` and the index directory are not. Symlinks are never
/// followed or indexed, which also guarantees nothing outside the root is read.
pub fn scan(config: &Config, prev: Option<&Snapshot>, opts: &WalkOptions) -> Result<Scan> {
    let root = &config.root;
    if !root.is_dir() {
        return Err(Error::InvalidArgument(format!(
            "root is not a directory: {}",
            root.display()
        )));
    }
    let taken_ns = now_ns();
    let mut stats = WalkStats::default();

    let index_dir = config.index_dir.clone();
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .hidden(false)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .parents(false)
        .require_git(false)
        .follow_links(false)
        .add_custom_ignore_filename(IGNORE_FILE)
        .filter_entry(move |e| e.file_name() != ".git" && e.path() != index_dir);

    let mut candidates = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(%err, "walk error");
                stats.errors += 1;
                continue;
            }
        };
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            stats.skipped_symlink += 1;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let Some(rel) = rel_path(root, entry.path()) else {
            tracing::warn!(path = %entry.path().display(), "skipping non-UTF-8 path");
            stats.errors += 1;
            continue;
        };
        stats.seen += 1;
        if !config.index_secrets && is_secret_path(&rel) {
            stats.skipped_secret += 1;
            continue;
        }
        let md = match entry.metadata() {
            Ok(md) => md,
            Err(err) => {
                tracing::warn!(%err, path = %rel, "stat failed");
                stats.errors += 1;
                continue;
            }
        };
        if md.len() > config.max_file_bytes {
            stats.skipped_large += 1;
            continue;
        }
        candidates.push(Candidate {
            abs: entry.into_path(),
            rel,
            size: md.len(),
            mtime_ns: md.modified().map(system_time_ns).unwrap_or(0),
        });
    }

    let prev_taken = prev.map(|p| p.taken_ns);
    let outcomes: Vec<Outcome> = candidates
        .par_iter()
        .map(|c| process(c, prev, prev_taken, opts))
        .collect();

    let mut snapshot = Snapshot {
        taken_ns,
        ..Snapshot::default()
    };
    for outcome in outcomes {
        match outcome {
            Outcome::Reused(path, meta) => {
                stats.reused += 1;
                snapshot.files.insert(path, meta);
            }
            Outcome::Hashed(path, meta) => {
                stats.hashed += 1;
                snapshot.files.insert(path, meta);
            }
            Outcome::Binary => stats.skipped_binary += 1,
            Outcome::Minified => stats.skipped_minified += 1,
            Outcome::Failed => stats.errors += 1,
        }
    }

    let diff = diff(prev, &snapshot);
    Ok(Scan {
        snapshot,
        diff,
        stats,
    })
}

fn process(
    c: &Candidate,
    prev: Option<&Snapshot>,
    prev_taken: Option<i64>,
    opts: &WalkOptions,
) -> Outcome {
    if !opts.force_rehash
        && let (Some(prev), Some(prev_taken)) = (prev, prev_taken)
        && let Some(old) = prev.files.get(&c.rel)
        && old.size == c.size
        && old.mtime_ns == c.mtime_ns
        && c.mtime_ns < prev_taken - RACY_MARGIN_NS
    {
        return Outcome::Reused(c.rel.clone(), old.clone());
    }

    match read_and_classify(&c.abs) {
        Ok(Some(bytes)) => {
            let head = &bytes[..bytes.len().min(256)];
            let meta = FileMeta {
                size: bytes.len() as u64,
                mtime_ns: c.mtime_ns,
                blob: BlobId::of(&bytes),
                lang: detect_language(&c.rel, head).map(str::to_string),
            };
            Outcome::Hashed(c.rel.clone(), meta)
        }
        Ok(None) => Outcome::Binary,
        Err(ReadSkip::Minified) => Outcome::Minified,
        Err(ReadSkip::Io(err)) => {
            tracing::warn!(%err, path = %c.rel, "read failed");
            Outcome::Failed
        }
    }
}

enum ReadSkip {
    Minified,
    Io(std::io::Error),
}

/// `Ok(Some(bytes))` for indexable text, `Ok(None)` for binary.
fn read_and_classify(path: &Path) -> std::result::Result<Option<Vec<u8>>, ReadSkip> {
    let mut file = File::open(path).map_err(ReadSkip::Io)?;
    let mut buf = Vec::new();
    (&mut file)
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut buf)
        .map_err(ReadSkip::Io)?;
    if buf.contains(&0) {
        return Ok(None);
    }
    if buf.len() == SNIFF_BYTES {
        file.read_to_end(&mut buf).map_err(ReadSkip::Io)?;
    }
    if is_minified(&buf) {
        return Err(ReadSkip::Minified);
    }
    Ok(Some(buf))
}

/// True if the file at `root/path` no longer has the content the snapshot recorded for it (edited,
/// deleted, or unreadable). Exact: it compares blob hashes, so an edit that keeps the same size
/// and modification time is still caught, which a `stat` comparison cannot do.
pub fn is_stale(root: &Path, path: &str, meta: &FileMeta) -> bool {
    match std::fs::read(root.join(path)) {
        Ok(bytes) => BlobId::of(&bytes) != meta.blob,
        Err(_) => true,
    }
}

/// The same content checks the scanner applies: no NUL byte in the first 8 KiB (binary) and not
/// minified. Used by the daemon's fast path so overlay admission matches the indexer's.
pub fn is_indexable_content(bytes: &[u8]) -> bool {
    !bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0) && !is_minified(bytes)
}

fn is_minified(bytes: &[u8]) -> bool {
    if bytes.len() < MINIFIED_MIN_BYTES {
        return false;
    }
    let lines = bytes.iter().filter(|&&b| b == b'\n').count() + 1;
    bytes.len() / lines >= MINIFIED_AVG_LINE_BYTES
}

fn rel_path(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let mut out = String::new();
    for comp in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(comp.as_os_str().to_str()?);
    }
    (!out.is_empty()).then_some(out)
}

fn system_time_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

fn now_ns() -> i64 {
    system_time_ns(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    struct Repo {
        dir: tempfile::TempDir,
    }

    impl Repo {
        fn new() -> Self {
            Repo {
                dir: tempfile::tempdir().unwrap(),
            }
        }
        fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
            let p = self.dir.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
        fn remove(&self, rel: &str) {
            fs::remove_file(self.dir.path().join(rel)).unwrap();
        }
        /// Make every file look old, so reuse is allowed (outside the racy window).
        fn age_all(&self) {
            let old = SystemTime::now() - Duration::from_secs(3600);
            for e in walk_all(self.dir.path()) {
                File::options()
                    .write(true)
                    .open(&e)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }
        }
        fn config(&self) -> Config {
            // Canonicalize: macOS tempdirs live under a /var -> /private/var symlink.
            Config::new(self.dir.path().canonicalize().unwrap())
        }
        fn scan(&self, prev: Option<&Snapshot>) -> Scan {
            scan(&self.config(), prev, &WalkOptions::default()).unwrap()
        }
    }

    fn walk_all(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in fs::read_dir(root).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.extend(walk_all(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    fn paths(s: &Scan) -> Vec<&str> {
        s.snapshot.files.keys().map(String::as_str).collect()
    }

    #[test]
    fn indexes_text_files_with_language() {
        let r = Repo::new();
        r.write("src/main.rs", "fn main() {}\n");
        r.write("README.md", "# hi\n");
        r.write("notes", "plain\n");
        let s = r.scan(None);
        assert_eq!(paths(&s), ["README.md", "notes", "src/main.rs"]);
        assert_eq!(
            s.snapshot.get("src/main.rs").unwrap().lang.as_deref(),
            Some("rust")
        );
        assert_eq!(s.snapshot.get("notes").unwrap().lang, None);
        assert_eq!(s.diff.added.len(), 3);
        assert_eq!(s.stats.hashed, 3);
    }

    #[test]
    fn honors_gitignore_without_git_dir_and_loupeignore() {
        let r = Repo::new();
        r.write(".gitignore", "target/\n*.log\n");
        r.write(".loupeignore", "vendor/\n");
        r.write("target/out.txt", "x");
        r.write("a.log", "x");
        r.write("vendor/lib.c", "x");
        r.write("keep.txt", "x");
        let s = r.scan(None);
        assert_eq!(paths(&s), [".gitignore", ".loupeignore", "keep.txt"]);
    }

    #[test]
    fn includes_hidden_but_not_git_or_index_dir() {
        let r = Repo::new();
        r.write(".github/workflows/ci.yml", "x");
        r.write(".git/HEAD", "ref");
        r.write(".loupe/manifest.json", "{}");
        let s = r.scan(None);
        assert_eq!(paths(&s), [".github/workflows/ci.yml"]);
    }

    #[test]
    fn skips_binary_large_minified() {
        let r = Repo::new();
        r.write("img.bin", [0x89, b'P', 0, 1, 2]);
        r.write("big.txt", vec![b'a'; 2000]);
        r.write("min.js", format!("{}\n", "x".repeat(5000)));
        r.write("ok.txt", "fine\n");
        let mut cfg = r.config();
        cfg.max_file_bytes = 1000;
        let s = scan(&cfg, None, &WalkOptions::default()).unwrap();
        assert_eq!(paths(&s), ["ok.txt"]);
        assert_eq!(s.stats.skipped_large, 2); // big.txt and min.js (5001 bytes)
        assert_eq!(s.stats.skipped_binary, 1);

        // With a generous cap, min.js is rejected by the minified heuristic instead.
        let s = r.scan(None);
        assert_eq!(paths(&s), ["big.txt", "ok.txt"]);
        assert_eq!(s.stats.skipped_minified, 1);
    }

    #[test]
    fn skips_secrets_unless_enabled() {
        let r = Repo::new();
        r.write(".env", "TOKEN=abc");
        r.write(".env.example", "TOKEN=");
        r.write("src/lib.rs", "");
        let s = r.scan(None);
        assert_eq!(paths(&s), [".env.example", "src/lib.rs"]);
        assert_eq!(s.stats.skipped_secret, 1);

        let mut cfg = r.config();
        cfg.index_secrets = true;
        let s = scan(&cfg, None, &WalkOptions::default()).unwrap();
        assert!(paths(&s).contains(&".env"));
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_symlinks() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "outside").unwrap();
        let r = Repo::new();
        r.write("real.txt", "inside");
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            r.dir.path().join("link.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), r.dir.path().join("linkdir")).unwrap();
        let s = r.scan(None);
        assert_eq!(paths(&s), ["real.txt"]);
        assert_eq!(s.stats.skipped_symlink, 2);
    }

    #[test]
    fn diff_tracks_add_modify_remove() {
        let r = Repo::new();
        r.write("a.txt", "one");
        r.write("b.txt", "two");
        r.write("c.txt", "three");
        r.age_all();
        let s1 = r.scan(None);

        r.write("a.txt", "ONE!"); // size differs, so detected even without mtime help
        r.remove("b.txt");
        r.write("d.txt", "four");
        let s2 = r.scan(Some(&s1.snapshot));
        assert_eq!(s2.diff.added, ["d.txt"]);
        assert_eq!(s2.diff.modified, ["a.txt"]);
        assert_eq!(s2.diff.removed, ["b.txt"]);
    }

    #[test]
    fn unchanged_rerun_does_no_hashing() {
        let r = Repo::new();
        for i in 0..50 {
            r.write(&format!("d{}/f{i}.txt", i % 5), format!("content {i}"));
        }
        r.age_all();
        let s1 = r.scan(None);
        assert_eq!(s1.stats.hashed, 50);

        let s2 = r.scan(Some(&s1.snapshot));
        assert_eq!(s2.stats.hashed, 0, "nothing should be re-read");
        assert_eq!(s2.stats.reused, 50);
        assert!(s2.diff.is_empty());
        assert_eq!(s2.snapshot.files, s1.snapshot.files);
    }

    #[test]
    fn recently_modified_files_are_rehashed() {
        // Freshly written files sit inside the racy window, so a rerun re-reads them. Content
        // is unchanged, so the diff is still empty.
        let r = Repo::new();
        r.write("a.txt", "x");
        let s1 = r.scan(None);
        let s2 = r.scan(Some(&s1.snapshot));
        assert_eq!(s2.stats.hashed, 1);
        assert!(s2.diff.is_empty());
    }

    #[test]
    fn same_size_edit_inside_racy_window_is_caught() {
        let r = Repo::new();
        r.write("a.txt", "aaaa");
        let s1 = r.scan(None);
        r.write("a.txt", "bbbb"); // same size, likely same mtime tick
        let s2 = r.scan(Some(&s1.snapshot));
        assert_eq!(s2.diff.modified, ["a.txt"]);
    }

    #[test]
    fn touched_but_identical_is_not_modified() {
        let r = Repo::new();
        r.write("a.txt", "same");
        r.age_all();
        let s1 = r.scan(None);
        r.write("a.txt", "same"); // new mtime, same bytes
        let s2 = r.scan(Some(&s1.snapshot));
        assert!(s2.diff.is_empty());
        assert_eq!(s2.stats.hashed, 1);
    }

    #[test]
    fn force_rehash_reads_everything() {
        let r = Repo::new();
        r.write("a.txt", "x");
        r.write("b.txt", "y");
        r.age_all();
        let s1 = r.scan(None);
        let forced = scan(
            &r.config(),
            Some(&s1.snapshot),
            &WalkOptions { force_rehash: true },
        )
        .unwrap();
        assert_eq!(forced.stats.hashed, 2);
        assert_eq!(forced.stats.reused, 0);
    }

    #[test]
    fn missing_root_is_an_error() {
        let cfg = Config::new("/definitely/not/here");
        assert!(scan(&cfg, None, &WalkOptions::default()).is_err());
    }

    #[test]
    fn output_is_deterministic() {
        let r = Repo::new();
        for n in ["z.txt", "a.txt", "m/n.txt", "m/a.txt"] {
            r.write(n, n);
        }
        let a = r.scan(None);
        let b = r.scan(None);
        assert_eq!(paths(&a), paths(&b));
        assert_eq!(paths(&a), ["a.txt", "m/a.txt", "m/n.txt", "z.txt"]);
    }
}
