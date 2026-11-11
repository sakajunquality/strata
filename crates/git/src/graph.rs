//! Commit-graph: the tree, parents and date of every commit in a pack, laid
//! out for lookup without inflating any commit object.
//!
//! This is a sidecar of one pack in a format of our own (git's commit-graph
//! describes a whole repository, not a pack).

use bytes::Bytes;
use crate::sha1::Sha1;

use crate::object::CommitInfo;
use crate::{corrupt, Oid, Result};

const MAGIC: &[u8; 8] = b"SCGR\0\0\0\x01";
const OIDS_AT: usize = 12;
/// tree id, commit time, index of the first parent in the parent list, parent count.
const RECORD_LEN: usize = 20 + 8 + 4 + 4;

#[derive(Clone)]
pub struct CommitGraph {
    raw: Bytes,
    count: usize,
}

impl CommitGraph {
    pub fn build(mut commits: Vec<(Oid, CommitInfo)>) -> CommitGraph {
        commits.sort_by_key(|(oid, _)| *oid);
        commits.dedup_by_key(|(oid, _)| *oid);
        let mut out = Vec::with_capacity(OIDS_AT + commits.len() * (20 + RECORD_LEN + 20) + 20);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(commits.len() as u32).to_be_bytes());
        for (oid, _) in &commits {
            out.extend_from_slice(oid.as_bytes());
        }
        let mut parent_index = 0u32;
        for (_, info) in &commits {
            out.extend_from_slice(info.tree.as_bytes());
            out.extend_from_slice(&info.time.to_be_bytes());
            out.extend_from_slice(&parent_index.to_be_bytes());
            out.extend_from_slice(&(info.parents.len() as u32).to_be_bytes());
            parent_index += info.parents.len() as u32;
        }
        for (_, info) in &commits {
            for parent in &info.parents {
                out.extend_from_slice(parent.as_bytes());
            }
        }
        let checksum = Sha1::digest(&out);
        out.extend_from_slice(&checksum);
        CommitGraph { raw: out.into(), count: commits.len() }
    }

    pub fn decode(raw: Bytes) -> Result<CommitGraph> {
        if raw.len() < OIDS_AT + 20 || &raw[..8] != MAGIC {
            return Err(corrupt("not a commit graph"));
        }
        let body_len = raw.len() - 20;
        if Sha1::digest(&raw[..body_len]).as_slice() != &raw[body_len..] {
            return Err(corrupt("commit graph checksum mismatch"));
        }
        let count = u32::from_be_bytes(raw[8..12].try_into().expect("4 bytes")) as usize;
        let graph = CommitGraph { raw, count };
        let parents_at = graph.parents_at();
        if parents_at > body_len || !(body_len - parents_at).is_multiple_of(20) {
            return Err(corrupt("truncated commit graph"));
        }
        let parent_slots = (body_len - parents_at) / 20;
        for i in 0..count {
            let (start, n) = graph.parent_range(i);
            if start + n > parent_slots {
                return Err(corrupt("bad parent range in commit graph"));
            }
            if i > 0 && graph.oid_bytes(i - 1) >= graph.oid_bytes(i) {
                return Err(corrupt("commit graph is not sorted"));
            }
        }
        Ok(graph)
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

    fn records_at(&self) -> usize {
        OIDS_AT + self.count * 20
    }

    fn parents_at(&self) -> usize {
        self.records_at() + self.count * RECORD_LEN
    }

    fn oid_bytes(&self, i: usize) -> &[u8] {
        &self.raw[OIDS_AT + i * 20..OIDS_AT + (i + 1) * 20]
    }

    fn parent_range(&self, i: usize) -> (usize, usize) {
        let at = self.records_at() + i * RECORD_LEN + 28;
        let be32 = |pos: usize| u32::from_be_bytes(self.raw[pos..pos + 4].try_into().expect("4 bytes")) as usize;
        (be32(at), be32(at + 4))
    }

    pub fn get(&self, oid: &Oid) -> Option<CommitInfo> {
        let (mut lo, mut hi) = (0, self.count);
        let i = loop {
            if lo >= hi {
                return None;
            }
            let mid = lo + (hi - lo) / 2;
            match self.oid_bytes(mid).cmp(oid.as_bytes().as_slice()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => break mid,
            }
        };
        let at = self.records_at() + i * RECORD_LEN;
        let tree = Oid::from_bytes(&self.raw[at..at + 20]).expect("20 bytes");
        let time = i64::from_be_bytes(self.raw[at + 20..at + 28].try_into().expect("8 bytes"));
        let (start, n) = self.parent_range(i);
        let parents_at = self.parents_at() + start * 20;
        let parents =
            self.raw[parents_at..parents_at + n * 20].as_chunks::<20>().0.iter().map(|c| Oid::from_bytes(c).expect("20 bytes")).collect();
        Some(CommitInfo { time, tree, parents })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let oid = |b: u8| Oid::from_bytes(&[b; 20]).unwrap();
        let graph = CommitGraph::build(vec![
            (oid(9), CommitInfo { time: 300, tree: oid(19), parents: vec![oid(5), oid(1)] }),
            (oid(1), CommitInfo { time: 100, tree: oid(11), parents: vec![] }),
            (oid(5), CommitInfo { time: -7, tree: oid(15), parents: vec![oid(1)] }),
        ]);
        let graph = CommitGraph::decode(graph.encoded().clone()).unwrap();
        assert_eq!(graph.len(), 3);
        let merge = graph.get(&oid(9)).unwrap();
        assert_eq!((merge.time, merge.tree, merge.parents), (300, oid(19), vec![oid(5), oid(1)]));
        assert_eq!(graph.get(&oid(5)).unwrap().time, -7);
        assert!(graph.get(&oid(1)).unwrap().parents.is_empty());
        assert!(graph.get(&oid(2)).is_none());

        let mut damaged = graph.encoded().to_vec();
        damaged[OIDS_AT] ^= 1;
        assert!(CommitGraph::decode(damaged.into()).is_err());
    }
}
