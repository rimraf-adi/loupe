//! AST-aware chunking: one chunk per function/method/type definition, windowed fallback for
//! files without symbols.
//!
//! A chunk's embedded text is `doc comment + definition source` (the definition's text already
//! starts with its signature), capped in size. The path is deliberately *not* part of the text or
//! the cache key, so a moved or renamed file reuses its vectors; the breadcrumb is kept as
//! metadata only. Container definitions (classes, impls, traits) contribute a header chunk
//! (signature, doc, member names) rather than duplicating their members' bodies.

use loupe_symbols::{Symbol, SymbolKind};

/// Bump when chunking changes in a way that alters chunk text.
pub const CHUNKER_VERSION: u32 = 1;
const MAX_CHUNKS_PER_FILE: usize = 400;

#[derive(Clone, Debug)]
pub struct ChunkOptions {
    /// Prepend the definition's doc comment to its chunk text. Production indexes want this;
    /// evaluations that use doc comments as queries must turn it off to avoid leaking the answer.
    pub include_docs: bool,
    pub max_bytes: usize,
    pub window_lines: usize,
    pub window_overlap: usize,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        ChunkOptions {
            include_docs: true,
            max_bytes: 1500,
            window_lines: 40,
            window_overlap: 10,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkKind {
    Symbol(SymbolKind),
    Window,
}

impl ChunkKind {
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            ChunkKind::Window => 0,
            ChunkKind::Symbol(k) => 1 + SymbolKind::ALL.iter().position(|x| *x == k).unwrap() as u8,
        }
    }

    pub(crate) fn from_u8(v: u8) -> Option<ChunkKind> {
        match v {
            0 => Some(ChunkKind::Window),
            n => SymbolKind::ALL
                .get(n as usize - 1)
                .map(|k| ChunkKind::Symbol(*k)),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ChunkKind::Window => "text",
            ChunkKind::Symbol(k) => k.as_str(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// 1-based, inclusive.
    pub start_line: u32,
    pub end_line: u32,
    pub kind: ChunkKind,
    /// `Parent > name` for definitions, `L10-49` for windows. Display only.
    pub breadcrumb: String,
    /// What gets embedded.
    pub text: String,
    /// 128-bit cache key: model id, chunker version, and text.
    pub hash: [u8; 16],
}

fn chunk_hash(model_id: &str, text: &str) -> [u8; 16] {
    let mut h = blake3::Hasher::new();
    h.update(model_id.as_bytes());
    h.update(&[0]);
    h.update(&CHUNKER_VERSION.to_le_bytes());
    h.update(&[0]);
    h.update(text.as_bytes());
    h.finalize().as_bytes()[..16].try_into().unwrap()
}

/// Cut `s` to at most `max` bytes, at a line boundary when one exists in the second half.
fn cap(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    match s[..cut].rfind('\n') {
        Some(nl) if nl > max / 2 => &s[..nl],
        _ => &s[..cut],
    }
}

fn breadcrumb(symbols: &[Symbol], i: usize) -> String {
    let mut parts = vec![symbols[i].name.as_str()];
    let mut cur = symbols[i].parent;
    while let Some(p) = cur {
        let p = p as usize;
        if p >= i || parts.len() > 8 {
            break;
        }
        parts.push(symbols[p].name.as_str());
        cur = symbols[p].parent;
    }
    parts.reverse();
    parts.join(" > ")
}

/// Chunk one file. `symbols` are its extracted definitions (empty for files with no grammar).
pub fn chunk_file(
    source: &[u8],
    symbols: &[Symbol],
    opts: &ChunkOptions,
    model_id: &str,
) -> Vec<Chunk> {
    let text = String::from_utf8_lossy(source);
    let mut chunks = if symbols.is_empty() {
        window_chunks(&text, opts)
    } else {
        symbol_chunks(&text, source.len(), symbols, opts)
    };
    chunks.truncate(MAX_CHUNKS_PER_FILE);
    for c in &mut chunks {
        c.hash = chunk_hash(model_id, &c.text);
    }
    chunks
}

fn symbol_chunks(
    text: &str,
    source_len: usize,
    symbols: &[Symbol],
    opts: &ChunkOptions,
) -> Vec<Chunk> {
    let lossless = text.len() == source_len; // byte offsets only valid if decoding did not change length
    let mut out = Vec::new();
    for (i, s) in symbols.iter().enumerate() {
        if matches!(
            s.kind,
            SymbolKind::Variable | SymbolKind::Module | SymbolKind::Constant
        ) {
            continue;
        }
        let doc = if opts.include_docs {
            s.doc.as_deref().unwrap_or("")
        } else {
            ""
        };
        let body = if s.kind.is_container_kind() {
            // Header only: the members get their own chunks.
            let members: Vec<&str> = symbols
                .iter()
                .filter(|m| m.parent == Some(i as u32))
                .map(|m| m.name.as_str())
                .take(20)
                .collect();
            if members.is_empty() {
                definition_source(text, s, lossless)
            } else {
                format!("{}\nmembers: {}", s.signature, members.join(", "))
            }
        } else {
            definition_source(text, s, lossless)
        };
        let full = if doc.is_empty() {
            body
        } else {
            format!("{doc}\n{body}")
        };
        out.push(Chunk {
            start_line: s.start_line,
            end_line: s.end_line,
            kind: ChunkKind::Symbol(s.kind),
            breadcrumb: breadcrumb(symbols, i),
            text: cap(&full, opts.max_bytes).to_string(),
            hash: [0; 16],
        });
    }
    out
}

fn definition_source(text: &str, s: &Symbol, lossless: bool) -> String {
    if lossless && let Some(slice) = text.get(s.start_byte as usize..s.end_byte as usize) {
        return slice.to_string();
    }
    s.signature.clone()
}

fn window_chunks(text: &str, opts: &ChunkOptions) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    let step = opts.window_lines.saturating_sub(opts.window_overlap).max(1);
    let mut out = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let end = (start + opts.window_lines).min(lines.len());
        let body = lines[start..end].join("\n");
        if body.chars().any(char::is_alphanumeric) {
            out.push(Chunk {
                start_line: start as u32 + 1,
                end_line: end as u32,
                kind: ChunkKind::Window,
                breadcrumb: format!("L{}-{}", start + 1, end),
                text: cap(&body, opts.max_bytes).to_string(),
                hash: [0; 16],
            });
        }
        if end == lines.len() {
            break;
        }
        start += step;
    }
    out
}

trait ContainerKind {
    fn is_container_kind(&self) -> bool;
}

impl ContainerKind for SymbolKind {
    fn is_container_kind(&self) -> bool {
        matches!(
            self,
            SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Impl
                | SymbolKind::Trait
                | SymbolKind::Interface
                | SymbolKind::Enum
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loupe_symbols::{Registry, extract_symbols};

    const MODEL: &str = "test-model";

    fn rust_chunks(src: &str, opts: &ChunkOptions) -> Vec<Chunk> {
        let syms = extract_symbols(Registry::global(), Some("rust"), src.as_bytes());
        chunk_file(src.as_bytes(), &syms, opts, MODEL)
    }

    const SRC: &str = "/// Opens a connection.\npub fn open(host: &str) -> Conn {\n    Conn::connect(host)\n}\n\n/// A pool.\npub struct Pool {\n    size: usize,\n}\n\nimpl Pool {\n    /// Make one.\n    pub fn new(size: usize) -> Pool {\n        Pool { size }\n    }\n\n    fn grow(&mut self) {\n        self.size += 1;\n    }\n}\n";

    #[test]
    fn functions_become_chunks_with_docs_and_bodies() {
        let c = rust_chunks(SRC, &ChunkOptions::default());
        let open = c.iter().find(|c| c.breadcrumb == "open").unwrap();
        assert_eq!(open.kind, ChunkKind::Symbol(SymbolKind::Function));
        assert!(
            open.text
                .starts_with("Opens a connection.\npub fn open(host: &str) -> Conn"),
            "{}",
            open.text
        );
        assert!(open.text.contains("Conn::connect(host)"));
        assert_eq!((open.start_line, open.end_line), (2, 4));
    }

    #[test]
    fn methods_get_a_parent_breadcrumb_and_containers_get_header_chunks() {
        let c = rust_chunks(SRC, &ChunkOptions::default());
        let crumbs: Vec<&str> = c.iter().map(|c| c.breadcrumb.as_str()).collect();
        assert!(
            crumbs.contains(&"Pool > new") && crumbs.contains(&"Pool > grow"),
            "{crumbs:?}"
        );
        let imp = c
            .iter()
            .find(|c| c.kind == ChunkKind::Symbol(SymbolKind::Impl))
            .unwrap();
        assert!(imp.text.contains("members: new, grow"), "{}", imp.text);
        assert!(
            !imp.text.contains("self.size += 1"),
            "container chunks do not duplicate member bodies"
        );
        let new = c.iter().find(|c| c.breadcrumb == "Pool > new").unwrap();
        assert!(new.text.starts_with("Make one."));
    }

    #[test]
    fn docs_can_be_left_out() {
        let c = rust_chunks(
            SRC,
            &ChunkOptions {
                include_docs: false,
                ..ChunkOptions::default()
            },
        );
        let open = c.iter().find(|c| c.breadcrumb == "open").unwrap();
        assert!(!open.text.contains("Opens a connection"), "{}", open.text);
        assert!(open.text.starts_with("pub fn open"));
    }

    #[test]
    fn hash_covers_text_model_and_ignores_path_and_position() {
        let a = rust_chunks(SRC, &ChunkOptions::default());
        let shifted = format!("// a new leading comment\n\n{SRC}");
        let b = rust_chunks(&shifted, &ChunkOptions::default());
        let ha = a.iter().find(|c| c.breadcrumb == "open").unwrap().hash;
        let hb = b.iter().find(|c| c.breadcrumb == "open").unwrap().hash;
        assert_eq!(
            ha, hb,
            "moving a function within or between files keeps its key"
        );
        let edited = rust_chunks(
            &SRC.replace("Conn::connect(host)", "Conn::dial(host)"),
            &ChunkOptions::default(),
        );
        assert_ne!(
            ha,
            edited.iter().find(|c| c.breadcrumb == "open").unwrap().hash
        );
        let syms = extract_symbols(Registry::global(), Some("rust"), SRC.as_bytes());
        let other_model = chunk_file(
            SRC.as_bytes(),
            &syms,
            &ChunkOptions::default(),
            "other-model",
        );
        assert_ne!(
            ha,
            other_model
                .iter()
                .find(|c| c.breadcrumb == "open")
                .unwrap()
                .hash
        );
    }

    #[test]
    fn big_definitions_are_capped_on_line_boundaries() {
        let body: String = (0..200)
            .map(|i| format!("    let value_{i} = compute({i});\n"))
            .collect();
        let src = format!("fn big() {{\n{body}}}\n");
        let c = rust_chunks(
            &src,
            &ChunkOptions {
                max_bytes: 300,
                ..ChunkOptions::default()
            },
        );
        assert_eq!(c.len(), 1);
        assert!(c[0].text.len() <= 300);
        assert!(
            !c[0].text.ends_with("let value_"),
            "cut at a line boundary: {:?}",
            &c[0].text[c[0].text.len() - 20..]
        );
    }

    #[test]
    fn files_without_symbols_use_overlapping_windows() {
        let text: String = (1..=100)
            .map(|i| format!("line {i} has some words\n"))
            .collect();
        let c = chunk_file(
            text.as_bytes(),
            &[],
            &ChunkOptions {
                window_lines: 40,
                window_overlap: 10,
                ..ChunkOptions::default()
            },
            MODEL,
        );
        let ranges: Vec<(u32, u32)> = c.iter().map(|c| (c.start_line, c.end_line)).collect();
        assert_eq!(ranges, [(1, 40), (31, 70), (61, 100)]);
        assert!(c.iter().all(|c| c.kind == ChunkKind::Window));
        assert_eq!(c[1].breadcrumb, "L31-70");
        // Blank-only windows are skipped.
        let blank = chunk_file(
            "\n".repeat(100).as_bytes(),
            &[],
            &ChunkOptions::default(),
            MODEL,
        );
        assert!(blank.is_empty());
        assert!(chunk_file(b"", &[], &ChunkOptions::default(), MODEL).is_empty());
    }

    #[test]
    fn non_utf8_and_chunk_cap_do_not_panic() {
        let mut bytes = b"fn f() { /* \xff\xfe */ }\n".to_vec();
        bytes.extend_from_slice("fn g() {}\n".repeat(1000).as_bytes());
        let syms = extract_symbols(Registry::global(), Some("rust"), &bytes);
        let c = chunk_file(&bytes, &syms, &ChunkOptions::default(), MODEL);
        assert!(!c.is_empty() && c.len() <= MAX_CHUNKS_PER_FILE);
    }

    #[test]
    fn kind_codes_roundtrip() {
        assert_eq!(
            ChunkKind::from_u8(ChunkKind::Window.to_u8()),
            Some(ChunkKind::Window)
        );
        for k in SymbolKind::ALL {
            assert_eq!(
                ChunkKind::from_u8(ChunkKind::Symbol(k).to_u8()),
                Some(ChunkKind::Symbol(k))
            );
        }
        assert_eq!(ChunkKind::from_u8(200), None);
    }
}
