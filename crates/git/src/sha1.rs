//! SHA-1, which names every git object: receiving a push hashes everything
//! in it, so this is where much of a push's time goes.
//!
//! The `sha1` crate uses the processor's SHA instructions on x86 but not on
//! ARM, where it computes in software at about a third of the speed. Here
//! the ARM instructions are used when the processor has them, and the
//! crate's block function everywhere else.

/// A SHA-1 computation in progress.
#[derive(Clone)]
pub struct Sha1 {
    state: [u32; 5],
    /// Input not yet hashed: less than one block.
    pending: [u8; 64],
    pending_len: usize,
    /// Bytes fed so far.
    length: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Sha1::new()
    }
}

impl Sha1 {
    pub fn new() -> Sha1 {
        Sha1 { state: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0], pending: [0; 64], pending_len: 0, length: 0 }
    }

    pub fn digest(data: impl AsRef<[u8]>) -> [u8; 20] {
        let mut hash = Sha1::new();
        hash.update(data);
        hash.finalize()
    }

    pub fn update(&mut self, data: impl AsRef<[u8]>) {
        let mut data = data.as_ref();
        self.length += data.len() as u64;
        if self.pending_len > 0 {
            let take = data.len().min(64 - self.pending_len);
            self.pending[self.pending_len..self.pending_len + take].copy_from_slice(&data[..take]);
            self.pending_len += take;
            data = &data[take..];
            if self.pending_len < 64 {
                return;
            }
            compress(&mut self.state, &self.pending);
            self.pending_len = 0;
        }
        let whole = data.len() - data.len() % 64;
        if whole > 0 {
            compress(&mut self.state, &data[..whole]);
        }
        let rest = &data[whole..];
        self.pending[..rest.len()].copy_from_slice(rest);
        self.pending_len = rest.len();
    }

    pub fn finalize(mut self) -> [u8; 20] {
        let bits = self.length * 8;
        // A one bit, zeros up to eight bytes short of a block, then the length.
        let mut padding = [0u8; 72];
        padding[0] = 0x80;
        let zeros = (119 - self.pending_len) % 64;
        padding[1 + zeros..9 + zeros].copy_from_slice(&bits.to_be_bytes());
        self.update(&padding[..9 + zeros]);
        debug_assert_eq!(self.pending_len, 0);
        let mut out = [0u8; 20];
        for (bytes, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            bytes.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// Hashes whole blocks: `blocks` is a multiple of 64 bytes long.
fn compress(state: &mut [u32; 5], blocks: &[u8]) {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("sha2") {
        // Safe: the processor has the instructions, as just checked.
        unsafe { arm::compress(state, blocks) };
        return;
    }
    for block in blocks.as_chunks::<64>().0 {
        sha1::compress(state, std::slice::from_ref(sha1::digest::generic_array::GenericArray::from_slice(block)));
    }
}

#[cfg(target_arch = "aarch64")]
mod arm {
    use core::arch::aarch64::*;

    const K: [u32; 4] = [0x5A827999, 0x6ED9EBA1, 0x8F1BBCDC, 0xCA62C1D6];

    /// SHA-1 by the instructions made for it. Each step does four of the
    /// eighty rounds, and all but the last four also work out the part of
    /// the message schedule that is needed four steps on.
    #[target_feature(enable = "sha2")]
    pub(super) unsafe fn compress(state: &mut [u32; 5], blocks: &[u8]) {
        let mut abcd = vld1q_u32(state.as_ptr());
        let mut e = state[4];
        let (k0, k1, k2, k3) = (vdupq_n_u32(K[0]), vdupq_n_u32(K[1]), vdupq_n_u32(K[2]), vdupq_n_u32(K[3]));
        macro_rules! step {
            ($round:ident, $k:ident, $w:ident) => {
                let input = vaddq_u32($w, $k);
                let e_next = vsha1h_u32(vgetq_lane_u32::<0>(abcd));
                abcd = $round(abcd, e, input);
                e = e_next;
            };
            ($round:ident, $k:ident, $w:ident, $w1:ident, $w2:ident, $w3:ident) => {
                step!($round, $k, $w);
                $w = vsha1su1q_u32(vsha1su0q_u32($w, $w1, $w2), $w3);
            };
        }
        for block in blocks.as_chunks::<64>().0 {
            let (abcd_before, e_before) = (abcd, e);
            let load = |at: usize| vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block.as_ptr().add(at))));
            let (mut m0, mut m1, mut m2, mut m3) = (load(0), load(16), load(32), load(48));
            step!(vsha1cq_u32, k0, m0, m1, m2, m3);
            step!(vsha1cq_u32, k0, m1, m2, m3, m0);
            step!(vsha1cq_u32, k0, m2, m3, m0, m1);
            step!(vsha1cq_u32, k0, m3, m0, m1, m2);
            step!(vsha1cq_u32, k0, m0, m1, m2, m3);
            step!(vsha1pq_u32, k1, m1, m2, m3, m0);
            step!(vsha1pq_u32, k1, m2, m3, m0, m1);
            step!(vsha1pq_u32, k1, m3, m0, m1, m2);
            step!(vsha1pq_u32, k1, m0, m1, m2, m3);
            step!(vsha1pq_u32, k1, m1, m2, m3, m0);
            step!(vsha1mq_u32, k2, m2, m3, m0, m1);
            step!(vsha1mq_u32, k2, m3, m0, m1, m2);
            step!(vsha1mq_u32, k2, m0, m1, m2, m3);
            step!(vsha1mq_u32, k2, m1, m2, m3, m0);
            step!(vsha1mq_u32, k2, m2, m3, m0, m1);
            step!(vsha1pq_u32, k3, m3, m0, m1, m2);
            step!(vsha1pq_u32, k3, m0);
            step!(vsha1pq_u32, k3, m1);
            step!(vsha1pq_u32, k3, m2);
            step!(vsha1pq_u32, k3, m3);
            abcd = vaddq_u32(abcd, abcd_before);
            e = e.wrapping_add(e_before);
        }
        vst1q_u32(state.as_mut_ptr(), abcd);
        state[4] = e;
    }
}

#[cfg(test)]
mod tests {
    use super::Sha1;
    use sha1::Digest;

    #[test]
    fn known_digests() {
        let hex = |digest: [u8; 20]| hex::encode(digest);
        assert_eq!(hex(Sha1::digest(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(Sha1::digest(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(Sha1::digest(b"blob 0\0")), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    #[test]
    fn agrees_with_the_reference_however_the_input_is_cut() {
        // Deterministic noise, long enough to cross many blocks.
        let mut seed = 0x2545F4914F6CDD1Du64;
        let data: Vec<u8> = (0..5000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        for length in (0..300).chain([1000, 4095, 4096, 4097, 5000]) {
            let expected: [u8; 20] = sha1::Sha1::digest(&data[..length]).into();
            assert_eq!(Sha1::digest(&data[..length]), expected, "{length} bytes at once");
            for step in [1, 7, 63, 64, 65, 200] {
                let mut hash = Sha1::new();
                for piece in data[..length].chunks(step) {
                    hash.update(piece);
                }
                assert_eq!(hash.finalize(), expected, "{length} bytes, {step} at a time");
            }
        }
    }
}
