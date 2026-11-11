use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use async_trait::async_trait;
use strata_git::Oid;

pub const DEFAULT_HEAD: &str = "refs/heads/main";

/// A consistent snapshot of one repository's metadata.
#[derive(Clone, Debug)]
pub struct RepoState {
    pub name: String,
    /// The object pool holding this repository's packs. Kept separate from the
    /// repository so that forks can later share one pool.
    pub pool_id: String,
    /// The ref that HEAD points at.
    pub head: String,
    /// The refs that were asked for, see [`RefScope`]. The one HEAD points at
    /// is always among them, if it exists.
    pub refs: BTreeMap<String, Oid>,
    /// Pack ids in the pool, oldest first.
    pub packs: Vec<String>,
    /// Incremented whenever objects are removed from the pool. Whoever checked
    /// that objects exist must make sure this has not moved before relying on it.
    pub epoch: u64,
    /// Incremented whenever a ref or HEAD changes.
    pub refs_version: u64,
}

/// Which of a repository's refs a reader needs. A repository can have
/// hundreds of thousands, and most requests are about a handful: reading
/// them all for each would make every request as slow as the list is long.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefScope {
    All,
    /// The refs with exactly these names.
    Names(Vec<String>),
    /// The refs whose names start with one of these.
    Prefixes(Vec<String>),
}

impl RefScope {
    /// No refs but the one HEAD points at.
    pub fn none() -> RefScope {
        RefScope::Names(Vec::new())
    }

    pub fn includes(&self, name: &str) -> bool {
        match self {
            RefScope::All => true,
            RefScope::Names(names) => names.iter().any(|n| n == name),
            RefScope::Prefixes(prefixes) => prefixes.iter().any(|p| name.starts_with(p.as_str())),
        }
    }
}

/// The smallest string greater than every string starting with `prefix`, for
/// reading a prefix as a range of an ordered index. `None` if there is none:
/// every string is in the range then.
pub(crate) fn prefix_end(prefix: &str) -> Option<String> {
    let mut end: Vec<char> = prefix.chars().collect();
    while let Some(last) = end.pop() {
        // The next character, past the gap that is not characters.
        if let Some(next) = (last as u32 + 1..=char::MAX as u32).find_map(char::from_u32) {
            end.push(next);
            return Some(end.into_iter().collect());
        }
    }
    None
}

/// A compare-and-swap on one ref. `None` means "does not exist".
#[derive(Clone, Debug)]
pub struct RefUpdate {
    pub name: String,
    pub old: Option<Oid>,
    pub new: Option<Oid>,
}

/// One atomic change to a repository. For a push, this is the commit point
/// that makes its newly uploaded pack visible.
#[derive(Clone, Debug, Default)]
pub struct Txn {
    pub ref_updates: Vec<RefUpdate>,
    pub add_pack: Option<String>,
    /// Packs to drop from the manifest, each of which must still be in it.
    /// They become garbage, to be deleted from object storage after a grace
    /// period that lets requests already reading them finish.
    pub remove_packs: Vec<String>,
    pub set_head: Option<String>,
    /// Fail with [`MetaError::Stale`] unless the repository is at this epoch.
    pub expect_epoch: Option<u64>,
    /// Fail with [`MetaError::Stale`] unless the refs are at this version.
    pub expect_refs_version: Option<u64>,
    /// Set by a change that removes objects from the pool.
    pub bump_epoch: bool,
    /// Recorded in the event feed if, and only if, this change commits.
    pub event: Option<String>,
}

/// One entry of the event feed: something that happened to a repository,
/// recorded in the same transaction as the change itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Where this event sits in the feed. Opaque; pass it back as `after`
    /// to read what follows.
    pub cursor: String,
    pub repo: String,
    /// Unix seconds, by the clock of the node that committed it.
    pub created_at: i64,
    /// What happened, as the JSON text given in [`Txn::event`].
    pub payload: String,
}

/// A pack no manifest refers to any more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Garbage {
    pub pool_id: String,
    pub pack_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("repository not found")]
    NotFound,
    #[error("already exists")]
    AlreadyExists,
    #[error("ref {name} changed concurrently")]
    Conflict { name: String },
    #[error("pack manifest changed concurrently")]
    PackConflict,
    #[error("repository changed since it was read")]
    Stale,
    #[error("not a position in the event feed")]
    BadCursor,
    #[error("metadata backend: {0}")]
    Backend(String),
}

#[async_trait]
pub trait MetaStore: Send + Sync {
    async fn create_repo(&self, name: &str) -> Result<RepoState, MetaError>;
    /// The repository as it is now, with the refs `refs` selects.
    async fn get_repo(&self, name: &str, refs: &RefScope) -> Result<Option<RepoState>, MetaError>;
    async fn list_repos(&self) -> Result<Vec<String>, MetaError>;
    async fn commit(&self, name: &str, txn: Txn) -> Result<(), MetaError>;
    /// Forgets a repository. Its packs become garbage. Returns whether it existed.
    async fn delete_repo(&self, name: &str) -> Result<bool, MetaError>;

    /// Up to `limit` events that follow `after` (or the oldest ones kept), in
    /// the order they were committed. An event is never inserted before one
    /// that was already returned, so a reader that remembers the last cursor
    /// it saw misses nothing.
    async fn list_events(&self, after: Option<&str>, limit: usize) -> Result<Vec<Event>, MetaError>;
    /// Drops events created before `created_before` (Unix seconds).
    async fn purge_events(&self, created_before: i64) -> Result<(), MetaError>;

    /// Packs removed from their manifest before `removed_before` (Unix seconds).
    async fn list_garbage(&self, removed_before: i64) -> Result<Vec<Garbage>, MetaError>;
    /// Forgets a garbage pack, once its files are gone.
    async fn purge_garbage(&self, garbage: &Garbage) -> Result<(), MetaError>;

    /// Claims `key` for `holder` for `ttl_secs`, unless someone else holds an
    /// unexpired claim. Used so that only one node does a given piece of
    /// maintenance at a time; correctness never depends on it.
    async fn try_lease(&self, key: &str, holder: &str, ttl_secs: i64) -> Result<bool, MetaError>;
    async fn release_lease(&self, key: &str, holder: &str) -> Result<(), MetaError>;
}

/// What a set of ref updates leaves behind: per ref, its final value. A ref
/// named twice ends up as its last update says.
pub(crate) fn final_refs(updates: &[RefUpdate]) -> BTreeMap<&str, Option<Oid>> {
    updates.iter().map(|u| (u.name.as_str(), u.new)).collect()
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub(crate) fn new_repo(name: &str) -> RepoState {
    RepoState {
        name: name.to_string(),
        pool_id: uuid::Uuid::new_v4().simple().to_string(),
        head: DEFAULT_HEAD.to_string(),
        refs: BTreeMap::new(),
        packs: Vec::new(),
        epoch: 0,
        refs_version: 0,
    }
}

/// Non-durable store for tests.
#[derive(Default)]
pub struct MemoryMetaStore {
    repos: Mutex<HashMap<String, RepoState>>,
    garbage: Mutex<Vec<(Garbage, i64)>>,
    leases: Mutex<HashMap<String, (String, i64)>>,
    /// Numbered from 1 in the order committed.
    events: Mutex<Vec<(u64, Event)>>,
}

#[async_trait]
impl MetaStore for MemoryMetaStore {
    async fn create_repo(&self, name: &str) -> Result<RepoState, MetaError> {
        let mut repos = self.repos.lock().unwrap();
        if repos.contains_key(name) {
            return Err(MetaError::AlreadyExists);
        }
        let repo = new_repo(name);
        repos.insert(name.to_string(), repo.clone());
        Ok(repo)
    }

    async fn get_repo(&self, name: &str, refs: &RefScope) -> Result<Option<RepoState>, MetaError> {
        let repos = self.repos.lock().unwrap();
        let Some(repo) = repos.get(name) else { return Ok(None) };
        let selected = repo.refs.iter().filter(|(name, _)| **name == repo.head || refs.includes(name)).map(|(name, oid)| (name.clone(), *oid)).collect();
        Ok(Some(RepoState { refs: selected, ..repo.clone() }))
    }

    async fn list_repos(&self) -> Result<Vec<String>, MetaError> {
        Ok(self.repos.lock().unwrap().keys().cloned().collect())
    }

    async fn commit(&self, name: &str, txn: Txn) -> Result<(), MetaError> {
        let mut repos = self.repos.lock().unwrap();
        let repo = repos.get_mut(name).ok_or(MetaError::NotFound)?;
        if txn.expect_epoch.is_some_and(|e| e != repo.epoch) || txn.expect_refs_version.is_some_and(|v| v != repo.refs_version) {
            return Err(MetaError::Stale);
        }
        for update in &txn.ref_updates {
            if repo.refs.get(&update.name) != update.old.as_ref() {
                return Err(MetaError::Conflict { name: update.name.clone() });
            }
        }
        if txn.remove_packs.iter().any(|pack| !repo.packs.contains(pack)) {
            return Err(MetaError::PackConflict);
        }
        repo.packs.retain(|pack| !txn.remove_packs.contains(pack));
        let now = unix_now();
        let pool_id = repo.pool_id.clone();
        self.garbage
            .lock()
            .unwrap()
            .extend(txn.remove_packs.into_iter().map(|pack_id| (Garbage { pool_id: pool_id.clone(), pack_id }, now)));
        if !txn.ref_updates.is_empty() || txn.set_head.is_some() {
            repo.refs_version += 1;
        }
        for update in txn.ref_updates {
            match update.new {
                Some(oid) => repo.refs.insert(update.name, oid),
                None => repo.refs.remove(&update.name),
            };
        }
        if let Some(pack) = txn.add_pack {
            if !repo.packs.contains(&pack) {
                repo.packs.push(pack);
            }
        }
        if let Some(head) = txn.set_head {
            repo.head = head;
        }
        if txn.bump_epoch {
            repo.epoch += 1;
        }
        if let Some(payload) = txn.event {
            let mut events = self.events.lock().unwrap();
            let id = events.last().map_or(1, |(id, _)| id + 1);
            events.push((id, Event { cursor: id.to_string(), repo: name.to_string(), created_at: now, payload }));
        }
        Ok(())
    }

    async fn delete_repo(&self, name: &str) -> Result<bool, MetaError> {
        let Some(repo) = self.repos.lock().unwrap().remove(name) else { return Ok(false) };
        let now = unix_now();
        let pool_id = repo.pool_id;
        self.garbage
            .lock()
            .unwrap()
            .extend(repo.packs.into_iter().map(|pack_id| (Garbage { pool_id: pool_id.clone(), pack_id }, now)));
        Ok(true)
    }

    async fn list_events(&self, after: Option<&str>, limit: usize) -> Result<Vec<Event>, MetaError> {
        let after: u64 = after.map_or(Ok(0), str::parse).map_err(|_| MetaError::BadCursor)?;
        let events = self.events.lock().unwrap();
        Ok(events.iter().filter(|(id, _)| *id > after).take(limit).map(|(_, event)| event.clone()).collect())
    }

    async fn purge_events(&self, created_before: i64) -> Result<(), MetaError> {
        self.events.lock().unwrap().retain(|(_, event)| event.created_at >= created_before);
        Ok(())
    }

    async fn list_garbage(&self, removed_before: i64) -> Result<Vec<Garbage>, MetaError> {
        Ok(self.garbage.lock().unwrap().iter().filter(|(_, at)| *at < removed_before).map(|(g, _)| g.clone()).collect())
    }

    async fn purge_garbage(&self, garbage: &Garbage) -> Result<(), MetaError> {
        self.garbage.lock().unwrap().retain(|(g, _)| g != garbage);
        Ok(())
    }

    async fn try_lease(&self, key: &str, holder: &str, ttl_secs: i64) -> Result<bool, MetaError> {
        let mut leases = self.leases.lock().unwrap();
        let now = unix_now();
        if leases.get(key).is_some_and(|(other, expires)| other != holder && *expires > now) {
            return Ok(false);
        }
        leases.insert(key.to_string(), (holder.to_string(), now + ttl_secs));
        Ok(true)
    }

    async fn release_lease(&self, key: &str, holder: &str) -> Result<(), MetaError> {
        let mut leases = self.leases.lock().unwrap();
        if leases.get(key).is_some_and(|(current, _)| current == holder) {
            leases.remove(key);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn conforms() {
        crate::conformance::check(Arc::new(MemoryMetaStore::default())).await;
    }

    #[test]
    fn prefix_ranges() {
        assert_eq!(prefix_end("refs/heads/").as_deref(), Some("refs/heads0"));
        assert_eq!(prefix_end("a\u{10FFFF}").as_deref(), Some("b"));
        assert_eq!(prefix_end("a\u{D7FF}").as_deref(), Some("a\u{E000}"));
        assert_eq!(prefix_end(""), None);
        assert_eq!(prefix_end("\u{10FFFF}"), None);
    }
}
