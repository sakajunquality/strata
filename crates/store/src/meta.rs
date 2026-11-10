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
    pub refs: BTreeMap<String, Oid>,
    /// Pack ids in the pool, oldest first.
    pub packs: Vec<String>,
    /// Incremented whenever objects are removed from the pool. Whoever checked
    /// that objects exist must make sure this has not moved before relying on it.
    pub epoch: u64,
    /// Incremented whenever a ref or HEAD changes.
    pub refs_version: u64,
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
}

/// A pack no manifest refers to any more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Garbage {
    pub pool_id: String,
    pub pack_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    Read,
    Write,
    Admin,
}

impl Access {
    pub fn parse(s: &str) -> Option<Access> {
        match s {
            "read" => Some(Access::Read),
            "write" => Some(Access::Write),
            "admin" => Some(Access::Admin),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Admin => "admin",
        }
    }
}

/// Access to the repositories matching `pattern`: a full name, a prefix
/// ending in `/*`, or `*` for all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub pattern: String,
    pub access: Access,
}

impl Grant {
    pub fn covers(&self, repo: &str) -> bool {
        match self.pattern.strip_suffix('*') {
            Some("") => true,
            Some(prefix) => prefix.ends_with('/') && repo.starts_with(prefix),
            None => self.pattern == repo,
        }
    }
}

/// An access token. Only a hash of the secret is stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenRecord {
    pub name: String,
    /// Hex SHA-256 of the secret.
    pub hash: String,
    pub grants: Vec<Grant>,
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
    #[error("metadata backend: {0}")]
    Backend(String),
}

#[async_trait]
pub trait MetaStore: Send + Sync {
    async fn create_repo(&self, name: &str) -> Result<RepoState, MetaError>;
    async fn get_repo(&self, name: &str) -> Result<Option<RepoState>, MetaError>;
    async fn list_repos(&self) -> Result<Vec<String>, MetaError>;
    async fn commit(&self, name: &str, txn: Txn) -> Result<(), MetaError>;

    /// Packs removed from their manifest before `removed_before` (Unix seconds).
    async fn list_garbage(&self, removed_before: i64) -> Result<Vec<Garbage>, MetaError>;
    /// Forgets a garbage pack, once its files are gone.
    async fn purge_garbage(&self, garbage: &Garbage) -> Result<(), MetaError>;

    /// Claims `key` for `holder` for `ttl_secs`, unless someone else holds an
    /// unexpired claim. Used so that only one node does a given piece of
    /// maintenance at a time; correctness never depends on it.
    async fn try_lease(&self, key: &str, holder: &str, ttl_secs: i64) -> Result<bool, MetaError>;
    async fn release_lease(&self, key: &str, holder: &str) -> Result<(), MetaError>;

    /// Stores a token, replacing any other of the same name.
    async fn put_token(&self, token: TokenRecord) -> Result<(), MetaError>;
    async fn find_token(&self, hash: &str) -> Result<Option<TokenRecord>, MetaError>;
    /// Returns whether a token of that name existed.
    async fn delete_token(&self, name: &str) -> Result<bool, MetaError>;
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
    tokens: Mutex<Vec<TokenRecord>>,
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

    async fn get_repo(&self, name: &str) -> Result<Option<RepoState>, MetaError> {
        Ok(self.repos.lock().unwrap().get(name).cloned())
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

    async fn put_token(&self, token: TokenRecord) -> Result<(), MetaError> {
        let mut tokens = self.tokens.lock().unwrap();
        tokens.retain(|t| t.name != token.name);
        tokens.push(token);
        Ok(())
    }

    async fn find_token(&self, hash: &str) -> Result<Option<TokenRecord>, MetaError> {
        Ok(self.tokens.lock().unwrap().iter().find(|t| t.hash == hash).cloned())
    }

    async fn delete_token(&self, name: &str) -> Result<bool, MetaError> {
        let mut tokens = self.tokens.lock().unwrap();
        let before = tokens.len();
        tokens.retain(|t| t.name != name);
        Ok(tokens.len() != before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_patterns() {
        let grant = |pattern: &str| Grant { pattern: pattern.into(), access: Access::Read };
        assert!(grant("*").covers("acme/app"));
        assert!(grant("acme/*").covers("acme/app"));
        assert!(grant("acme/*").covers("acme/team/app"));
        assert!(!grant("acme/*").covers("acme-evil/app"));
        assert!(!grant("acme/*").covers("acme"));
        assert!(grant("acme/app").covers("acme/app"));
        assert!(!grant("acme/app").covers("acme/app2"));
        // A bare prefix is not a wildcard.
        assert!(!grant("acme*").covers("acme/app"));
        assert!(Access::Admin > Access::Write && Access::Write > Access::Read);
    }
}
