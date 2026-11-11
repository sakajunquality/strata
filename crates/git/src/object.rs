use bytes::Bytes;
use crate::sha1::Sha1;

use crate::{corrupt, Oid, Result};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectKind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjectKind {
    pub fn name(self) -> &'static str {
        match self {
            ObjectKind::Commit => "commit",
            ObjectKind::Tree => "tree",
            ObjectKind::Blob => "blob",
            ObjectKind::Tag => "tag",
        }
    }

    /// The type number used in pack entry headers.
    pub fn pack_type(self) -> u8 {
        match self {
            ObjectKind::Commit => 1,
            ObjectKind::Tree => 2,
            ObjectKind::Blob => 3,
            ObjectKind::Tag => 4,
        }
    }

    pub fn from_pack_type(t: u8) -> Option<ObjectKind> {
        match t {
            1 => Some(ObjectKind::Commit),
            2 => Some(ObjectKind::Tree),
            3 => Some(ObjectKind::Blob),
            4 => Some(ObjectKind::Tag),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Object {
    pub kind: ObjectKind,
    pub data: Bytes,
}

impl Object {
    pub fn oid(&self) -> Oid {
        object_id(self.kind, &self.data)
    }
}

/// The id git gives to an object of this kind and content.
pub fn object_id(kind: ObjectKind, data: &[u8]) -> Oid {
    // TODO: switch to a collision-detecting SHA-1 before accepting untrusted pushes.
    let mut h = Sha1::new();
    h.update(format!("{} {}\0", kind.name(), data.len()));
    h.update(data);
    Oid::from_bytes(&h.finalize()).expect("sha1 is 20 bytes")
}

/// What graph walks need to know about a commit.
#[derive(Clone, Debug)]
pub struct CommitInfo {
    /// Committer timestamp, in seconds since the epoch.
    pub time: i64,
    pub tree: Oid,
    pub parents: Vec<Oid>,
}

/// Extracts the tree, parents and date from a commit's headers.
pub fn commit_info(data: &[u8]) -> Result<CommitInfo> {
    let mut tree = None;
    let mut parents = Vec::new();
    for line in data.split(|b| *b == b'\n') {
        if line.is_empty() {
            break;
        }
        if let Some(hex) = line.strip_prefix(b"tree ") {
            tree = Some(Oid::from_hex(hex).ok_or_else(|| corrupt("bad tree id in commit"))?);
        } else if let Some(hex) = line.strip_prefix(b"parent ") {
            parents.push(Oid::from_hex(hex).ok_or_else(|| corrupt("bad parent id in commit"))?);
        }
    }
    Ok(CommitInfo {
        time: commit_time(data).unwrap_or(0),
        tree: tree.ok_or_else(|| corrupt("commit without tree"))?,
        parents,
    })
}

/// The committer timestamp, in seconds since the epoch.
fn commit_time(data: &[u8]) -> Option<i64> {
    let line = data.split(|b| *b == b'\n').take_while(|l| !l.is_empty()).find(|l| l.starts_with(b"committer "))?;
    // "committer Name <email> <seconds> <timezone>"
    let seconds = line.rsplit(|b| *b == b' ').nth(1)?;
    std::str::from_utf8(seconds).ok()?.parse().ok()
}

/// Returns the object an annotated tag points at.
pub fn parse_tag(data: &[u8]) -> Result<Oid> {
    for line in data.split(|b| *b == b'\n') {
        if line.is_empty() {
            break;
        }
        if let Some(hex) = line.strip_prefix(b"object ") {
            return Oid::from_hex(hex).ok_or_else(|| corrupt("bad object id in tag"));
        }
    }
    Err(corrupt("tag without object"))
}

pub struct TreeEntry<'a> {
    pub mode: u32,
    pub name: &'a [u8],
    pub oid: Oid,
}

impl TreeEntry<'_> {
    pub fn is_tree(&self) -> bool {
        self.mode == 0o40000
    }

    /// A submodule pointer: the commit lives in another repository.
    pub fn is_gitlink(&self) -> bool {
        self.mode == 0o160000
    }
}

impl TreeEntry<'_> {
    /// Where this entry sorts among those of a tree: by name, a directory
    /// as if its name ended in a slash. Equal only for entries of the same
    /// name that are both directories or both not.
    pub fn order(&self, other: &TreeEntry<'_>) -> std::cmp::Ordering {
        let shared = self.name.len().min(other.name.len());
        self.name[..shared].cmp(&other.name[..shared]).then_with(|| {
            let after = |entry: &TreeEntry<'_>| entry.name.get(shared).copied().unwrap_or(if entry.is_tree() { b'/' } else { 0 });
            after(self).cmp(&after(other))
        })
    }
}

pub fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry<'_>>> {
    let mut entries = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| corrupt("tree entry without mode"))?;
        // Octal, at most seven digits: read as it stands, since every tree
        // of a walk through history comes through here.
        let mut mode = 0u32;
        if space == 0 || space > 7 || !rest[..space].iter().all(|digit| (b'0'..=b'7').contains(digit)) {
            return Err(corrupt("bad tree entry mode"));
        }
        for digit in &rest[..space] {
            mode = mode * 8 + u32::from(digit - b'0');
        }
        rest = &rest[space + 1..];
        let nul = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| corrupt("tree entry without name"))?;
        let name = &rest[..nul];
        rest = &rest[nul + 1..];
        if rest.len() < 20 {
            return Err(corrupt("truncated tree entry"));
        }
        let oid = Oid::from_bytes(&rest[..20]).expect("20 bytes");
        rest = &rest[20..];
        entries.push(TreeEntry { mode, name, oid });
    }
    Ok(entries)
}
