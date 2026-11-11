//! Packfile reading, writing, and indexing of packs received from clients.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use bytes::{Buf, Bytes};
use flate2::write::ZlibEncoder;
use flate2::{Compression, Decompress, FlushDecompress, Status};
use crate::sha1::Sha1;

use crate::bitmap::PackBitmaps;
use crate::bitset::BitSet;
use crate::graph::CommitGraph;
use crate::idx::{IdxEntry, PackIndex};
use crate::object::{commit_info, object_id, parse_tag, parse_tree, CommitInfo};
use crate::{corrupt, delta, Error, Object, ObjectCache, ObjectKind, ObjectSource, Oid, PackData, Result};

const HEADER_LEN: u64 = 12;
const TRAILER_LEN: u64 = 20;
const OFS_DELTA: u8 = 6;
const REF_DELTA: u8 = 7;

/// Objects larger than this are never used as input for computing a new delta.
const MAX_DELTA_INPUT: usize = 16 << 20;

#[derive(Clone, Copy)]
enum EntryKind {
    Full(ObjectKind),
    /// Delta against the entry starting at this absolute pack offset.
    OfsDelta(u64),
    RefDelta(Oid),
}

struct EntryHeader {
    kind: EntryKind,
    /// Inflated size of the entry's payload.
    size: usize,
}

/// Sequential reader over [`PackData`], pulling one chunk at a time. Can
/// checksum what it reads, so that validating a pack takes a single pass.
struct Cursor<'a> {
    data: &'a dyn PackData,
    chunk: Bytes,
    /// Absolute offset of the next unread byte.
    offset: u64,
    /// When set, fed every byte consumed.
    sha: Option<Sha1>,
    crc: Option<crc32fast::Hasher>,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a dyn PackData, offset: u64) -> Self {
        Cursor { data, chunk: Bytes::new(), offset, sha: None, crc: None }
    }

    /// The unread bytes currently in memory; empty only at end of pack.
    fn fill(&mut self) -> Result<&[u8]> {
        if self.chunk.is_empty() {
            self.chunk = self.data.chunk(self.offset)?;
        }
        Ok(&self.chunk)
    }

    fn consume(&mut self, n: usize) {
        if let Some(sha) = &mut self.sha {
            sha.update(&self.chunk[..n]);
        }
        if let Some(crc) = &mut self.crc {
            crc.update(&self.chunk[..n]);
        }
        self.chunk.advance(n);
        self.offset += n as u64;
    }

    fn byte(&mut self) -> Result<u8> {
        let b = *self.fill()?.first().ok_or_else(|| corrupt("truncated pack"))?;
        self.consume(1);
        Ok(b)
    }

    fn read_exact(&mut self, out: &mut [u8]) -> Result<()> {
        for b in out {
            *b = self.byte()?;
        }
        Ok(())
    }
}

/// Parses an entry header starting at `offset`, where the cursor must be.
/// Leaves the cursor at the entry's payload.
fn read_entry_header(cursor: &mut Cursor<'_>, offset: u64) -> Result<EntryHeader> {
    let mut c = cursor.byte()?;
    let type_bits = (c >> 4) & 7;
    let mut size = u64::from(c & 0x0f);
    let mut shift = 4;
    while c & 0x80 != 0 {
        c = cursor.byte()?;
        if shift > 57 {
            return Err(corrupt("pack entry size overflow"));
        }
        size |= u64::from(c & 0x7f) << shift;
        shift += 7;
    }
    let size = usize::try_from(size).map_err(|_| corrupt("pack entry size overflow"))?;

    let kind = match type_bits {
        OFS_DELTA => {
            let mut c = cursor.byte()?;
            let mut distance = u64::from(c & 0x7f);
            while c & 0x80 != 0 {
                c = cursor.byte()?;
                if distance >= 1 << 48 {
                    return Err(corrupt("delta base offset overflow"));
                }
                distance = ((distance + 1) << 7) | u64::from(c & 0x7f);
            }
            let base = Some(distance)
                .filter(|d| *d != 0)
                .and_then(|d| offset.checked_sub(d))
                .ok_or_else(|| corrupt("delta base offset out of range"))?;
            EntryKind::OfsDelta(base)
        }
        REF_DELTA => {
            let mut id = [0u8; 20];
            cursor.read_exact(&mut id)?;
            EntryKind::RefDelta(Oid::from_bytes(&id).expect("20 bytes"))
        }
        t => EntryKind::Full(ObjectKind::from_pack_type(t).ok_or_else(|| corrupt("unknown pack entry type"))?),
    };
    Ok(EntryHeader { kind, size })
}

fn entry_header_at(data: &dyn PackData, offset: u64) -> Result<(EntryHeader, Cursor<'_>)> {
    let mut cursor = Cursor::new(data, offset);
    let header = read_entry_header(&mut cursor, offset)?;
    Ok((header, cursor))
}

/// Inflates one zlib stream at the cursor, which must produce exactly
/// `expected` bytes, handing the output to `sink` piece by piece. Leaves the
/// cursor just past the stream.
fn inflate_to(cursor: &mut Cursor<'_>, expected: usize, sink: &mut dyn FnMut(&[u8])) -> Result<()> {
    let mut z = Decompress::new(true);
    let mut buf = vec![0u8; expected.clamp(1, 64 << 10)];
    loop {
        let (consumed, produced) = (z.total_in(), z.total_out());
        let input = cursor.fill()?;
        let status = z.decompress(input, &mut buf, FlushDecompress::None).map_err(|e| corrupt(format!("zlib: {e}")))?;
        let used = (z.total_in() - consumed) as usize;
        let made = (z.total_out() - produced) as usize;
        cursor.consume(used);
        sink(&buf[..made]);
        if z.total_out() > expected as u64 {
            return Err(corrupt("pack entry larger than declared"));
        }
        if status == Status::StreamEnd {
            break;
        }
        if used == 0 && made == 0 {
            return Err(corrupt("truncated zlib stream"));
        }
    }
    if z.total_out() != expected as u64 {
        return Err(corrupt("pack entry size mismatch"));
    }
    Ok(())
}

fn inflate(cursor: &mut Cursor<'_>, expected: usize) -> Result<Vec<u8>> {
    // `expected` comes from untrusted input: cap the up-front allocation.
    let mut out = Vec::with_capacity(expected.min(16 << 20));
    inflate_to(cursor, expected, &mut |piece| out.extend_from_slice(piece))?;
    Ok(out)
}

fn write_entry_header(out: &mut Vec<u8>, type_bits: u8, mut size: usize) {
    let mut c = (type_bits << 4) | (size & 0x0f) as u8;
    size >>= 4;
    while size != 0 {
        out.push(c | 0x80);
        c = (size & 0x7f) as u8;
        size >>= 7;
    }
    out.push(c);
}

fn deflate_into(out: &mut Vec<u8>, data: &[u8]) {
    let mut z = ZlibEncoder::new(out, Compression::default());
    z.write_all(data).expect("writing to a Vec cannot fail");
    z.finish().expect("writing to a Vec cannot fail");
}

fn write_entry(out: &mut Vec<u8>, kind: ObjectKind, data: &[u8]) {
    write_entry_header(out, kind.pack_type(), data.len());
    deflate_into(out, data);
}

fn pack_header(count: u32) -> [u8; 12] {
    let mut header = [0u8; 12];
    header[..4].copy_from_slice(b"PACK");
    header[4..8].copy_from_slice(&2u32.to_be_bytes());
    header[8..].copy_from_slice(&count.to_be_bytes());
    header
}

/// What [`PackWriter::finish`] hands back.
pub struct Finished<W> {
    pub out: W,
    pub checksum: Oid,
    /// Total bytes written, trailer included.
    pub size: u64,
    /// One entry per object, if the writer was asked to record them.
    pub entries: Vec<IdxEntry>,
}

/// Streams a pack to `out`.
pub struct PackWriter<W: Write> {
    out: W,
    hash: Sha1,
    offset: u64,
    declared: u32,
    written: u32,
    /// Index entries of what has been written, when recording.
    entries: Option<Vec<IdxEntry>>,
    entry_start: u64,
    entry_crc: crc32fast::Hasher,
}

impl<W: Write> PackWriter<W> {
    pub fn new(out: W, count: u32) -> Result<Self> {
        let mut writer = PackWriter {
            out,
            hash: Sha1::new(),
            offset: 0,
            declared: count,
            written: 0,
            entries: None,
            entry_start: 0,
            entry_crc: crc32fast::Hasher::new(),
        };
        writer.put(&pack_header(count))?;
        Ok(writer)
    }

    /// Like [`PackWriter::new`], and also collects what is needed to build the pack's index.
    pub fn recording(out: W, count: u32) -> Result<Self> {
        let mut writer = Self::new(out, count)?;
        writer.entries = Some(Vec::with_capacity(count as usize));
        Ok(writer)
    }

    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.hash.update(bytes);
        if self.entries.is_some() {
            self.entry_crc.update(bytes);
        }
        self.offset += bytes.len() as u64;
        self.out.write_all(bytes).map_err(|e| Error::Io(e.to_string()))
    }

    fn begin_entry(&mut self) {
        self.entry_start = self.offset;
        self.entry_crc = crc32fast::Hasher::new();
    }

    fn end_entry(&mut self, oid: &Oid) {
        self.written += 1;
        if let Some(entries) = &mut self.entries {
            let crc32 = std::mem::take(&mut self.entry_crc).finalize();
            entries.push(IdxEntry { oid: *oid, offset: self.entry_start, crc32 });
        }
    }

    /// Adds an object by compressing it.
    pub fn add(&mut self, kind: ObjectKind, data: &[u8]) -> Result<()> {
        let oid = if self.entries.is_some() { object_id(kind, data) } else { Oid::ZERO };
        self.add_whole(&oid, kind, data)
    }

    fn add_whole(&mut self, oid: &Oid, kind: ObjectKind, data: &[u8]) -> Result<()> {
        let mut entry = Vec::new();
        write_entry(&mut entry, kind, data);
        self.begin_entry();
        self.put(&entry)?;
        self.end_entry(oid);
        Ok(())
    }

    /// Adds the object `oid`, stored at `offset` of `pack`, copying its bytes
    /// without recompressing wherever the result stays valid: whole objects
    /// always, and deltas when `in_pack` says their base is part of the pack
    /// being written. Otherwise the object is rebuilt, with `packs` resolving
    /// bases stored elsewhere.
    pub fn add_entry(
        &mut self,
        pack: &PackFile,
        offset: u64,
        oid: &Oid,
        in_pack: &dyn Fn(&Oid) -> bool,
        packs: &PackSet,
    ) -> Result<()> {
        let (header, cursor) = entry_header_at(&*pack.data, offset)?;
        let payload_at = cursor.offset;
        match header.kind {
            EntryKind::Full(_) => {
                self.begin_entry();
                self.copy(pack, offset, pack.entry_end(offset)?)?;
            }
            EntryKind::RefDelta(base) if in_pack(&base) => {
                self.begin_entry();
                self.copy(pack, offset, pack.entry_end(offset)?)?;
            }
            EntryKind::OfsDelta(base_offset) if in_pack(&pack.oid_at_offset(base_offset)?) => {
                // Entry order changes in the new pack, so name the base by id instead of by distance.
                let mut entry = Vec::new();
                write_entry_header(&mut entry, REF_DELTA, header.size);
                entry.extend_from_slice(pack.oid_at_offset(base_offset)?.as_bytes());
                self.begin_entry();
                self.put(&entry)?;
                self.copy(pack, payload_at, pack.entry_end(offset)?)?;
            }
            EntryKind::RefDelta(_) | EntryKind::OfsDelta(_) => {
                let object = pack.read_at(offset, &|base| packs.get(base))?;
                return self.add_whole(oid, object.kind, &object.data);
            }
        }
        self.end_entry(oid);
        Ok(())
    }

    /// Adds `oid` from `packs` in the cheapest form available.
    ///
    /// `delta_base` suggests an object likely to resemble it (the previous
    /// version of the same file). With `thin`, meaning the receiver is known
    /// to have that object, and unless a stored delta can be reused as is, a
    /// new delta against it is computed.
    ///
    /// New deltas are only ever made against objects outside the pack being
    /// written. A delta against an object inside it could close a cycle with
    /// a reused stored delta pointing the other way (git stores old versions
    /// as deltas against new ones), which no receiver could resolve.
    pub fn add_best(
        &mut self,
        packs: &PackSet,
        oid: &Oid,
        delta_base: Option<&Oid>,
        in_pack: &dyn Fn(&Oid) -> bool,
        thin: bool,
    ) -> Result<()> {
        let (pack, offset) = packs.locate(oid).ok_or(Error::Missing(*oid))?;
        if let Some(base) = delta_base.filter(|base| thin && !in_pack(base)) {
            let (header, _) = entry_header_at(&*pack.data, offset)?;
            let stored_base = match header.kind {
                EntryKind::Full(_) => None,
                EntryKind::RefDelta(stored) => Some(stored),
                EntryKind::OfsDelta(stored) => Some(pack.oid_at_offset(stored)?),
            };
            if stored_base == Some(*base) {
                // Already stored as a delta against exactly what the receiver has.
                return self.add_entry(pack, offset, oid, &|o| o == base || in_pack(o), packs);
            }
            let stored_delta_travels = stored_base.is_some_and(|stored| in_pack(&stored));
            if !stored_delta_travels && self.add_new_delta(packs, oid, base)? {
                return Ok(());
            }
        }
        self.add_entry(pack, offset, oid, in_pack, packs)
    }

    /// Adds `oid` as a newly computed delta against `base`. Returns false,
    /// writing nothing, when that is not worthwhile.
    ///
    /// The caller must make sure the receiver can resolve the delta: `base`
    /// has to be available to it, and must not itself depend on `oid`.
    pub fn add_new_delta(&mut self, packs: &PackSet, oid: &Oid, base: &Oid) -> Result<bool> {
        let Some(base_object) = packs.get(base)? else { return Ok(false) };
        let target = packs.get(oid)?.ok_or(Error::Missing(*oid))?;
        if base_object.kind != target.kind
            || base_object.data.is_empty()
            || base_object.data.len() > MAX_DELTA_INPUT
            || target.data.len() > MAX_DELTA_INPUT
        {
            return Ok(false);
        }
        let delta = delta::encode(&base_object.data, &target.data);
        if delta.len() >= target.data.len() / 2 {
            return Ok(false);
        }
        let mut entry = Vec::new();
        write_entry_header(&mut entry, REF_DELTA, delta.len());
        entry.extend_from_slice(base.as_bytes());
        deflate_into(&mut entry, &delta);
        self.begin_entry();
        self.put(&entry)?;
        self.end_entry(oid);
        Ok(true)
    }

    fn copy(&mut self, pack: &PackFile, from: u64, to: u64) -> Result<()> {
        let mut cursor = Cursor::new(&*pack.data, from);
        while cursor.offset < to {
            let remaining = (to - cursor.offset).min(usize::MAX as u64) as usize;
            if cursor.fill()?.is_empty() {
                return Err(corrupt("pack shorter than its index claims"));
            }
            let take = cursor.chunk.len().min(remaining);
            let piece = cursor.chunk.slice(..take);
            self.put(&piece)?;
            cursor.consume(take);
        }
        Ok(())
    }

    /// Writes the trailer.
    pub fn finish(mut self) -> Result<Finished<W>> {
        if self.declared != self.written {
            return Err(corrupt("pack object count mismatch"));
        }
        let checksum = self.hash.clone().finalize();
        self.out.write_all(&checksum).map_err(|e| Error::Io(e.to_string()))?;
        Ok(Finished {
            out: self.out,
            checksum: Oid::from_bytes(&checksum).expect("sha1 is 20 bytes"),
            size: self.offset + TRAILER_LEN,
            entries: self.entries.unwrap_or_default(),
        })
    }
}

static NEXT_PACK_ID: AtomicU64 = AtomicU64::new(0);

/// An immutable pack: an index held in memory and data read on demand.
pub struct PackFile {
    /// Process-unique, for cache keys.
    id: u64,
    data: Arc<dyn PackData>,
    index: PackIndex,
    cache: Option<Arc<dyn ObjectCache>>,
    graph: Option<CommitGraph>,
    bitmaps: Option<PackBitmaps>,
    /// Entry offsets in pack order with their index positions, built on first use.
    by_offset: OnceLock<Vec<(u64, u32)>>,
}

impl PackFile {
    pub fn new(data: Arc<dyn PackData>, index: PackIndex, cache: Option<Arc<dyn ObjectCache>>) -> Result<PackFile> {
        if data.size() < HEADER_LEN + TRAILER_LEN {
            return Err(corrupt("not a packfile"));
        }
        Ok(PackFile {
            id: NEXT_PACK_ID.fetch_add(1, Ordering::Relaxed),
            data,
            index,
            cache,
            graph: None,
            bitmaps: None,
            by_offset: OnceLock::new(),
        })
    }

    /// Attaches the pack's optional sidecar files. Bitmaps that do not match
    /// the index are dropped rather than trusted.
    pub fn with_sidecars(mut self, graph: Option<CommitGraph>, bitmaps: Option<PackBitmaps>) -> PackFile {
        self.graph = graph;
        self.bitmaps = bitmaps.filter(|b| b.objects() == self.index.len());
        self
    }

    pub fn index(&self) -> &PackIndex {
        &self.index
    }

    pub fn size(&self) -> u64 {
        self.data.size()
    }

    pub fn graph(&self) -> Option<&CommitGraph> {
        self.graph.as_ref()
    }

    pub fn bitmaps(&self) -> Option<&PackBitmaps> {
        self.bitmaps.as_ref()
    }

    /// Approximate memory held by the index and sidecars, for cache accounting.
    pub fn memory(&self) -> usize {
        // The in-pack-order table is built lazily; count it up front.
        self.index.encoded().len()
            + self.index.len() * 12
            + self.graph.as_ref().map_or(0, |g| g.encoded().len())
            + self.bitmaps.as_ref().map_or(0, PackBitmaps::memory)
    }

    /// Every entry as (offset, position in the index), in the order stored.
    pub fn entries_in_pack_order(&self) -> &[(u64, u32)] {
        self.by_offset.get_or_init(|| {
            let mut entries: Vec<(u64, u32)> = (0..self.index.len()).map(|i| (self.index.offset_at(i), i as u32)).collect();
            entries.sort_unstable();
            entries
        })
    }

    fn position_of(&self, offset: u64) -> Result<usize> {
        self.entries_in_pack_order()
            .binary_search_by_key(&offset, |e| e.0)
            .map_err(|_| corrupt("offset is not a pack entry"))
    }

    /// Where the entry starting at `offset` ends.
    fn entry_end(&self, offset: u64) -> Result<u64> {
        let next = self.position_of(offset)? + 1;
        Ok(self.entries_in_pack_order().get(next).map_or(self.data.size() - TRAILER_LEN, |e| e.0))
    }

    fn oid_at_offset(&self, offset: u64) -> Result<Oid> {
        let (_, index_position) = self.entries_in_pack_order()[self.position_of(offset)?];
        Ok(self.index.oid_at(index_position as usize))
    }

    /// The kind of the object stored at `offset`, found from entry headers
    /// alone (following delta bases within this pack).
    pub fn kind_at(&self, offset: u64) -> Result<ObjectKind> {
        let mut offset = offset;
        loop {
            match entry_header_at(&*self.data, offset)?.0.kind {
                EntryKind::Full(kind) => return Ok(kind),
                EntryKind::OfsDelta(base) => offset = base,
                EntryKind::RefDelta(base) => offset = self.index.find(&base).ok_or(Error::Missing(base))?,
            }
        }
    }

    /// The size of the object stored at `offset`, without reading it: a
    /// whole object's entry says, and a delta starts with the size of what
    /// it produces.
    pub fn size_at(&self, offset: u64) -> Result<u64> {
        let (header, mut cursor) = entry_header_at(&*self.data, offset)?;
        if let EntryKind::Full(_) = header.kind {
            return Ok(header.size as u64);
        }
        // Two sizes of at most ten bytes each open a delta.
        let mut start = [0u8; 20];
        let mut z = Decompress::new(true);
        let mut made = 0;
        while made < start.len().min(header.size) {
            let before = (z.total_in(), z.total_out());
            let input = cursor.fill()?;
            if input.is_empty() {
                return Err(corrupt("truncated delta"));
            }
            let status = z.decompress(input, &mut start[made..], FlushDecompress::None).map_err(|e| corrupt(format!("zlib: {e}")))?;
            cursor.consume((z.total_in() - before.0) as usize);
            made += (z.total_out() - before.1) as usize;
            if status == Status::StreamEnd {
                break;
            }
        }
        Ok(delta::result_size(&start[..made])? as u64)
    }

    fn cached(&self, offset: u64) -> Option<Object> {
        self.cache.as_ref()?.get(self.id, offset)
    }

    /// Reads the object stored at `offset`. `external` resolves ref-delta
    /// bases that live outside this pack.
    fn read_at(&self, offset: u64, external: &dyn Fn(&Oid) -> Result<Option<Object>>) -> Result<Object> {
        let mut offset = offset;
        // Deltas still to apply, outermost first, with the offset each one reconstructs.
        let mut deltas: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut object = loop {
            if let Some(object) = self.cached(offset) {
                break object;
            }
            let (header, mut cursor) = entry_header_at(&*self.data, offset)?;
            let data = inflate(&mut cursor, header.size)?;
            match header.kind {
                EntryKind::Full(kind) => {
                    let object = Object { kind, data: data.into() };
                    if let Some(cache) = &self.cache {
                        cache.put(self.id, offset, &object);
                    }
                    break object;
                }
                EntryKind::OfsDelta(base) => {
                    deltas.push((offset, data));
                    offset = base;
                }
                EntryKind::RefDelta(base) => {
                    deltas.push((offset, data));
                    match self.index.find(&base) {
                        Some(base_offset) => offset = base_offset,
                        None => break external(&base)?.ok_or(Error::Missing(base))?,
                    }
                }
            }
        };
        for (offset, delta) in deltas.iter().rev() {
            object.data = delta::apply(&object.data, delta)?.into();
            if let Some(cache) = &self.cache {
                cache.put(self.id, *offset, &object);
            }
        }
        Ok(object)
    }
}

/// The packs that make up one object pool, searched newest first.
///
/// Compaction keeps the number of packs logarithmic in the pool's size, so a
/// linear scan over them is acceptable.
#[derive(Clone, Default)]
pub struct PackSet {
    packs: Vec<Arc<PackFile>>,
}

impl PackSet {
    /// `packs` is ordered oldest first, as in a pool's manifest.
    pub fn new(mut packs: Vec<Arc<PackFile>>) -> PackSet {
        packs.reverse();
        PackSet { packs }
    }

    /// Returns a set that also contains `pack`, without modifying `self`.
    pub fn with_pack(&self, pack: Arc<PackFile>) -> PackSet {
        let mut packs = Vec::with_capacity(self.packs.len() + 1);
        packs.push(pack);
        packs.extend(self.packs.iter().cloned());
        PackSet { packs }
    }

    /// The packs, newest first.
    pub fn packs(&self) -> &[Arc<PackFile>] {
        &self.packs
    }

    /// A set holding the packs of both, with those of `self` searched first.
    pub fn merged_with(&self, other: &PackSet) -> PackSet {
        PackSet { packs: self.packs.iter().chain(&other.packs).cloned().collect() }
    }

    pub(crate) fn locate(&self, oid: &Oid) -> Option<(&PackFile, u64)> {
        self.packs.iter().find_map(|pack| Some((&**pack, pack.index.find(oid)?)))
    }

    /// The object `oid` is stored as a delta against, if it is stored as one.
    pub fn stored_base(&self, oid: &Oid) -> Result<Option<Oid>> {
        let Some((pack, offset)) = self.locate(oid) else { return Ok(None) };
        Ok(match entry_header_at(&*pack.data, offset)?.0.kind {
            EntryKind::Full(_) => None,
            EntryKind::RefDelta(base) => Some(base),
            EntryKind::OfsDelta(base) => Some(pack.oid_at_offset(base)?),
        })
    }

    /// Whether some pack has a bitmap entry for `commit`. The entry may still
    /// turn out to be unusable; see [`PackSet::closure`].
    pub fn has_bitmap(&self, commit: &Oid) -> bool {
        self.packs.iter().filter_map(|pack| pack.bitmaps.as_ref()).any(|bitmaps| bitmaps.has(commit))
    }

    /// Everything reachable from `commit` according to bitmaps, as bits per
    /// pack (keyed by position in [`PackSet::packs`]). `None` if `commit` has
    /// no entry or the entries it builds on are no longer all there.
    pub fn closure(&self, commit: &Oid) -> Result<Option<HashMap<usize, BitSet>>> {
        let mut out = HashMap::new();
        Ok(self.closure_into(commit, &mut out, 0)?.then_some(out))
    }

    fn closure_into(&self, commit: &Oid, out: &mut HashMap<usize, BitSet>, depth: usize) -> Result<bool> {
        // Layers only ever build on other packs, so chains are short.
        if depth > self.packs.len() {
            return Ok(false);
        }
        for (number, pack) in self.packs.iter().enumerate() {
            let Some((bits, bases)) = pack.bitmaps.as_ref().map(|b| b.get(commit)).transpose()?.flatten() else {
                continue;
            };
            match out.get_mut(&number) {
                Some(have) => have.union_with(&bits),
                None => {
                    out.insert(number, bits);
                }
            }
            for base in bases {
                if !self.closure_into(base, out, depth + 1)? {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        Ok(false)
    }
}

impl ObjectSource for PackSet {
    fn get(&self, oid: &Oid) -> Result<Option<Object>> {
        match self.locate(oid) {
            Some((pack, offset)) => pack.read_at(offset, &|base| self.get(base)).map(Some),
            None => Ok(None),
        }
    }

    fn contains(&self, oid: &Oid) -> bool {
        self.locate(oid).is_some()
    }

    fn size(&self, oid: &Oid) -> Result<Option<u64>> {
        self.locate(oid).map(|(pack, offset)| pack.size_at(offset)).transpose()
    }

    fn commit(&self, oid: &Oid) -> Result<Option<CommitInfo>> {
        if let Some(info) = self.packs.iter().filter_map(|pack| pack.graph.as_ref()).find_map(|graph| graph.get(oid)) {
            return Ok(Some(info));
        }
        match self.get(oid)? {
            Some(object) if object.kind == ObjectKind::Commit => commit_info(&object.data).map(Some),
            _ => Ok(None),
        }
    }
}

/// The outcome of validating a received pack.
pub struct IndexedPack {
    pub index: PackIndex,
    /// The commits in the pack.
    pub graph: CommitGraph,
    /// Objects that the pack's objects point at and the pack does not hold.
    /// With these in place already, everything in the pack is connected:
    /// nothing has to be read a second time to check.
    pub outside: Vec<Oid>,
    /// End of the entries in the pack as received.
    body_end: u64,
    /// Present when the pack was thin and had to be completed.
    completion: Option<Completion>,
}

struct Completion {
    header: Bytes,
    /// The missing delta bases as pack entries, followed by the new trailer.
    tail: Bytes,
}

impl IndexedPack {
    /// The pack to store, given the bytes that were indexed. A thin pack is
    /// presented with its missing delta bases appended (and the header and
    /// trailer adjusted to match), making it self-contained; nothing is copied.
    pub fn data(&self, received: Arc<dyn PackData>) -> Arc<dyn PackData> {
        match &self.completion {
            None => received,
            Some(completion) => Arc::new(CompletedPack {
                received,
                body_end: self.body_end,
                header: completion.header.clone(),
                tail: completion.tail.clone(),
            }),
        }
    }
}

struct CompletedPack {
    received: Arc<dyn PackData>,
    body_end: u64,
    header: Bytes,
    tail: Bytes,
}

impl PackData for CompletedPack {
    fn size(&self) -> u64 {
        self.body_end + self.tail.len() as u64
    }

    fn chunk(&self, offset: u64) -> Result<Bytes> {
        if offset < HEADER_LEN {
            Ok(self.header.slice(offset as usize..))
        } else if offset < self.body_end {
            let mut chunk = self.received.chunk(offset)?;
            chunk.truncate((self.body_end - offset).min(usize::MAX as u64) as usize);
            Ok(chunk)
        } else {
            let start = ((offset - self.body_end).min(self.tail.len() as u64)) as usize;
            Ok(self.tail.slice(start..))
        }
    }
}

/// What is remembered about each entry while indexing. Kept small: a large
/// import has tens of millions of these.
struct RawEntry {
    offset: u64,
    /// Inflated size of the payload.
    size: u64,
    crc32: u32,
    /// Distance from `offset` to the payload.
    header_len: u8,
    /// The object's kind, if the entry holds a whole object.
    whole: Option<ObjectKind>,
    oid: Option<Oid>,
}

/// What resolving deltas finds out, gathered apart by each thread at it.
#[derive(Default)]
struct Noted {
    /// The ids worked out for entries, by entry number.
    ids: Vec<(usize, Oid)>,
    /// Every commit seen, for the pack's commit-graph.
    commits: Vec<(Oid, CommitInfo)>,
    /// Every object some object of the pack points at.
    references: HashSet<Oid>,
}

impl Noted {
    /// Notes what an object points at: a commit's tree and parents, a tag's
    /// target, a tree's entries.
    fn note(&mut self, oid: Oid, kind: ObjectKind, data: &[u8]) -> Result<()> {
        match kind {
            ObjectKind::Commit => {
                let info = commit_info(data)?;
                self.references.insert(info.tree);
                self.references.extend(&info.parents);
                self.commits.push((oid, info));
            }
            ObjectKind::Tag => {
                self.references.insert(parse_tag(data)?);
            }
            ObjectKind::Tree => {
                // A submodule's commit lives in another repository.
                self.references.extend(parse_tree(data)?.iter().filter(|entry| !entry.is_gitlink()).map(|entry| entry.oid));
            }
            ObjectKind::Blob => {}
        }
        Ok(())
    }

    fn merge(&mut self, other: Noted) {
        self.ids.extend(other.ids);
        self.commits.extend(other.commits);
        self.references.extend(other.references);
    }
}

struct Resolver<'a> {
    data: &'a dyn PackData,
    raws: Vec<RawEntry>,
    /// Ofs-deltas waiting on the entry at each index.
    ofs_children: HashMap<usize, Vec<usize>>,
    /// Ref-deltas waiting on an object id. Whoever resolves that object
    /// takes them, whichever thread it is on.
    ref_waiting: Mutex<HashMap<Oid, Vec<usize>>>,
    /// How many ids are waited on. Packs that git pushes use ofs-deltas, so
    /// this is mostly zero, and then nobody has to take the lock to see.
    ref_left: AtomicUsize,
}

impl Resolver<'_> {
    fn payload(&self, index: usize) -> Result<Vec<u8>> {
        let raw = &self.raws[index];
        let size = usize::try_from(raw.size).map_err(|_| corrupt("pack entry size overflow"))?;
        inflate(&mut Cursor::new(self.data, raw.offset + u64::from(raw.header_len)), size)
    }

    fn wait_on(&mut self, base: Oid, index: usize) {
        let waiting = self.ref_waiting.get_mut().expect("no panics while held");
        waiting.entry(base).or_default().push(index);
        *self.ref_left.get_mut() = waiting.len();
    }

    fn take_waiting(&self, oid: &Oid) -> Vec<usize> {
        if self.ref_left.load(Ordering::Relaxed) == 0 {
            return Vec::new();
        }
        let mut waiting = self.ref_waiting.lock().expect("no panics while held");
        let taken = waiting.remove(oid).unwrap_or_default();
        self.ref_left.store(waiting.len(), Ordering::Relaxed);
        taken
    }

    fn has_dependants(&self, index: usize, oid: &Oid) -> bool {
        self.ofs_children.contains_key(&index) || self.ref_waiting.lock().expect("no panics while held").contains_key(oid)
    }

    /// Works out the id of every delta that transitively depends on `object`
    /// (the entry at `index`, if it is part of the pack). Deltas form trees,
    /// so two calls for different objects never meet.
    fn resolve_from(&self, index: Option<usize>, object: Object, noted: &mut Noted) -> Result<()> {
        let mut stack = vec![(index, object)];
        while let Some((index, object)) = stack.pop() {
            let oid = object.oid();
            let mut children = Vec::new();
            if let Some(i) = index {
                // A whole object of the pack was noted when it was first read.
                if self.raws[i].whole.is_none() {
                    noted.note(oid, object.kind, &object.data)?;
                }
                noted.ids.push((i, oid));
                children = self.ofs_children.get(&i).cloned().unwrap_or_default();
            }
            children.extend(self.take_waiting(&oid));
            for child in children {
                let delta = self.payload(child)?;
                let data = delta::apply(&object.data, &delta)?;
                stack.push((Some(child), Object { kind: object.kind, data: data.into() }));
            }
        }
        Ok(())
    }

    /// Resolves everything that hangs off the whole objects of the pack, on
    /// several threads when there is enough of it: each takes the next whole
    /// object and the deltas built on it, a family at a time.
    fn resolve_all(&self) -> Result<Noted> {
        let roots: Vec<usize> =
            (0..self.raws.len()).filter(|i| matches!((self.raws[*i].whole, self.raws[*i].oid), (Some(_), Some(oid)) if self.has_dependants(*i, &oid))).collect();
        let threads = match roots.len() {
            0..RESOLVE_PARALLEL_FROM => 1,
            _ => std::thread::available_parallelism().map_or(1, usize::from).min(MAX_RESOLVE_THREADS),
        };
        let (next, failed) = (AtomicUsize::new(0), AtomicBool::new(false));
        let work = || -> Result<Noted> {
            let mut noted = Noted::default();
            while let Some(&i) = roots.get(next.fetch_add(1, Ordering::Relaxed)).filter(|_| !failed.load(Ordering::Relaxed)) {
                let kind = self.raws[i].whole.expect("roots are whole objects");
                let resolved = self.payload(i).and_then(|data| self.resolve_from(Some(i), Object { kind, data: data.into() }, &mut noted));
                if let Err(e) = resolved {
                    failed.store(true, Ordering::Relaxed);
                    return Err(e);
                }
            }
            Ok(noted)
        };
        let parts: Vec<Result<Noted>> = match threads {
            1 => vec![work()],
            _ => std::thread::scope(|scope| {
                let workers: Vec<_> = (0..threads).map(|_| scope.spawn(work)).collect();
                workers.into_iter().map(|worker| worker.join().expect("resolving does not panic")).collect()
            }),
        };
        let mut all = Noted::default();
        for part in parts {
            all.merge(part?);
        }
        Ok(all)
    }
}

/// Below this many delta families, threads cost more than they save.
const RESOLVE_PARALLEL_FROM: usize = 256;
/// Each thread holds a delta family in memory.
const MAX_RESOLVE_THREADS: usize = 8;

/// Validates a pack received from a client and builds its index.
///
/// `existing` supplies delta bases the client omitted (a "thin" pack).
///
/// The pack is read through [`PackData`], so it can sit in a spool file. Whole
/// objects are checksummed as they stream past; only delta bases and the
/// objects built from them are held in memory, one delta family at a time.
pub fn index_pack(data: &dyn PackData, existing: &dyn ObjectSource) -> Result<IndexedPack> {
    if data.size() < HEADER_LEN + TRAILER_LEN {
        return Err(corrupt("not a packfile"));
    }
    let body_end = data.size() - TRAILER_LEN;

    // Pass 1, sequential: entry boundaries, checksums, and the ids of whole objects.
    let mut cursor = Cursor::new(data, 0);
    cursor.sha = Some(Sha1::new());
    let mut header = [0u8; HEADER_LEN as usize];
    cursor.read_exact(&mut header)?;
    if &header[..4] != b"PACK" {
        return Err(corrupt("not a packfile"));
    }
    let version = u32::from_be_bytes(header[4..8].try_into().expect("4 bytes"));
    if version != 2 && version != 3 {
        return Err(corrupt(format!("unsupported pack version {version}")));
    }
    let count = u32::from_be_bytes(header[8..12].try_into().expect("4 bytes")) as usize;

    let mut resolver = Resolver {
        data,
        raws: Vec::with_capacity(count.min(1 << 20)),
        ofs_children: HashMap::new(),
        ref_waiting: Mutex::new(HashMap::new()),
        ref_left: AtomicUsize::new(0),
    };
    let mut noted = Noted::default();
    for i in 0..count {
        let offset = cursor.offset;
        cursor.crc = Some(crc32fast::Hasher::new());
        let entry = read_entry_header(&mut cursor, offset)?;
        let payload_at = cursor.offset;
        let oid = match entry.kind {
            EntryKind::Full(kind) => {
                let mut hash = Sha1::new();
                hash.update(format!("{} {}\0", kind.name(), entry.size));
                // Everything but a blob points at other objects, and is
                // kept for a moment to see which; blobs are only hashed.
                let mut text = Vec::new();
                inflate_to(&mut cursor, entry.size, &mut |piece| {
                    hash.update(piece);
                    if kind != ObjectKind::Blob {
                        text.extend_from_slice(piece);
                    }
                })?;
                let oid = Oid::from_bytes(&hash.finalize()).expect("sha1 is 20 bytes");
                noted.note(oid, kind, &text)?;
                Some(oid)
            }
            EntryKind::OfsDelta(base) => {
                // Entries are in offset order, so the base can be found by binary search.
                let base = resolver
                    .raws
                    .binary_search_by_key(&base, |raw| raw.offset)
                    .map_err(|_| corrupt("delta base offset is not an entry"))?;
                resolver.ofs_children.entry(base).or_default().push(i);
                inflate_to(&mut cursor, entry.size, &mut |_| {})?;
                None
            }
            EntryKind::RefDelta(base) => {
                resolver.wait_on(base, i);
                inflate_to(&mut cursor, entry.size, &mut |_| {})?;
                None
            }
        };
        if cursor.offset > body_end {
            return Err(corrupt("pack entry runs into the trailer"));
        }
        let crc32 = cursor.crc.take().expect("set above").finalize();
        let whole = match entry.kind {
            EntryKind::Full(kind) => Some(kind),
            EntryKind::OfsDelta(_) | EntryKind::RefDelta(_) => None,
        };
        let header_len = u8::try_from(payload_at - offset).map_err(|_| corrupt("oversized pack entry header"))?;
        resolver.raws.push(RawEntry { offset, size: entry.size as u64, crc32, header_len, whole, oid });
    }
    if cursor.offset != body_end {
        return Err(corrupt("trailing data after last pack entry"));
    }
    let computed = cursor.sha.take().expect("set above").finalize();
    let mut trailer = [0u8; TRAILER_LEN as usize];
    cursor.read_exact(&mut trailer)?;
    if computed.as_slice() != trailer {
        return Err(corrupt("pack checksum mismatch"));
    }

    // Pass 2: resolve deltas outward from every whole object something depends on.
    noted.merge(resolver.resolve_all()?);

    // Whatever is still waiting has its base outside the pack.
    let mut thin_bases = Vec::new();
    loop {
        let waited_on = resolver.ref_waiting.lock().expect("no panics while held").keys().next().copied();
        let Some(base) = waited_on else { break };
        let object = existing.get(&base)?.ok_or(Error::Missing(base))?;
        thin_bases.push((base, object.clone()));
        resolver.resolve_from(None, object, &mut noted)?;
    }
    for (index, oid) in std::mem::take(&mut noted.ids) {
        resolver.raws[index].oid = Some(oid);
    }

    let mut entries = Vec::with_capacity(count + thin_bases.len());
    for raw in &resolver.raws {
        let oid = raw.oid.ok_or_else(|| corrupt("unresolvable delta in pack"))?;
        entries.push(IdxEntry { oid, offset: raw.offset, crc32: raw.crc32 });
    }

    let graph = CommitGraph::build(std::mem::take(&mut noted.commits));
    let own: HashSet<Oid> = entries.iter().map(|entry| entry.oid).collect();
    let mut outside: Vec<Oid> = noted.references.iter().filter(|oid| !own.contains(oid)).copied().collect();
    outside.sort();
    if thin_bases.is_empty() {
        let checksum = Oid::from_bytes(&trailer).expect("20 bytes");
        return Ok(IndexedPack { index: PackIndex::new(entries, checksum), graph, outside, body_end, completion: None });
    }

    // Appending keeps every existing entry offset (and thus every ofs-delta) valid.
    let total = u32::try_from(count + thin_bases.len()).map_err(|_| corrupt("too many objects"))?;
    let header = pack_header(total);
    let mut tail = Vec::new();
    for (oid, object) in &thin_bases {
        let start = tail.len();
        write_entry(&mut tail, object.kind, &object.data);
        entries.push(IdxEntry { oid: *oid, offset: body_end + start as u64, crc32: crc32fast::hash(&tail[start..]) });
    }
    // The header changed, so the checksum covers different bytes from the start.
    let mut hash = Sha1::new();
    hash.update(header);
    let mut cursor = Cursor::new(data, HEADER_LEN);
    while cursor.offset < body_end {
        let remaining = (body_end - cursor.offset).min(usize::MAX as u64) as usize;
        let take = cursor.fill()?.len().min(remaining);
        hash.update(&cursor.chunk[..take]);
        cursor.consume(take);
    }
    hash.update(&tail);
    let checksum = hash.finalize();
    tail.extend_from_slice(&checksum);

    let checksum = Oid::from_bytes(&checksum).expect("sha1 is 20 bytes");
    let completion = Completion { header: Bytes::copy_from_slice(&header), tail: tail.into() };
    Ok(IndexedPack { index: PackIndex::new(entries, checksum), graph, outside, body_end, completion: Some(completion) })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn blob(data: &[u8]) -> Object {
        Object { kind: ObjectKind::Blob, data: Bytes::copy_from_slice(data) }
    }

    pub(crate) fn pack_of(objects: &[Object]) -> Bytes {
        let mut w = PackWriter::new(Vec::new(), objects.len() as u32).unwrap();
        for o in objects {
            w.add(o.kind, &o.data).unwrap();
        }
        w.finish().unwrap().out.into()
    }

    /// Indexes `pack` and opens the result the way the server would store it.
    pub(crate) fn open(pack: &Bytes, existing: &PackSet) -> Arc<PackFile> {
        let indexed = index_pack(pack, existing).unwrap();
        let data = indexed.data(Arc::new(pack.clone()));
        Arc::new(PackFile::new(data, indexed.index, None).unwrap())
    }

    fn set_of(pack: &Bytes, existing: &PackSet) -> PackSet {
        PackSet::new(vec![open(pack, existing)])
    }

    /// A pack holding one ref-delta that turns `base` into `base + suffix`.
    fn thin_pack(base: &Object, suffix: &[u8]) -> Bytes {
        // Sizes are written as single bytes below.
        assert!(base.data.len() + suffix.len() < 128);
        let n = base.data.len() as u8;
        let delta = [&[n, n + suffix.len() as u8, 0x90, n, suffix.len() as u8][..], suffix].concat();
        let mut out = pack_header(1).to_vec();
        write_entry_header(&mut out, REF_DELTA, delta.len());
        out.extend_from_slice(base.oid().as_bytes());
        deflate_into(&mut out, &delta);
        let checksum = Sha1::digest(&out);
        out.extend_from_slice(&checksum);
        out.into()
    }

    /// Serves a pack a few bytes at a time, like a block cache with tiny blocks.
    struct Trickle(Bytes);

    impl PackData for Trickle {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }

        fn chunk(&self, offset: u64) -> Result<Bytes> {
            let start = (offset as usize).min(self.0.len());
            Ok(self.0.slice(start..(start + 3).min(self.0.len())))
        }
    }

    fn first_entry_is_delta(pack: &Bytes) -> bool {
        matches!(entry_header_at(pack, HEADER_LEN).unwrap().0.kind, EntryKind::RefDelta(_))
    }

    #[test]
    fn write_then_index_round_trip() {
        let objects = [blob(b"hello"), blob(b""), Object { kind: ObjectKind::Tree, data: Bytes::new() }];
        let set = set_of(&pack_of(&objects), &PackSet::default());
        for o in &objects {
            let got = set.get(&o.oid()).unwrap().unwrap();
            assert_eq!((got.kind, &got.data), (o.kind, &o.data));
        }
        assert!(!set.contains(&blob(b"other").oid()));
    }

    #[test]
    fn reads_and_indexes_across_chunk_boundaries() {
        let big = blob(&(0..5000u32).flat_map(|i| i.to_le_bytes()).collect::<Vec<u8>>());
        let pack = pack_of(&[blob(b"small"), big.clone()]);
        // Indexing itself must cope with data arriving in tiny pieces.
        let indexed = index_pack(&Trickle(pack.clone()), &PackSet::default()).unwrap();
        let file = PackFile::new(Arc::new(Trickle(pack)), indexed.index, None).unwrap();
        let set = PackSet::new(vec![Arc::new(file)]);
        assert_eq!(set.get(&big.oid()).unwrap().unwrap().data, big.data);

        // Copying stored entries must cope with the same chunking.
        let mut w = PackWriter::new(Vec::new(), 1).unwrap();
        w.add_best(&set, &big.oid(), None, &|_| true, false).unwrap();
        let copy = set_of(&w.finish().unwrap().out.into(), &PackSet::default());
        assert_eq!(copy.get(&big.oid()).unwrap().unwrap().data, big.data);
    }

    #[test]
    fn thin_pack_is_completed() {
        let base = blob(b"hello world");
        let existing = set_of(&pack_of(std::slice::from_ref(&base)), &PackSet::default());
        let thin = thin_pack(&base, b"!!");

        assert!(matches!(index_pack(&thin, &PackSet::default()), Err(Error::Missing(_))));

        let indexed = index_pack(&thin, &existing).unwrap();
        assert_eq!(indexed.index.len(), 2);
        let completed = indexed.data(Arc::new(thin.clone()));

        // The completed pack is a valid pack in its own right: reading it back
        // as plain bytes must index cleanly with nothing else available.
        let mut bytes = Vec::new();
        let mut cursor = Cursor::new(&*completed, 0);
        while !cursor.fill().unwrap().is_empty() {
            bytes.extend_from_slice(&cursor.chunk);
            let n = cursor.chunk.len();
            cursor.consume(n);
        }
        assert_eq!(bytes.len() as u64, completed.size());
        let alone = set_of(&bytes.into(), &PackSet::default());
        let got = alone.get(&blob(b"hello world!!").oid()).unwrap().unwrap();
        assert_eq!(&got.data[..], b"hello world!!");
        assert_eq!(alone.packs()[0].index().pack_checksum(), indexed.index.pack_checksum());
    }

    #[test]
    fn stored_deltas_are_reused_only_with_their_base() {
        let base = blob(&[b'a'; 100]);
        let derived = blob(&[&[b'a'; 100][..], b"!!"].concat());
        let existing = set_of(&pack_of(std::slice::from_ref(&base)), &PackSet::default());
        let stored = existing.with_pack(open(&thin_pack(&base, b"!!"), &existing));

        let build = |oids: &[Oid]| -> Bytes {
            let mut w = PackWriter::new(Vec::new(), oids.len() as u32).unwrap();
            for oid in oids {
                w.add_best(&stored, oid, None, &|o| oids.contains(o), false).unwrap();
            }
            w.finish().unwrap().out.into()
        };

        // With its base alongside, the delta travels as a delta and the pack stands alone.
        let both = build(&[derived.oid(), base.oid()]);
        assert!(first_entry_is_delta(&both));
        let reread = set_of(&both, &PackSet::default());
        assert_eq!(reread.get(&derived.oid()).unwrap().unwrap().data, derived.data);
        assert_eq!(reread.get(&base.oid()).unwrap().unwrap().data, base.data);

        // Without it, the object must be sent whole.
        let alone = build(&[derived.oid()]);
        assert!(!first_entry_is_delta(&alone));
        let reread = set_of(&alone, &PackSet::default());
        assert_eq!(reread.get(&derived.oid()).unwrap().unwrap().data, derived.data);
    }

    #[test]
    fn new_deltas_are_computed_against_a_suggested_base() {
        let old: Vec<u8> = (0..4000u32).flat_map(|i| format!("line {i}\n").into_bytes()).collect();
        let new = [&old[..], b"one more line\n"].concat();
        let (old, new) = (blob(&old), blob(&new));
        // Both stored whole: there is no delta to reuse.
        let stored = set_of(&pack_of(&[old.clone(), new.clone()]), &PackSet::default());

        let build = |thin: bool| -> Bytes {
            let mut w = PackWriter::new(Vec::new(), 1).unwrap();
            w.add_best(&stored, &new.oid(), Some(&old.oid()), &|_| false, thin).unwrap();
            w.finish().unwrap().out.into()
        };

        // The receiver has the old version: send only the difference.
        let thin = build(true);
        assert!(first_entry_is_delta(&thin));
        assert!(thin.len() < 200, "thin pack is {} bytes", thin.len());
        let received = set_of(&thin, &stored);
        assert_eq!(received.get(&new.oid()).unwrap().unwrap().data, new.data);

        // The receiver may not have it: the object must travel whole.
        assert!(!first_entry_is_delta(&build(false)));

        // The old version is in the same pack: no new delta, since the old
        // version might itself be sent as a stored delta against the new one.
        let mut w = PackWriter::new(Vec::new(), 1).unwrap();
        w.add_best(&stored, &new.oid(), Some(&old.oid()), &|_| true, true).unwrap();
        assert!(!first_entry_is_delta(&w.finish().unwrap().out.into()));
    }

    #[test]
    fn recording_writer_produces_a_matching_index() {
        let objects = [blob(b"one"), blob(b"two"), blob(b"three")];
        let mut w = PackWriter::recording(Vec::new(), 3).unwrap();
        for o in &objects {
            w.add(o.kind, &o.data).unwrap();
        }
        let finished = w.finish().unwrap();
        assert_eq!(finished.size, finished.out.len() as u64);
        let written = PackIndex::new(finished.entries, finished.checksum);
        let reindexed = index_pack(&Bytes::from(finished.out), &PackSet::default()).unwrap().index;
        assert_eq!(written.encoded(), reindexed.encoded());
    }

    #[test]
    fn indexing_collects_commits() {
        let tree = Object { kind: ObjectKind::Tree, data: Bytes::new() };
        let text = format!("tree {}\nauthor A <a@example.com> 5 +0000\ncommitter A <a@example.com> 7 +0000\n\nmsg\n", tree.oid());
        let commit = Object { kind: ObjectKind::Commit, data: text.into_bytes().into() };
        let indexed = index_pack(&pack_of(&[commit.clone(), tree.clone(), blob(b"x")]), &PackSet::default()).unwrap();
        assert_eq!(indexed.graph.len(), 1);
        let info = indexed.graph.get(&commit.oid()).unwrap();
        assert_eq!((info.time, info.tree, info.parents.len()), (7, tree.oid(), 0));
    }

    #[test]
    fn indexing_tells_what_the_pack_points_at_outside_itself() {
        let (kept, absent) = (blob(b"kept"), blob(b"absent"));
        let entry = |name: &str, object: &Object| [format!("100644 {name}\0").as_bytes(), object.oid().as_bytes()].concat();
        let tree = Object { kind: ObjectKind::Tree, data: [entry("absent", &absent), entry("kept", &kept)].concat().into() };
        let parent = Oid::from_hex("1111111111111111111111111111111111111111").unwrap();
        let text = format!("tree {}\nparent {parent}\nauthor A <a@example.com> 5 +0000\ncommitter A <a@example.com> 7 +0000\n\nmsg\n", tree.oid());
        let commit = Object { kind: ObjectKind::Commit, data: text.into_bytes().into() };
        // Everything the commit needs is in the pack, but for its parent.
        let whole = index_pack(&pack_of(&[commit.clone(), tree.clone(), kept.clone(), absent.clone()]), &PackSet::default()).unwrap();
        assert_eq!(whole.outside, [parent]);
        // Leave a file out, and it shows.
        let mut expected = vec![parent, absent.oid()];
        expected.sort();
        assert_eq!(index_pack(&pack_of(&[commit, tree, kept]), &PackSet::default()).unwrap().outside, expected);
    }

    #[test]
    fn many_delta_families_are_resolved_side_by_side() {
        // Enough families for several threads, each a whole object and a delta on it.
        let families: Vec<(Object, Object)> = (0..600u32)
            .map(|n| {
                let old: Vec<u8> = (0..200).flat_map(|i| format!("family {n} line {i}\n").into_bytes()).collect();
                let new = [&old[..], b"and one more\n"].concat();
                (blob(&old), blob(&new))
            })
            .collect();
        let all: Vec<Object> = families.iter().flat_map(|(old, new)| [old.clone(), new.clone()]).collect();
        let stored = set_of(&pack_of(&all), &PackSet::default());
        let mut w = PackWriter::recording(Vec::new(), 1200).unwrap();
        for (old, new) in &families {
            w.add(old.kind, &old.data).unwrap();
            assert!(w.add_new_delta(&stored, &new.oid(), &old.oid()).unwrap());
        }
        let finished = w.finish().unwrap();
        let written = PackIndex::new(finished.entries, finished.checksum);
        let indexed = index_pack(&Bytes::from(finished.out), &PackSet::default()).unwrap();
        assert_eq!(indexed.index.len(), 1200);
        assert_eq!(written.encoded(), indexed.index.encoded());
    }

    #[test]
    fn sizes_are_read_without_the_objects() {
        let base = blob(&[b'a'; 100]);
        let existing = set_of(&pack_of(std::slice::from_ref(&base)), &PackSet::default());
        // Stored as a delta: its size is in the delta, not in the entry.
        let stored = existing.with_pack(open(&thin_pack(&base, b"!!"), &existing));
        let derived = blob(&[&[b'a'; 100][..], b"!!"].concat());
        assert_eq!(stored.size(&base.oid()).unwrap(), Some(100));
        assert_eq!(stored.size(&derived.oid()).unwrap(), Some(102));
        assert_eq!(stored.size(&blob(b"elsewhere").oid()).unwrap(), None);
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut pack = pack_of(&[blob(b"x")]).to_vec();
        let last = pack.len() - 1;
        pack[last] ^= 1;
        assert!(index_pack(&Bytes::from(pack), &PackSet::default()).is_err());
    }
}
