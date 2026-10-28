//! Symbol extraction, the per-segment symbol index, and outlines (milestone M4).

mod extract;
mod index;
mod outline;
mod registry;
mod search;
mod symbol;

pub use extract::extract_symbols;
pub use index::{
    BuiltSymbols, NAMES_FILE, SYMBOLS_FILE, SymbolIndex, SymbolIndexBuilder, SymbolRef,
};
pub use outline::{OutlineOptions, render_outline};
pub use registry::{LanguageSpec, Registry, VisibilityRule};
pub use search::{Outline, OutlineSource, SymbolHit, SymbolSearcher};
pub use symbol::{Symbol, SymbolKind, Visibility};
