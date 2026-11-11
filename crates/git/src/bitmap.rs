//! Reachability bitmaps: for selected commits, the set of objects reachable
//! from them, as one bit per object of a pack (in index order).
//!
//! A pack rarely holds all of a commit's history, so an entry may be layered:
//! its bits cover the objects in this pack, and it names "base" commits whose
//! own entries, in other packs, cover the rest. An entry is usable only if all
//! of its bases still resolve.
//!
//! A sidecar of one pack, in a format of our own.

use std::collections::HashMap;
use std::io::{Read, Write};

use bytes::Bytes;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use crate::sha1::Sha1;

use crate::bitset::BitSet;
use crate::{corrupt, ObjectKind, Oid, Result};

const MAGIC: &[u8; 8] = b"SBMP\0\0\0\x02";
const KINDS: [ObjectKind; 4] = [ObjectKind::Commit, ObjectKind::Tree, ObjectKind::Blob, ObjectKind::Tag];

fn compress(bits: &BitSet) -> Vec<u8> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
    z.write_all(&bits.to_bytes()).expect("writing to a Vec cannot fail");
    z.finish().expect("writing to a Vec cannot fail")
}

fn decompress(len: usize, data: &[u8]) -> Result<BitSet> {
    let expected = len.div_ceil(64) * 8;
    let mut bytes = Vec::with_capacity(expected);
    ZlibDecoder::new(data)
        .take(expected as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| corrupt(format!("bitmap: {e}")))?;
    BitSet::from_bytes(len, &bytes).ok_or_else(|| corrupt("bitmap has the wrong size"))
}

/// Bitmaps are kept compressed and expanded on use: they are consulted a few
/// times per fetch, and a large pack's bitmap is megabytes when expanded.
#[derive(Clone)]
pub struct PackBitmaps {
    objects: usize,
    kinds: [Bytes; 4],
    commits: HashMap<Oid, Entry>,
}

#[derive(Clone)]
struct Entry {
    bits: Bytes,
    bases: Vec<Oid>,
}

/// One commit's reachable objects, as given to [`PackBitmaps::build`].
pub struct Reachable {
    pub commit: Oid,
    /// Reachable objects that are in this pack.
    pub bits: BitSet,
    /// Commits with entries elsewhere that account for everything else.
    pub bases: Vec<Oid>,
}

impl PackBitmaps {
    /// `kinds` holds, per object kind, which positions are of that kind.
    pub fn build(objects: usize, kinds: [BitSet; 4], commits: Vec<Reachable>) -> PackBitmaps {
        PackBitmaps {
            objects,
            kinds: kinds.map(|bits| compress(&bits).into()),
            commits: commits
                .into_iter()
                .map(|r| (r.commit, Entry { bits: compress(&r.bits).into(), bases: r.bases }))
                .collect(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.objects as u32).to_be_bytes());
        for kind in &self.kinds {
            out.extend_from_slice(&(kind.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
        }
        out.extend_from_slice(&(self.commits.len() as u32).to_be_bytes());
        let mut commits: Vec<_> = self.commits.iter().collect();
        commits.sort_by_key(|(oid, _)| **oid);
        for (oid, entry) in commits {
            out.extend_from_slice(oid.as_bytes());
            out.extend_from_slice(&(entry.bases.len() as u32).to_be_bytes());
            for base in &entry.bases {
                out.extend_from_slice(base.as_bytes());
            }
            out.extend_from_slice(&(entry.bits.len() as u32).to_be_bytes());
            out.extend_from_slice(&entry.bits);
        }
        let checksum = Sha1::digest(&out);
        out.extend_from_slice(&checksum);
        out
    }

    pub fn decode(raw: Bytes) -> Result<PackBitmaps> {
        if raw.len() < 32 || &raw[..8] != MAGIC {
            return Err(corrupt("not a bitmap file"));
        }
        let body = raw.slice(..raw.len() - 20);
        if Sha1::digest(&body).as_slice() != &raw[body.len()..] {
            return Err(corrupt("bitmap checksum mismatch"));
        }
        let mut pos = 8;
        let mut take = |n: usize| -> Result<Bytes> {
            let end = pos + n;
            if end > body.len() {
                return Err(corrupt("truncated bitmap file"));
            }
            let slice = body.slice(pos..end);
            pos = end;
            Ok(slice)
        };
        let be32 = |b: Bytes| u32::from_be_bytes(b[..].try_into().expect("4 bytes")) as usize;

        let objects = be32(take(4)?);
        let mut kinds = Vec::with_capacity(4);
        for _ in 0..4 {
            let len = be32(take(4)?);
            kinds.push(take(len)?);
        }
        let count = be32(take(4)?);
        let mut commits = HashMap::with_capacity(count.min(1 << 16));
        for _ in 0..count {
            let oid = Oid::from_bytes(&take(20)?).expect("20 bytes");
            let mut bases = Vec::new();
            for _ in 0..be32(take(4)?) {
                bases.push(Oid::from_bytes(&take(20)?).expect("20 bytes"));
            }
            let len = be32(take(4)?);
            commits.insert(oid, Entry { bits: take(len)?, bases });
        }
        let kinds: [Bytes; 4] = kinds.try_into().expect("four kinds");
        Ok(PackBitmaps { objects, kinds, commits })
    }

    /// Number of objects in the pack these bitmaps describe.
    pub fn objects(&self) -> usize {
        self.objects
    }

    pub fn has(&self, commit: &Oid) -> bool {
        self.commits.contains_key(commit)
    }

    /// The objects of this pack reachable from `commit`, and the commits
    /// whose entries cover the rest, if `commit` has an entry.
    pub fn get(&self, commit: &Oid) -> Result<Option<(BitSet, &[Oid])>> {
        match self.commits.get(commit) {
            Some(entry) => Ok(Some((decompress(self.objects, &entry.bits)?, &entry.bases))),
            None => Ok(None),
        }
    }

    /// Approximate memory held, for cache accounting.
    pub fn memory(&self) -> usize {
        self.kinds.iter().map(Bytes::len).sum::<usize>()
            + self.commits.values().map(|e| e.bits.len() + e.bases.len() * 20 + 64).sum::<usize>()
    }

    /// Which objects of the pack are of `kind`.
    pub fn of_kind(&self, kind: ObjectKind) -> Result<BitSet> {
        let i = KINDS.iter().position(|k| *k == kind).expect("all kinds listed");
        decompress(self.objects, &self.kinds[i])
    }
}

/// Position of each kind in the array [`PackBitmaps::build`] expects.
pub fn kind_slot(kind: ObjectKind) -> usize {
    KINDS.iter().position(|k| *k == kind).expect("all kinds listed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let oid = |b: u8| Oid::from_bytes(&[b; 20]).unwrap();
        let bits = |ones: &[usize]| {
            let mut b = BitSet::new(200);
            ones.iter().for_each(|i| b.set(*i));
            b
        };
        let kinds = [bits(&[0, 1]), bits(&[2, 199]), bits(&[3]), bits(&[])];
        let built = PackBitmaps::build(
            200,
            kinds,
            vec![
                Reachable { commit: oid(1), bits: bits(&[0, 2, 3]), bases: vec![] },
                Reachable { commit: oid(2), bits: bits(&[0, 1, 2, 3, 199]), bases: vec![oid(8), oid(9)] },
            ],
        );
        let decoded = PackBitmaps::decode(built.encode().into()).unwrap();
        assert_eq!(decoded.objects(), 200);
        assert_eq!(decoded.get(&oid(1)).unwrap(), Some((bits(&[0, 2, 3]), &[][..])));
        let (layered, bases) = decoded.get(&oid(2)).unwrap().unwrap();
        assert_eq!((layered.count(), bases), (5, &[oid(8), oid(9)][..]));
        assert!(decoded.get(&oid(3)).unwrap().is_none());
        assert_eq!(decoded.of_kind(ObjectKind::Tree).unwrap(), bits(&[2, 199]));
        assert_eq!(decoded.of_kind(ObjectKind::Tag).unwrap().count(), 0);

        let mut damaged = built.encode();
        damaged[10] ^= 1;
        assert!(PackBitmaps::decode(damaged.into()).is_err());
    }
}
