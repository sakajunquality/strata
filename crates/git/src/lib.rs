//! Git wire and on-disk formats, independent of any storage backend.
//!
//! The storage layer decides where pack bytes come from ([`PackData`]); this
//! crate never assumes a whole pack fits in memory.

pub mod bitmap;
pub mod bitset;
pub mod compact;
pub mod delta;
pub mod graph;
pub mod idx;
pub mod object;
pub mod oid;
pub mod pack;
pub mod pktline;
pub mod sha1;
pub mod sparse;
pub mod walk;

use bytes::Bytes;

pub use bitmap::PackBitmaps;
pub use graph::CommitGraph;
pub use idx::PackIndex;
pub use object::{CommitInfo, Object, ObjectKind};
pub use oid::{Oid, OidMap, OidSet};
pub use pack::{PackFile, PackSet};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("corrupt data: {0}")]
    Corrupt(String),
    #[error("missing object {0}")]
    Missing(Oid),
    /// The request itself cannot be satisfied; the message is safe to show to the client.
    #[error("{0}")]
    Request(String),
    #[error("i/o: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn corrupt(msg: impl Into<String>) -> Error {
    Error::Corrupt(msg.into())
}

/// Read access to a set of git objects.
pub trait ObjectSource {
    fn get(&self, oid: &Oid) -> Result<Option<Object>>;
    fn contains(&self, oid: &Oid) -> bool;

    /// How large an object is, or `None` if it is not here. Implementations
    /// that can tell without reading the object should.
    fn size(&self, oid: &Oid) -> Result<Option<u64>> {
        Ok(self.get(oid)?.map(|object| object.data.len() as u64))
    }

    /// What graph walks need from a commit, or `None` if `oid` is not a
    /// commit in this source. Implementations with a commit-graph answer
    /// without reading the object.
    fn commit(&self, oid: &Oid) -> Result<Option<CommitInfo>> {
        match self.get(oid)? {
            Some(object) if object.kind == ObjectKind::Commit => object::commit_info(&object.data).map(Some),
            _ => Ok(None),
        }
    }
}

/// Random access to the bytes of one packfile. Calls may block on I/O.
pub trait PackData: Send + Sync {
    /// Total size of the pack in bytes.
    fn size(&self) -> u64;

    /// Returns the bytes starting at `offset`, up to wherever is convenient
    /// for the implementation (typically the end of a cached block). Empty
    /// only at or past the end of the pack.
    fn chunk(&self, offset: u64) -> Result<Bytes>;
}

impl PackData for Bytes {
    fn size(&self) -> u64 {
        self.len() as u64
    }

    fn chunk(&self, offset: u64) -> Result<Bytes> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(self.len());
        Ok(self.slice(start..))
    }
}

/// Cache of objects already reconstructed from a pack, keyed by pack and
/// entry offset. Saves re-walking delta chains.
pub trait ObjectCache: Send + Sync {
    fn get(&self, pack: u64, offset: u64) -> Option<Object>;
    fn put(&self, pack: u64, offset: u64, object: &Object);
}
