//! Per-segment symbol index.
//!
//! ```text
//! symbols.bin        "LPSY" | version u32 | n_symbols u32 | n_blobs u32 | heap_len u32
//!                    blob table: n_blobs x (first record u32, count u32)  (indexed by local blob id)
//!                    records, 48 bytes each, grouped by blob in start-byte order:
//!                      kind u8 | visibility u8 | pad u16 | parent u32 (record index or MAX)
//!                      name off,len | signature off,len | doc off,len (doc len MAX = none)
//!                      start_line | end_line | start_byte | end_byte      (all u32)
//!                    string heap
//! symbols.names.fst  "LPSN" | version u32 | fst_len u64 | fst (name -> first<<32 | count)
//!                    | order: n_symbols x u32 record indices, grouped by name
//! ```

use std::collections::HashMap;

use fst::automaton::{Automaton, Str};
use fst::{IntoStreamer, Map, MapBuilder, Streamer};
use loupe_core::{Error, LocalBlobId, Result};
use loupe_store::{Segment, SegmentWriter};

use crate::symbol::{Symbol, SymbolKind, Visibility};

pub const SYMBOLS_FILE: &str = "symbols.bin";
pub const NAMES_FILE: &str = "symbols.names.fst";

const SYM_MAGIC: &[u8; 4] = b"LPSY";
const NAMES_MAGIC: &[u8; 4] = b"LPSN";
const VERSION: u32 = 1;
const SYM_HEADER: usize = 20;
const BLOB_ENTRY: usize = 8;
const RECORD: usize = 48;
const NO_PARENT: u32 = u32::MAX;
const NO_DOC: u32 = u32::MAX;
const NAMES_HEADER: usize = 16;

fn corrupt(msg: &str) -> Error {
    Error::Corrupt(format!("symbol index: {msg}"))
}

/// Built index files.
#[derive(Debug)]
pub struct BuiltSymbols {
    pub symbols: Vec<u8>,
    pub names: Vec<u8>,
}

impl BuiltSymbols {
    pub fn write_to(&self, writer: &mut SegmentWriter) -> Result<()> {
        writer.add_file(SYMBOLS_FILE, &self.symbols)?;
        writer.add_file(NAMES_FILE, &self.names)
    }

    pub fn view(&self) -> Result<SymbolIndex<'_>> {
        SymbolIndex::from_parts(&self.symbols, &self.names)
    }
}

/// Collects symbols per blob. Blobs must be added in increasing local-id order.
#[derive(Debug, Default)]
pub struct SymbolIndexBuilder {
    blobs: Vec<(u32, Vec<Symbol>)>,
}

impl SymbolIndexBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, blob: LocalBlobId, symbols: Vec<Symbol>) -> Result<()> {
        if self.blobs.last().is_some_and(|(last, _)| blob.0 <= *last) {
            return Err(Error::InvalidArgument(
                "blobs must be added in increasing order".into(),
            ));
        }
        if !symbols.is_empty() {
            self.blobs.push((blob.0, symbols));
        }
        Ok(())
    }

    /// `n_blobs` is the segment's blob count, so the blob table covers blobs with no symbols too.
    pub fn finish(self, n_blobs: u32) -> Result<BuiltSymbols> {
        let total: usize = self.blobs.iter().map(|(_, s)| s.len()).sum();
        let mut table = vec![(0u32, 0u32); n_blobs as usize];
        let mut heap: Vec<u8> = Vec::new();
        let mut interned: HashMap<String, (u32, u32)> = HashMap::new();
        let mut records: Vec<u8> = Vec::with_capacity(total * RECORD);
        let mut names: Vec<(Vec<u8>, u32)> = Vec::with_capacity(total);

        let mut index = 0u32;
        for (blob, symbols) in &self.blobs {
            if *blob >= n_blobs {
                return Err(Error::InvalidArgument("blob id beyond segment".into()));
            }
            table[*blob as usize] = (index, symbols.len() as u32);
            let first = index;
            for s in symbols {
                let (name_off, name_len) = *interned.entry(s.name.clone()).or_insert_with(|| {
                    let off = heap.len() as u32;
                    heap.extend_from_slice(s.name.as_bytes());
                    (off, s.name.len() as u32)
                });
                let sig_off = heap.len() as u32;
                heap.extend_from_slice(s.signature.as_bytes());
                let (doc_off, doc_len) = match &s.doc {
                    Some(d) => {
                        let off = heap.len() as u32;
                        heap.extend_from_slice(d.as_bytes());
                        (off, d.len() as u32)
                    }
                    None => (0, NO_DOC),
                };
                records.push(s.kind.to_u8());
                records.push(s.visibility.to_u8());
                records.extend_from_slice(&0u16.to_le_bytes());
                let parent = s.parent.map_or(NO_PARENT, |p| first + p);
                for v in [
                    parent,
                    name_off,
                    name_len,
                    sig_off,
                    s.signature.len() as u32,
                    doc_off,
                    doc_len,
                    s.start_line,
                    s.end_line,
                    s.start_byte,
                    s.end_byte,
                ] {
                    records.extend_from_slice(&v.to_le_bytes());
                }
                names.push((s.name.as_bytes().to_vec(), index));
                index += 1;
            }
        }
        debug_assert_eq!(records.len(), total * RECORD);

        let mut symbols =
            Vec::with_capacity(SYM_HEADER + table.len() * BLOB_ENTRY + records.len() + heap.len());
        symbols.extend_from_slice(SYM_MAGIC);
        for v in [VERSION, total as u32, n_blobs, heap.len() as u32] {
            symbols.extend_from_slice(&v.to_le_bytes());
        }
        for (first, count) in &table {
            symbols.extend_from_slice(&first.to_le_bytes());
            symbols.extend_from_slice(&count.to_le_bytes());
        }
        symbols.extend_from_slice(&records);
        symbols.extend_from_slice(&heap);

        // Names: group record indices by name; the fst maps a name to its group in `order`.
        names.sort();
        let mut fst = MapBuilder::memory();
        let mut order: Vec<u8> = Vec::with_capacity(names.len() * 4);
        let mut i = 0;
        while i < names.len() {
            let mut j = i;
            while j < names.len() && names[j].0 == names[i].0 {
                order.extend_from_slice(&names[j].1.to_le_bytes());
                j += 1;
            }
            fst.insert(&names[i].0, ((i as u64) << 32) | (j - i) as u64)
                .map_err(|e| corrupt(&format!("fst build: {e}")))?;
            i = j;
        }
        let fst_bytes = fst
            .into_inner()
            .map_err(|e| corrupt(&format!("fst build: {e}")))?;
        let mut names_file = Vec::with_capacity(NAMES_HEADER + fst_bytes.len() + order.len());
        names_file.extend_from_slice(NAMES_MAGIC);
        names_file.extend_from_slice(&VERSION.to_le_bytes());
        names_file.extend_from_slice(&(fst_bytes.len() as u64).to_le_bytes());
        names_file.extend_from_slice(&fst_bytes);
        names_file.extend_from_slice(&order);

        Ok(BuiltSymbols {
            symbols,
            names: names_file,
        })
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// A symbol located inside a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SymbolRef {
    pub blob: LocalBlobId,
    /// Record index within the segment.
    pub record: u32,
}

pub struct SymbolIndex<'a> {
    n_symbols: usize,
    n_blobs: usize,
    blob_table: &'a [u8],
    records: &'a [u8],
    heap: &'a [u8],
    names: Map<&'a [u8]>,
    order: &'a [u8],
}

impl std::fmt::Debug for SymbolIndex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SymbolIndex")
            .field("symbols", &self.n_symbols)
            .field("blobs", &self.n_blobs)
            .finish()
    }
}

impl<'a> SymbolIndex<'a> {
    /// `None` if the segment predates symbols (it has neither file).
    pub fn from_segment(segment: &'a Segment) -> Result<Option<SymbolIndex<'a>>> {
        match (segment.file(SYMBOLS_FILE), segment.file(NAMES_FILE)) {
            (Some(s), Some(n)) => Self::from_parts(s, n).map(Some),
            (None, None) => Ok(None),
            _ => Err(corrupt("segment has only one of the symbol files")),
        }
    }

    pub fn from_parts(sym: &'a [u8], names: &'a [u8]) -> Result<SymbolIndex<'a>> {
        if sym.len() < SYM_HEADER || &sym[..4] != SYM_MAGIC {
            return Err(corrupt("bad symbols header"));
        }
        let version = u32_at(sym, 4);
        if version != VERSION {
            return Err(Error::FormatVersion {
                found: version,
                expected: VERSION,
            });
        }
        let (n_symbols, n_blobs, heap_len) = (
            u32_at(sym, 8) as usize,
            u32_at(sym, 12) as usize,
            u32_at(sym, 16) as usize,
        );
        let table_end = SYM_HEADER + n_blobs * BLOB_ENTRY;
        let records_end = table_end + n_symbols * RECORD;
        if sym.len() != records_end + heap_len {
            return Err(corrupt("symbols length mismatch"));
        }

        if names.len() < NAMES_HEADER || &names[..4] != NAMES_MAGIC {
            return Err(corrupt("bad names header"));
        }
        let version = u32_at(names, 4);
        if version != VERSION {
            return Err(Error::FormatVersion {
                found: version,
                expected: VERSION,
            });
        }
        let fst_len = usize::try_from(u64::from_le_bytes(names[8..16].try_into().unwrap()))
            .map_err(|_| corrupt("fst length"))?;
        let fst_end = NAMES_HEADER
            .checked_add(fst_len)
            .filter(|&e| e <= names.len())
            .ok_or_else(|| corrupt("fst length exceeds file"))?;
        let order = &names[fst_end..];
        if order.len() != n_symbols * 4 {
            return Err(corrupt("name order length mismatch"));
        }
        Ok(SymbolIndex {
            n_symbols,
            n_blobs,
            blob_table: &sym[SYM_HEADER..table_end],
            records: &sym[table_end..records_end],
            heap: &sym[records_end..],
            names: Map::new(&names[NAMES_HEADER..fst_end])
                .map_err(|e| corrupt(&format!("names fst: {e}")))?,
            order,
        })
    }

    pub fn len(&self) -> usize {
        self.n_symbols
    }

    pub fn is_empty(&self) -> bool {
        self.n_symbols == 0
    }

    fn heap_str(&self, off: u32, len: u32) -> Result<&'a str> {
        let (off, len) = (off as usize, len as usize);
        let bytes = off
            .checked_add(len)
            .and_then(|end| self.heap.get(off..end))
            .ok_or_else(|| corrupt("string outside heap"))?;
        std::str::from_utf8(bytes).map_err(|_| corrupt("invalid utf-8"))
    }

    fn record(&self, i: u32) -> Result<&'a [u8]> {
        let at = i as usize * RECORD;
        self.records
            .get(at..at + RECORD)
            .ok_or_else(|| corrupt("record index out of range"))
    }

    fn decode(&self, i: u32, first_of_blob: u32) -> Result<Symbol> {
        let r = self.record(i)?;
        let kind = SymbolKind::from_u8(r[0]).ok_or_else(|| corrupt("bad kind"))?;
        let visibility = Visibility::from_u8(r[1]).ok_or_else(|| corrupt("bad visibility"))?;
        let parent = u32_at(r, 4);
        let doc_len = u32_at(r, 28);
        Ok(Symbol {
            name: self.heap_str(u32_at(r, 8), u32_at(r, 12))?.to_string(),
            kind,
            visibility,
            parent: (parent != NO_PARENT).then(|| parent.saturating_sub(first_of_blob)),
            signature: self.heap_str(u32_at(r, 16), u32_at(r, 20))?.to_string(),
            doc: (doc_len != NO_DOC)
                .then(|| self.heap_str(u32_at(r, 24), doc_len).map(str::to_string))
                .transpose()?,
            start_line: u32_at(r, 32),
            end_line: u32_at(r, 36),
            start_byte: u32_at(r, 40),
            end_byte: u32_at(r, 44),
        })
    }

    fn blob_range(&self, blob: LocalBlobId) -> Option<(u32, u32)> {
        if blob.0 as usize >= self.n_blobs {
            return None;
        }
        let at = blob.0 as usize * BLOB_ENTRY;
        Some((u32_at(self.blob_table, at), u32_at(self.blob_table, at + 4)))
    }

    /// Symbols of one blob in source order, `parent` indices relative to this list.
    pub fn symbols_of(&self, blob: LocalBlobId) -> Result<Vec<Symbol>> {
        let Some((first, count)) = self.blob_range(blob) else {
            return Ok(Vec::new());
        };
        (first..first + count)
            .map(|i| self.decode(i, first))
            .collect()
    }

    /// Symbols named exactly `name`, across all blobs of the segment.
    pub fn find(&self, name: &str) -> Result<Vec<SymbolRef>> {
        let Some(packed) = self.names.get(name) else {
            return Ok(Vec::new());
        };
        let (start, count) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
        let group = self
            .order
            .get(start * 4..(start + count) * 4)
            .ok_or_else(|| corrupt("name group outside order table"))?;
        let mut out = Vec::with_capacity(count);
        for chunk in group.chunks_exact(4) {
            let record = u32::from_le_bytes(chunk.try_into().unwrap());
            out.push(SymbolRef {
                blob: self.blob_of(record)?,
                record,
            });
        }
        Ok(out)
    }

    /// Decode one located symbol (its `parent` is relative to its blob's list).
    pub fn symbol(&self, r: SymbolRef) -> Result<Symbol> {
        let (first, _) = self
            .blob_range(r.blob)
            .ok_or_else(|| corrupt("blob out of range"))?;
        self.decode(r.record, first)
    }

    /// Names for which `pred` holds, in byte order, at most `limit`. Streams the name map, so it
    /// allocates only for matches.
    pub fn names_where(&self, mut pred: impl FnMut(&[u8]) -> bool, limit: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut stream = self.names.stream();
        while let Some((key, _)) = stream.next() {
            if pred(key) {
                out.push(String::from_utf8_lossy(key).into_owned());
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    /// Every distinct symbol name in the segment, in byte order.
    pub fn names(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.names.len());
        let mut stream = self.names.stream();
        while let Some((key, _)) = stream.next() {
            out.push(String::from_utf8_lossy(key).into_owned());
        }
        out
    }

    /// Up to `limit` distinct symbol names starting with `prefix`, in byte order.
    pub fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut stream = self
            .names
            .search(Str::new(prefix).starts_with())
            .into_stream();
        while let Some((key, _)) = stream.next() {
            if out.len() >= limit {
                break;
            }
            out.push(String::from_utf8_lossy(key).into_owned());
        }
        out
    }

    /// Which blob a record belongs to (binary search over the blob table).
    fn blob_of(&self, record: u32) -> Result<LocalBlobId> {
        // Blob table entries are in blob order and records are grouped by blob in the same
        // order, so the owning blob is the last one whose range starts at or before `record`.
        let (mut lo, mut hi) = (0usize, self.n_blobs);
        let mut found = None;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            // Empty blobs have count 0 and a meaningless `first`; skip over them linearly.
            let mut m = mid;
            while m < hi
                && self
                    .blob_range(LocalBlobId(m as u32))
                    .is_some_and(|(_, c)| c == 0)
            {
                m += 1;
            }
            if m == hi {
                hi = mid;
                continue;
            }
            let (first, count) = self.blob_range(LocalBlobId(m as u32)).unwrap();
            if record < first {
                hi = mid;
            } else if record >= first + count {
                lo = m + 1;
            } else {
                found = Some(m as u32);
                break;
            }
        }
        found
            .map(LocalBlobId)
            .ok_or_else(|| corrupt("record not covered by any blob"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, kind: SymbolKind, parent: Option<u32>, line: u32) -> Symbol {
        Symbol {
            name: name.into(),
            kind,
            visibility: Visibility::Public,
            parent,
            start_line: line,
            end_line: line + 1,
            start_byte: line * 10,
            end_byte: line * 10 + 9,
            signature: format!("{} {name}()", kind.as_str()),
            doc: line.is_multiple_of(2).then(|| format!("doc of {name}")),
        }
    }

    fn built() -> BuiltSymbols {
        let mut b = SymbolIndexBuilder::new();
        // Blob 0 empty (never added), blob 1 has a class with a method, blob 3 a function,
        // blob 2 empty, blob 4 shares a name with blob 1.
        b.add(
            LocalBlobId(1),
            vec![
                sym("Server", SymbolKind::Class, None, 1),
                sym("run", SymbolKind::Method, Some(0), 2),
            ],
        )
        .unwrap();
        b.add(
            LocalBlobId(3),
            vec![sym("helper", SymbolKind::Function, None, 4)],
        )
        .unwrap();
        b.add(
            LocalBlobId(4),
            vec![sym("run", SymbolKind::Function, None, 6)],
        )
        .unwrap();
        b.finish(6).unwrap()
    }

    #[test]
    fn roundtrip_per_blob() {
        let built = built();
        let idx = built.view().unwrap();
        assert_eq!(idx.len(), 4);
        assert!(idx.symbols_of(LocalBlobId(0)).unwrap().is_empty());
        let s1 = idx.symbols_of(LocalBlobId(1)).unwrap();
        assert_eq!(s1.len(), 2);
        assert_eq!(s1[0].name, "Server");
        assert_eq!(
            s1[1].parent,
            Some(0),
            "parent is relative to the blob's list"
        );
        assert_eq!(s1[1].doc.as_deref(), Some("doc of run"));
        assert_eq!(s1[0].doc, None);
        let s3 = idx.symbols_of(LocalBlobId(3)).unwrap();
        assert_eq!(s3[0].signature, "function helper()");
        assert!(idx.symbols_of(LocalBlobId(5)).unwrap().is_empty());
        assert!(idx.symbols_of(LocalBlobId(99)).unwrap().is_empty());
    }

    #[test]
    fn name_lookup_finds_every_blob_and_prefix_works() {
        let built = built();
        let idx = built.view().unwrap();
        let refs = idx.find("run").unwrap();
        let mut blobs: Vec<u32> = refs.iter().map(|r| r.blob.0).collect();
        blobs.sort();
        assert_eq!(blobs, [1, 4]);
        let kinds: Vec<SymbolKind> = refs.iter().map(|r| idx.symbol(*r).unwrap().kind).collect();
        assert!(kinds.contains(&SymbolKind::Method) && kinds.contains(&SymbolKind::Function));
        assert!(idx.find("nope").unwrap().is_empty());
        assert_eq!(idx.names_with_prefix("", 10), ["Server", "helper", "run"]);
        assert_eq!(idx.names_with_prefix("he", 10), ["helper"]);
        assert_eq!(idx.names_with_prefix("", 2).len(), 2);
    }

    #[test]
    fn empty_index_is_valid() {
        let built = SymbolIndexBuilder::new().finish(3).unwrap();
        let idx = built.view().unwrap();
        assert!(idx.is_empty());
        assert!(idx.find("x").unwrap().is_empty());
    }

    #[test]
    fn order_is_enforced() {
        let mut b = SymbolIndexBuilder::new();
        b.add(
            LocalBlobId(2),
            vec![sym("a", SymbolKind::Function, None, 1)],
        )
        .unwrap();
        assert!(b.add(LocalBlobId(2), vec![]).is_err());
        assert!(b.add(LocalBlobId(1), vec![]).is_err());
        assert!(SymbolIndexBuilder::new().finish(0).is_ok());
        let mut b = SymbolIndexBuilder::new();
        b.add(
            LocalBlobId(5),
            vec![sym("a", SymbolKind::Function, None, 1)],
        )
        .unwrap();
        assert!(b.finish(3).is_err(), "blob beyond the segment");
    }

    #[test]
    fn every_truncation_and_garbage_is_an_error_not_a_panic() {
        let built = built();
        for n in 0..built.symbols.len() {
            assert!(
                SymbolIndex::from_parts(&built.symbols[..n], &built.names).is_err(),
                "symbols cut {n}"
            );
        }
        for n in 0..built.names.len() {
            assert!(
                SymbolIndex::from_parts(&built.symbols, &built.names[..n]).is_err(),
                "names cut {n}"
            );
        }
        // Corrupt heap offsets must error on read, never panic.
        let mut bad = built.symbols.clone();
        let rec0 = SYM_HEADER + 6 * BLOB_ENTRY;
        bad[rec0 + 8..rec0 + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        let idx = SymbolIndex::from_parts(&bad, &built.names).unwrap();
        assert!(idx.symbols_of(LocalBlobId(1)).is_err());
    }

    #[test]
    fn many_blobs_with_gaps_resolve_correctly() {
        let mut b = SymbolIndexBuilder::new();
        let mut expect = Vec::new();
        for blob in (0..200u32).filter(|b| b % 3 != 1) {
            b.add(
                LocalBlobId(blob),
                vec![sym(
                    &format!("sym_{blob}"),
                    SymbolKind::Function,
                    None,
                    blob,
                )],
            )
            .unwrap();
            expect.push(blob);
        }
        let built = b.finish(200).unwrap();
        let idx = built.view().unwrap();
        for blob in expect {
            let refs = idx.find(&format!("sym_{blob}")).unwrap();
            assert_eq!(refs.len(), 1);
            assert_eq!(refs[0].blob.0, blob, "blob_of for sym_{blob}");
        }
    }
}
