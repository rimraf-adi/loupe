//! Tiny length-checked binary codec plus a checksum trailer.

use loupe_core::{Error, Result};

/// Little-endian encoder.
#[derive(Default)]
pub struct Enc(pub Vec<u8>);

impl Enc {
    pub fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn raw(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }
    /// String with a u16 length prefix. Callers must have checked the length.
    pub fn str16(&mut self, s: &str) {
        self.u16(u16::try_from(s.len()).expect("str16 too long"));
        self.raw(s.as_bytes());
    }
    pub fn str32(&mut self, s: &str) {
        self.u32(u32::try_from(s.len()).expect("str32 too long"));
        self.raw(s.as_bytes());
    }
}

/// Bounds-checked little-endian decoder; every shortfall is `Error::Corrupt`.
pub struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
    what: &'static str,
}

impl<'a> Dec<'a> {
    pub fn new(buf: &'a [u8], what: &'static str) -> Self {
        Dec { buf, pos: 0, what }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or_else(|| Error::Corrupt(format!("{}: truncated", self.what)))?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn array32(&mut self) -> Result<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    pub fn magic(&mut self, expected: &[u8; 4]) -> Result<()> {
        if self.take(4)? != expected {
            return Err(Error::Corrupt(format!("{}: bad magic", self.what)));
        }
        Ok(())
    }
    pub fn str16(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        self.string(n)
    }
    pub fn str32(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        self.string(n)
    }
    fn string(&mut self, n: usize) -> Result<String> {
        let bytes = self.take(n)?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| Error::Corrupt(format!("{}: invalid utf-8", self.what)))
    }

    pub fn finish(self) -> Result<()> {
        if self.pos != self.buf.len() {
            return Err(Error::Corrupt(format!("{}: trailing bytes", self.what)));
        }
        Ok(())
    }
}

/// Append a blake3 checksum of `body`.
pub fn seal(mut body: Vec<u8>) -> Vec<u8> {
    let sum = blake3::hash(&body);
    body.extend_from_slice(sum.as_bytes());
    body
}

/// Verify and strip the trailer written by [`seal`].
pub fn unseal<'a>(sealed: &'a [u8], what: &'static str) -> Result<&'a [u8]> {
    let Some(split) = sealed.len().checked_sub(32) else {
        return Err(Error::Corrupt(format!("{what}: too short")));
    };
    let (body, sum) = sealed.split_at(split);
    if blake3::hash(body).as_bytes() != sum {
        return Err(Error::Corrupt(format!("{what}: checksum mismatch")));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut e = Enc::default();
        e.u16(7);
        e.u32(70_000);
        e.u64(1 << 40);
        e.i64(-5);
        e.str16("héllo");
        let sealed = seal(e.0);
        let body = unseal(&sealed, "t").unwrap();
        let mut d = Dec::new(body, "t");
        assert_eq!(d.u16().unwrap(), 7);
        assert_eq!(d.u32().unwrap(), 70_000);
        assert_eq!(d.u64().unwrap(), 1 << 40);
        assert_eq!(d.i64().unwrap(), -5);
        assert_eq!(d.str16().unwrap(), "héllo");
        d.finish().unwrap();
    }

    #[test]
    fn detects_flipped_bit_and_truncation() {
        let sealed = seal(vec![1, 2, 3, 4]);
        let mut bad = sealed.clone();
        bad[1] ^= 1;
        assert!(unseal(&bad, "t").is_err());
        assert!(unseal(&sealed[..sealed.len() - 1], "t").is_err());
        assert!(unseal(&[], "t").is_err());
    }

    #[test]
    fn decoder_never_reads_past_end() {
        let mut d = Dec::new(&[1, 2], "t");
        assert!(d.u32().is_err());
        let mut d = Dec::new(&[0xff, 0xff, 0xff, 0xff], "t");
        assert!(d.str32().is_err());
    }
}
