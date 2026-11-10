//! Pack index in git's own `.idx` v2 format, so stored packs stay readable by
//! stock git tooling.
//!
//! Lookups run directly on the encoded bytes: opening an index costs one
//! validation pass and no per-object allocation.

use bytes::Bytes;
use sha1::{Digest, Sha1};

use crate::{corrupt, Oid, Result};

const MAGIC: [u8; 4] = [0xff, b't', b'O', b'c'];
const FANOUT_AT: usize = 8;
const OIDS_AT: usize = FANOUT_AT + 256 * 4;

pub struct IdxEntry {
    pub oid: Oid,
    pub offset: u64,
    pub crc32: u32,
}

#[derive(Clone)]
pub struct PackIndex {
    raw: Bytes,
    count: usize,
}

impl PackIndex {
    /// Builds an index. An object stored twice in the pack keeps both entries,
    /// so that every pack entry can be found by its offset.
    pub fn new(mut entries: Vec<IdxEntry>, pack_checksum: Oid) -> PackIndex {
        entries.sort_by_key(|e| (e.oid, e.offset));
        let count = entries.len();
        let mut out = Vec::with_capacity(OIDS_AT + count * 28 + 40);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&2u32.to_be_bytes());

        let mut fanout = [0u32; 256];
        for entry in &entries {
            fanout[usize::from(entry.oid.as_bytes()[0])] += 1;
        }
        let mut total = 0u32;
        for bucket in fanout {
            total += bucket;
            out.extend_from_slice(&total.to_be_bytes());
        }

        for entry in &entries {
            out.extend_from_slice(entry.oid.as_bytes());
        }
        for entry in &entries {
            out.extend_from_slice(&entry.crc32.to_be_bytes());
        }
        let mut large = Vec::new();
        for entry in &entries {
            let word = match u32::try_from(entry.offset) {
                Ok(small) if small < 0x8000_0000 => small,
                _ => {
                    large.push(entry.offset);
                    0x8000_0000 | (large.len() - 1) as u32
                }
            };
            out.extend_from_slice(&word.to_be_bytes());
        }
        for offset in large {
            out.extend_from_slice(&offset.to_be_bytes());
        }

        out.extend_from_slice(pack_checksum.as_bytes());
        let checksum = Sha1::digest(&out);
        out.extend_from_slice(&checksum);
        PackIndex { raw: out.into(), count }
    }

    pub fn decode(raw: Bytes) -> Result<PackIndex> {
        if raw.len() < OIDS_AT + 40 || raw[..4] != MAGIC || raw[4..8] != 2u32.to_be_bytes() {
            return Err(corrupt("not a v2 pack index"));
        }
        let body_len = raw.len() - 20;
        if Sha1::digest(&raw[..body_len]).as_slice() != &raw[body_len..] {
            return Err(corrupt("pack index checksum mismatch"));
        }
        let count = u32::from_be_bytes(raw[OIDS_AT - 4..OIDS_AT].try_into().expect("4 bytes")) as usize;
        let index = PackIndex { raw, count };
        if index.large_at() + 40 > index.raw.len() {
            return Err(corrupt("truncated pack index"));
        }

        // Everything below is trusted by the accessors, so check it once here.
        let mut previous = 0;
        for bucket in 0..256 {
            let end = index.fanout(bucket);
            if end < previous || end > count {
                return Err(corrupt("bad fanout in pack index"));
            }
            previous = end;
        }
        let large_entries = (index.raw.len() - 40 - index.large_at()) / 8;
        for i in 0..count {
            if i > 0 && index.oid_bytes(i - 1) > index.oid_bytes(i) {
                return Err(corrupt("pack index is not sorted"));
            }
            let bucket = usize::from(index.oid_bytes(i)[0]);
            if i >= index.fanout(bucket) || (bucket > 0 && i < index.fanout(bucket - 1)) {
                return Err(corrupt("bad fanout in pack index"));
            }
            let word = index.be32(index.offsets_at() + i * 4);
            if word & 0x8000_0000 != 0 && (word & 0x7fff_ffff) as usize >= large_entries {
                return Err(corrupt("bad large offset in pack index"));
            }
        }
        Ok(index)
    }

    pub fn encoded(&self) -> &Bytes {
        &self.raw
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn pack_checksum(&self) -> Oid {
        let at = self.raw.len() - 40;
        Oid::from_bytes(&self.raw[at..at + 20]).expect("20 bytes")
    }

    fn be32(&self, pos: usize) -> u32 {
        u32::from_be_bytes(self.raw[pos..pos + 4].try_into().expect("4 bytes"))
    }

    /// Number of entries whose first id byte is at most `bucket`.
    fn fanout(&self, bucket: usize) -> usize {
        self.be32(FANOUT_AT + bucket * 4) as usize
    }

    fn offsets_at(&self) -> usize {
        OIDS_AT + self.count * 24
    }

    fn large_at(&self) -> usize {
        OIDS_AT + self.count * 28
    }

    fn oid_bytes(&self, i: usize) -> &[u8] {
        &self.raw[OIDS_AT + i * 20..OIDS_AT + (i + 1) * 20]
    }

    pub fn oid_at(&self, i: usize) -> Oid {
        Oid::from_bytes(self.oid_bytes(i)).expect("20 bytes")
    }

    pub fn offset_at(&self, i: usize) -> u64 {
        let word = self.be32(self.offsets_at() + i * 4);
        if word & 0x8000_0000 == 0 {
            return u64::from(word);
        }
        let at = self.large_at() + (word & 0x7fff_ffff) as usize * 8;
        u64::from_be_bytes(self.raw[at..at + 8].try_into().expect("8 bytes"))
    }

    /// Where `oid` sits in the index's sorted order.
    pub fn position(&self, oid: &Oid) -> Option<usize> {
        let bucket = usize::from(oid.as_bytes()[0]);
        let mut lo = if bucket == 0 { 0 } else { self.fanout(bucket - 1) };
        let mut hi = self.fanout(bucket);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.oid_bytes(mid).cmp(oid.as_bytes().as_slice()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// The pack offset of `oid`.
    pub fn find(&self, oid: &Oid) -> Option<u64> {
        self.position(oid).map(|i| self.offset_at(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let oid = |b: u8| Oid::from_bytes(&[b; 20]).unwrap();
        let entries = vec![
            IdxEntry { oid: oid(9), offset: 12, crc32: 1 },
            IdxEntry { oid: oid(3), offset: 5_000_000_000, crc32: 2 },
            IdxEntry { oid: oid(200), offset: 400, crc32: 3 },
            IdxEntry { oid: oid(0), offset: 77, crc32: 4 },
        ];
        let index = PackIndex::new(entries, oid(7));
        let decoded = PackIndex::decode(index.encoded().clone()).unwrap();
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded.find(&oid(0)), Some(77));
        assert_eq!(decoded.find(&oid(3)), Some(5_000_000_000));
        assert_eq!(decoded.find(&oid(9)), Some(12));
        assert_eq!(decoded.find(&oid(200)), Some(400));
        assert_eq!(decoded.find(&oid(4)), None);
        assert_eq!(decoded.find(&oid(255)), None);
        assert_eq!(decoded.pack_checksum(), oid(7));
        assert_eq!((decoded.oid_at(1), decoded.offset_at(1)), (oid(3), 5_000_000_000));
    }

    #[test]
    fn rejects_corruption() {
        let oid = |b: u8| Oid::from_bytes(&[b; 20]).unwrap();
        let index = PackIndex::new(vec![IdxEntry { oid: oid(1), offset: 12, crc32: 0 }], oid(7));
        let mut raw = index.encoded().to_vec();
        raw[OIDS_AT] ^= 0xff;
        assert!(PackIndex::decode(raw.into()).is_err());
    }
}
