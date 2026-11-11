use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::sync::OnceLock;

/// A SHA-1 object id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Oid([u8; 20]);

/// An id is a hash already: eight of its bytes stand for it in a table. Two
/// ids that agree in those take 2^64 work to make.
impl Hash for Oid {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(u64::from_le_bytes(self.0[..8].try_into().expect("8 bytes")));
    }
}

/// Hashing for tables keyed by object id, which the walks through history
/// spend much of their time in. The bytes of an id are mixed with a secret
/// drawn once per process, so that ids cannot be made to pile up in one
/// bucket by someone who chooses them; that is all the general-purpose
/// hasher was adding, at several times the cost.
#[derive(Clone, Copy)]
pub struct OidState {
    seed: u64,
    factor: u64,
}

impl Default for OidState {
    fn default() -> Self {
        static SECRET: OnceLock<(u64, u64)> = OnceLock::new();
        let (seed, factor) = *SECRET.get_or_init(|| {
            let random = RandomState::new();
            (random.hash_one(1u8), random.hash_one(2u8) | 1)
        });
        OidState { seed, factor }
    }
}

impl BuildHasher for OidState {
    type Hasher = OidHasher;

    fn build_hasher(&self) -> OidHasher {
        OidHasher { value: 0, state: *self }
    }
}

pub struct OidHasher {
    value: u64,
    state: OidState,
}

impl OidHasher {
    /// Both halves of the product, so that every bit of the input reaches
    /// the low bits a table looks at.
    fn mix(&mut self, word: u64) {
        let product = u128::from(self.value ^ word ^ self.state.seed) * u128::from(self.state.factor);
        self.value = (product as u64) ^ ((product >> 64) as u64);
    }
}

impl Hasher for OidHasher {
    fn write_u64(&mut self, word: u64) {
        self.mix(word);
    }

    fn write(&mut self, bytes: &[u8]) {
        for piece in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..piece.len()].copy_from_slice(piece);
            self.mix(u64::from_le_bytes(word));
        }
        self.mix(bytes.len() as u64);
    }

    fn finish(&self) -> u64 {
        self.value
    }
}

pub type OidSet = HashSet<Oid, OidState>;
pub type OidMap<V> = HashMap<Oid, V, OidState>;

impl Oid {
    pub const ZERO: Oid = Oid([0; 20]);

    pub fn from_bytes(bytes: &[u8]) -> Option<Oid> {
        Some(Oid(bytes.try_into().ok()?))
    }

    pub fn from_hex(hex: impl AsRef<[u8]>) -> Option<Oid> {
        let mut out = [0u8; 20];
        hex::decode_to_slice(hex.as_ref(), &mut out).ok()?;
        Some(Oid(out))
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 20]
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
