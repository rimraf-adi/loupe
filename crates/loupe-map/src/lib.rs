//! Repo map and module summaries (milestone M6).

mod cache;
mod mapper;
mod merkle;
mod summarize;

pub use cache::SummaryCache;
pub use mapper::{
    DiskFiles, FileSource, MapContext, MapOptions, ModuleSummary, RepoMap, RepoMapper,
    SummarizeStats, SymbolSource, TopSymbol,
};
pub use merkle::{DirNode, Hash, MerkleTree};
pub use summarize::{
    CallbackSummarizer, FileInfo, HeuristicSummarizer, ModuleInput, Summarizer, extract_purpose,
};
