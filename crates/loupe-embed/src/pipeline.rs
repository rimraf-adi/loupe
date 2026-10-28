//! Turning chunks into quantised vectors, reusing cached ones.

use std::collections::HashMap;

use loupe_core::{Error, Result};
use rayon::prelude::*;

use crate::cache::{ChunkHash, QuantVec, VectorCache};
use crate::chunk::Chunk;
use crate::embedder::{Embedder, quantize};

/// Texts embedded per call; bounds memory and matches how neural models batch.
const BATCH: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EmbedStats {
    /// Chunks that had to be embedded.
    pub embedded: usize,
    /// Chunks answered from the cache (including duplicates within the call).
    pub reused: usize,
}

/// One quantised vector per chunk, in order. Chunks whose hash is already cached (or repeated in
/// `chunks`) are not embedded again; the rest are embedded in parallel batches and cached.
pub fn embed_chunks(
    embedder: &dyn Embedder,
    cache: &VectorCache,
    chunks: &[&Chunk],
) -> Result<(Vec<QuantVec>, EmbedStats)> {
    let dim = embedder.dim();
    let mut by_hash: HashMap<ChunkHash, QuantVec> = HashMap::new();
    let mut to_embed: Vec<&Chunk> = Vec::new();
    for c in chunks {
        if by_hash.contains_key(&c.hash) || to_embed.iter().any(|t| t.hash == c.hash) {
            continue;
        }
        match cache.get(&c.hash, dim) {
            Some(v) => {
                by_hash.insert(c.hash, v);
            }
            None => to_embed.push(c),
        }
    }

    let batches: Vec<Result<Vec<(ChunkHash, QuantVec)>>> = to_embed
        .par_chunks(BATCH)
        .map(|batch| {
            let texts: Vec<&str> = batch.iter().map(|c| c.text.as_str()).collect();
            let vectors = embedder.embed_batch(&texts)?;
            if vectors.len() != batch.len() || vectors.iter().any(|v| v.len() != dim) {
                return Err(Error::InvalidArgument(format!(
                    "embedder {} returned vectors that do not match dim {dim}",
                    embedder.model_id()
                )));
            }
            Ok(batch
                .iter()
                .zip(vectors)
                .map(|(c, v)| {
                    let (scale, values) = quantize(&v);
                    (c.hash, QuantVec { scale, values })
                })
                .collect())
        })
        .collect();

    let mut fresh: Vec<(ChunkHash, QuantVec)> = Vec::new();
    for b in batches {
        fresh.extend(b?);
    }
    cache.put_many(&fresh);
    let embedded = fresh.len();
    by_hash.extend(fresh);

    let out = chunks.iter().map(|c| by_hash[&c.hash].clone()).collect();
    Ok((
        out,
        EmbedStats {
            embedded,
            reused: chunks.len() - embedded,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::ChunkKind;
    use crate::embedder::HashingEmbedder;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Wraps an embedder and counts how many texts it was asked to embed.
    struct Counting {
        inner: HashingEmbedder,
        texts: AtomicUsize,
    }
    impl Embedder for Counting {
        fn model_id(&self) -> String {
            self.inner.model_id()
        }
        fn dim(&self) -> usize {
            self.inner.dim()
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            self.texts.fetch_add(texts.len(), Ordering::SeqCst);
            self.inner.embed_batch(texts)
        }
    }

    fn counting() -> Counting {
        Counting {
            inner: HashingEmbedder::default(),
            texts: AtomicUsize::new(0),
        }
    }

    fn chunk(text: &str) -> Chunk {
        let mut h = [0u8; 16];
        h.copy_from_slice(&blake3::hash(text.as_bytes()).as_bytes()[..16]);
        Chunk {
            start_line: 1,
            end_line: 1,
            kind: ChunkKind::Window,
            breadcrumb: "x".into(),
            text: text.into(),
            hash: h,
        }
    }

    #[test]
    fn only_new_chunks_are_embedded_and_results_keep_order() {
        let e = counting();
        let cache = VectorCache::in_memory();
        let (a, b, c) = (
            chunk("alpha beta"),
            chunk("gamma delta"),
            chunk("epsilon zeta"),
        );

        let (v1, s1) = embed_chunks(&e, &cache, &[&a, &b]).unwrap();
        assert_eq!((s1.embedded, s1.reused), (2, 0));
        assert_eq!(e.texts.load(Ordering::SeqCst), 2);

        let (v2, s2) = embed_chunks(&e, &cache, &[&b, &c, &a]).unwrap();
        assert_eq!(
            (s2.embedded, s2.reused),
            (1, 2),
            "only the new chunk is embedded"
        );
        assert_eq!(e.texts.load(Ordering::SeqCst), 3);
        assert_eq!(
            v2[0], v1[1],
            "order follows the input, values come from the cache"
        );
        assert_eq!(v2[2], v1[0]);
        assert_ne!(v2[1], v2[0]);
    }

    #[test]
    fn duplicates_within_one_call_are_embedded_once() {
        let e = counting();
        let cache = VectorCache::in_memory();
        let a = chunk("same text here");
        let (v, s) = embed_chunks(&e, &cache, &[&a, &a, &a]).unwrap();
        assert_eq!((s.embedded, s.reused), (1, 2));
        assert_eq!(e.texts.load(Ordering::SeqCst), 1);
        assert!(v[0] == v[1] && v[1] == v[2]);
    }

    #[test]
    fn large_batches_split_and_all_get_vectors() {
        let e = counting();
        let cache = VectorCache::in_memory();
        let chunks: Vec<Chunk> = (0..300)
            .map(|i| chunk(&format!("function number_{i} does thing_{}", i % 7)))
            .collect();
        let refs: Vec<&Chunk> = chunks.iter().collect();
        let (v, s) = embed_chunks(&e, &cache, &refs).unwrap();
        assert_eq!(v.len(), 300);
        assert_eq!(s.embedded, 300);
        assert!(v.iter().all(|q| q.values.len() == 256));
    }

    #[test]
    fn empty_input_and_a_misbehaving_embedder() {
        let e = counting();
        let cache = VectorCache::in_memory();
        let (v, s) = embed_chunks(&e, &cache, &[]).unwrap();
        assert!(v.is_empty() && s == EmbedStats::default());

        struct Broken;
        impl Embedder for Broken {
            fn model_id(&self) -> String {
                "broken".into()
            }
            fn dim(&self) -> usize {
                8
            }
            fn embed_batch(&self, _: &[&str]) -> Result<Vec<Vec<f32>>> {
                Ok(vec![vec![0.0; 3]])
            }
        }
        assert!(embed_chunks(&Broken, &cache, &[&chunk("x y z")]).is_err());
    }

    #[test]
    fn cache_entries_do_not_cross_models() {
        let cache = VectorCache::in_memory();
        let small = HashingEmbedder::new(64);
        let big = HashingEmbedder::default();
        let a = chunk("shared text for both");
        embed_chunks(&small, &cache, &[&a]).unwrap();
        // Same hash but a different dimension: must be a miss and be recomputed correctly.
        let (v, s) = embed_chunks(&big, &cache, &[&a]).unwrap();
        assert_eq!(s.embedded, 1);
        assert_eq!(v[0].values.len(), 256);
    }
}
