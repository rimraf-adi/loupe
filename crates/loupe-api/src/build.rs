//! Building one segment: text, symbol, import, and embedding indexes from one read of each file.

use std::fs;
use std::path::Path;

use loupe_core::{BlobId, Result};
use loupe_embed::{
    Chunk, ChunkMeta, ChunkOptions, EmbedBuilder, Embedder, VectorCache, chunk_file, embed_chunks,
};
use loupe_graph::{ImportIndexBuilder, extract_imports};
use loupe_store::SegmentWriter;
use loupe_symbols::{Registry, Symbol, SymbolIndexBuilder, extract_symbols};
use loupe_text::{FileTerms, TextIndexBuilder, extract_terms};
use rayon::prelude::*;

/// Files are read and parsed this many at a time, bounding memory to one chunk of contents.
const CHUNK: usize = 256;

/// Files a segment must contain to be usable by the current code. A live segment missing any of
/// them (built by an older version) is dropped and its blobs are re-indexed.
pub const REQUIRED_SEGMENT_FILES: [&str; 8] = [
    loupe_text::TRI_FILE,
    loupe_text::POST_FILE,
    loupe_text::TOK_FILE,
    loupe_symbols::SYMBOLS_FILE,
    loupe_symbols::NAMES_FILE,
    loupe_graph::IMPORTS_FILE,
    loupe_embed::CHUNKS_FILE,
    loupe_embed::EMBED_FILE,
];

pub struct Item {
    pub blob: BlobId,
    /// Repo-relative path to read the content from.
    pub path: String,
    pub lang: Option<String>,
    /// File size, used to split large builds into memory-bounded segments.
    pub size: u64,
}

/// How chunks are embedded for this build.
pub struct EmbedCtx<'a> {
    pub embedder: &'a dyn Embedder,
    pub cache: &'a VectorCache,
    pub chunking: &'a ChunkOptions,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildStats {
    pub indexed: usize,
    /// The file's content no longer matched its blob (edited mid-run).
    pub skipped_changed: usize,
    pub skipped_unreadable: usize,
    /// Chunks that needed a fresh embedding.
    pub chunks_embedded: usize,
    /// Chunks whose vector came from the cache.
    pub chunks_reused: usize,
}

struct Parsed {
    terms: FileTerms,
    symbols: Vec<Symbol>,
    imports: Vec<String>,
    chunks: Vec<Chunk>,
}

enum Extracted {
    Ok(Box<Parsed>),
    Changed,
    Unreadable,
}

/// Index every item into `writer`. A file is re-hashed after reading; if it no longer matches
/// its blob it is skipped rather than indexed under the wrong identity (the next scan sees the
/// new content).
pub fn build_segment(
    writer: &mut SegmentWriter,
    root: &Path,
    items: &[Item],
    embed: &EmbedCtx<'_>,
) -> Result<BuildStats> {
    let registry = Registry::global();
    let model_id = embed.embedder.model_id();
    let mut stats = BuildStats::default();
    let mut text = TextIndexBuilder::new();
    let mut symbols = SymbolIndexBuilder::new();
    let mut imports = ImportIndexBuilder::new();
    let mut vectors = EmbedBuilder::new(model_id.clone(), embed.embedder.dim());

    for chunk in items.chunks(CHUNK) {
        let mut extracted: Vec<Extracted> = chunk
            .par_iter()
            .map(|item| match fs::read(root.join(&item.path)) {
                Err(_) => Extracted::Unreadable,
                Ok(bytes) if BlobId::of(&bytes) != item.blob => Extracted::Changed,
                Ok(bytes) => {
                    let syms = extract_symbols(registry, item.lang.as_deref(), &bytes);
                    let chunks = chunk_file(&bytes, &syms, embed.chunking, &model_id);
                    Extracted::Ok(Box::new(Parsed {
                        terms: extract_terms(&bytes),
                        imports: extract_imports(item.lang.as_deref(), &bytes),
                        symbols: syms,
                        chunks,
                    }))
                }
            })
            .collect();

        // One embedding pass for every chunk of every file in this group, reusing the cache.
        let all: Vec<&Chunk> = extracted
            .iter()
            .filter_map(|e| match e {
                Extracted::Ok(p) => Some(p.chunks.iter()),
                _ => None,
            })
            .flatten()
            .collect();
        let (quantised, estats) = embed_chunks(embed.embedder, embed.cache, &all)?;
        stats.chunks_embedded += estats.embedded;
        stats.chunks_reused += estats.reused;
        let mut next_vector = quantised.into_iter();

        for (item, result) in chunk.iter().zip(extracted.drain(..)) {
            match result {
                Extracted::Ok(p) => {
                    let p = *p;
                    let local = writer.add_blob(item.blob);
                    text.add(local, p.terms)?;
                    symbols.add(local, p.symbols)?;
                    imports.add(local, p.imports)?;
                    for c in &p.chunks {
                        let q = next_vector.next().expect("one vector per chunk");
                        vectors.add(
                            ChunkMeta {
                                blob: local,
                                start_line: c.start_line,
                                end_line: c.end_line,
                                kind: c.kind,
                                breadcrumb: c.breadcrumb.clone(),
                                hash: c.hash,
                            },
                            q.scale,
                            &q.values,
                        )?;
                    }
                    stats.indexed += 1;
                }
                Extracted::Changed => stats.skipped_changed += 1,
                Extracted::Unreadable => stats.skipped_unreadable += 1,
            }
        }
    }

    text.finish()?.write_to(writer)?;
    symbols.finish(stats.indexed as u32)?.write_to(writer)?;
    imports.write_to(writer, stats.indexed as u32)?;
    vectors.finish().write_to(writer)?;
    Ok(stats)
}
