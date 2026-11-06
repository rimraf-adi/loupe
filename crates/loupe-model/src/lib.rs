//! Local embedding models: a small registry, a checksummed downloader, and (behind the
//! `real-embedder` feature) a candle BERT embedder implementing [`loupe_embed::Embedder`].
//!
//! Weights are never fetched implicitly: `loupe model fetch <id>` is the only code path that
//! touches the network, and it verifies every file against a pinned SHA-256.

mod fetch;
mod registry;

#[cfg(feature = "real-embedder")]
mod bert;

pub use fetch::{ModelStatus, fetch, models_dir, status, verify};
pub use registry::{ModelFile, ModelSpec, REGISTRY, lookup};

#[cfg(feature = "real-embedder")]
pub use bert::BertEmbedder;
