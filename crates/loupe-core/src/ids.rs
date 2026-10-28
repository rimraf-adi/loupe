use std::fmt;

/// Content hash of a file's bytes (blake3, 256-bit).
///
/// Segments store a 128-bit truncation (see [`BlobId::short`]); the manifest keeps the full hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobId([u8; 32]);

impl BlobId {
    pub fn of(bytes: &[u8]) -> Self {
        BlobId(*blake3::hash(bytes).as_bytes())
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        BlobId(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 128-bit truncation used inside segments.
    pub fn short(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out.copy_from_slice(&self.0[..16]);
        out
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobId({})", &self.to_hex()[..12])
    }
}

/// Dense path-level id within one snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// Dense blob id within one segment. Postings use this, not [`FileId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalBlobId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(pub u64);

/// Symbol id: `(segment, index within the segment)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SymbolId {
    pub segment: SegmentId,
    pub index: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_content_same_id() {
        assert_eq!(BlobId::of(b"hello"), BlobId::of(b"hello"));
        assert_ne!(BlobId::of(b"hello"), BlobId::of(b"hellp"));
    }

    #[test]
    fn short_is_prefix_of_full() {
        let id = BlobId::of(b"x");
        assert_eq!(&id.short()[..], &id.as_bytes()[..16]);
    }

    #[test]
    fn hex_is_64_chars() {
        assert_eq!(BlobId::of(b"").to_hex().len(), 64);
    }
}
