use std::collections::HashMap;

use crate::{corrupt, Result};

fn read_size(delta: &[u8], pos: &mut usize) -> Result<usize> {
    let mut size = 0u64;
    let mut shift = 0;
    loop {
        let b = *delta.get(*pos).ok_or_else(|| corrupt("truncated delta header"))?;
        *pos += 1;
        if shift > 56 {
            return Err(corrupt("delta size overflow"));
        }
        size |= u64::from(b & 0x7f) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return usize::try_from(size).map_err(|_| corrupt("delta size overflow"));
        }
    }
}

/// Applies a git delta to `base`, returning the reconstructed object data.
/// The size of what a delta produces, from the start of the delta alone.
pub fn result_size(delta_start: &[u8]) -> Result<usize> {
    let mut pos = 0;
    read_size(delta_start, &mut pos)?;
    read_size(delta_start, &mut pos)
}

pub fn apply(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0;
    let base_size = read_size(delta, &mut pos)?;
    let result_size = read_size(delta, &mut pos)?;
    if base_size != base.len() {
        return Err(corrupt("delta base size mismatch"));
    }
    // The declared size is untrusted: cap the up-front allocation.
    let mut out = Vec::with_capacity(result_size.min(16 << 20));
    while pos < delta.len() {
        let cmd = delta[pos];
        pos += 1;
        if cmd & 0x80 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for i in 0..4 {
                if cmd & (1 << i) != 0 {
                    let b = *delta.get(pos).ok_or_else(|| corrupt("truncated delta copy"))?;
                    pos += 1;
                    offset |= usize::from(b) << (8 * i);
                }
            }
            for i in 0..3 {
                if cmd & (0x10 << i) != 0 {
                    let b = *delta.get(pos).ok_or_else(|| corrupt("truncated delta copy"))?;
                    pos += 1;
                    size |= usize::from(b) << (8 * i);
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            let end = offset
                .checked_add(size)
                .filter(|end| *end <= base.len())
                .ok_or_else(|| corrupt("delta copy out of range"))?;
            out.extend_from_slice(&base[offset..end]);
        } else if cmd != 0 {
            let end = pos + usize::from(cmd);
            let chunk = delta.get(pos..end).ok_or_else(|| corrupt("truncated delta insert"))?;
            out.extend_from_slice(chunk);
            pos = end;
        } else {
            return Err(corrupt("reserved delta opcode"));
        }
        if out.len() > result_size {
            return Err(corrupt("delta result too large"));
        }
    }
    if out.len() != result_size {
        return Err(corrupt("delta result size mismatch"));
    }
    Ok(out)
}

fn write_size(out: &mut Vec<u8>, mut size: usize) {
    loop {
        let b = (size & 0x7f) as u8;
        size >>= 7;
        if size == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Length of the blocks of the base that are indexed for matching.
const BLOCK: usize = 16;
const MAX_COPY: usize = 0x10000;

fn block_hash(block: &[u8]) -> u64 {
    let a = u64::from_le_bytes(block[..8].try_into().expect("8 bytes"));
    let b = u64::from_le_bytes(block[8..16].try_into().expect("8 bytes"));
    (a.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ b).wrapping_mul(0xff51_afd7_ed55_8ccd)
}

fn write_copy(out: &mut Vec<u8>, mut offset: usize, mut len: usize) {
    while len > 0 {
        let size = len.min(MAX_COPY);
        let at = out.len();
        out.push(0x80);
        for i in 0..4 {
            let byte = (offset >> (8 * i)) as u8;
            if byte != 0 {
                out[at] |= 1 << i;
                out.push(byte);
            }
        }
        // A size of exactly MAX_COPY is encoded as "no size bytes".
        if size != MAX_COPY {
            for i in 0..2 {
                let byte = (size >> (8 * i)) as u8;
                if byte != 0 {
                    out[at] |= 0x10 << i;
                    out.push(byte);
                }
            }
        }
        offset += size;
        len -= size;
    }
}

fn write_insert(out: &mut Vec<u8>, data: &[u8]) {
    for chunk in data.chunks(127) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
}

/// Encodes `target` as a delta against `base`, which must be smaller than 4 GiB.
///
/// Finds runs shared with the base by indexing the base in fixed blocks; it
/// favours speed over the smallest possible output.
pub fn encode(base: &[u8], target: &[u8]) -> Vec<u8> {
    assert!(base.len() <= u32::MAX as usize, "delta base too large");
    let mut out = Vec::new();
    write_size(&mut out, base.len());
    write_size(&mut out, target.len());

    let mut index: HashMap<u64, u32> = HashMap::with_capacity(base.len() / BLOCK);
    for start in (0..base.len().saturating_sub(BLOCK - 1)).step_by(BLOCK) {
        index.entry(block_hash(&base[start..start + BLOCK])).or_insert(start as u32);
    }

    let mut literal_from = 0;
    let mut t = 0;
    while t + BLOCK <= target.len() {
        let candidate = index.get(&block_hash(&target[t..t + BLOCK])).map(|b| *b as usize);
        let Some(mut b) = candidate.filter(|b| base[*b..*b + BLOCK] == target[t..t + BLOCK]) else {
            t += 1;
            continue;
        };
        // Grow the match in both directions.
        let mut start = t;
        while start > literal_from && b > 0 && base[b - 1] == target[start - 1] {
            b -= 1;
            start -= 1;
        }
        let mut len = t - start + BLOCK;
        while b + len < base.len() && start + len < target.len() && base[b + len] == target[start + len] {
            len += 1;
        }
        write_insert(&mut out, &target[literal_from..start]);
        write_copy(&mut out, b, len);
        t = start + len;
        literal_from = t;
    }
    write_insert(&mut out, &target[literal_from..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_and_insert() {
        let base = b"hello world";
        // base size 11, result size 9: copy "hello" (offset 0, len 5), insert " git"
        let delta = [11, 9, 0x90, 5, 4, b' ', b'g', b'i', b't'];
        assert_eq!(apply(base, &delta).unwrap(), b"hello git");
    }

    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn encode_round_trips_and_shrinks_small_edits() {
        let base = pseudo_random(200_000, 1);
        let mut edited = base.clone();
        edited.splice(50_000..50_010, b"an inserted run of bytes".iter().copied());
        edited.truncate(180_000);
        edited.extend_from_slice(b"new tail");

        let delta = encode(&base, &edited);
        assert_eq!(apply(&base, &delta).unwrap(), edited);
        assert!(delta.len() < 200, "delta is {} bytes", delta.len());

        // Unrelated or degenerate inputs must still round-trip.
        let other = pseudo_random(5_000, 2);
        for (b, t) in [(&base[..], &other[..]), (&other[..], &base[..]), (&base[..], &[][..]), (&[][..], &other[..]), (&other[..10], &other[..12])] {
            assert_eq!(apply(b, &encode(b, t)).unwrap(), t);
        }
    }

    #[test]
    fn rejects_out_of_range_copy() {
        let delta = [3, 5, 0x91, 2, 5];
        assert!(apply(b"abc", &delta).is_err());
    }
}
