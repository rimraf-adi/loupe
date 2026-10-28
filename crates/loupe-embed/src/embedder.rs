//! The `Embedder` trait and the built-in offline embedder.
//!
//! [`HashingEmbedder`] maps text to a fixed-size vector by hashing identifier *subwords*
//! (camelCase / snake_case split) and their bigrams into buckets with a random sign, then
//! L2-normalising. It is deterministic, dependency-free, and fast, and it captures vocabulary
//! overlap including partial identifier matches (`parse_config` ~ `ConfigParser`). It is **not**
//! a neural model: it cannot relate words that never co-occur. A neural embedder implements the
//! same trait and plugs in unchanged.

use loupe_core::Result;

pub trait Embedder: Send + Sync {
    /// Identifies the model and its settings; part of every chunk cache key and recorded in each
    /// segment, so vectors from different models are never mixed.
    fn model_id(&self) -> String;

    fn dim(&self) -> usize;

    /// One vector per text, each `dim()` long and L2-normalised.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed_batch(&[text])?.remove(0))
    }

    /// Embed a search query. Asymmetric models (query vs passage) override this; the default
    /// treats a query like any other text.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.embed(text)
    }
}

/// Words that carry no signal in code or prose.
const STOPWORDS: &[&str] = &[
    "the", "an", "of", "to", "in", "and", "or", "is", "it", "for", "on", "with", "as", "at", "by",
    "be", "this", "that", "self", "let", "mut", "pub", "fn", "def", "return", "if", "else", "use",
    "impl", "const", "var",
];

#[derive(Clone, Debug)]
pub struct HashingEmbedder {
    dim: usize,
}

impl HashingEmbedder {
    pub const DEFAULT_DIM: usize = 256;

    pub fn new(dim: usize) -> Self {
        assert!(
            dim >= 8 && dim.is_multiple_of(8),
            "dim must be a multiple of 8, at least 8"
        );
        HashingEmbedder { dim }
    }
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self::new(Self::DEFAULT_DIM)
    }
}

/// FNV-1a with a final avalanche. Stable across platforms and releases (vectors are cached and
/// persisted, so the hash must never change without bumping the model id).
fn hash64(bytes: &[u8], seed: u64) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h
}

/// Split into lowercase subwords: identifiers are cut at `_`, digits, and camelCase boundaries
/// (`HTTPServer2Go` -> `http`, `server`, `go`). Returns `(whole identifier, its subwords)` groups.
fn identifiers(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<(String, Vec<String>)>| {
        if cur.is_empty() {
            return;
        }
        let subs = split_subwords(cur);
        if !subs.is_empty() {
            out.push((cur.to_lowercase(), subs));
        }
        cur.clear();
    };
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn split_subwords(ident: &str) -> Vec<String> {
    let chars: Vec<char> = ident.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let boundary = match prev {
            Some(p) if p.is_alphanumeric() => {
                (p.is_lowercase() && c.is_uppercase())
                    || (p.is_uppercase()
                        && c.is_uppercase()
                        && next.is_some_and(char::is_lowercase))
                    || (p.is_alphabetic() != c.is_alphabetic())
            }
            _ => false,
        };
        if boundary && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
        .into_iter()
        .filter(|w| w.chars().count() >= 2 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

impl HashingEmbedder {
    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut counts: std::collections::HashMap<(u64, u8), f32> =
            std::collections::HashMap::new();
        let mut add = |feature: &str, kind: u8| {
            *counts
                .entry((hash64(feature.as_bytes(), u64::from(kind)), kind))
                .or_default() += 1.0;
        };
        for (whole, subs) in identifiers(text) {
            for s in &subs {
                add(s, 0);
            }
            for pair in subs.windows(2) {
                add(&format!("{} {}", pair[0], pair[1]), 1);
            }
            if subs.len() >= 2 {
                add(&whole, 2);
            }
        }
        let mut v = vec![0f32; self.dim];
        for ((h, kind), tf) in counts {
            let weight = match kind {
                0 => 1.0,
                1 => 0.6,
                _ => 0.8,
            };
            // Sublinear term frequency, so a word repeated 50 times does not drown the rest.
            let w = weight * (1.0 + tf.ln());
            let idx = (h % self.dim as u64) as usize;
            let sign = if (h >> 40) & 1 == 0 { 1.0 } else { -1.0 };
            v[idx] += sign * w;
        }
        normalize(&mut v);
        v
    }
}

impl Embedder for HashingEmbedder {
    fn model_id(&self) -> String {
        format!("loupe-hash-v1-d{}", self.dim)
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }
}

pub(crate) fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v {
            *x /= norm;
        }
    }
}

/// Symmetric int8 quantisation: `v ~ scale * q`.
pub fn quantize(v: &[f32]) -> (f32, Vec<i8>) {
    let max = v.iter().fold(0f32, |m, x| m.max(x.abs()));
    if max == 0.0 {
        return (0.0, vec![0; v.len()]);
    }
    let scale = max / 127.0;
    (
        scale,
        v.iter()
            .map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8)
            .collect(),
    )
}

/// Integer dot product; `auto-vectorises` well because the accumulation is i32.
#[inline]
pub fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| i32::from(x) * i32::from(y))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cos(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    fn sub(s: &str) -> Vec<String> {
        identifiers(s).into_iter().flat_map(|(_, w)| w).collect()
    }

    #[test]
    fn subword_splitting() {
        assert_eq!(sub("parseConfigFile"), ["parse", "config", "file"]);
        assert_eq!(sub("parse_config_file"), ["parse", "config", "file"]);
        assert_eq!(sub("HTTPServer"), ["http", "server"]);
        assert_eq!(sub("Utf8Decoder2Go"), ["utf", "decoder", "go"]);
        assert_eq!(
            sub("the fn pub x"),
            Vec::<String>::new(),
            "stopwords and 1-letter words dropped"
        );
        assert_eq!(sub("a.b-c"), Vec::<String>::new());
    }

    #[test]
    fn vectors_are_unit_length_deterministic_and_the_right_size() {
        let e = HashingEmbedder::default();
        let v = e.embed("fn parse_config(path: &Path) -> Config").unwrap();
        assert_eq!(v.len(), 256);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert_eq!(
            v,
            e.embed("fn parse_config(path: &Path) -> Config").unwrap()
        );
        assert_eq!(e.embed_batch(&["a b", "c d"]).unwrap().len(), 2);
        assert_eq!(e.model_id(), "loupe-hash-v1-d256");
        assert_eq!(HashingEmbedder::new(64).model_id(), "loupe-hash-v1-d64");
    }

    #[test]
    fn empty_and_stopword_only_text_gives_a_zero_vector_not_nan() {
        let e = HashingEmbedder::default();
        for t in ["", "   ", "the of and"] {
            let v = e.embed(t).unwrap();
            assert!(v.iter().all(|x| *x == 0.0), "{t:?}");
        }
    }

    #[test]
    fn related_text_scores_higher_than_unrelated() {
        let e = HashingEmbedder::default();
        let q = e.embed("parse the configuration file").unwrap();
        let related = e
            .embed("fn parse_config_file(path: &Path) -> Config { read the file }")
            .unwrap();
        let partial = e.embed("struct ConfigParser { file: PathBuf }").unwrap();
        let unrelated = e
            .embed("fn render_triangle_mesh(vertices: &[Vertex], shader: Shader)")
            .unwrap();
        let (r, p, u) = (cos(&q, &related), cos(&q, &partial), cos(&q, &unrelated));
        assert!(
            r > p && p > u,
            "related {r:.3} > partial {p:.3} > unrelated {u:.3}"
        );
        assert!(r > 0.3 && u < 0.15, "{r:.3} {u:.3}");
    }

    #[test]
    fn identifier_style_does_not_matter() {
        let e = HashingEmbedder::default();
        let a = e.embed("parseConfigFile").unwrap();
        let b = e.embed("parse_config_file").unwrap();
        assert!(cos(&a, &b) > 0.7, "{}", cos(&a, &b));
    }

    #[test]
    fn quantisation_preserves_similarity() {
        let e = HashingEmbedder::default();
        let a = e
            .embed("fn open_connection(host: &str) -> Connection")
            .unwrap();
        let b = e
            .embed("fn open_connection_pool(host: &str) -> Pool")
            .unwrap();
        let exact = cos(&a, &b);
        let (sa, qa) = quantize(&a);
        let (sb, qb) = quantize(&b);
        let approx = dot_i8(&qa, &qb) as f32 * sa * sb;
        assert!(
            (exact - approx).abs() < 0.02,
            "exact {exact:.4} vs quantised {approx:.4}"
        );
        assert_eq!(quantize(&[0.0; 8]), (0.0, vec![0i8; 8]));
    }

    #[test]
    fn hash_is_pinned() {
        // Persisted vectors depend on this exact function; changing it requires a new model id.
        assert_eq!(hash64(b"config", 0), 2986159632879634967);
        assert_ne!(hash64(b"config", 0), hash64(b"config", 1));
        assert_ne!(hash64(b"config", 0), hash64(b"Config", 0));
    }
}
