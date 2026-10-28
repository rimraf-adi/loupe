use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use loupe_core::{BlobId, Error, LocalBlobId, Result, SegmentId};
use memmap2::Mmap;

use crate::codec::{Dec, Enc, seal, unseal};
use crate::fsio::{crash_point, io, sync_dir, write_synced};

pub const META_FILE: &str = "meta.bin";
const MAGIC: &[u8; 4] = b"LPSG";
pub const SEGMENT_FORMAT_VERSION: u32 = 1;

pub(crate) fn segment_dir_name(id: SegmentId) -> String {
    format!("{:016x}", id.0)
}

pub(crate) fn tmp_dir_name(id: SegmentId) -> String {
    format!(".tmp-{:016x}", id.0)
}

struct FileRecord {
    name: String,
    len: u64,
    checksum: [u8; 32],
}

/// Builds one immutable segment.
///
/// Files are written into a temp directory; [`SegmentWriter::finish`] writes `meta.bin` last,
/// fsyncs, and renames the directory into place. Dropping an unfinished writer deletes the
/// temp directory, and a crash leaves it for [`crate::Store::gc`].
#[derive(Debug)]
pub struct SegmentWriter {
    id: SegmentId,
    segments_dir: PathBuf,
    tmp_dir: PathBuf,
    blobs: Vec<BlobId>,
    blob_index: HashMap<BlobId, LocalBlobId>,
    files: Vec<FileRecord>,
    finished: bool,
}

impl std::fmt::Debug for FileRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({} bytes)", self.name, self.len)
    }
}

impl SegmentWriter {
    pub(crate) fn create(segments_dir: &Path, id: SegmentId) -> Result<Self> {
        let tmp_dir = segments_dir.join(tmp_dir_name(id));
        if tmp_dir.exists() {
            io(&tmp_dir, fs::remove_dir_all(&tmp_dir))?;
        }
        io(&tmp_dir, fs::create_dir_all(&tmp_dir))?;
        Ok(SegmentWriter {
            id,
            segments_dir: segments_dir.to_path_buf(),
            tmp_dir,
            blobs: Vec::new(),
            blob_index: HashMap::new(),
            files: Vec::new(),
            finished: false,
        })
    }

    pub fn id(&self) -> SegmentId {
        self.id
    }

    /// Register a blob in this segment. Idempotent: the same blob gets the same local id.
    pub fn add_blob(&mut self, blob: BlobId) -> LocalBlobId {
        if let Some(&id) = self.blob_index.get(&blob) {
            return id;
        }
        let id = LocalBlobId(u32::try_from(self.blobs.len()).expect("too many blobs in segment"));
        self.blobs.push(blob);
        self.blob_index.insert(blob, id);
        id
    }

    /// Add a payload file (e.g. `text.tri`). Names are flat, unique, and not `meta.bin`.
    pub fn add_file(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        let bad = name.is_empty()
            || name.starts_with('.')
            || name.contains(['/', '\\'])
            || name == META_FILE
            || name.len() > usize::from(u16::MAX);
        if bad {
            return Err(Error::InvalidArgument(format!(
                "invalid segment file name: {name:?}"
            )));
        }
        if self.files.iter().any(|f| f.name == name) {
            return Err(Error::InvalidArgument(format!(
                "duplicate segment file: {name}"
            )));
        }
        write_synced(&self.tmp_dir.join(name), bytes)?;
        crash_point("segment.payload_written");
        self.files.push(FileRecord {
            name: name.to_string(),
            len: bytes.len() as u64,
            checksum: *blake3::hash(bytes).as_bytes(),
        });
        Ok(())
    }

    /// Publish the segment. After this returns, the segment is durable but not yet live: it
    /// becomes live when a manifest that lists it is committed.
    pub fn finish(mut self) -> Result<SegmentId> {
        let mut e = Enc::default();
        e.raw(MAGIC);
        e.u32(SEGMENT_FORMAT_VERSION);
        e.u64(self.id.0);
        e.u32(u32::try_from(self.blobs.len()).expect("too many blobs"));
        for b in &self.blobs {
            e.raw(b.as_bytes());
        }
        e.u32(u32::try_from(self.files.len()).expect("too many files"));
        for f in &self.files {
            e.str16(&f.name);
            e.u64(f.len);
            e.raw(&f.checksum);
        }
        crash_point("segment.before_meta");
        write_synced(&self.tmp_dir.join(META_FILE), &seal(e.0))?;
        sync_dir(&self.tmp_dir)?;
        crash_point("segment.before_rename");

        let final_dir = self.segments_dir.join(segment_dir_name(self.id));
        io(&final_dir, fs::rename(&self.tmp_dir, &final_dir))?;
        self.finished = true;
        crash_point("segment.after_rename");
        sync_dir(&self.segments_dir)?;
        Ok(self.id)
    }
}

impl Drop for SegmentWriter {
    fn drop(&mut self) {
        if !self.finished {
            let _ = fs::remove_dir_all(&self.tmp_dir);
        }
    }
}

enum MappedFile {
    Empty,
    Map(Mmap),
}

impl MappedFile {
    fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        if file.metadata()?.len() == 0 {
            return Ok(MappedFile::Empty); // zero-length mappings are an error on some platforms
        }
        // SAFETY: segment files are never modified after publication (writers only create
        // new segments and delete whole unreferenced ones). If something outside this
        // program truncates a mapped file, reads can fault; that is outside our threat model.
        Ok(MappedFile::Map(unsafe { Mmap::map(&file)? }))
    }

    fn bytes(&self) -> &[u8] {
        match self {
            MappedFile::Empty => &[],
            MappedFile::Map(m) => m,
        }
    }
}

/// A verified, memory-mapped segment.
///
/// [`Segment::open`] checks `meta.bin`'s checksum and every payload file's length and
/// checksum, so any `Ok` segment is intact as of open time.
pub struct Segment {
    id: SegmentId,
    blobs: Vec<BlobId>,
    blob_index: HashMap<BlobId, LocalBlobId>,
    files: HashMap<String, MappedFile>,
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segment")
            .field("id", &self.id)
            .field("blobs", &self.blobs.len())
            .field("files", &self.files.len())
            .finish()
    }
}

impl Segment {
    pub(crate) fn open(segments_dir: &Path, id: SegmentId) -> Result<Segment> {
        let dir = segments_dir.join(segment_dir_name(id));
        let meta_path = dir.join(META_FILE);
        let raw = io(&meta_path, fs::read(&meta_path))?;
        let body = unseal(&raw, "segment meta")?;

        let mut d = Dec::new(body, "segment meta");
        d.magic(MAGIC)?;
        let version = d.u32()?;
        if version != SEGMENT_FORMAT_VERSION {
            return Err(Error::FormatVersion {
                found: version,
                expected: SEGMENT_FORMAT_VERSION,
            });
        }
        let stored_id = d.u64()?;
        if stored_id != id.0 {
            return Err(Error::Corrupt(format!(
                "segment {} claims id {:016x}",
                segment_dir_name(id),
                stored_id
            )));
        }

        let blob_count = d.u32()? as usize;
        let mut blobs = Vec::new();
        let mut blob_index = HashMap::new();
        for i in 0..blob_count {
            let blob = BlobId::from_bytes(d.array32()?);
            blob_index.insert(blob, LocalBlobId(i as u32));
            blobs.push(blob);
        }

        let file_count = d.u32()? as usize;
        let mut files = HashMap::new();
        for _ in 0..file_count {
            let name = d.str16()?;
            let len = d.u64()?;
            let checksum = d.array32()?;
            let path = dir.join(&name);
            let mapped = MappedFile::open(&path).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    Error::Corrupt(format!("segment file missing: {}", path.display()))
                }
                _ => Error::io(&path, e),
            })?;
            let bytes = mapped.bytes();
            if bytes.len() as u64 != len {
                return Err(Error::Corrupt(format!(
                    "{}: length {} != recorded {}",
                    path.display(),
                    bytes.len(),
                    len
                )));
            }
            if blake3::hash(bytes).as_bytes() != &checksum {
                return Err(Error::Corrupt(format!(
                    "{}: checksum mismatch",
                    path.display()
                )));
            }
            files.insert(name, mapped);
        }
        d.finish()?;

        Ok(Segment {
            id,
            blobs,
            blob_index,
            files,
        })
    }

    pub fn id(&self) -> SegmentId {
        self.id
    }

    /// Blobs in local-id order.
    pub fn blobs(&self) -> &[BlobId] {
        &self.blobs
    }

    pub fn blob(&self, id: LocalBlobId) -> Option<BlobId> {
        self.blobs.get(id.0 as usize).copied()
    }

    pub fn local_id(&self, blob: &BlobId) -> Option<LocalBlobId> {
        self.blob_index.get(blob).copied()
    }

    /// Bytes of a payload file, or `None` if the segment has no such file.
    pub fn file(&self, name: &str) -> Option<&[u8]> {
        self.files.get(name).map(MappedFile::bytes)
    }
}
