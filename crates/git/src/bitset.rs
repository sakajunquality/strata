/// A fixed-size set of small integers, one bit each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitSet {
    words: Vec<u64>,
    len: usize,
}

impl BitSet {
    pub fn new(len: usize) -> BitSet {
        BitSet { words: vec![0; len.div_ceil(64)], len }
    }

    /// Number of positions, set or not.
    pub fn capacity(&self) -> usize {
        self.len
    }

    pub fn set(&mut self, i: usize) {
        self.words[i / 64] |= 1 << (i % 64);
    }

    pub fn clear(&mut self, i: usize) {
        self.words[i / 64] &= !(1 << (i % 64));
    }

    pub fn get(&self, i: usize) -> bool {
        i < self.len && self.words[i / 64] & (1 << (i % 64)) != 0
    }

    pub fn union_with(&mut self, other: &BitSet) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a |= b;
        }
    }

    pub fn subtract(&mut self, other: &BitSet) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a &= !b;
        }
    }

    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    pub fn from_bytes(len: usize, bytes: &[u8]) -> Option<BitSet> {
        if bytes.len() != len.div_ceil(64) * 8 {
            return None;
        }
        let words = bytes.as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect();
        Some(BitSet { words, len })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations() {
        let mut a = BitSet::new(130);
        a.set(0);
        a.set(64);
        a.set(129);
        let mut b = BitSet::new(130);
        b.set(64);
        b.set(5);

        let mut union = a.clone();
        union.union_with(&b);
        assert_eq!(union.count(), 4);

        a.subtract(&b);
        assert!(a.get(0) && !a.get(64) && a.get(129) && !a.get(500));
        a.clear(0);
        assert_eq!(a.count(), 1);

        assert_eq!(BitSet::from_bytes(130, &union.to_bytes()), Some(union));
        assert_eq!(BitSet::from_bytes(130, &[0; 8]), None);
    }
}
