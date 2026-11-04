use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ignore::WalkBuilder;
use loupe_core::config::IGNORE_FILE;
use loupe_core::{BlobId, Config, Error, FileMeta, Result, Snapshot};
use rayon::prelude::*;

use crate::diff::{Diff, diff};
use crate::exclude::{has_skipped_extension, is_excluded_dir};
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
    /// Files with binary/generated extensions (`.o`, `.rlib`, `.map`, ...), never opened.
    pub skipped_extension: usize,
    /// Files dropped because the tree exceeded `Config::max_files` or `max_total_bytes`.
    pub skipped_budget: usize,
    /// Directories not descended into (build output, caches, dependencies).
    pub skipped_dirs: usize,
    /// The walk stopped early or the budget dropped files: the index is a subset of the tree.
    pub truncated: bool,
    /// Top-level directories that lost the most files to the budget, most first (at most 3).
    pub dropped_top_dirs: Vec<(String, usize)>,
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
    let skipped_dirs = Arc::new(AtomicUsize::new(0));
    let skipped_dirs_in_filter = Arc::clone(&skipped_dirs);
    let default_excludes = config.default_excludes;
    let walk_root = root.clone();
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
        // Deterministic traversal: if the walk limit cuts the walk short, it cuts at the same
        // place on every run instead of wherever `readdir` happened to start.
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |e| {
            if e.file_name() == ".git" || e.path() == index_dir {
                return false;
            }
            if default_excludes
                && e.path() != walk_root
                && e.file_type().is_some_and(|t| t.is_dir())
                && is_excluded_dir(e.path())
            {
                skipped_dirs_in_filter.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            true
        });

    // The walk itself is bounded: a runaway tree must not exhaust memory before the budget runs.
    let walk_limit = config.max_files.saturating_mul(4).max(1024);
    let mut candidates = Vec::new();
    for result in builder.build() {
        if stats.seen >= walk_limit {
            tracing::warn!(walk_limit, "walk limit reached; the index will be partial");
            stats.truncated = true;
            break;
        }
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
        if config.default_excludes && has_skipped_extension(&rel) {
            stats.skipped_extension += 1;
            continue;
        }
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
            rel,
            size: md.len(),
            mtime_ns: md.modified().map(system_time_ns).unwrap_or(0),
        });
    }

    stats.skipped_dirs = skipped_dirs.load(Ordering::Relaxed);
    let (dropped, top) = apply_budget(&mut candidates, config);
    if dropped > 0 {
        tracing::warn!(
            dropped,
            max_files = config.max_files,
            max_total_bytes = config.max_total_bytes,
            ?top,
            "tree exceeds the index budget; keeping source files first. Add build output to \
             .loupeignore or raise the limits"
        );
        stats.skipped_budget = dropped;
        stats.truncated = true;
        stats.dropped_top_dirs = top;
    }

    let prev_taken = prev.map(|p| p.taken_ns);
    let outcomes: Vec<Outcome> = candidates
        .par_iter()
        .map(|c| process(root, c, prev, prev_taken, opts))
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

/// Directory names that usually hold generated output rather than hand-written source.
const GENERATED_DIR_NAMES: &[&str] = &[
    "build",
    "dist",
    "out",
    "obj",
    "bin",
    "generated",
    "gen",
    "_build",
    ".output",
    "coverage",
    "storybook-static",
    "site-packages",
];

/// Lower is kept first: 0 = source in a known language outside generated-looking directories,
/// 1 = known language under a directory like `out/` or `dist/`, 2 = everything else.
fn priority(rel: &str) -> u8 {
    if detect_language(rel, &[]).is_none() {
        return 2;
    }
    let mut parts = rel.split('/');
    parts.next_back(); // the file name
    if parts.any(|d| GENERATED_DIR_NAMES.contains(&d)) {
        1
    } else {
        0
    }
}

/// Keep within `max_files` / `max_total_bytes`. Selection is deterministic (so the kept set does
/// not flap between runs): by [`priority`], then smaller files before larger ones, then path.
/// Returns how many candidates were dropped and the top-level directories that lost the most.
fn apply_budget(candidates: &mut Vec<Candidate>, config: &Config) -> (usize, Vec<(String, usize)>) {
    let total: u64 = candidates.iter().map(|c| c.size).sum();
    if candidates.len() <= config.max_files && total <= config.max_total_bytes {
        return (0, Vec::new());
    }
    let before = candidates.len();
    candidates.sort_by_cached_key(|c| (priority(&c.rel), c.size, c.rel.clone()));
    let mut kept = 0usize;
    let mut bytes = 0u64;
    let mut lost: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    candidates.retain(|c| {
        if kept < config.max_files && bytes.saturating_add(c.size) <= config.max_total_bytes {
            kept += 1;
            bytes += c.size;
            true
        } else {
            let top = c.rel.split('/').next().unwrap_or("");
            let top = if top == c.rel { "." } else { top };
            *lost.entry(top.to_string()).or_default() += 1;
            false
        }
    });
    let mut top: Vec<(String, usize)> = lost.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(3);
    (before - candidates.len(), top)
}

fn process(
    root: &Path,
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

    match read_and_classify(&root.join(&c.rel)) {
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
    use std::path::PathBuf;
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
    fn default_excludes_skip_build_trees_without_opening_them() {
        let r = Repo::new();
        r.write("Cargo.toml", "[package]");
        r.write("src/lib.rs", "pub fn a() {}");
        r.write("target/debug/deps/lib.d", "text that would be indexed");
        r.write("target/debug/libx.rlib", "binaryish");
        r.write("node_modules/pkg/index.js", "x");
        r.write("web/bundle.js.map", "{}");
        let s = r.scan(None);
        assert_eq!(paths(&s), ["Cargo.toml", "src/lib.rs"]);
        assert_eq!(s.stats.skipped_dirs, 2);
        assert_eq!(s.stats.skipped_extension, 1);

        let mut cfg = r.config();
        cfg.default_excludes = false;
        let s = scan(&cfg, None, &WalkOptions::default()).unwrap();
        assert!(paths(&s).contains(&"target/debug/deps/lib.d"));
        assert!(paths(&s).contains(&"node_modules/pkg/index.js"));
    }

    #[test]
    fn budget_keeps_source_first_and_is_deterministic() {
        let r = Repo::new();
        r.write("src/a.rs", "fn a() {}");
        r.write("src/b.rs", "fn b() {}");
        r.write("gen/blob1.txt", vec![b'x'; 500]);
        r.write("gen/blob2.txt", vec![b'y'; 400]);
        let mut cfg = r.config();
        cfg.max_files = 3;
        let first = scan(&cfg, None, &WalkOptions::default()).unwrap();
        // Both sources, then the smaller unknown-language file.
        assert_eq!(paths(&first), ["gen/blob2.txt", "src/a.rs", "src/b.rs"]);
        assert_eq!(first.stats.skipped_budget, 1);
        assert!(first.stats.truncated);
        let second = scan(&cfg, None, &WalkOptions::default()).unwrap();
        assert_eq!(paths(&first), paths(&second));

        cfg.max_files = 100;
        cfg.max_total_bytes = 20; // only the two tiny sources fit
        let s = scan(&cfg, None, &WalkOptions::default()).unwrap();
        assert_eq!(paths(&s), ["src/a.rs", "src/b.rs"]);
        assert_eq!(s.stats.skipped_budget, 2);
    }

    #[test]
    fn budget_prefers_hand_written_source_over_generated_directories() {
        let r = Repo::new();
        for i in 0..5 {
            r.write(
                &format!("src/m{i}.rs"),
                format!("fn f{i}() {{ /* padding padding */ }}"),
            );
        }
        for i in 0..5 {
            r.write(&format!("out/bundle{i}.js"), "var a=1;\n"); // smaller, but generated-looking
        }
        let mut cfg = r.config();
        cfg.max_files = 5;
        let s = scan(&cfg, None, &WalkOptions::default()).unwrap();
        assert!(
            paths(&s).iter().all(|p| p.starts_with("src/")),
            "{:?}",
            paths(&s)
        );
        assert_eq!(s.stats.dropped_top_dirs, [("out".to_string(), 5)]);
    }

    #[test]
    fn walk_limit_stops_a_runaway_tree() {
        let r = Repo::new();
        for i in 0..2000 {
            r.write(&format!("f{i}.txt"), "x");
        }
        let mut cfg = r.config();
        cfg.max_files = 100; // walk limit is max(4 * 100, 1024) = 1024 entries
        let s = scan(&cfg, None, &WalkOptions::default()).unwrap();
        assert!(s.stats.truncated);
        assert_eq!(s.snapshot.len(), 100);
        assert!(s.stats.seen <= 1024);
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
