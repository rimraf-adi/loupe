//! Optional daemon: one warm index shared across processes, kept fresh by a file watcher.
//!
//! The daemon is a thin wrapper over `loupe-api`. It owns the index's writer lock, serves
//! length-prefixed MessagePack requests on a Unix socket, and keeps results fresh in two tiers:
//!
//! 1. *Fast path:* the watcher reads a changed file straight into the in-memory overlay, so the
//!    edit is searchable within the debounce window.
//! 2. *Background path:* a worker re-scans the working tree and swaps in a new snapshot, then
//!    discards the overlay entries that scan observed. It also runs on a timer
//!    (reconciliation, for missed watcher events) and force-rehashes periodically.
//!
//! No async runtime: connections are served by plain threads, which is plenty for a handful of
//! local clients and keeps the core synchronous.

mod client;
mod metrics;
mod server;

pub use client::Client;
pub use server::{DaemonHandle, DaemonOptions, Stopper, start};

use std::path::{Path, PathBuf};

/// Socket path for an index directory.
///
/// Unix socket paths are limited to ~100 bytes, so the path is a short name in the temp dir
/// derived from a hash of the index directory rather than a file inside it.
pub fn socket_path(index_dir: &Path) -> PathBuf {
    let canonical = index_dir
        .canonicalize()
        .unwrap_or_else(|_| index_dir.to_path_buf());
    let hash = blake3::hash(canonical.as_os_str().as_encoded_bytes());
    let name: String = hash.as_bytes()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::env::temp_dir().join(format!("loupe-{name}.sock"))
}
