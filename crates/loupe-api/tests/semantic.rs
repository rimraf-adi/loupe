//! Semantic search and embedding reuse over a real index.

use std::fs;
use std::sync::Arc;

use loupe_api::{ChunkOptions, HashingEmbedder, Index, SemanticOptions, SemanticState};
use loupe_core::Config;

struct Project {
    repo: tempfile::TempDir,
    idx: tempfile::TempDir,
}

impl Project {
    fn new() -> Project {
        let p = Project {
            repo: tempfile::tempdir().unwrap(),
            idx: tempfile::tempdir().unwrap(),
        };
        p.write(
            "net/client.rs",
            "/// Open a network connection to a remote host.\npub fn open_connection(host: &str) -> Conn {\n    Conn::dial(host)\n}\n\npub fn close_connection(c: Conn) {\n    c.shutdown()\n}\n",
        );
        p.write(
            "config/parser.rs",
            "/// Parse the configuration file from disk.\npub fn parse_config_file(path: &str) -> Config {\n    read_file(path)\n}\n",
        );
        p.write(
            "render/mesh.rs",
            "pub fn render_triangle_mesh(vertices: &[Vertex]) {\n    draw(vertices)\n}\n",
        );
        p.write(
            "notes.md",
            "# Notes\n\nRemember to rotate the signing keys every quarter.\n",
        );
        p
    }
    fn write(&self, rel: &str, content: &str) {
        let path = self.repo.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn config(&self) -> Config {
        let mut c = Config::new(self.repo.path().canonicalize().unwrap());
        c.index_dir = self.idx.path().to_path_buf();
        c
    }
    fn index(&self) -> Index {
        let mut idx = Index::open_writer(self.config()).unwrap();
        idx.update(false).unwrap();
        idx
    }
}

fn opts(k: usize) -> SemanticOptions {
    SemanticOptions {
        k,
        ..SemanticOptions::default()
    }
}

#[test]
fn natural_language_queries_find_the_right_definitions() {
    let p = Project::new();
    let idx = p.index();
    let r = idx
        .search_semantic("how do I open a connection to a remote host", &opts(3))
        .unwrap();
    assert_eq!(r.state, SemanticState::Full);
    assert!(r.snapshot_id > 0 && r.chunks_scanned > 0);
    assert_eq!(r.hits[0].path, "net/client.rs");
    assert_eq!(r.hits[0].breadcrumb, "open_connection");
    assert!(
        r.hits[0].snippet.contains("open_connection"),
        "{}",
        r.hits[0].snippet
    );
    assert_eq!((r.hits[0].start_line, r.hits[0].end_line), (2, 4));

    let r = idx
        .search_semantic("read configuration from a file", &opts(3))
        .unwrap();
    assert_eq!(
        r.hits[0].path,
        "config/parser.rs",
        "{:?}",
        r.hits
            .iter()
            .map(|h| (&h.path, h.score))
            .collect::<Vec<_>>()
    );

    let r = idx
        .search_semantic("signing keys rotation", &opts(3))
        .unwrap();
    assert_eq!(
        r.hits[0].path, "notes.md",
        "prose files use windowed chunks"
    );
    assert_eq!(r.hits[0].breadcrumb, "L1-3");
    assert!(r.hits.windows(2).all(|w| w[0].score >= w[1].score));
}

#[test]
fn scope_and_per_file_limits_apply() {
    let p = Project::new();
    let idx = p.index();
    let all = idx.search_semantic("connection", &opts(10)).unwrap();
    let in_net = idx
        .search_semantic(
            "connection",
            &SemanticOptions {
                scope: Some("net/".into()),
                ..opts(10)
            },
        )
        .unwrap();
    assert!(in_net.hits.iter().all(|h| h.path.starts_with("net/")));
    assert!(all.hits.len() >= in_net.hits.len());
    let one = idx
        .search_semantic(
            "connection",
            &SemanticOptions {
                per_file: 1,
                ..opts(10)
            },
        )
        .unwrap();
    let mut paths: Vec<&str> = one.hits.iter().map(|h| h.path.as_str()).collect();
    let n = paths.len();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), n, "one chunk per file");
    assert!(
        idx.search_semantic("anything", &opts(0))
            .unwrap()
            .hits
            .is_empty()
    );
}

#[test]
fn unchanged_chunks_are_never_re_embedded() {
    let p = Project::new();
    let mut idx = Index::open_writer(p.config()).unwrap();
    let first = idx.update(false).unwrap();
    assert!(first.chunks_embedded > 0);
    assert_eq!(first.chunks_reused, 0);

    // Edit one function body in one file: only that chunk changes.
    p.write(
        "net/client.rs",
        "/// Open a network connection to a remote host.\npub fn open_connection(host: &str) -> Conn {\n    Conn::dial_with_timeout(host, 30)\n}\n\npub fn close_connection(c: Conn) {\n    c.shutdown()\n}\n",
    );
    let second = idx.update(false).unwrap();
    assert_eq!(second.chunks_embedded, 1, "only the edited function");
    assert_eq!(
        second.chunks_reused, 1,
        "close_connection came from the cache"
    );

    // Renaming a file changes no chunk text, so nothing is embedded.
    fs::rename(
        p.repo.path().join("render/mesh.rs"),
        p.repo.path().join("render/triangles.rs"),
    )
    .unwrap();
    p.write(
        "render/triangles.rs",
        "pub fn render_triangle_mesh(vertices: &[Vertex]) {\n    draw(vertices)\n}\n",
    );
    let third = idx.update(false).unwrap();
    assert_eq!(third.chunks_embedded, 0, "same content under a new name");

    // A moved function in a brand-new file reuses its vector too.
    p.write(
        "net/more.rs",
        "pub fn close_connection(c: Conn) {\n    c.shutdown()\n}\n",
    );
    let fourth = idx.update(false).unwrap();
    assert_eq!(
        fourth.chunks_embedded, 0,
        "identical function text found in the cache"
    );
    assert_eq!(fourth.chunks_reused, 1);
}

#[test]
fn a_new_model_rebuilds_segments_and_never_mixes_vectors() {
    let p = Project::new();
    drop(p.index());

    // A reader using a different model sees no usable embeddings rather than wrong ones.
    let other = Arc::new(HashingEmbedder::new(64));
    let reader = Index::open_reader(p.config())
        .unwrap()
        .with_embedding(other.clone(), ChunkOptions::default());
    let r = reader.search_semantic("open connection", &opts(5)).unwrap();
    assert_eq!(r.state, SemanticState::Off);
    assert!(r.hits.is_empty());
    drop(reader);

    // The writer with the new model drops and rebuilds the old segment.
    let mut idx = Index::open_writer(p.config())
        .unwrap()
        .with_embedding(other, ChunkOptions::default());
    let rep = idx.update(false).unwrap();
    assert_eq!(rep.dropped_segments, 1);
    assert!(rep.chunks_embedded > 0);
    let r = idx
        .search_semantic("open a network connection", &opts(3))
        .unwrap();
    assert_eq!(r.state, SemanticState::Full);
    assert_eq!(r.hits[0].path, "net/client.rs");
    assert!(idx.update(false).unwrap().unchanged, "stable once rebuilt");
}

#[test]
fn the_overlay_is_searched_and_hides_stale_snapshot_content() {
    let p = Project::new();
    let mut idx = p.index();
    idx.put(
        "scratch/new.rs",
        b"/// Compress a buffer with zstd.\npub fn compress_buffer(data: &[u8]) -> Vec<u8> {\n    zstd::encode(data)\n}\n".to_vec(),
    )
    .unwrap();
    let r = idx
        .search_semantic("compress data with zstd", &opts(3))
        .unwrap();
    assert_eq!(r.hits[0].path, "scratch/new.rs");
    assert_eq!(r.hits[0].breadcrumb, "compress_buffer");

    // An unsaved edit that removes the function hides the on-disk version.
    idx.put("net/client.rs", b"pub fn unrelated() {}\n".to_vec())
        .unwrap();
    let r = idx
        .search_semantic("open a network connection to a remote host", &opts(5))
        .unwrap();
    assert!(
        r.hits.iter().all(|h| h.breadcrumb != "open_connection"),
        "{:?}",
        r.hits.iter().map(|h| &h.breadcrumb).collect::<Vec<_>>()
    );
    idx.remove("config/parser.rs");
    let r = idx
        .search_semantic("parse the configuration file", &opts(5))
        .unwrap();
    assert!(r.hits.iter().all(|h| h.path != "config/parser.rs"));
}

#[test]
fn empty_index_and_empty_query_are_fine() {
    let p = Project::new();
    {
        // Nothing indexed yet: no segments, so the answer is trivially complete and empty.
        let idx = Index::open_writer(p.config()).unwrap();
        let r = idx.search_semantic("anything", &opts(5)).unwrap();
        assert_eq!((r.state, r.hits.len()), (SemanticState::Full, 0));
    }
    let idx = p.index();
    let r = idx.search_semantic("", &opts(5)).unwrap();
    assert!(
        r.hits.iter().all(|h| h.score.abs() < 1e-6),
        "a zero query scores nothing: {:?}",
        r.hits
    );
}
