//! A pinned set of searchers over one snapshot: everything the query layer and tool surfaces need.

use loupe_core::{ApproxTokenCounter, Result};
use loupe_embed::SemanticSearcher;
use loupe_graph::Graph;
use loupe_query::{QueryContext, QueryOptions, QueryResult};
use loupe_symbols::SymbolSearcher;
use loupe_text::{Overlay, TextSearcher};

/// Searchers and the graph, all built from the same snapshot. Building one re-verifies segment
/// checksums and builds the graph, so make one per snapshot and reuse it.
pub struct Session {
    pub text: TextSearcher,
    pub symbols: SymbolSearcher,
    pub semantic: SemanticSearcher,
    pub graph: Graph,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("snapshot_id", &self.text.snapshot_id())
            .field("files", &self.text.file_count())
            .finish()
    }
}

impl Session {
    pub fn snapshot_id(&self) -> u64 {
        self.text.snapshot_id()
    }

    /// Hybrid retrieval (see `loupe-query`). The overlay, if given, is merged into every stage.
    pub fn query(
        &self,
        question: &str,
        opts: &QueryOptions,
        overlay: Option<&Overlay>,
    ) -> Result<QueryResult> {
        loupe_query::query(
            &QueryContext {
                text: &self.text,
                symbols: &self.symbols,
                semantic: &self.semantic,
                graph: Some(&self.graph),
                overlay,
                counter: &ApproxTokenCounter,
            },
            question,
            opts,
        )
    }
}
