//! Chunking, embedding, vector cache, and semantic search (milestone M7).

mod cache;
mod chunk;
mod embedder;
mod pipeline;
mod search;
mod store;

pub use cache::{ChunkHash, QuantVec, VectorCache};
pub use chunk::{CHUNKER_VERSION, Chunk, ChunkKind, ChunkOptions, chunk_file};
pub use embedder::{Embedder, HashingEmbedder, dot_i8, quantize};
pub use pipeline::{EmbedStats, embed_chunks};
pub use search::{SemanticHit, SemanticOptions, SemanticResult, SemanticSearcher, SemanticState};
pub use store::{
    BuiltEmbeddings, CHUNKS_FILE, ChunkMeta, EMBED_FILE, EmbedBuilder, EmbedIndex, QuantQuery,
};
