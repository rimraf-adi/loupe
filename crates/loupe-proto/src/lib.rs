//! Wire protocol shared by the daemon, its clients, and (later) the MCP surface.
//!
//! Frames are `u32` little-endian length + MessagePack body. The payload types for results live
//! in `loupe-text` (library-first) and are re-exported here, so the library API, the socket
//! protocol, and the tool surface cannot drift apart.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use loupe_graph::{EdgeKind, Impacted, Related};
pub use loupe_map::{ModuleSummary, RepoMap};
pub use loupe_symbols::{Outline, SymbolHit, SymbolKind};
pub use loupe_text::{FileHit, FuzzyHit, SearchOptions, SearchResult, SearchStats, Snippet};

/// Frames larger than this are rejected before allocating.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Ping,
    Status,
    /// Regex/literal search, or whole-identifier search when `word` is set.
    Search {
        pattern: String,
        word: bool,
        opts: SearchOptions,
    },
    FindFiles {
        query: String,
        limit: usize,
    },
    FindTokens {
        query: String,
        limit: usize,
    },
    /// Definitions named exactly `name`, optionally of one kind.
    FindSymbol {
        name: String,
        kind: Option<SymbolKind>,
        limit: usize,
    },
    /// Signature outline of one file.
    Outline {
        path: String,
    },
    /// Files most connected to `path` (imports, tests, references, co-change).
    Related {
        path: String,
        kinds: Vec<EdgeKind>,
        hops: u8,
        limit: usize,
    },
    /// Files that depend on `path`, transitively.
    Impact {
        path: String,
        depth: u8,
        limit: usize,
    },
    /// The most important files by PageRank.
    TopRanked {
        limit: usize,
    },
    /// Token-budgeted map of the whole repo.
    RepoMap {
        budget: usize,
        focus: Vec<String>,
        name: String,
    },
    /// One to three lines about a directory.
    ModuleSummary {
        path: String,
    },
    /// Dirty-buffer API: `path` (repo-relative, `/`-separated) now has this content.
    Put {
        path: String,
        content: Vec<u8>,
    },
    /// `path` was deleted.
    Remove {
        path: String,
    },
    /// Re-scan the working tree now and wait for it.
    Update {
        force_rehash: bool,
    },
    /// Merge fragmented / mostly-dead segments and collect garbage.
    Compact,
    /// Per-request-type counts and latency percentiles since the daemon started.
    Metrics,
    Shutdown,
}

impl Request {
    /// A stable short name for metrics and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Request::Ping => "ping",
            Request::Status => "status",
            Request::Search { word: false, .. } => "search",
            Request::Search { word: true, .. } => "search_word",
            Request::FindFiles { .. } => "find_files",
            Request::FindTokens { .. } => "find_tokens",
            Request::FindSymbol { .. } => "find_symbol",
            Request::Outline { .. } => "outline",
            Request::Related { .. } => "related",
            Request::Impact { .. } => "impact",
            Request::TopRanked { .. } => "top_ranked",
            Request::RepoMap { .. } => "repo_map",
            Request::ModuleSummary { .. } => "module_summary",
            Request::Put { .. } => "put",
            Request::Remove { .. } => "remove",
            Request::Update { .. } => "update",
            Request::Compact => "compact",
            Request::Metrics => "metrics",
            Request::Shutdown => "shutdown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Pong,
    Status(StatusInfo),
    Search(SearchResult),
    Fuzzy(Vec<FuzzyHit>),
    Symbols(Vec<SymbolHit>),
    Map(RepoMap),
    Module(ModuleSummary),
    Related(Vec<Related>),
    Impact(Vec<Impacted>),
    /// `(path, rank)` best first; ranks sum to 1 over all files.
    Ranked(Vec<(String, f32)>),
    /// `None` if the file is unknown or removed.
    Outline(Option<Outline>),
    Updated(UpdateSummary),
    Compacted(CompactSummary),
    Metrics(MetricsInfo),
    Ok,
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub pid: u32,
    pub snapshot_id: u64,
    pub files: usize,
    pub segments: usize,
    pub overlay_files: usize,
    pub overlay_bytes: usize,
    /// Updates completed since the daemon started.
    pub updates: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSummary {
    pub snapshot_id: u64,
    pub files: usize,
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    /// Nothing changed, so nothing was written.
    pub unchanged: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactSummary {
    pub merged: usize,
    pub written: usize,
    pub blobs_dropped_dead: usize,
    /// Segment directories deleted by the garbage collection that followed.
    pub segments_removed: usize,
}

/// Latency of one kind of request. Percentiles are upper bounds of power-of-two microsecond
/// buckets (so they over-estimate by at most 2x); `max_us` is exact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestStats {
    pub kind: String,
    pub count: u64,
    pub errors: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsInfo {
    pub uptime_secs: u64,
    /// Sorted by kind.
    pub requests: Vec<RequestStats>,
    pub updates: u64,
    pub compactions: u64,
    pub segments: usize,
    /// Bytes the index directory occupies on disk.
    pub index_bytes: u64,
    pub overlay_files: usize,
    pub overlay_bytes: usize,
}

/// Reject paths that are not plain repo-relative: absolute, empty, or containing `..`/`.` parts.
pub fn validate_rel_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return Err(format!("not a repo-relative path: {path:?}"));
    }
    if path
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(format!("path has empty or dot components: {path:?}"));
    }
    Ok(())
}

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let body = rmp_serde::to_vec_named(value).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one frame. `Ok(None)` is a clean end of stream (peer closed between frames); a close in
/// the middle of a frame is an error.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read(&mut len[..1])? {
        0 => return Ok(None),
        _ => r.read_exact(&mut len[1..])?,
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    rmp_serde::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: T) {
        let mut buf = Vec::new();
        write_frame(&mut buf, &v).unwrap();
        let back: T = read_frame(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn requests_and_responses_roundtrip() {
        roundtrip(Request::Ping);
        roundtrip(Request::Search {
            pattern: "foo".into(),
            word: true,
            opts: SearchOptions::default(),
        });
        roundtrip(Request::Put {
            path: "a/b.rs".into(),
            content: vec![0, 1, 255],
        });
        roundtrip(Response::Error("nope".into()));
        roundtrip(Request::FindSymbol {
            name: "x".into(),
            kind: Some(SymbolKind::Method),
            limit: 5,
        });
        roundtrip(Response::Outline(None));
        roundtrip(Request::Compact);
        roundtrip(Response::Compacted(CompactSummary {
            merged: 3,
            written: 1,
            blobs_dropped_dead: 9,
            segments_removed: 3,
        }));
        roundtrip(Response::Metrics(MetricsInfo {
            uptime_secs: 5,
            requests: vec![RequestStats {
                kind: "search".into(),
                count: 2,
                errors: 0,
                p50_us: 64,
                p95_us: 128,
                p99_us: 128,
                max_us: 100,
            }],
            updates: 1,
            compactions: 0,
            segments: 2,
            index_bytes: 4096,
            overlay_files: 0,
            overlay_bytes: 0,
        }));
        roundtrip(Response::Fuzzy(vec![FuzzyHit {
            text: "x".into(),
            score: 3,
        }]));
        roundtrip(Response::Status(StatusInfo {
            pid: 1,
            snapshot_id: 2,
            files: 3,
            segments: 4,
            overlay_files: 5,
            overlay_bytes: 6,
            updates: 7,
        }));
    }

    #[test]
    fn options_with_timeout_survive() {
        let opts = SearchOptions {
            timeout: Some(std::time::Duration::from_millis(1500)),
            ..SearchOptions::default()
        };
        roundtrip(Request::Search {
            pattern: "x".into(),
            word: false,
            opts,
        });
    }

    #[test]
    fn several_frames_in_a_row_and_clean_eof() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Request::Ping).unwrap();
        write_frame(&mut buf, &Request::Status).unwrap();
        let mut c = Cursor::new(buf);
        assert_eq!(
            read_frame::<_, Request>(&mut c).unwrap(),
            Some(Request::Ping)
        );
        assert_eq!(
            read_frame::<_, Request>(&mut c).unwrap(),
            Some(Request::Status)
        );
        assert_eq!(read_frame::<_, Request>(&mut c).unwrap(), None);
    }

    #[test]
    fn truncated_oversized_and_garbage_frames_are_errors() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Request::Status).unwrap();
        for n in 1..buf.len() {
            assert!(
                read_frame::<_, Request>(&mut Cursor::new(&buf[..n])).is_err(),
                "cut at {n}"
            );
        }
        let huge = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes();
        assert!(read_frame::<_, Request>(&mut Cursor::new(huge.to_vec())).is_err());
        let mut garbage = 4u32.to_le_bytes().to_vec();
        garbage.extend_from_slice(&[0xc1, 0xc1, 0xc1, 0xc1]); // 0xc1 is never valid msgpack
        assert!(read_frame::<_, Request>(&mut Cursor::new(garbage)).is_err());
    }

    #[test]
    fn path_validation() {
        for ok in ["a", "a/b.rs", ".github/x.yml"] {
            assert!(validate_rel_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "a\\b",
            "a/",
        ] {
            assert!(validate_rel_path(bad).is_err(), "{bad}");
        }
    }
}
