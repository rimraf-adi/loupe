//! Posting lists: strictly increasing `u32` ids, stored as delta-varints.
//!
//! A list is self-delimiting: `varint(count)`, then the first id, then successive deltas.

use loupe_core::{Error, Result};

pub fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn get_varint(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let byte = *buf
            .get(*pos)
            .ok_or_else(|| Error::Corrupt("posting list truncated".into()))?;
        *pos += 1;
        let bits = u32::from(byte & 0x7f);
        if shift == 28 && bits > 0x0f {
            return Err(Error::Corrupt("varint overflows u32".into()));
        }
        value |= bits << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Error::Corrupt("varint too long".into()))
}

/// Append a list. `ids` must be strictly increasing.
pub fn encode_list(out: &mut Vec<u8>, ids: &[u32]) {
    debug_assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids not increasing");
    put_varint(
        out,
        u32::try_from(ids.len()).expect("posting list too long"),
    );
    let mut prev = 0u32;
    for (i, &id) in ids.iter().enumerate() {
        put_varint(out, if i == 0 { id } else { id - prev });
        prev = id;
    }
}

/// Decode the list starting at `offset`.
pub fn decode_list(buf: &[u8], offset: usize) -> Result<Vec<u32>> {
    let mut pos = offset;
    let count = get_varint(buf, &mut pos)? as usize;
    // Every entry takes at least one byte, so a larger count is corruption, not a reason
    // to allocate.
    if count > buf.len().saturating_sub(pos) {
        return Err(Error::Corrupt("posting count exceeds data".into()));
    }
    let mut ids = Vec::with_capacity(count);
    let mut prev = 0u32;
    for i in 0..count {
        let v = get_varint(buf, &mut pos)?;
        let id = if i == 0 {
            v
        } else {
            if v == 0 {
                return Err(Error::Corrupt("posting list not increasing".into()));
            }
            prev.checked_add(v)
                .ok_or_else(|| Error::Corrupt("posting id overflow".into()))?
        };
        ids.push(id);
        prev = id;
    }
    Ok(ids)
}

/// Sorted-set intersection. Gallops through the longer list when the sizes are lopsided.
pub fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(small.len());
    if small.len().saturating_mul(16) < large.len() {
        let mut lo = 0usize;
        for &x in small {
            lo += large[lo..].partition_point(|&y| y < x);
            if lo == large.len() {
                break;
            }
            if large[lo] == x {
                out.push(x);
            }
        }
    } else {
        let (mut i, mut j) = (0, 0);
        while i < small.len() && j < large.len() {
            match small[i].cmp(&large[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    out.push(small[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
    }
    out
}

/// Sorted-set union.
pub fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    fn sorted_set() -> impl Strategy<Value = Vec<u32>> {
        proptest::collection::btree_set(0u32..5000, 0..300).prop_map(|s| s.into_iter().collect())
    }

    proptest! {
        #[test]
        fn list_roundtrip(ids in proptest::collection::btree_set(any::<u32>(), 0..200)) {
            let ids: Vec<u32> = ids.into_iter().collect();
            let mut buf = vec![0xAA, 0xBB]; // unrelated prefix: offsets must work
            let off = buf.len();
            encode_list(&mut buf, &ids);
            encode_list(&mut buf, &[7]); // and a following list must not interfere
            prop_assert_eq!(decode_list(&buf, off).unwrap(), ids);
        }

        #[test]
        fn intersect_matches_naive(a in sorted_set(), b in sorted_set()) {
            let sa: BTreeSet<_> = a.iter().copied().collect();
            let sb: BTreeSet<_> = b.iter().copied().collect();
            let want: Vec<u32> = sa.intersection(&sb).copied().collect();
            prop_assert_eq!(intersect(&a, &b), want);
        }

        #[test]
        fn intersect_gallop_path_matches_naive(
            small in proptest::collection::btree_set(0u32..100_000, 0..5),
            large in proptest::collection::btree_set(0u32..100_000, 500..1500),
        ) {
            let (small, large): (Vec<u32>, Vec<u32>) =
                (small.into_iter().collect(), large.into_iter().collect());
            let ss: BTreeSet<_> = small.iter().copied().collect();
            let sl: BTreeSet<_> = large.iter().copied().collect();
            let want: Vec<u32> = ss.intersection(&sl).copied().collect();
            prop_assert_eq!(intersect(&small, &large), want.clone());
            prop_assert_eq!(intersect(&large, &small), want);
        }

        #[test]
        fn union_matches_naive(a in sorted_set(), b in sorted_set()) {
            let want: Vec<u32> = a.iter().chain(&b).copied().collect::<BTreeSet<_>>().into_iter().collect();
            prop_assert_eq!(union(&a, &b), want);
        }

        #[test]
        fn decoder_never_panics_on_garbage(bytes in proptest::collection::vec(any::<u8>(), 0..64), off in 0usize..80) {
            let _ = decode_list(&bytes, off);
        }
    }

    #[test]
    fn varint_boundaries() {
        for v in [0, 1, 127, 128, 16383, 16384, u32::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            let mut pos = 0;
            assert_eq!(get_varint(&buf, &mut pos).unwrap(), v);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn rejects_overflowing_varint() {
        let mut pos = 0;
        assert!(get_varint(&[0xff, 0xff, 0xff, 0xff, 0x7f], &mut pos).is_err());
    }
}
