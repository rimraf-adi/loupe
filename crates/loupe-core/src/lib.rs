//! Shared foundations: identifiers, errors, configuration, token budgeting.

pub mod budget;
pub mod config;
pub mod error;
pub mod ids;
pub mod snapshot;

pub use budget::{ApproxTokenCounter, TokenCounter};
pub use config::Config;
pub use error::{Error, Result};
pub use ids::{BlobId, FileId, LocalBlobId, SegmentId, SymbolId};
pub use snapshot::{FileMeta, Snapshot};
