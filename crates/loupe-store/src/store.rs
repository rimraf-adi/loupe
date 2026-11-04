use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use loupe_core::{Error, Result, SegmentId, Snapshot};

use crate::fsio::{atomic_write, crash_point, io, tmp_path};
use crate::manifest::{MANIFEST_FILE, Manifest};
use crate::segment::{Segment, SegmentWriter, segment_dir_name};
use crate::snapshot_io;

const LOCK_FILE: &str = "LOCK";
const SEGMENTS_DIR: &str = "segments";
const SNAPSHOTS_DIR: &str = "snapshots";
/// One marker file per retired segment, holding the unix time it left the live set.
const RETIRED_DIR: &str = "retired";
/// How long a retired segment's files are kept so readers that opened the previous manifest can
/// finish with it.
pub const DEFAULT_RETIRE_GRACE: Duration = Duration::from_secs(30);

/// What [`Store::gc`] removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub tmp_removed: usize,
    /// Unreferenced segments deleted: crash orphans immediately, retired ones after the grace.
    pub segments_removed: usize,
    /// Retired segments still inside their grace period.
    pub segments_retained: usize,
    pub snapshots_removed: usize,
}

/// An opened index directory.
///
/// At most one writer exists per directory, enforced by an advisory lock held for the
/// writer's lifetime; any number of readers may open the same directory.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    manifest: Manifest,
    next_segment_id: u64,
    /// Held for its lock; `None` for readers.
    lock: Option<File>,
    retire_grace: Duration,
}

impl Store {
    /// Open for writing, creating the directory if needed. Fails with [`Error::Locked`] if
    /// another writer holds the lock. Runs [`Store::gc`] before returning.
    ///
    /// Readers pin nothing yet, so GC here may delete a segment a concurrent reader is still
    /// using; reader pinning with a grace period arrives with compaction (M10). Until then
    /// the single-process case is safe, and a reader that loses a segment gets an error
    /// rather than wrong data.
    pub fn open_writer(dir: impl AsRef<Path>) -> Result<Store> {
        let dir = dir.as_ref().to_path_buf();
        io(&dir, fs::create_dir_all(dir.join(SEGMENTS_DIR)))?;
        io(&dir, fs::create_dir_all(dir.join(SNAPSHOTS_DIR)))?;
        mark_as_cache(&dir);

        let lock_path = dir.join(LOCK_FILE);
        let lock = io(
            &lock_path,
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path),
        )?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Error::Locked(dir)),
            Err(TryLockError::Error(e)) => return Err(Error::io(lock_path, e)),
        }

        let manifest = Manifest::load(&dir)?;
        let store = Store {
            next_segment_id: manifest.next_segment_id,
            dir,
            manifest,
            lock: Some(lock),
            retire_grace: DEFAULT_RETIRE_GRACE,
        };
        let report = store.gc()?;
        if report != GcReport::default() {
            tracing::info!(?report, "gc on open");
        }
        Ok(store)
    }

    /// Open read-only. The directory must exist. Never takes the lock and never deletes.
    pub fn open_reader(dir: impl AsRef<Path>) -> Result<Store> {
        let dir = dir.as_ref().to_path_buf();
        if !dir.is_dir() {
            return Err(Error::io(
                &dir,
                std::io::Error::from(std::io::ErrorKind::NotFound),
            ));
        }
        let manifest = Manifest::load(&dir)?;
        Ok(Store {
            next_segment_id: manifest.next_segment_id,
            dir,
            manifest,
            lock: None,
            retire_grace: DEFAULT_RETIRE_GRACE,
        })
    }

    /// How long retired segments keep their files (default 30 s). Zero deletes them at the next
    /// [`Store::gc`]; use it in tests, or when no reader can be mid-query.
    pub fn set_retire_grace(&mut self, grace: Duration) {
        self.retire_grace = grace;
    }

    pub fn is_writer(&self) -> bool {
        self.lock.is_some()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Snapshot id of the current generation (0 = empty index).
    pub fn snapshot_id(&self) -> u64 {
        self.manifest.generation
    }

    pub fn segment_ids(&self) -> Vec<SegmentId> {
        self.manifest.segment_ids()
    }

    /// Re-read the manifest to follow commits made by a writer. Readers only.
    pub fn refresh(&mut self) -> Result<()> {
        self.manifest = Manifest::load(&self.dir)?;
        self.next_segment_id = self.manifest.next_segment_id;
        Ok(())
    }

    pub fn load_snapshot(&self) -> Result<Snapshot> {
        if self.manifest.generation == 0 {
            return Ok(Snapshot::default());
        }
        let path = self.snapshot_path(self.manifest.generation);
        let bytes = io(&path, fs::read(&path))?;
        snapshot_io::decode(&bytes)
    }

    pub fn open_segment(&self, id: SegmentId) -> Result<Segment> {
        Segment::open(&self.segments_dir(), id)
    }

    /// Start a new segment with a fresh id. The segment is invisible until a [`Store::commit`]
    /// lists it.
    pub fn new_segment(&mut self) -> Result<SegmentWriter> {
        self.require_writer()?;
        let id = SegmentId(self.next_segment_id);
        self.next_segment_id += 1;
        SegmentWriter::create(&self.segments_dir(), id)
    }

    /// Atomically make `snapshot` and exactly `segments` the live state; returns the new
    /// snapshot id.
    ///
    /// Order matters for crash safety: the snapshot file is durable before the manifest
    /// that points at it is replaced, and segments were made durable by `finish`. Crashing
    /// anywhere leaves the previous manifest intact.
    pub fn commit(&mut self, snapshot: &Snapshot, segments: &[SegmentId]) -> Result<u64> {
        self.require_writer()?;
        let mut seen = std::collections::HashSet::new();
        for &id in segments {
            if !seen.insert(id) {
                return Err(Error::InvalidArgument(format!("duplicate segment {id:?}")));
            }
            if !self.segments_dir().join(segment_dir_name(id)).is_dir() {
                return Err(Error::InvalidArgument(format!(
                    "segment {id:?} does not exist"
                )));
            }
        }

        let generation = self.manifest.generation + 1;
        atomic_write(
            &self.snapshot_path(generation),
            &snapshot_io::encode(snapshot),
        )?;
        crash_point("commit.after_snapshot");
        // Segments leaving the live set get a marker first, so a crash between here and the
        // manifest swap only delays their deletion.
        self.mark_retired(segments)?;
        let next = Manifest {
            format_version: self.manifest.format_version,
            generation,
            next_segment_id: self.next_segment_id,
            segments: segments.iter().map(|s| s.0).collect(),
        };
        next.save(&self.dir)?;
        crash_point("commit.after_manifest");
        self.manifest = next;
        Ok(generation)
    }

    fn mark_retired(&self, keep: &[SegmentId]) -> Result<()> {
        let dropped: Vec<SegmentId> = self
            .manifest
            .segment_ids()
            .into_iter()
            .filter(|id| !keep.contains(id))
            .collect();
        if dropped.is_empty() {
            return Ok(());
        }
        let dir = self.dir.join(RETIRED_DIR);
        io(&dir, fs::create_dir_all(&dir))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        for id in dropped {
            let marker = dir.join(segment_dir_name(id));
            crate::fsio::write_synced(&marker, now.to_string().as_bytes())?;
        }
        Ok(())
    }

    /// Open every live segment and return the ones that fail verification.
    pub fn verify(&self) -> Vec<(SegmentId, Error)> {
        self.manifest
            .segment_ids()
            .into_iter()
            .filter_map(|id| self.open_segment(id).err().map(|e| (id, e)))
            .collect()
    }

    /// Remove temp files, segments the manifest does not list, and stale snapshots.
    /// Writers only.
    pub fn gc(&self) -> Result<GcReport> {
        self.require_writer()?;
        let mut report = GcReport::default();

        let live: std::collections::HashSet<String> = self
            .manifest
            .segment_ids()
            .into_iter()
            .map(segment_dir_name)
            .collect();
        for entry in read_dir(&self.segments_dir())? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if name.starts_with(".tmp-") {
                io(&path, fs::remove_dir_all(&path))?;
                report.tmp_removed += 1;
            } else if is_segment_dir_name(&name) && !live.contains(&name) {
                // Retired segments (once live) wait out the grace for readers on an older
                // manifest; segments that were never live are crash orphans and go at once.
                let marker = self.dir.join(RETIRED_DIR).join(&name);
                if let Some(age) = retired_age(&marker)
                    && age < self.retire_grace
                {
                    report.segments_retained += 1;
                    continue;
                }
                io(&path, fs::remove_dir_all(&path))?;
                let _ = fs::remove_file(&marker);
                report.segments_removed += 1;
            }
        }
        // Markers whose segment directory is already gone have nothing left to guard.
        if let Ok(markers) = fs::read_dir(self.dir.join(RETIRED_DIR)) {
            for m in markers.flatten() {
                if !self.segments_dir().join(m.file_name()).exists() {
                    let _ = fs::remove_file(m.path());
                }
            }
        }

        // Keep the previous generation's snapshot too: a reader that loaded the old manifest may
        // still be about to read it.
        let keep_snapshots = [
            format!("{:016x}.snap", self.manifest.generation),
            format!("{:016x}.snap", self.manifest.generation.saturating_sub(1)),
        ];
        for entry in read_dir(&self.dir.join(SNAPSHOTS_DIR))? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stale = name.ends_with(".snap") && !keep_snapshots.contains(&name);
            if stale || name.ends_with(".snap.tmp") {
                let path = entry.path();
                io(&path, fs::remove_file(&path))?;
                report.snapshots_removed += 1;
            }
        }

        let manifest_tmp = tmp_path(&self.dir.join(MANIFEST_FILE));
        if manifest_tmp.exists() {
            io(&manifest_tmp, fs::remove_file(&manifest_tmp))?;
            report.tmp_removed += 1;
        }
        Ok(report)
    }

    fn require_writer(&self) -> Result<()> {
        if self.lock.is_none() {
            return Err(Error::InvalidArgument(
                "store is open read-only".to_string(),
            ));
        }
        Ok(())
    }

    fn segments_dir(&self) -> PathBuf {
        self.dir.join(SEGMENTS_DIR)
    }

    fn snapshot_path(&self, generation: u64) -> PathBuf {
        self.dir
            .join(SNAPSHOTS_DIR)
            .join(format!("{generation:016x}.snap"))
    }
}

/// Seconds since a segment was retired, from its marker; `None` if it has no marker.
fn retired_age(marker: &Path) -> Option<Duration> {
    let secs: u64 = fs::read_to_string(marker).ok()?.trim().parse().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    Some(now.saturating_sub(Duration::from_secs(secs)))
}

fn is_segment_dir_name(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_dir(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut out = Vec::new();
    for entry in io(dir, fs::read_dir(dir))? {
        out.push(io(dir, entry)?);
    }
    Ok(out)
}

/// Tell git and other tools to leave the index directory alone: a `.gitignore` that ignores
/// everything (so users need not edit theirs) and a `CACHEDIR.TAG` per the Cache Directory
/// Tagging Specification (so backup tools and loupe's own walker skip it). Best effort.
fn mark_as_cache(dir: &Path) {
    for (name, body) in [
        (".gitignore", "*\n"),
        (
            "CACHEDIR.TAG",
            "Signature: 8a477f597d28d172789f06886806bc55\n# This directory is a loupe index cache.\n",
        ),
    ] {
        let path = dir.join(name);
        if !path.exists() {
            let _ = fs::write(path, body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loupe_core::{BlobId, FileMeta};
    use std::fs;

    fn snapshot_of(files: &[(&str, &str)]) -> Snapshot {
        let mut s = Snapshot {
            taken_ns: 42,
            ..Snapshot::default()
        };
        for (p, c) in files {
            s.files.insert(
                (*p).into(),
                FileMeta {
                    size: c.len() as u64,
                    mtime_ns: 1,
                    blob: BlobId::of(c.as_bytes()),
                    lang: None,
                },
            );
        }
        s
    }

    /// Build and publish a segment holding `contents` as blobs plus one payload file.
    fn make_segment(store: &mut Store, contents: &[&str]) -> SegmentId {
        let mut w = store.new_segment().unwrap();
        for c in contents {
            w.add_blob(BlobId::of(c.as_bytes()));
        }
        w.add_file("payload.bin", b"payload bytes").unwrap();
        w.add_file("empty.bin", b"").unwrap();
        w.finish().unwrap()
    }

    fn segment_file(store: &Store, id: SegmentId, name: &str) -> PathBuf {
        store
            .dir()
            .join("segments")
            .join(segment_dir_name(id))
            .join(name)
    }

    #[test]
    fn empty_store_is_generation_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_writer(dir.path()).unwrap();
        assert_eq!(store.snapshot_id(), 0);
        assert!(store.load_snapshot().unwrap().is_empty());
        assert!(store.segment_ids().is_empty());
    }

    #[test]
    fn segment_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open_writer(dir.path()).unwrap();
        let id = make_segment(&mut store, &["one", "two", "one"]);
        let seg = store.open_segment(id).unwrap();
        assert_eq!(seg.blobs().len(), 2, "duplicate blob is deduped");
        let two = BlobId::of(b"two");
        let local = seg.local_id(&two).unwrap();
        assert_eq!(seg.blob(local), Some(two));
        assert_eq!(seg.file("payload.bin"), Some(&b"payload bytes"[..]));
        assert_eq!(seg.file("empty.bin"), Some(&b""[..]));
        assert_eq!(seg.file("nope"), None);
    }

    #[test]
    fn segment_file_names_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open_writer(dir.path()).unwrap();
        let mut w = store.new_segment().unwrap();
        for bad in ["", "../x", "a/b", ".hidden", "meta.bin"] {
            assert!(w.add_file(bad, b"x").is_err(), "{bad:?} accepted");
        }
        w.add_file("ok", b"x").unwrap();
        assert!(w.add_file("ok", b"y").is_err(), "duplicate accepted");
    }

    #[test]
    fn commit_then_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let snap = snapshot_of(&[("a.txt", "A"), ("b.txt", "B")]);
        let seg;
        {
            let mut store = Store::open_writer(dir.path()).unwrap();
            seg = make_segment(&mut store, &["A", "B"]);
            assert_eq!(store.commit(&snap, &[seg]).unwrap(), 1);
        }
        let store = Store::open_writer(dir.path()).unwrap();
        assert_eq!(store.snapshot_id(), 1);
        assert_eq!(store.load_snapshot().unwrap(), snap);
        assert_eq!(store.segment_ids(), [seg]);
        assert!(store.verify().is_empty());
    }

    #[test]
    fn segment_ids_are_never_reused() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open_writer(dir.path()).unwrap();
        let a = make_segment(&mut store, &["a"]);
        store.commit(&snapshot_of(&[("a", "a")]), &[a]).unwrap();
        drop(store);
        let mut store = Store::open_writer(dir.path()).unwrap();
        let b = make_segment(&mut store, &["b"]);
        assert!(b.0 > a.0);
    }

    #[test]
    fn commit_rejects_unknown_and_duplicate_segments() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open_writer(dir.path()).unwrap();
        let s = Snapshot::default();
        assert!(store.commit(&s, &[SegmentId(99)]).is_err());
        let a = make_segment(&mut store, &["a"]);
        assert!(store.commit(&s, &[a, a]).is_err());
        assert_eq!(store.snapshot_id(), 0, "failed commits change nothing");
    }

    // ---- corruption detection -------------------------------------------------------

    fn committed_segment(dir: &Path) -> (Store, SegmentId) {
        let mut store = Store::open_writer(dir).unwrap();
        let id = make_segment(&mut store, &["x", "y"]);
        store.commit(&snapshot_of(&[("x", "x")]), &[id]).unwrap();
        (store, id)
    }

    #[test]
    fn bit_flip_in_payload_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let (store, id) = committed_segment(dir.path());
        let p = segment_file(&store, id, "payload.bin");
        let mut bytes = fs::read(&p).unwrap();
        bytes[0] ^= 0x01;
        fs::write(&p, bytes).unwrap();
        assert!(matches!(store.open_segment(id), Err(Error::Corrupt(_))));
        let bad = store.verify();
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].0, id);
    }

    #[test]
    fn truncated_payload_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let (store, id) = committed_segment(dir.path());
        let p = segment_file(&store, id, "payload.bin");
        let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(3).unwrap();
        assert!(matches!(store.open_segment(id), Err(Error::Corrupt(_))));
    }

    #[test]
    fn missing_payload_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let (store, id) = committed_segment(dir.path());
        fs::remove_file(segment_file(&store, id, "payload.bin")).unwrap();
        assert!(matches!(store.open_segment(id), Err(Error::Corrupt(_))));
    }

    #[test]
    fn corrupt_or_truncated_meta_is_detected_at_every_length() {
        let dir = tempfile::tempdir().unwrap();
        let (store, id) = committed_segment(dir.path());
        let p = segment_file(&store, id, "meta.bin");
        let original = fs::read(&p).unwrap();
        for n in 0..original.len() {
            fs::write(&p, &original[..n]).unwrap();
            assert!(store.open_segment(id).is_err(), "meta cut to {n} accepted");
        }
        let mut flipped = original.clone();
        flipped[10] ^= 0x80;
        fs::write(&p, flipped).unwrap();
        assert!(matches!(store.open_segment(id), Err(Error::Corrupt(_))));
        fs::write(&p, original).unwrap();
        assert!(store.open_segment(id).is_ok());
    }

    #[test]
    fn corrupt_snapshot_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = committed_segment(dir.path());
        let p = store.snapshot_path(store.snapshot_id());
        let mut bytes = fs::read(&p).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        fs::write(&p, bytes).unwrap();
        assert!(matches!(store.load_snapshot(), Err(Error::Corrupt(_))));
    }

    #[test]
    fn corrupt_manifest_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        drop(committed_segment(dir.path()));
        fs::write(dir.path().join(MANIFEST_FILE), b"{ not json").unwrap();
        assert!(matches!(
            Store::open_writer(dir.path()),
            Err(Error::Corrupt(_))
        ));
    }

    // ---- crash safety ---------------------------------------------------------------
    //
    // These construct the on-disk state a kill would leave at each step of a commit and
    // check recovery. They do not SIGKILL a live process.

    #[test]
    fn crash_mid_segment_write_leaves_only_temp() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, id) = committed_segment(dir.path());
        let gen_before = store.snapshot_id();

        let mut w = store.new_segment().unwrap();
        w.add_file("payload.bin", b"half written").unwrap();
        std::mem::forget(w); // no Drop cleanup: simulates the process dying here
        drop(store);

        let tmp_dirs = |p: &Path| {
            fs::read_dir(p.join("segments"))
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".tmp-")
                })
                .count()
        };
        assert_eq!(tmp_dirs(dir.path()), 1);

        let store = Store::open_writer(dir.path()).unwrap();
        assert_eq!(tmp_dirs(dir.path()), 0, "gc removed the temp segment");
        assert_eq!(store.snapshot_id(), gen_before);
        assert_eq!(store.segment_ids(), [id]);
        assert!(store.verify().is_empty());
    }

    #[test]
    fn crash_after_segment_rename_before_manifest_orphans_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, id) = committed_segment(dir.path());
        let orphan = make_segment(&mut store, &["orphan"]); // published, never committed
        assert!(segment_file(&store, orphan, "meta.bin").exists());
        drop(store);

        let store = Store::open_writer(dir.path()).unwrap();
        assert!(!segment_file(&store, orphan, "meta.bin").exists());
        assert_eq!(store.segment_ids(), [id]);
        assert_eq!(store.snapshot_id(), 1);
    }

    #[test]
    fn crash_after_snapshot_before_manifest_keeps_old_state() {
        let dir = tempfile::tempdir().unwrap();
        let (store, id) = committed_segment(dir.path());
        let old = store.load_snapshot().unwrap();
        // The next generation's snapshot made it to disk, but the manifest never moved.
        let stray = store.snapshot_path(2);
        fs::write(&stray, snapshot_io::encode(&snapshot_of(&[("new", "new")]))).unwrap();
        drop(store);

        let store = Store::open_writer(dir.path()).unwrap();
        assert!(!stray.exists(), "gc removed the unreferenced snapshot");
        assert_eq!(store.snapshot_id(), 1);
        assert_eq!(store.load_snapshot().unwrap(), old);
        assert_eq!(store.segment_ids(), [id]);
    }

    #[test]
    fn crash_mid_manifest_write_keeps_old_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let (store, id) = committed_segment(dir.path());
        // atomic_write died after creating the temp file but before the rename.
        let tmp = tmp_path(&dir.path().join(MANIFEST_FILE));
        fs::write(&tmp, b"{\"format_version\":1,\"gener").unwrap();
        drop(store);

        let store = Store::open_writer(dir.path()).unwrap();
        assert!(!tmp.exists());
        assert_eq!(store.snapshot_id(), 1);
        assert_eq!(store.segment_ids(), [id]);
    }

    #[test]
    fn unfinished_writer_cleans_up_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open_writer(dir.path()).unwrap();
        let mut w = store.new_segment().unwrap();
        w.add_file("a", b"a").unwrap();
        drop(w);
        let leftover: Vec<_> = fs::read_dir(dir.path().join("segments")).unwrap().collect();
        assert!(leftover.is_empty());
    }

    #[test]
    fn gc_drops_retired_segments_and_old_snapshots_once_the_grace_allows() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, first) = committed_segment(dir.path());
        store.set_retire_grace(Duration::ZERO);
        let second = make_segment(&mut store, &["z"]);
        // Generation 2 drops the first segment entirely.
        store
            .commit(&snapshot_of(&[("z", "z")]), &[second])
            .unwrap();
        let report = store.gc().unwrap();
        assert_eq!(report.segments_removed, 1);
        assert!(!segment_file(&store, first, "meta.bin").exists());
        assert_eq!(
            report.snapshots_removed, 0,
            "the previous generation's snapshot is kept for readers"
        );
        // A third generation lets the first snapshot go.
        store
            .commit(&snapshot_of(&[("z", "z")]), &[second])
            .unwrap();
        assert_eq!(store.gc().unwrap().snapshots_removed, 1);
        assert!(store.verify().is_empty());
        assert!(
            !store
                .dir()
                .join("retired")
                .join(segment_dir_name(first))
                .exists(),
            "marker removed with its segment"
        );
    }

    #[test]
    fn a_retired_segment_survives_its_grace_period_for_readers_on_the_old_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let (mut writer, first) = committed_segment(dir.path());
        let reader = Store::open_reader(dir.path()).unwrap(); // pinned to the generation holding `first`
        let second = make_segment(&mut writer, &["z"]);
        writer
            .commit(&snapshot_of(&[("z", "z")]), &[second])
            .unwrap();

        // Default grace: GC leaves the retired segment alone, so the old reader still works.
        let report = writer.gc().unwrap();
        assert_eq!((report.segments_removed, report.segments_retained), (0, 1));
        assert!(
            reader.open_segment(first).is_ok(),
            "reader on the old manifest is not broken"
        );
        assert!(reader.load_snapshot().is_ok());

        // Once the grace has passed, it is deleted.
        writer.set_retire_grace(Duration::ZERO);
        assert_eq!(writer.gc().unwrap().segments_removed, 1);
        assert!(reader.open_segment(first).is_err());
    }

    #[test]
    fn crash_orphans_are_deleted_immediately_even_with_a_long_grace() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _live) = committed_segment(dir.path());
        store.set_retire_grace(Duration::from_secs(3600));
        let orphan = make_segment(&mut store, &["never committed"]);
        let report = store.gc().unwrap();
        assert_eq!(
            (report.segments_removed, report.segments_retained),
            (1, 0),
            "no marker, so not a retired segment"
        );
        assert!(!segment_file(&store, orphan, "meta.bin").exists());
    }

    #[test]
    fn a_crash_between_marking_and_the_manifest_swap_only_delays_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, live) = committed_segment(dir.path());
        // Simulate the marker having been written for a segment that is in fact still live.
        let marker_dir = store.dir().join("retired");
        fs::create_dir_all(&marker_dir).unwrap();
        fs::write(marker_dir.join(segment_dir_name(live)), "0").unwrap();
        store.set_retire_grace(Duration::ZERO);
        assert_eq!(
            store.gc().unwrap().segments_removed,
            0,
            "live segments are never deleted, marker or not"
        );
        assert!(store.verify().is_empty());
        assert!(
            store.open_segment(live).is_ok(),
            "the live segment is still intact and readable"
        );
    }

    // ---- locking --------------------------------------------------------------------

    #[test]
    fn second_writer_is_refused_until_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let first = Store::open_writer(dir.path()).unwrap();
        assert!(matches!(
            Store::open_writer(dir.path()),
            Err(Error::Locked(_))
        ));
        drop(first);
        assert!(Store::open_writer(dir.path()).is_ok());
    }

    #[test]
    fn readers_coexist_with_writer_and_follow_commits() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Store::open_writer(dir.path()).unwrap();
        let mut reader = Store::open_reader(dir.path()).unwrap();
        assert!(!reader.is_writer());
        assert_eq!(reader.snapshot_id(), 0);

        let id = make_segment(&mut writer, &["a"]);
        let snap = snapshot_of(&[("a", "a")]);
        writer.commit(&snap, &[id]).unwrap();

        assert_eq!(
            reader.snapshot_id(),
            0,
            "reader keeps its view until refresh"
        );
        reader.refresh().unwrap();
        assert_eq!(reader.snapshot_id(), 1);
        assert_eq!(reader.load_snapshot().unwrap(), snap);
        assert!(reader.open_segment(id).is_ok());
    }

    #[test]
    fn readers_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        drop(Store::open_writer(dir.path()).unwrap());
        let mut reader = Store::open_reader(dir.path()).unwrap();
        assert!(reader.new_segment().is_err());
        assert!(reader.commit(&Snapshot::default(), &[]).is_err());
        assert!(reader.gc().is_err());
    }

    #[test]
    fn reader_of_missing_dir_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Store::open_reader(dir.path().join("nope")).is_err());
    }
}
