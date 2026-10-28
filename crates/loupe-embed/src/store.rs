//! Per-segment chunk table and quantised vectors.
//!
//! ```text
//! chunks.bin  "LPCH" | version u32 | chunker_version u32 | count u32 | heap_len u32
//!             count x 40-byte records:
//!               blob u32 | start_line u32 | end_line u32 | kind u8 + 3 pad
//!               breadcrumb off u32 | breadcrumb len u32 | chunk hash [16]
//!             string heap
//! embed.vec   "LPEV" | version u32 | dim u32 | count u32
//!             model id (u16 len + bytes), padded to a multiple of 4
//!             count x f32 scales | count x dim i8 vectors
//! ```
//!
//! Record `i` of `chunks.bin` and vector `i` of `embed.vec` describe the same chunk.

use loupe_core::{Error, LocalBlobId, Result};
use loupe_store::{Segment, SegmentWriter};
use rayon::prelude::*;

use crate::cache::ChunkHash;
use crate::chunk::{CHUNKER_VERSION, ChunkKind};
use crate::embedder::dot_i8;

pub const CHUNKS_FILE: &str = "chunks.bin";
pub const EMBED_FILE: &str = "embed.vec";

const CH_MAGIC: &[u8; 4] = b"LPCH";
const EV_MAGIC: &[u8; 4] = b"LPEV";
const VERSION: u32 = 1;
const CH_HEADER: usize = 20;
const RECORD: usize = 40;
const EV_HEADER: usize = 16;

fn corrupt(msg: &str) -> Error {
    Error::Corrupt(format!("embedding index: {msg}"))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkMeta {
    pub blob: LocalBlobId,
    pub start_line: u32,
    pub end_line: u32,
    pub kind: ChunkKind,
    pub breadcrumb: String,
    pub hash: ChunkHash,
}

#[derive(Debug)]
pub struct BuiltEmbeddings {
    pub chunks: Vec<u8>,
    pub vectors: Vec<u8>,
}

impl BuiltEmbeddings {
    pub fn write_to(&self, writer: &mut SegmentWriter) -> Result<()> {
        writer.add_file(CHUNKS_FILE, &self.chunks)?;
        writer.add_file(EMBED_FILE, &self.vectors)
    }

    pub fn view(&self) -> Result<EmbedIndex<'_>> {
        EmbedIndex::from_parts(&self.chunks, &self.vectors)
    }
}

/// Collects chunks and their vectors. Blobs must be added in increasing local-id order.
#[derive(Debug)]
pub struct EmbedBuilder {
    dim: usize,
    model_id: String,
    records: Vec<ChunkMeta>,
    scales: Vec<f32>,
    vectors: Vec<i8>,
    last_blob: Option<u32>,
}

impl EmbedBuilder {
    pub fn new(model_id: impl Into<String>, dim: usize) -> Self {
        EmbedBuilder {
            dim,
            model_id: model_id.into(),
            records: Vec::new(),
            scales: Vec::new(),
            vectors: Vec::new(),
            last_blob: None,
        }
    }

    pub fn add(&mut self, meta: ChunkMeta, scale: f32, values: &[i8]) -> Result<()> {
        if values.len() != self.dim {
            return Err(Error::InvalidArgument(format!(
                "vector has {} dims, index expects {}",
                values.len(),
                self.dim
            )));
        }
        if self.last_blob.is_some_and(|last| meta.blob.0 < last) {
            return Err(Error::InvalidArgument(
                "blobs must be added in increasing order".into(),
            ));
        }
        self.last_blob = Some(meta.blob.0);
        self.records.push(meta);
        self.scales.push(scale);
        self.vectors.extend_from_slice(values);
        Ok(())
    }

    pub fn finish(self) -> BuiltEmbeddings {
        let mut heap: Vec<u8> = Vec::new();
        let mut table = Vec::with_capacity(self.records.len() * RECORD);
        for r in &self.records {
            let off = heap.len() as u32;
            heap.extend_from_slice(r.breadcrumb.as_bytes());
            for v in [r.blob.0, r.start_line, r.end_line] {
                table.extend_from_slice(&v.to_le_bytes());
            }
            table.push(r.kind.to_u8());
            table.extend_from_slice(&[0, 0, 0]);
            table.extend_from_slice(&off.to_le_bytes());
            table.extend_from_slice(&(r.breadcrumb.len() as u32).to_le_bytes());
            table.extend_from_slice(&r.hash);
        }
        let mut chunks = Vec::with_capacity(CH_HEADER + table.len() + heap.len());
        chunks.extend_from_slice(CH_MAGIC);
        for v in [
            VERSION,
            CHUNKER_VERSION,
            self.records.len() as u32,
            heap.len() as u32,
        ] {
            chunks.extend_from_slice(&v.to_le_bytes());
        }
        chunks.extend_from_slice(&table);
        chunks.extend_from_slice(&heap);

        let mut vectors = Vec::new();
        vectors.extend_from_slice(EV_MAGIC);
        for v in [VERSION, self.dim as u32, self.records.len() as u32] {
            vectors.extend_from_slice(&v.to_le_bytes());
        }
        vectors.extend_from_slice(&(self.model_id.len() as u16).to_le_bytes());
        vectors.extend_from_slice(self.model_id.as_bytes());
        while !vectors.len().is_multiple_of(4) {
            vectors.push(0);
        }
        for s in &self.scales {
            vectors.extend_from_slice(&s.to_le_bytes());
        }
        vectors.extend(self.vectors.iter().map(|&v| v as u8));
        BuiltEmbeddings { chunks, vectors }
    }
}

/// A query vector quantised once and scored against many chunk vectors.
#[derive(Clone, Debug)]
pub struct QuantQuery {
    pub scale: f32,
    pub values: Vec<i8>,
}

pub struct EmbedIndex<'a> {
    count: usize,
    dim: usize,
    model_id: &'a str,
    records: &'a [u8],
    heap: &'a [u8],
    scales: &'a [u8],
    vectors: &'a [u8],
}

impl std::fmt::Debug for EmbedIndex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedIndex")
            .field("chunks", &self.count)
            .field("dim", &self.dim)
            .field("model_id", &self.model_id)
            .finish()
    }
}

impl<'a> EmbedIndex<'a> {
    /// `None` if the segment has no embedding files (built before embeddings existed).
    pub fn from_segment(segment: &'a Segment) -> Result<Option<EmbedIndex<'a>>> {
        match (segment.file(CHUNKS_FILE), segment.file(EMBED_FILE)) {
            (Some(c), Some(v)) => Self::from_parts(c, v).map(Some),
            (None, None) => Ok(None),
            _ => Err(corrupt("segment has only one of the embedding files")),
        }
    }

    pub fn from_parts(chunks: &'a [u8], vecs: &'a [u8]) -> Result<EmbedIndex<'a>> {
        if chunks.len() < CH_HEADER || &chunks[..4] != CH_MAGIC {
            return Err(corrupt("bad chunk table header"));
        }
        if u32_at(chunks, 4) != VERSION {
            return Err(Error::FormatVersion {
                found: u32_at(chunks, 4),
                expected: VERSION,
            });
        }
        let count = u32_at(chunks, 12) as usize;
        let heap_len = u32_at(chunks, 16) as usize;
        let records_end = CH_HEADER + count * RECORD;
        if chunks.len() != records_end + heap_len {
            return Err(corrupt("chunk table length mismatch"));
        }

        if vecs.len() < EV_HEADER + 2 || &vecs[..4] != EV_MAGIC {
            return Err(corrupt("bad vector file header"));
        }
        if u32_at(vecs, 4) != VERSION {
            return Err(Error::FormatVersion {
                found: u32_at(vecs, 4),
                expected: VERSION,
            });
        }
        let dim = u32_at(vecs, 8) as usize;
        if u32_at(vecs, 12) as usize != count {
            return Err(corrupt("vector count does not match chunk count"));
        }
        let id_len =
            u16::from_le_bytes(vecs[EV_HEADER..EV_HEADER + 2].try_into().unwrap()) as usize;
        let id_end = EV_HEADER + 2 + id_len;
        let model_id = vecs
            .get(EV_HEADER + 2..id_end)
            .and_then(|b| std::str::from_utf8(b).ok())
            .ok_or_else(|| corrupt("bad model id"))?;
        let scales_at = id_end.next_multiple_of(4);
        let vectors_at = scales_at + count * 4;
        if dim == 0 || vecs.len() != vectors_at + count * dim {
            return Err(corrupt("vector file length mismatch"));
        }
        Ok(EmbedIndex {
            count,
            dim,
            model_id,
            records: &chunks[CH_HEADER..records_end],
            heap: &chunks[records_end..],
            scales: &vecs[scales_at..vectors_at],
            vectors: &vecs[vectors_at..],
        })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn model_id(&self) -> &str {
        self.model_id
    }

    pub fn chunk(&self, i: usize) -> Result<ChunkMeta> {
        let r = self
            .records
            .get(i * RECORD..(i + 1) * RECORD)
            .ok_or_else(|| corrupt("chunk index out of range"))?;
        let (off, len) = (u32_at(r, 16) as usize, u32_at(r, 20) as usize);
        let crumb = off
            .checked_add(len)
            .and_then(|end| self.heap.get(off..end))
            .and_then(|b| std::str::from_utf8(b).ok())
            .ok_or_else(|| corrupt("breadcrumb outside heap"))?;
        Ok(ChunkMeta {
            blob: LocalBlobId(u32_at(r, 0)),
            start_line: u32_at(r, 4),
            end_line: u32_at(r, 8),
            kind: ChunkKind::from_u8(r[12]).ok_or_else(|| corrupt("bad chunk kind"))?,
            breadcrumb: crumb.to_string(),
            hash: r[24..40].try_into().map_err(|_| corrupt("bad hash"))?,
        })
    }

    fn scale(&self, i: usize) -> f32 {
        f32::from_le_bytes(self.scales[i * 4..i * 4 + 4].try_into().unwrap())
    }

    fn vector(&self, i: usize) -> &[i8] {
        let bytes = &self.vectors[i * self.dim..(i + 1) * self.dim];
        // SAFETY: i8 and u8 have identical size and alignment, and every bit pattern is valid.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<i8>(), bytes.len()) }
    }

    /// Exact top-`top` chunks by cosine similarity, best first, as `(score, chunk index)`.
    /// Scans every vector in parallel; with int8 vectors this is a few milliseconds for 100k
    /// chunks, so no approximate index is needed at the supported scale.
    pub fn top(&self, q: &QuantQuery, top: usize) -> Vec<(f32, usize)> {
        if self.count == 0 || q.values.len() != self.dim || top == 0 {
            return Vec::new();
        }
        const BLOCK: usize = 4096;
        let blocks: Vec<usize> = (0..self.count).step_by(BLOCK).collect();
        let mut best: Vec<(f32, usize)> = blocks
            .par_iter()
            .flat_map(|&start| {
                let end = (start + BLOCK).min(self.count);
                let mut local: Vec<(f32, usize)> = (start..end)
                    .map(|i| {
                        (
                            dot_i8(&q.values, self.vector(i)) as f32 * q.scale * self.scale(i),
                            i,
                        )
                    })
                    .collect();
                let keep = top.min(local.len());
                local.select_nth_unstable_by(keep - 1, |a, b| b.0.total_cmp(&a.0));
                local.truncate(keep);
                local
            })
            .collect();
        best.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        best.truncate(top);
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::{Embedder, HashingEmbedder, quantize};
    use loupe_symbols::SymbolKind;

    fn meta(blob: u32, line: u32, crumb: &str) -> ChunkMeta {
        ChunkMeta {
            blob: LocalBlobId(blob),
            start_line: line,
            end_line: line + 3,
            kind: ChunkKind::Symbol(SymbolKind::Function),
            breadcrumb: crumb.into(),
            hash: [blob as u8; 16],
        }
    }

    fn build(texts: &[(u32, &str, &str)]) -> (BuiltEmbeddings, HashingEmbedder) {
        let e = HashingEmbedder::default();
        let mut b = EmbedBuilder::new(e.model_id(), e.dim());
        for (blob, crumb, text) in texts {
            let (scale, q) = quantize(&e.embed(text).unwrap());
            b.add(meta(*blob, 10, crumb), scale, &q).unwrap();
        }
        (b.finish(), e)
    }

    fn query(e: &HashingEmbedder, text: &str) -> QuantQuery {
        let (scale, values) = quantize(&e.embed(text).unwrap());
        QuantQuery { scale, values }
    }

    #[test]
    fn roundtrip_metadata_and_header() {
        let (built, e) = build(&[
            (0, "A > open", "open connection"),
            (2, "close", "close connection"),
        ]);
        let idx = built.view().unwrap();
        assert_eq!(
            (idx.len(), idx.dim(), idx.model_id()),
            (2, 256, e.model_id().as_str())
        );
        let c = idx.chunk(1).unwrap();
        assert_eq!(
            (c.blob, c.start_line, c.end_line, c.breadcrumb.as_str()),
            (LocalBlobId(2), 10, 13, "close")
        );
        assert_eq!(c.hash, [2u8; 16]);
        assert_eq!(idx.chunk(0).unwrap().breadcrumb, "A > open");
        assert!(idx.chunk(2).is_err());
    }

    #[test]
    fn top_k_ranks_by_similarity_and_matches_exact_brute_force() {
        let docs = [
            "fn parse_config_file(path: &Path) -> Config",
            "fn render_triangle_mesh(vertices: &[Vertex])",
            "struct ConfigParser { file: PathBuf }",
            "fn send_http_request(url: &str) -> Response",
            "fn read_file_contents(path: &Path) -> String",
        ];
        let entries: Vec<(u32, String, &str)> = docs
            .iter()
            .enumerate()
            .map(|(i, d)| (i as u32, format!("c{i}"), *d))
            .collect();
        let refs: Vec<(u32, &str, &str)> = entries
            .iter()
            .map(|(i, c, d)| (*i, c.as_str(), *d))
            .collect();
        let (built, e) = build(&refs);
        let idx = built.view().unwrap();
        let q = query(&e, "parse the config file");
        let top = idx.top(&q, 3);
        assert_eq!(top.len(), 3);
        assert_eq!(top[0].1, 0, "the config parser function is the best match");
        assert!(top.windows(2).all(|w| w[0].0 >= w[1].0), "best first");

        // Exact: the top-3 equals sorting all scores.
        let mut all: Vec<(f32, usize)> = (0..idx.len())
            .map(|i| {
                (
                    dot_i8(&q.values, idx.vector(i)) as f32 * q.scale * idx.scale(i),
                    i,
                )
            })
            .collect();
        all.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        assert_eq!(
            top.iter().map(|t| t.1).collect::<Vec<_>>(),
            all.iter().take(3).map(|t| t.1).collect::<Vec<_>>()
        );
        assert_eq!(
            idx.top(&q, 100).len(),
            5,
            "asking for more than exist returns all"
        );
        assert!(idx.top(&q, 0).is_empty());
    }

    #[test]
    fn many_vectors_cross_the_parallel_block_boundary_correctly() {
        let e = HashingEmbedder::default();
        let mut b = EmbedBuilder::new(e.model_id(), e.dim());
        let mut target = 0;
        for i in 0..10_000u32 {
            let text = if i == 7777 {
                "the unique_needle_function handles zebra crossings".to_string()
            } else {
                format!("filler_{i} helper value_{}", i % 97)
            };
            if i == 7777 {
                target = b.records.len();
            }
            let (s, q) = quantize(&e.embed(&text).unwrap());
            b.add(meta(i / 4, i, "x"), s, &q).unwrap();
        }
        let built = b.finish();
        let idx = built.view().unwrap();
        let top = idx.top(&query(&e, "unique needle function zebra crossings"), 5);
        assert_eq!(top[0].1, target);
        assert!(top[0].0 > top[1].0 + 0.1, "clear margin: {top:?}");
    }

    #[test]
    fn empty_index_and_dimension_mismatch() {
        let e = HashingEmbedder::default();
        let built = EmbedBuilder::new(e.model_id(), e.dim()).finish();
        let idx = built.view().unwrap();
        assert!(idx.is_empty());
        assert!(idx.top(&query(&e, "anything"), 5).is_empty());
        let mut b = EmbedBuilder::new("m", 8);
        assert!(b.add(meta(0, 1, "x"), 1.0, &[1, 2, 3]).is_err());
        b.add(meta(3, 1, "x"), 1.0, &[0; 8]).unwrap();
        assert!(
            b.add(meta(2, 1, "x"), 1.0, &[0; 8]).is_err(),
            "blob order enforced"
        );
        let wrong_dim = QuantQuery {
            scale: 1.0,
            values: vec![0; 4],
        };
        assert!(b.finish().view().unwrap().top(&wrong_dim, 3).is_empty());
    }

    #[test]
    fn every_truncation_and_corruption_is_an_error_not_a_panic() {
        let (built, _) = build(&[
            (0, "alpha", "open connection"),
            (1, "beta", "close connection"),
        ]);
        for n in 0..built.chunks.len() {
            assert!(
                EmbedIndex::from_parts(&built.chunks[..n], &built.vectors).is_err(),
                "chunks cut {n}"
            );
        }
        for n in 0..built.vectors.len() {
            assert!(
                EmbedIndex::from_parts(&built.chunks, &built.vectors[..n]).is_err(),
                "vectors cut {n}"
            );
        }
        let mut bad = built.chunks.clone();
        let rec = CH_HEADER;
        bad[rec + 16..rec + 20].copy_from_slice(&u32::MAX.to_le_bytes()); // breadcrumb offset
        let idx = EmbedIndex::from_parts(&bad, &built.vectors).unwrap();
        assert!(idx.chunk(0).is_err());
        let mut bad = built.chunks.clone();
        bad[rec + 12] = 250; // chunk kind
        assert!(
            EmbedIndex::from_parts(&bad, &built.vectors)
                .unwrap()
                .chunk(0)
                .is_err()
        );
    }
}
