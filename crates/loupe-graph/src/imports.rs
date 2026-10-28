//! Import extraction (per file, content-addressed) and import resolution (per snapshot).
//!
//! Extraction is regex-based and heuristic by design: it records the raw import strings of a
//! file so they can be stored per blob. Resolution then maps them to snapshot paths, which needs
//! the whole file list and so happens when a graph is built.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use loupe_core::{Error, LocalBlobId, Result};
use loupe_store::{Segment, SegmentWriter};
use regex::bytes::Regex;

const MAX_IMPORTS_PER_FILE: usize = 300;
const MAX_IMPORT_LEN: usize = 300;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("import regex"))
}

/// Raw import strings found in `source`. Rust items are prefixed `mod:` / `use:` so resolution
/// can tell them apart; other languages return the bare module/path text.
pub fn extract_imports(lang: Option<&str>, source: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !s.is_empty() && s.len() <= MAX_IMPORT_LEN {
            out.push(s);
        }
    };
    let text = |m: Option<regex::bytes::Match<'_>>| {
        m.map(|m| String::from_utf8_lossy(m.as_bytes()).trim().to_string())
            .unwrap_or_default()
    };

    match lang {
        Some("rust") => {
            static MOD: OnceLock<Regex> = OnceLock::new();
            static USE: OnceLock<Regex> = OnceLock::new();
            for c in re(
                &MOD,
                r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?mod[ \t]+(\w+)[ \t]*;",
            )
            .captures_iter(source)
            {
                push(format!("mod:{}", text(c.get(1))));
            }
            for c in re(
                &USE,
                r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?use[ \t]+([^;]+);",
            )
            .captures_iter(source)
            {
                push(format!(
                    "use:{}",
                    text(c.get(1))
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join("")
                ));
            }
        }
        Some("python") => {
            static IMPORT: OnceLock<Regex> = OnceLock::new();
            static FROM: OnceLock<Regex> = OnceLock::new();
            for c in re(&IMPORT, r"(?m)^[ \t]*import[ \t]+([\w., \t]+)").captures_iter(source) {
                for part in text(c.get(1)).split(',') {
                    push(part.split_whitespace().next().unwrap_or("").to_string());
                }
            }
            for c in re(
                &FROM,
                r"(?m)^[ \t]*from[ \t]+(\.*[\w.]*)[ \t]+import[ \t]+([\w*, \t()]+)",
            )
            .captures_iter(source)
            {
                let module = text(c.get(1));
                if module.trim_matches('.').is_empty() {
                    // `from . import a, b`: each name may be a sibling module.
                    for name in text(c.get(2)).trim_matches(['(', ')']).split(',') {
                        let name = name.split_whitespace().next().unwrap_or("");
                        push(format!("{module}{name}"));
                    }
                } else {
                    push(module);
                }
            }
        }
        Some("javascript" | "typescript" | "tsx") => {
            static FROM: OnceLock<Regex> = OnceLock::new();
            static REQ: OnceLock<Regex> = OnceLock::new();
            static BARE: OnceLock<Regex> = OnceLock::new();
            for c in re(
                &FROM,
                r#"(?m)\b(?:import|export)\b[^;'"]*?\bfrom[ \t]*['"]([^'"\n]+)['"]"#,
            )
            .captures_iter(source)
            {
                push(text(c.get(1)));
            }
            for c in re(
                &REQ,
                r#"\b(?:require|import)[ \t]*\([ \t]*['"]([^'"\n]+)['"][ \t]*\)"#,
            )
            .captures_iter(source)
            {
                push(text(c.get(1)));
            }
            for c in re(&BARE, r#"(?m)^[ \t]*import[ \t]*['"]([^'"\n]+)['"]"#).captures_iter(source)
            {
                push(text(c.get(1)));
            }
        }
        Some("go") => {
            static SINGLE: OnceLock<Regex> = OnceLock::new();
            static BLOCK: OnceLock<Regex> = OnceLock::new();
            static LINE: OnceLock<Regex> = OnceLock::new();
            for c in re(
                &SINGLE,
                r#"(?m)^[ \t]*import[ \t]+(?:[\w.]+[ \t]+)?"([^"\n]+)""#,
            )
            .captures_iter(source)
            {
                push(text(c.get(1)));
            }
            for block in re(&BLOCK, r"(?ms)^[ \t]*import[ \t]*\((.*?)\)").captures_iter(source) {
                let body = block.get(1).map_or(&[][..], |m| m.as_bytes());
                for c in re(&LINE, r#"(?m)^[ \t]*(?:[\w.]+[ \t]+)?"([^"\n]+)""#).captures_iter(body)
                {
                    push(text(c.get(1)));
                }
            }
        }
        Some("c" | "cpp") => {
            static INC: OnceLock<Regex> = OnceLock::new();
            for c in re(&INC, r#"(?m)^[ \t]*#[ \t]*include[ \t]+"([^"\n]+)""#).captures_iter(source)
            {
                push(text(c.get(1)));
            }
        }
        Some("java") => {
            static IMP: OnceLock<Regex> = OnceLock::new();
            for c in re(
                &IMP,
                r"(?m)^[ \t]*import[ \t]+(?:static[ \t]+)?([\w.]+(?:\.\*)?)[ \t]*;",
            )
            .captures_iter(source)
            {
                push(text(c.get(1)));
            }
        }
        _ => {}
    }
    out.sort_unstable();
    out.dedup();
    out.truncate(MAX_IMPORTS_PER_FILE);
    out
}

// ---- resolution ----------------------------------------------------------------------

/// Lookup tables over a snapshot's paths, built once and shared by all resolutions.
#[derive(Debug)]
pub struct PathIndex {
    paths: Vec<String>,
    by_path: HashMap<String, u32>,
    by_dir: HashMap<String, Vec<u32>>,
    by_name: HashMap<String, Vec<u32>>,
    /// Rust crate (snake_case dir name) -> its `src` directory, from `<dir>/src/lib.rs`.
    crate_src: HashMap<String, String>,
}

/// `tower-service-0.3.3` -> `tower-service` (vendored/registry directories carry a version).
fn strip_version(dir_name: &str) -> &str {
    let bytes = dir_name.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'-' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            // Only a version if everything after is digits, dots, and pre-release/build text.
            let rest = &dir_name[i + 1..];
            if rest.split('.').count() >= 2
                && rest
                    .split('.')
                    .all(|p| p.chars().next().is_some_and(|c| c.is_ascii_digit()))
            {
                return &dir_name[..i];
            }
        }
    }
    dir_name
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

fn name_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, n)| n)
}

/// `dir` + `rel`, resolving `.` and `..`; `None` if it climbs above the root.
fn join_norm(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for comp in rel.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    Some(parts.join("/"))
}

impl PathIndex {
    pub fn new<'a>(paths: impl IntoIterator<Item = &'a str>) -> PathIndex {
        let paths: Vec<String> = paths.into_iter().map(String::from).collect();
        let mut by_path = HashMap::new();
        let mut by_dir: HashMap<String, Vec<u32>> = HashMap::new();
        let mut by_name: HashMap<String, Vec<u32>> = HashMap::new();
        let mut crate_src = HashMap::new();
        for (i, p) in paths.iter().enumerate() {
            let i = i as u32;
            by_path.insert(p.clone(), i);
            by_dir.entry(dir_of(p).to_string()).or_default().push(i);
            by_name.entry(name_of(p).to_string()).or_default().push(i);
            if let Some(src) = p
                .strip_suffix("/lib.rs")
                .filter(|s| s.ends_with("/src") || *s == "src")
            {
                let crate_dir = dir_of(src);
                let name = strip_version(name_of(crate_dir)).replace('-', "_");
                if !name.is_empty() {
                    crate_src.entry(name).or_insert_with(|| src.to_string());
                }
            }
        }
        PathIndex {
            paths,
            by_path,
            by_dir,
            by_name,
            crate_src,
        }
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn path(&self, id: u32) -> &str {
        &self.paths[id as usize]
    }

    pub fn id_of(&self, path: &str) -> Option<u32> {
        self.by_path.get(path).copied()
    }

    fn first_existing<I: IntoIterator<Item = String>>(&self, candidates: I) -> Option<u32> {
        candidates.into_iter().find_map(|c| self.id_of(&c))
    }

    /// Files whose path ends with `/suffix` (or equals it), restricted to those whose name has
    /// one of `exts`.
    fn by_suffix(&self, suffix: &str, exts: &[&str]) -> Vec<u32> {
        let Some(candidates) = self.by_name.get(name_of(suffix)) else {
            return Vec::new();
        };
        candidates
            .iter()
            .copied()
            .filter(|&i| {
                let p = self.path(i);
                (p == suffix || p.ends_with(&format!("/{suffix}")))
                    && (exts.is_empty() || exts.iter().any(|e| p.ends_with(e)))
            })
            .collect()
    }
}

/// Map one raw import of file `from` (language `lang`) to snapshot file ids.
pub fn resolve(idx: &PathIndex, lang: &str, from: &str, raw: &str) -> Vec<u32> {
    let mut out = match lang {
        "rust" => resolve_rust(idx, from, raw),
        "python" => resolve_python(idx, from, raw),
        "javascript" | "typescript" | "tsx" => resolve_js(idx, from, raw),
        "go" => resolve_go(idx, raw),
        "c" | "cpp" => resolve_c(idx, from, raw),
        "java" => resolve_java(idx, raw),
        _ => Vec::new(),
    };
    out.retain(|&i| idx.path(i) != from);
    out.sort_unstable();
    out.dedup();
    out
}

/// Directory holding the child modules of the module defined by file `from`: the file's own
/// directory for `mod.rs`/`lib.rs`/`main.rs`, otherwise `<dir>/<stem>`.
fn module_dir(from: &str) -> String {
    let dir = dir_of(from);
    match name_of(from).strip_suffix(".rs") {
        Some("mod" | "lib" | "main") | None => dir.to_string(),
        Some(stem) => join_norm(dir, stem).unwrap_or_default(),
    }
}

fn resolve_rust(idx: &PathIndex, from: &str, raw: &str) -> Vec<u32> {
    let dir = dir_of(from);
    if let Some(name) = raw.strip_prefix("mod:") {
        let stem = name_of(from).strip_suffix(".rs").unwrap_or("");
        let mut bases = vec![dir.to_string()];
        if !matches!(stem, "mod" | "lib" | "main") {
            bases.insert(0, join_norm(dir, stem).unwrap_or_default());
        }
        return bases
            .iter()
            .filter_map(|b| {
                idx.first_existing([
                    join_norm(b, &format!("{name}.rs"))?,
                    join_norm(b, &format!("{name}/mod.rs"))?,
                ])
            })
            .take(1)
            .collect();
    }
    let Some(path) = raw.strip_prefix("use:") else {
        return Vec::new();
    };
    // `crate::a::{b, c}` -> segments up to the first brace/glob.
    let head = path.split(['{', '*']).next().unwrap_or("");
    let mut segs: Vec<&str> = head.split("::").filter(|s| !s.is_empty()).collect();
    let (root, rest): (String, Vec<&str>) = match segs.first().copied() {
        Some("crate") => {
            segs.remove(0);
            // The nearest ancestor `src` directory is the usual crate root. Flat layouts (no
            // `src`), `lib/`, and examples fall back to trying each ancestor, nearest first.
            let mut roots: Vec<String> = Vec::new();
            let mut d = dir;
            loop {
                if d == "src" || d.ends_with("/src") {
                    roots.push(d.to_string());
                    break;
                }
                if d.is_empty() {
                    break;
                }
                d = dir_of(d);
            }
            let mut d = dir;
            loop {
                if !roots.iter().any(|r| r == d) {
                    roots.push(d.to_string());
                }
                if d.is_empty() {
                    break;
                }
                d = dir_of(d);
            }
            // Never fall back to the repo root for a bare `use crate;`.
            return roots
                .iter()
                .find_map(|r| {
                    longest_module_prefix(idx, r, &segs)
                        .filter(|_| !segs.is_empty() || r.ends_with("src"))
                })
                .into_iter()
                .collect();
        }
        Some("self") => {
            segs.remove(0);
            (module_dir(from), segs)
        }
        Some("super") => {
            let mut base = module_dir(from);
            while segs.first() == Some(&"super") {
                base = dir_of(&base).to_string();
                segs.remove(0);
            }
            (base, segs)
        }
        Some(first) => match idx.crate_src.get(first) {
            Some(src) => {
                segs.remove(0);
                (src.clone(), segs)
            }
            None => return Vec::new(), // std / external crate
        },
        None => return Vec::new(),
    };
    longest_module_prefix(idx, &root, &rest)
        .into_iter()
        .collect()
}

/// The file for the longest prefix of `rest` that names a module under directory `root`.
fn longest_module_prefix(idx: &PathIndex, root: &str, rest: &[&str]) -> Option<u32> {
    for k in (0..=rest.len()).rev() {
        let sub = rest[..k].join("/");
        let candidates = if sub.is_empty() {
            // The module that owns `root`: a crate root file, `mod.rs`, or the sibling
            // `<dir>.rs` (2018-style `net.rs` next to `net/`).
            vec![
                join_norm(root, "lib.rs"),
                join_norm(root, "main.rs"),
                join_norm(root, "mod.rs"),
                join_norm(dir_of(root), &format!("{}.rs", name_of(root))),
            ]
        } else {
            vec![
                join_norm(root, &format!("{sub}.rs")),
                join_norm(root, &format!("{sub}/mod.rs")),
            ]
        };
        if let Some(id) = idx.first_existing(candidates.into_iter().flatten()) {
            return Some(id);
        }
    }
    None
}

fn resolve_python(idx: &PathIndex, from: &str, raw: &str) -> Vec<u32> {
    let dots = raw.chars().take_while(|&c| c == '.').count();
    let module = &raw[dots..];
    let parts: Vec<&str> = module.split('.').filter(|s| !s.is_empty()).collect();

    let mut bases: Vec<String> = Vec::new();
    if dots > 0 {
        let mut base = dir_of(from).to_string();
        for _ in 1..dots {
            base = dir_of(&base).to_string();
        }
        bases.push(base);
    } else {
        // Absolute: the repo root and every ancestor of the importing file (src layouts).
        bases.push(String::new());
        let mut d = dir_of(from);
        while !d.is_empty() {
            bases.push(d.to_string());
            d = dir_of(d);
        }
    }
    for k in (1..=parts.len()).rev() {
        let sub = parts[..k].join("/");
        for base in &bases {
            let found = idx.first_existing(
                [
                    join_norm(base, &format!("{sub}.py")),
                    join_norm(base, &format!("{sub}/__init__.py")),
                ]
                .into_iter()
                .flatten(),
            );
            if let Some(id) = found {
                return vec![id];
            }
        }
    }
    if parts.is_empty() && dots > 0 {
        return idx
            .first_existing([join_norm(&bases[0], "__init__.py").unwrap_or_default()])
            .into_iter()
            .collect();
    }
    Vec::new()
}

fn resolve_js(idx: &PathIndex, from: &str, raw: &str) -> Vec<u32> {
    if !raw.starts_with("./") && !raw.starts_with("../") && raw != "." && raw != ".." {
        return Vec::new(); // package import
    }
    let Some(base) = join_norm(dir_of(from), raw) else {
        return Vec::new();
    };
    const EXTS: [&str; 8] = ["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];
    let mut candidates = vec![base.clone()];
    // TypeScript ESM style imports name the compiled extension: './x.js' -> x.ts.
    if let Some(stem) = base.rsplit_once('.').map(|(s, _)| s.to_string()) {
        candidates.extend(EXTS.iter().map(|e| format!("{stem}.{e}")));
    }
    candidates.extend(EXTS.iter().map(|e| format!("{base}.{e}")));
    candidates.extend(EXTS.iter().map(|e| format!("{base}/index.{e}")));
    idx.first_existing(candidates).into_iter().collect()
}

fn resolve_go(idx: &PathIndex, raw: &str) -> Vec<u32> {
    let segs: Vec<&str> = raw.split('/').collect();
    // Longest suffix of the import path that is a directory in the repo.
    for k in 0..segs.len() {
        let suffix = segs[k..].join("/");
        if segs.len() - k < 2 && segs.len() > 1 {
            break; // a single trailing segment is too ambiguous unless the import is one segment
        }
        let files: Vec<u32> = idx
            .by_dir
            .iter()
            .filter(|(d, _)| **d == suffix || d.ends_with(&format!("/{suffix}")))
            .flat_map(|(_, f)| f.iter().copied())
            .filter(|&i| idx.path(i).ends_with(".go") && !idx.path(i).ends_with("_test.go"))
            .take(30)
            .collect();
        if !files.is_empty() {
            return files;
        }
    }
    Vec::new()
}

fn resolve_c(idx: &PathIndex, from: &str, raw: &str) -> Vec<u32> {
    if let Some(p) = join_norm(dir_of(from), raw)
        && let Some(id) = idx.id_of(&p)
    {
        return vec![id];
    }
    if let Some(id) = idx.id_of(raw) {
        return vec![id];
    }
    let found = idx.by_suffix(raw.trim_start_matches("./"), &[]);
    // Prefer the candidate sharing the longest directory prefix with the includer.
    found
        .into_iter()
        .max_by_key(|&i| common_prefix_len(idx.path(i), from))
        .into_iter()
        .collect()
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

fn resolve_java(idx: &PathIndex, raw: &str) -> Vec<u32> {
    let wildcard = raw.ends_with(".*");
    let dotted = raw.trim_end_matches(".*");
    let parts: Vec<&str> = dotted.split('.').collect();
    if wildcard {
        let dir = parts.join("/");
        return idx
            .by_dir
            .iter()
            .filter(|(d, _)| **d == dir || d.ends_with(&format!("/{dir}")))
            .flat_map(|(_, f)| f.iter().copied())
            .filter(|&i| idx.path(i).ends_with(".java"))
            .take(30)
            .collect();
    }
    // `a.b.C` or `a.b.C.Inner` / static member: try progressively shorter prefixes.
    for k in (1..=parts.len()).rev() {
        let suffix = format!("{}.java", parts[..k].join("/"));
        let found = idx.by_suffix(&suffix, &[]);
        if !found.is_empty() {
            return found.into_iter().take(1).collect();
        }
    }
    Vec::new()
}

// ---- per-segment storage -------------------------------------------------------------

pub const IMPORTS_FILE: &str = "imports.bin";
const MAGIC: &[u8; 4] = b"LPIM";
const VERSION: u32 = 1;
const HEADER: usize = 12;

fn corrupt(msg: &str) -> Error {
    Error::Corrupt(format!("import index: {msg}"))
}

/// Raw imports per blob, in the same segment as the other per-blob indexes.
///
/// ```text
/// imports.bin  "LPIM" | version u32 | n_blobs u32
///              table: n_blobs x (data offset u32, count u32)
///              data: count x (len u16, bytes), per blob
/// ```
#[derive(Debug, Default)]
pub struct ImportIndexBuilder {
    blobs: Vec<(u32, Vec<String>)>,
}

impl ImportIndexBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, blob: LocalBlobId, imports: Vec<String>) -> Result<()> {
        if self.blobs.last().is_some_and(|(last, _)| blob.0 <= *last) {
            return Err(Error::InvalidArgument(
                "blobs must be added in increasing order".into(),
            ));
        }
        if !imports.is_empty() {
            self.blobs.push((blob.0, imports));
        }
        Ok(())
    }

    pub fn finish(self, n_blobs: u32) -> Result<Vec<u8>> {
        let mut table = vec![(0u32, 0u32); n_blobs as usize];
        let mut data: Vec<u8> = Vec::new();
        for (blob, imports) in &self.blobs {
            if *blob >= n_blobs {
                return Err(Error::InvalidArgument("blob id beyond segment".into()));
            }
            table[*blob as usize] = (data.len() as u32, imports.len() as u32);
            for s in imports {
                data.extend_from_slice(&(s.len() as u16).to_le_bytes());
                data.extend_from_slice(s.as_bytes());
            }
        }
        let mut out = Vec::with_capacity(HEADER + table.len() * 8 + data.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&n_blobs.to_le_bytes());
        for (off, count) in table {
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
        }
        out.extend_from_slice(&data);
        Ok(out)
    }

    pub fn write_to(self, writer: &mut SegmentWriter, n_blobs: u32) -> Result<()> {
        writer.add_file(IMPORTS_FILE, &self.finish(n_blobs)?)
    }
}

pub struct ImportIndex<'a> {
    n_blobs: usize,
    table: &'a [u8],
    data: &'a [u8],
}

impl std::fmt::Debug for ImportIndex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportIndex")
            .field("blobs", &self.n_blobs)
            .finish()
    }
}

impl<'a> ImportIndex<'a> {
    /// `None` if the segment has no import file (built by an older version).
    pub fn from_segment(segment: &'a Segment) -> Result<Option<ImportIndex<'a>>> {
        segment.file(IMPORTS_FILE).map(Self::from_bytes).transpose()
    }

    pub fn from_bytes(bytes: &'a [u8]) -> Result<ImportIndex<'a>> {
        if bytes.len() < HEADER || &bytes[..4] != MAGIC {
            return Err(corrupt("bad header"));
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if version != VERSION {
            return Err(Error::FormatVersion {
                found: version,
                expected: VERSION,
            });
        }
        let n_blobs = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let table_end = HEADER + n_blobs * 8;
        if bytes.len() < table_end {
            return Err(corrupt("table truncated"));
        }
        Ok(ImportIndex {
            n_blobs,
            table: &bytes[HEADER..table_end],
            data: &bytes[table_end..],
        })
    }

    pub fn imports_of(&self, blob: LocalBlobId) -> Result<Vec<String>> {
        if blob.0 as usize >= self.n_blobs {
            return Ok(Vec::new());
        }
        let at = blob.0 as usize * 8;
        let off = u32::from_le_bytes(self.table[at..at + 4].try_into().unwrap()) as usize;
        let count = u32::from_le_bytes(self.table[at + 4..at + 8].try_into().unwrap()) as usize;
        let mut pos = off;
        let mut out = Vec::with_capacity(count.min(MAX_IMPORTS_PER_FILE));
        for _ in 0..count {
            let len_bytes = self
                .data
                .get(pos..pos + 2)
                .ok_or_else(|| corrupt("string header outside data"))?;
            let len = u16::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
            let s = self
                .data
                .get(pos + 2..pos + 2 + len)
                .ok_or_else(|| corrupt("string outside data"))?;
            out.push(String::from_utf8(s.to_vec()).map_err(|_| corrupt("invalid utf-8"))?);
            pos += 2 + len;
        }
        Ok(out)
    }
}

/// Resolve every import of `from` to file ids, deduped.
pub fn resolve_all(idx: &PathIndex, lang: &str, from: &str, raws: &[String]) -> Vec<u32> {
    let mut seen = HashSet::new();
    raws.iter()
        .flat_map(|r| resolve(idx, lang, from, r))
        .filter(|id| seen.insert(*id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imports(lang: &str, src: &str) -> Vec<String> {
        extract_imports(Some(lang), src.as_bytes())
    }

    #[test]
    fn rust_mods_and_uses() {
        let got = imports(
            "rust",
            "mod a;\npub mod b;\npub(crate) mod c;\nuse crate::x::y::{Z, W};\nuse std::fmt;\nuse super::sibling;\nmod inline { }\n",
        );
        assert_eq!(
            got,
            [
                "mod:a",
                "mod:b",
                "mod:c",
                "use:crate::x::y::{Z,W}",
                "use:std::fmt",
                "use:super::sibling"
            ]
        );
    }

    #[test]
    fn python_imports() {
        let got = imports(
            "python",
            "import os, sys as s\nimport pkg.mod\nfrom a.b import c\nfrom . import d, e\nfrom ..up import f\nfrom x import (g,\n h)\n",
        );
        assert_eq!(
            got,
            ["..up", ".d", ".e", "a.b", "os", "pkg.mod", "sys", "x"]
        );
    }

    #[test]
    fn js_ts_imports() {
        let got = imports(
            "typescript",
            "import a from './a';\nimport { b } from \"../b\";\nimport './side';\nexport * from './re';\nconst c = require('./c');\nconst d = await import('./d');\nimport e from 'react';\n",
        );
        assert_eq!(
            got,
            ["../b", "./a", "./c", "./d", "./re", "./side", "react"]
        );
    }

    #[test]
    fn go_c_java_imports() {
        assert_eq!(
            imports(
                "go",
                "import \"fmt\"\nimport (\n  \"os\"\n  str \"example.com/m/store\"\n)\n"
            ),
            ["example.com/m/store", "fmt", "os"]
        );
        assert_eq!(
            imports(
                "c",
                "#include <stdio.h>\n#include \"util/x.h\"\n  # include \"y.h\"\n"
            ),
            ["util/x.h", "y.h"]
        );
        assert_eq!(
            imports(
                "java",
                "import a.b.C;\nimport static a.b.D.m;\nimport e.f.*;\n"
            ),
            ["a.b.C", "a.b.D.m", "e.f.*"]
        );
        assert!(imports("kotlin", "import a.b").is_empty());
        assert!(extract_imports(None, b"import x").is_empty());
    }

    fn idx(paths: &[&str]) -> PathIndex {
        PathIndex::new(paths.iter().copied())
    }

    fn names(i: &PathIndex, ids: Vec<u32>) -> Vec<String> {
        ids.into_iter().map(|x| i.path(x).to_string()).collect()
    }

    #[test]
    fn rust_resolution() {
        let i = idx(&[
            "crates/app/src/main.rs",
            "crates/app/src/store.rs",
            "crates/app/src/net/mod.rs",
            "crates/app/src/net/tcp.rs",
            "crates/core/src/lib.rs",
            "crates/core/src/ids.rs",
        ]);
        let r = |from: &str, raw: &str| names(&i, resolve(&i, "rust", from, raw));
        assert_eq!(
            r("crates/app/src/main.rs", "mod:store"),
            ["crates/app/src/store.rs"]
        );
        assert_eq!(
            r("crates/app/src/main.rs", "mod:net"),
            ["crates/app/src/net/mod.rs"]
        );
        assert_eq!(
            r("crates/app/src/net/mod.rs", "mod:tcp"),
            ["crates/app/src/net/tcp.rs"]
        );
        assert_eq!(
            r("crates/app/src/main.rs", "use:crate::store::Store"),
            ["crates/app/src/store.rs"]
        );
        assert_eq!(
            r("crates/app/src/main.rs", "use:crate::net::tcp::{A,B}"),
            ["crates/app/src/net/tcp.rs"]
        );
        assert_eq!(
            r("crates/app/src/net/tcp.rs", "use:super::Thing"),
            ["crates/app/src/net/mod.rs"]
        );
        // Another workspace crate, by its snake_case directory name.
        assert_eq!(
            r("crates/app/src/main.rs", "use:core::ids::Id"),
            ["crates/core/src/ids.rs"]
        );
        assert_eq!(
            r("crates/app/src/main.rs", "use:core::Thing"),
            ["crates/core/src/lib.rs"]
        );
        assert!(r("crates/app/src/main.rs", "use:std::fmt").is_empty());
        assert_eq!(strip_version("tower-service-0.3.3"), "tower-service");
        assert_eq!(strip_version("libc-0.2.191"), "libc");
        assert_eq!(strip_version("h2-0.4.20"), "h2");
        assert_eq!(strip_version("my-crate"), "my-crate");
        assert_eq!(
            strip_version("ffi-2d-lib"),
            "ffi-2d-lib",
            "digits alone are not a version"
        );
        let v = idx(&[
            "vendor/tower-service-0.3.3/src/lib.rs",
            "vendor/tower-service-0.3.3/src/x.rs",
            "app/src/main.rs",
        ]);
        let ids = resolve(&v, "rust", "app/src/main.rs", "use:tower_service::x::Y");
        assert_eq!(
            ids.iter().map(|&i| v.path(i)).collect::<Vec<_>>(),
            ["vendor/tower-service-0.3.3/src/x.rs"]
        );
        assert!(r("crates/app/src/main.rs", "mod:missing").is_empty());
    }

    #[test]
    fn rust_crate_paths_resolve_in_flat_layouts_without_a_src_dir() {
        let i = idx(&["auth/token.rs", "auth/login.rs", "util.rs"]);
        let r = |from: &str, raw: &str| names(&i, resolve(&i, "rust", from, raw));
        assert_eq!(
            r("auth/login.rs", "use:crate::auth::token::validate"),
            ["auth/token.rs"]
        );
        assert_eq!(r("auth/login.rs", "use:crate::util::helper"), ["util.rs"]);
        assert!(r("auth/login.rs", "use:crate::missing::thing").is_empty());
        assert!(
            r("auth/login.rs", "use:crate").is_empty(),
            "a bare `use crate;` names no file here"
        );
    }

    #[test]
    fn python_resolution() {
        let i = idx(&[
            "app/main.py",
            "app/util.py",
            "app/pkg/__init__.py",
            "app/pkg/mod.py",
            "src/lib/core.py",
            "src/app2/run.py",
        ]);
        let r = |from: &str, raw: &str| names(&i, resolve(&i, "python", from, raw));
        assert_eq!(r("app/main.py", "app.util"), ["app/util.py"]);
        assert_eq!(r("app/main.py", "pkg.mod"), ["app/pkg/mod.py"]);
        assert_eq!(r("app/main.py", ".util"), ["app/util.py"]);
        assert_eq!(r("app/pkg/mod.py", "..util"), ["app/util.py"]);
        assert_eq!(r("app/main.py", ".pkg"), ["app/pkg/__init__.py"]);
        assert_eq!(
            r("src/app2/run.py", "lib.core"),
            ["src/lib/core.py"],
            "src-layout via ancestor dirs"
        );
        assert!(r("app/main.py", "os.path").is_empty());
    }

    #[test]
    fn js_resolution() {
        let i = idx(&[
            "src/a.ts",
            "src/b/index.ts",
            "src/c.js",
            "lib/d.tsx",
            "src/e.ts",
        ]);
        let r = |from: &str, raw: &str| names(&i, resolve(&i, "typescript", from, raw));
        assert_eq!(r("src/a.ts", "./c"), ["src/c.js"]);
        assert_eq!(r("src/a.ts", "./b"), ["src/b/index.ts"]);
        assert_eq!(r("src/a.ts", "../lib/d"), ["lib/d.tsx"]);
        assert_eq!(
            r("src/a.ts", "./e.js"),
            ["src/e.ts"],
            "ESM-style compiled extension"
        );
        assert!(r("src/a.ts", "react").is_empty());
        assert!(r("src/a.ts", "../../outside").is_empty());
    }

    #[test]
    fn go_c_java_resolution() {
        let i = idx(&[
            "cmd/main.go",
            "internal/store/store.go",
            "internal/store/store_test.go",
            "include/util.h",
            "src/util.c",
            "src/deep/x.h",
            "src/main/java/com/acme/Foo.java",
            "src/main/java/com/acme/util/Bar.java",
        ]);
        let go = names(
            &i,
            resolve(&i, "go", "cmd/main.go", "github.com/me/proj/internal/store"),
        );
        assert_eq!(
            go,
            ["internal/store/store.go"],
            "test files excluded, module prefix ignored"
        );
        assert!(resolve(&i, "go", "cmd/main.go", "fmt").is_empty());
        assert_eq!(
            names(&i, resolve(&i, "c", "src/util.c", "util.h")),
            ["include/util.h"]
        );
        assert_eq!(
            names(&i, resolve(&i, "c", "src/util.c", "deep/x.h")),
            ["src/deep/x.h"]
        );
        assert_eq!(
            names(&i, resolve(&i, "java", "x.java", "com.acme.Foo")),
            ["src/main/java/com/acme/Foo.java"]
        );
        assert_eq!(
            names(&i, resolve(&i, "java", "x.java", "com.acme.Foo.Inner")),
            ["src/main/java/com/acme/Foo.java"]
        );
        assert_eq!(
            names(&i, resolve(&i, "java", "x.java", "com.acme.util.*")),
            ["src/main/java/com/acme/util/Bar.java"]
        );
    }

    #[test]
    fn self_imports_are_dropped() {
        let i = idx(&["a/mod.rs", "a/x.rs"]);
        assert!(resolve(&i, "rust", "a/x.rs", "use:self").is_empty());
        let _ = resolve(&i, "rust", "a/x.rs", "use:crate::x");
    }

    #[test]
    fn per_blob_storage_roundtrip_and_corruption() {
        let mut b = ImportIndexBuilder::new();
        b.add(LocalBlobId(1), vec!["a".into(), "bb".into()])
            .unwrap();
        b.add(LocalBlobId(3), vec!["ccc".into()]).unwrap();
        assert!(b.add(LocalBlobId(3), vec![]).is_err());
        let bytes = b.finish(5).unwrap();
        let idx = ImportIndex::from_bytes(&bytes).unwrap();
        assert!(idx.imports_of(LocalBlobId(0)).unwrap().is_empty());
        assert_eq!(idx.imports_of(LocalBlobId(1)).unwrap(), ["a", "bb"]);
        assert_eq!(idx.imports_of(LocalBlobId(3)).unwrap(), ["ccc"]);
        assert!(idx.imports_of(LocalBlobId(99)).unwrap().is_empty());
        for n in 0..bytes.len() {
            if let Ok(i) = ImportIndex::from_bytes(&bytes[..n]) {
                // Truncated data may parse the header but must then fail on read, not panic.
                for blob in 0..5 {
                    let _ = i.imports_of(LocalBlobId(blob));
                }
            }
        }
    }
}
