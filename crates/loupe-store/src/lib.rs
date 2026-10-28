//! Segments, manifest, snapshots, writer lock, GC.
//!
//! On-disk layout (see docs/v1.md section 4):
//!
//! ```text
//! <index_dir>/
//!   LOCK                    advisory writer lock
//!   manifest.json           current generation + live segment list (atomic replace)
//!   snapshots/<gen>.snap    path -> FileMeta table for that generation (checksummed)
//!   segments/<id>/          immutable; meta.bin lists blobs + per-file checksums
//! ```
//!
//! Every write is "write temp, fsync, rename, fsync dir". A crash at any point leaves either
//! the old manifest or the new one valid; orphaned segments and temp files are removed by
//! [`Store::gc`] the next time a writer opens the store.

mod codec;
mod fsio;
mod manifest;
mod segment;
mod snapshot_io;
mod store;
mod view;

pub use manifest::Manifest;
pub use segment::{META_FILE, Segment, SegmentWriter};
pub use store::{GcReport, Store};
pub use view::SnapshotView;
