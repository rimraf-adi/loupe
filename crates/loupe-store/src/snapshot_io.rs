//! Snapshot file format: `LPSN`, version, then the path table, sealed with a checksum.

use loupe_core::{BlobId, Error, FileMeta, Result, Snapshot};

use crate::codec::{Dec, Enc, seal, unseal};

const MAGIC: &[u8; 4] = b"LPSN";
const SNAPSHOT_FORMAT_VERSION: u32 = 1;

pub fn encode(snapshot: &Snapshot) -> Vec<u8> {
    let mut e = Enc::default();
    e.raw(MAGIC);
    e.u32(SNAPSHOT_FORMAT_VERSION);
    e.i64(snapshot.taken_ns);
    e.u32(u32::try_from(snapshot.files.len()).expect("too many files"));
    for (path, meta) in &snapshot.files {
        e.str32(path);
        e.u64(meta.size);
        e.i64(meta.mtime_ns);
        e.raw(meta.blob.as_bytes());
        e.str16(meta.lang.as_deref().unwrap_or(""));
    }
    seal(e.0)
}

pub fn decode(sealed: &[u8]) -> Result<Snapshot> {
    let body = unseal(sealed, "snapshot")?;
    let mut d = Dec::new(body, "snapshot");
    d.magic(MAGIC)?;
    let version = d.u32()?;
    if version != SNAPSHOT_FORMAT_VERSION {
        return Err(Error::FormatVersion {
            found: version,
            expected: SNAPSHOT_FORMAT_VERSION,
        });
    }
    let taken_ns = d.i64()?;
    let count = d.u32()?;
    let mut snapshot = Snapshot {
        taken_ns,
        ..Snapshot::default()
    };
    for _ in 0..count {
        let path = d.str32()?;
        let size = d.u64()?;
        let mtime_ns = d.i64()?;
        let blob = BlobId::from_bytes(d.array32()?);
        let lang = d.str16()?;
        let meta = FileMeta {
            size,
            mtime_ns,
            blob,
            lang: (!lang.is_empty()).then_some(lang),
        };
        if snapshot.files.insert(path, meta).is_some() {
            return Err(Error::Corrupt("snapshot: duplicate path".into()));
        }
    }
    d.finish()?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        let mut s = Snapshot {
            taken_ns: 123,
            ..Snapshot::default()
        };
        s.files.insert(
            "src/lib.rs".into(),
            FileMeta {
                size: 10,
                mtime_ns: -1,
                blob: BlobId::of(b"a"),
                lang: Some("rust".into()),
            },
        );
        s.files.insert(
            "notes".into(),
            FileMeta {
                size: 0,
                mtime_ns: 5,
                blob: BlobId::of(b""),
                lang: None,
            },
        );
        s
    }

    #[test]
    fn roundtrip() {
        let s = sample();
        assert_eq!(decode(&encode(&s)).unwrap(), s);
    }

    #[test]
    fn empty_roundtrip() {
        let s = Snapshot::default();
        assert_eq!(decode(&encode(&s)).unwrap(), s);
    }

    #[test]
    fn every_truncation_is_rejected() {
        let bytes = encode(&sample());
        for n in 0..bytes.len() {
            assert!(decode(&bytes[..n]).is_err(), "prefix of {n} bytes accepted");
        }
    }
}
