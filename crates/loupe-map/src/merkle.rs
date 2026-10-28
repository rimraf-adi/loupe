//! Merkle tree over a snapshot: a file's hash is its blob id; a directory's hash covers its
//! children's names, kinds, and hashes. Equal directory hash means equal subtree, so anything
//! derived from a directory (a summary) can be cached by that hash and recomputed only along the
//! path from a changed file up to the root.

use std::collections::{BTreeMap, BTreeSet};

use loupe_core::Snapshot;

pub type Hash = [u8; 32];

#[derive(Clone, Debug)]
pub struct DirNode {
    pub hash: Hash,
    /// Immediate subdirectory names, sorted.
    pub dirs: Vec<String>,
    /// Immediate file names, sorted.
    pub files: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct MerkleTree {
    /// Directory path ("" for the root) -> node.
    dirs: BTreeMap<String, DirNode>,
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn parent_of(dir: &str) -> &str {
    dir.rsplit_once('/').map_or("", |(p, _)| p)
}

impl MerkleTree {
    pub fn build(snapshot: &Snapshot) -> MerkleTree {
        // Collect every directory (including intermediate ones), with its files and subdirs.
        let mut files: BTreeMap<String, Vec<(String, Hash)>> = BTreeMap::new();
        let mut subdirs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        files.entry(String::new()).or_default();
        for (path, meta) in &snapshot.files {
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            files
                .entry(dir.to_string())
                .or_default()
                .push((name.to_string(), *meta.blob.as_bytes()));
            // Register the whole ancestor chain.
            let mut d = dir;
            while !d.is_empty() {
                let p = parent_of(d);
                subdirs
                    .entry(p.to_string())
                    .or_default()
                    .insert(d.rsplit('/').next().unwrap().to_string());
                files.entry(d.to_string()).or_default();
                d = p;
            }
        }

        // Deepest directories first, so children are hashed before their parents.
        let mut order: Vec<&String> = files.keys().collect();
        order.sort_by_key(|d| {
            std::cmp::Reverse(d.matches('/').count() + usize::from(!d.is_empty()))
        });

        let mut dirs: BTreeMap<String, DirNode> = BTreeMap::new();
        for dir in order {
            let file_list = &files[dir];
            let mut file_names: Vec<&(String, Hash)> = file_list.iter().collect();
            file_names.sort_by(|a, b| a.0.cmp(&b.0));
            let sub_names: Vec<String> = subdirs
                .get(dir)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();

            let mut h = blake3::Hasher::new();
            h.update(b"dir\0");
            for (name, hash) in &file_names {
                h.update(b"f");
                h.update(&(name.len() as u32).to_le_bytes());
                h.update(name.as_bytes());
                h.update(hash);
            }
            for name in &sub_names {
                h.update(b"d");
                h.update(&(name.len() as u32).to_le_bytes());
                h.update(name.as_bytes());
                h.update(&dirs[&join(dir, name)].hash);
            }
            dirs.insert(
                dir.clone(),
                DirNode {
                    hash: *h.finalize().as_bytes(),
                    dirs: sub_names,
                    files: file_names.into_iter().map(|(n, _)| n.clone()).collect(),
                },
            );
        }
        MerkleTree { dirs }
    }

    pub fn root_hash(&self) -> Hash {
        self.dirs[""].hash
    }

    pub fn node(&self, dir: &str) -> Option<&DirNode> {
        self.dirs.get(dir)
    }

    pub fn dir_hash(&self, dir: &str) -> Option<Hash> {
        self.node(dir).map(|n| n.hash)
    }

    /// Every directory path, root first then sorted.
    pub fn dir_paths(&self) -> impl Iterator<Item = &str> {
        self.dirs.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.dirs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    /// Directories whose hash differs from (or is absent in) `older`, i.e. every directory on the
    /// path from each changed or added file up to the root.
    pub fn changed_dirs<'a>(&'a self, older: &MerkleTree) -> Vec<&'a str> {
        self.dirs
            .iter()
            .filter(|(path, node)| older.dir_hash(path) != Some(node.hash))
            .map(|(path, _)| path.as_str())
            .collect()
    }

    /// Subdirectory paths and file paths directly under `dir`.
    pub fn children_paths(&self, dir: &str) -> (Vec<String>, Vec<String>) {
        let Some(n) = self.node(dir) else {
            return (Vec::new(), Vec::new());
        };
        (
            n.dirs.iter().map(|d| join(dir, d)).collect(),
            n.files.iter().map(|f| join(dir, f)).collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loupe_core::{BlobId, FileMeta};

    fn snap(files: &[(&str, &str)]) -> Snapshot {
        let mut s = Snapshot::default();
        for (p, c) in files {
            s.files.insert(
                (*p).to_string(),
                FileMeta {
                    size: c.len() as u64,
                    mtime_ns: 0,
                    blob: BlobId::of(c.as_bytes()),
                    lang: None,
                },
            );
        }
        s
    }

    const BASE: &[(&str, &str)] = &[
        ("README.md", "r"),
        ("src/main.rs", "m"),
        ("src/net/tcp.rs", "t"),
        ("src/net/udp.rs", "u"),
        ("docs/guide.md", "g"),
    ];

    #[test]
    fn structure_includes_intermediate_directories() {
        let t = MerkleTree::build(&snap(BASE));
        let dirs: Vec<&str> = t.dir_paths().collect();
        assert_eq!(dirs, ["", "docs", "src", "src/net"]);
        assert_eq!(t.node("").unwrap().dirs, ["docs", "src"]);
        assert_eq!(t.node("").unwrap().files, ["README.md"]);
        assert_eq!(t.node("src").unwrap().dirs, ["net"]);
        assert_eq!(
            t.children_paths("src/net").1,
            ["src/net/tcp.rs", "src/net/udp.rs"]
        );
        assert!(t.node("nope").is_none());
    }

    #[test]
    fn hashes_are_deterministic_and_independent_of_insertion_order() {
        let a = MerkleTree::build(&snap(BASE));
        let mut reversed: Vec<_> = BASE.to_vec();
        reversed.reverse();
        let b = MerkleTree::build(&snap(&reversed));
        assert_eq!(a.root_hash(), b.root_hash());
    }

    #[test]
    fn editing_one_file_changes_exactly_its_ancestors() {
        let a = MerkleTree::build(&snap(BASE));
        let mut edited = BASE.to_vec();
        edited[2] = ("src/net/tcp.rs", "CHANGED");
        let b = MerkleTree::build(&snap(&edited));
        assert_eq!(b.changed_dirs(&a), ["", "src", "src/net"]);
        assert_eq!(a.dir_hash("docs"), b.dir_hash("docs"), "siblings untouched");
        assert_ne!(a.root_hash(), b.root_hash());
    }

    #[test]
    fn adding_removing_and_renaming_are_detected() {
        let a = MerkleTree::build(&snap(BASE));
        let mut added = BASE.to_vec();
        added.push(("docs/new.md", "n"));
        assert_eq!(
            MerkleTree::build(&snap(&added)).changed_dirs(&a),
            ["", "docs"]
        );

        let removed: Vec<_> = BASE
            .iter()
            .copied()
            .filter(|(p, _)| *p != "src/main.rs")
            .collect();
        assert_eq!(
            MerkleTree::build(&snap(&removed)).changed_dirs(&a),
            ["", "src"]
        );

        // Same content under a new name still changes the directory (names are hashed).
        let mut renamed = BASE.to_vec();
        renamed[4] = ("docs/handbook.md", "g");
        assert_eq!(
            MerkleTree::build(&snap(&renamed)).changed_dirs(&a),
            ["", "docs"]
        );

        // A new directory is "changed" even though no old hash exists for it.
        let mut newdir = BASE.to_vec();
        newdir.push(("extra/a.txt", "a"));
        assert_eq!(
            MerkleTree::build(&snap(&newdir)).changed_dirs(&a),
            ["", "extra"]
        );
    }

    #[test]
    fn identical_subtrees_hash_identically_regardless_of_location() {
        let t = MerkleTree::build(&snap(&[("a/x.rs", "same"), ("b/x.rs", "same")]));
        assert_eq!(t.dir_hash("a"), t.dir_hash("b"));
        let u = MerkleTree::build(&snap(&[("a/x.rs", "same"), ("b/y.rs", "same")]));
        assert_ne!(u.dir_hash("a"), u.dir_hash("b"), "names matter");
    }

    #[test]
    fn file_and_directory_with_similar_names_do_not_collide() {
        // A directory "ab" containing file "c" vs. a file "ab/c"-like layout differences must
        // not produce the same root hash.
        let one = MerkleTree::build(&snap(&[("ab/c", "x")]));
        let two = MerkleTree::build(&snap(&[("abc", "x")]));
        assert_ne!(one.root_hash(), two.root_hash());
    }

    #[test]
    fn empty_snapshot_has_a_root() {
        let t = MerkleTree::build(&Snapshot::default());
        assert_eq!(t.len(), 1);
        assert!(t.node("").unwrap().files.is_empty());
    }
}
