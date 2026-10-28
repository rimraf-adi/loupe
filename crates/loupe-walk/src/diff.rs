use loupe_core::Snapshot;

/// Paths that changed between two snapshots, each list sorted.
///
/// A rename shows up as one removal plus one addition; because segments are content-addressed,
/// the renamed file's blob is already indexed and costs nothing to re-add.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }
}

/// Compare by blob id: a file whose mtime changed but whose content did not is not modified.
/// `prev = None` means everything is added.
pub fn diff(prev: Option<&Snapshot>, next: &Snapshot) -> Diff {
    let mut out = Diff::default();
    let Some(prev) = prev else {
        out.added = next.files.keys().cloned().collect();
        return out;
    };
    for (path, meta) in &next.files {
        match prev.files.get(path) {
            None => out.added.push(path.clone()),
            Some(old) if old.blob != meta.blob => out.modified.push(path.clone()),
            Some(_) => {}
        }
    }
    for path in prev.files.keys() {
        if !next.files.contains_key(path) {
            out.removed.push(path.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use loupe_core::{BlobId, FileMeta};

    fn snap(entries: &[(&str, &str)]) -> Snapshot {
        let mut s = Snapshot::default();
        for (path, content) in entries {
            s.files.insert(
                (*path).to_string(),
                FileMeta {
                    size: content.len() as u64,
                    mtime_ns: 0,
                    blob: BlobId::of(content.as_bytes()),
                    lang: None,
                },
            );
        }
        s
    }

    #[test]
    fn none_prev_is_all_added() {
        let d = diff(None, &snap(&[("a", "1"), ("b", "2")]));
        assert_eq!(d.added, ["a", "b"]);
        assert!(d.modified.is_empty() && d.removed.is_empty());
    }

    #[test]
    fn classifies_changes() {
        let prev = snap(&[("keep", "k"), ("edit", "old"), ("gone", "g")]);
        let next = snap(&[("keep", "k"), ("edit", "new"), ("fresh", "f")]);
        let d = diff(Some(&prev), &next);
        assert_eq!(d.added, ["fresh"]);
        assert_eq!(d.modified, ["edit"]);
        assert_eq!(d.removed, ["gone"]);
    }

    #[test]
    fn identical_is_empty() {
        let s = snap(&[("a", "1")]);
        assert!(diff(Some(&s), &s).is_empty());
    }
}
