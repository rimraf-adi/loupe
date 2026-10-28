//! Turning new blobs into a text-index segment.

use std::fs;
use std::path::Path;

use loupe_core::{BlobId, Result};
use loupe_store::SegmentWriter;
use rayon::prelude::*;

use crate::index::TextIndexBuilder;
use crate::terms::{FileTerms, extract_terms};

/// Files are read and tokenized this many at a time, bounding memory to one chunk of contents.
const CHUNK: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildStats {
    pub indexed: usize,
    /// The file's content no longer matched the blob it was scanned as (edited mid-run).
    pub skipped_changed: usize,
    pub skipped_unreadable: usize,
}

enum Extracted {
    Terms(FileTerms),
    Changed,
    Unreadable,
}

/// Index each `(blob, path)` into `writer`, which gains the blobs it indexed and the text index
/// files. `path` is relative to `root`; a blob reachable at several paths is listed once.
///
/// A file is re-hashed after reading. If it no longer matches its blob (edited since the scan)
/// it is skipped rather than indexed under the wrong identity; the next scan sees the new
/// content and indexes it then.
pub fn write_text_index(
    writer: &mut SegmentWriter,
    root: &Path,
    items: &[(BlobId, String)],
) -> Result<BuildStats> {
    let mut stats = BuildStats::default();
    let mut builder = TextIndexBuilder::new();

    for chunk in items.chunks(CHUNK) {
        let extracted: Vec<Extracted> = chunk
            .par_iter()
            .map(|(blob, path)| match fs::read(root.join(path)) {
                Err(_) => Extracted::Unreadable,
                Ok(bytes) if BlobId::of(&bytes) != *blob => Extracted::Changed,
                Ok(bytes) => Extracted::Terms(extract_terms(&bytes)),
            })
            .collect();

        for ((blob, _), result) in chunk.iter().zip(extracted) {
            match result {
                Extracted::Terms(terms) => {
                    let local = writer.add_blob(*blob);
                    builder.add(local, terms)?;
                    stats.indexed += 1;
                }
                Extracted::Changed => stats.skipped_changed += 1,
                Extracted::Unreadable => stats.skipped_unreadable += 1,
            }
        }
    }

    builder.finish()?.write_to(writer)?;
    Ok(stats)
}
