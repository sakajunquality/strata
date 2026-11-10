//! Single-node metadata store. The schema is deliberately plain SQL so it
//! ports to a shared database (which several nodes need) unchanged in shape.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use strata_git::Oid;

use crate::meta::{new_repo, unix_now, Access, Garbage, Grant, MetaError, MetaStore, RepoState, TokenRecord, Txn};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS repos (
    name         TEXT PRIMARY KEY,
    pool_id      TEXT NOT NULL,
    head         TEXT NOT NULL,
    epoch        INTEGER NOT NULL DEFAULT 0,
    refs_version INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS refs (
    repo TEXT NOT NULL,
    name TEXT NOT NULL,
    oid  TEXT NOT NULL,
    PRIMARY KEY (repo, name)
);
CREATE TABLE IF NOT EXISTS packs (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    pool_id TEXT NOT NULL,
    pack_id TEXT NOT NULL,
    UNIQUE (pool_id, pack_id)
);
CREATE TABLE IF NOT EXISTS garbage (
    pool_id    TEXT NOT NULL,
    pack_id    TEXT NOT NULL,
    removed_at INTEGER NOT NULL,
    PRIMARY KEY (pool_id, pack_id)
);
CREATE TABLE IF NOT EXISTS leases (
    key        TEXT PRIMARY KEY,
    holder     TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tokens (
    name   TEXT PRIMARY KEY,
    hash   TEXT NOT NULL UNIQUE,
    grants TEXT NOT NULL
);
";

/// Columns added after the first version, for databases created before them.
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE repos ADD COLUMN epoch INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE repos ADD COLUMN refs_version INTEGER NOT NULL DEFAULT 0",
];

impl From<rusqlite::Error> for MetaError {
    fn from(e: rusqlite::Error) -> Self {
        MetaError::Backend(e.to_string())
    }
}

pub struct SqliteMetaStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteMetaStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MetaError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        for migration in MIGRATIONS {
            match conn.execute(migration, []) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column") => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(SqliteMetaStore { conn: Arc::new(Mutex::new(conn)) })
    }

    async fn run<T, F>(&self, f: F) -> Result<T, MetaError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, MetaError> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || f(&mut conn.lock().unwrap()))
            .await
            .map_err(|e| MetaError::Backend(e.to_string()))?
    }
}

fn load_repo(conn: &Connection, name: &str) -> Result<Option<RepoState>, MetaError> {
    let row = conn
        .query_row("SELECT pool_id, head, epoch, refs_version FROM repos WHERE name = ?1", [name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
        })
        .optional()?;
    let Some((pool_id, head, epoch, refs_version)) = row else { return Ok(None) };

    let mut refs = BTreeMap::new();
    let mut stmt = conn.prepare("SELECT name, oid FROM refs WHERE repo = ?1")?;
    let rows = stmt.query_map([name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (ref_name, hex) = row?;
        let oid = Oid::from_hex(&hex).ok_or_else(|| MetaError::Backend(format!("bad oid stored for {ref_name}")))?;
        refs.insert(ref_name, oid);
    }

    let mut stmt = conn.prepare("SELECT pack_id FROM packs WHERE pool_id = ?1 ORDER BY seq")?;
    let packs = stmt.query_map([&pool_id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;

    Ok(Some(RepoState {
        name: name.to_string(),
        pool_id,
        head,
        refs,
        packs,
        epoch: epoch as u64,
        refs_version: refs_version as u64,
    }))
}

fn encode_grants(grants: &[Grant]) -> String {
    grants.iter().map(|g| format!("{}\t{}", g.pattern, g.access.name())).collect::<Vec<_>>().join("\n")
}

fn decode_grants(text: &str) -> Vec<Grant> {
    text.lines()
        .filter_map(|line| {
            let (pattern, access) = line.split_once('\t')?;
            Some(Grant { pattern: pattern.to_string(), access: Access::parse(access)? })
        })
        .collect()
}

#[async_trait]
impl MetaStore for SqliteMetaStore {
    async fn create_repo(&self, name: &str) -> Result<RepoState, MetaError> {
        let repo = new_repo(name);
        self.run(move |conn| {
            let inserted = conn.execute(
                "INSERT OR IGNORE INTO repos (name, pool_id, head) VALUES (?1, ?2, ?3)",
                params![repo.name, repo.pool_id, repo.head],
            )?;
            if inserted == 0 {
                return Err(MetaError::AlreadyExists);
            }
            Ok(repo)
        })
        .await
    }

    async fn get_repo(&self, name: &str) -> Result<Option<RepoState>, MetaError> {
        let name = name.to_string();
        self.run(move |conn| {
            // One read transaction so refs and packs come from the same snapshot.
            let tx = conn.transaction()?;
            load_repo(&tx, &name)
        })
        .await
    }

    async fn list_repos(&self) -> Result<Vec<String>, MetaError> {
        self.run(move |conn| {
            let mut stmt = conn.prepare("SELECT name FROM repos ORDER BY name")?;
            let names = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(names)
        })
        .await
    }

    async fn commit(&self, name: &str, txn: Txn) -> Result<(), MetaError> {
        let name = name.to_string();
        self.run(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (pool_id, epoch, refs_version): (String, i64, i64) = tx
                .query_row("SELECT pool_id, epoch, refs_version FROM repos WHERE name = ?1", [&name], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?
                .ok_or(MetaError::NotFound)?;
            if txn.expect_epoch.is_some_and(|e| e != epoch as u64)
                || txn.expect_refs_version.is_some_and(|v| v != refs_version as u64)
            {
                return Err(MetaError::Stale);
            }

            for update in &txn.ref_updates {
                let current: Option<String> = tx
                    .query_row("SELECT oid FROM refs WHERE repo = ?1 AND name = ?2", params![name, update.name], |r| r.get(0))
                    .optional()?;
                if current != update.old.map(|oid| oid.to_string()) {
                    return Err(MetaError::Conflict { name: update.name.clone() });
                }
            }
            for update in &txn.ref_updates {
                match update.new {
                    Some(oid) => tx.execute(
                        "INSERT INTO refs (repo, name, oid) VALUES (?1, ?2, ?3)
                         ON CONFLICT (repo, name) DO UPDATE SET oid = excluded.oid",
                        params![name, update.name, oid.to_string()],
                    )?,
                    None => tx.execute("DELETE FROM refs WHERE repo = ?1 AND name = ?2", params![name, update.name])?,
                };
            }
            for pack in &txn.remove_packs {
                let removed = tx.execute("DELETE FROM packs WHERE pool_id = ?1 AND pack_id = ?2", params![pool_id, pack])?;
                if removed == 0 {
                    return Err(MetaError::PackConflict);
                }
                tx.execute(
                    "INSERT OR REPLACE INTO garbage (pool_id, pack_id, removed_at) VALUES (?1, ?2, ?3)",
                    params![pool_id, pack, unix_now()],
                )?;
            }
            if let Some(pack) = &txn.add_pack {
                tx.execute("INSERT OR IGNORE INTO packs (pool_id, pack_id) VALUES (?1, ?2)", params![pool_id, pack])?;
            }
            if let Some(head) = &txn.set_head {
                tx.execute("UPDATE repos SET head = ?1 WHERE name = ?2", params![head, name])?;
            }
            let refs_changed = !txn.ref_updates.is_empty() || txn.set_head.is_some();
            tx.execute(
                "UPDATE repos SET epoch = epoch + ?1, refs_version = refs_version + ?2 WHERE name = ?3",
                params![i64::from(txn.bump_epoch), i64::from(refs_changed), name],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    async fn list_garbage(&self, removed_before: i64) -> Result<Vec<Garbage>, MetaError> {
        self.run(move |conn| {
            let mut stmt = conn.prepare("SELECT pool_id, pack_id FROM garbage WHERE removed_at < ?1")?;
            let rows = stmt.query_map([removed_before], |r| Ok(Garbage { pool_id: r.get(0)?, pack_id: r.get(1)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    async fn purge_garbage(&self, garbage: &Garbage) -> Result<(), MetaError> {
        let garbage = garbage.clone();
        self.run(move |conn| {
            conn.execute("DELETE FROM garbage WHERE pool_id = ?1 AND pack_id = ?2", params![garbage.pool_id, garbage.pack_id])?;
            Ok(())
        })
        .await
    }

    async fn try_lease(&self, key: &str, holder: &str, ttl_secs: i64) -> Result<bool, MetaError> {
        let (key, holder) = (key.to_string(), holder.to_string());
        self.run(move |conn| {
            let now = unix_now();
            // Takes the lease if it is free, expired, or already ours.
            let taken = conn.execute(
                "INSERT INTO leases (key, holder, expires_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (key) DO UPDATE SET holder = excluded.holder, expires_at = excluded.expires_at
                 WHERE leases.holder = excluded.holder OR leases.expires_at <= ?4",
                params![key, holder, now + ttl_secs, now],
            )?;
            Ok(taken > 0)
        })
        .await
    }

    async fn release_lease(&self, key: &str, holder: &str) -> Result<(), MetaError> {
        let (key, holder) = (key.to_string(), holder.to_string());
        self.run(move |conn| {
            conn.execute("DELETE FROM leases WHERE key = ?1 AND holder = ?2", params![key, holder])?;
            Ok(())
        })
        .await
    }

    async fn put_token(&self, token: TokenRecord) -> Result<(), MetaError> {
        self.run(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO tokens (name, hash, grants) VALUES (?1, ?2, ?3)",
                params![token.name, token.hash, encode_grants(&token.grants)],
            )?;
            Ok(())
        })
        .await
    }

    async fn find_token(&self, hash: &str) -> Result<Option<TokenRecord>, MetaError> {
        let hash = hash.to_string();
        self.run(move |conn| {
            let row = conn
                .query_row("SELECT name, grants FROM tokens WHERE hash = ?1", [&hash], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .optional()?;
            Ok(row.map(|(name, grants)| TokenRecord { name, hash, grants: decode_grants(&grants) }))
        })
        .await
    }

    async fn delete_token(&self, name: &str) -> Result<bool, MetaError> {
        let name = name.to_string();
        self.run(move |conn| Ok(conn.execute("DELETE FROM tokens WHERE name = ?1", [&name])? > 0)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::RefUpdate;

    #[tokio::test]
    async fn cas_and_manifest() {
        let store = SqliteMetaStore::open(":memory:").unwrap();
        let repo = store.create_repo("a/b").await.unwrap();
        assert!(matches!(store.create_repo("a/b").await, Err(MetaError::AlreadyExists)));
        assert_eq!(store.list_repos().await.unwrap(), vec!["a/b".to_string()]);

        let one = Oid::from_bytes(&[1; 20]).unwrap();
        let two = Oid::from_bytes(&[2; 20]).unwrap();
        let update = |old, new| RefUpdate { name: "refs/heads/main".into(), old, new };

        let txn = Txn { ref_updates: vec![update(None, Some(one))], add_pack: Some("p1".into()), ..Txn::default() };
        store.commit("a/b", txn).await.unwrap();

        // A stale expectation must change nothing, including the manifest.
        let stale = Txn { ref_updates: vec![update(None, Some(two))], add_pack: Some("p2".into()), ..Txn::default() };
        assert!(matches!(store.commit("a/b", stale).await, Err(MetaError::Conflict { .. })));

        let state = store.get_repo("a/b").await.unwrap().unwrap();
        assert_eq!(state.pool_id, repo.pool_id);
        assert_eq!(state.refs.get("refs/heads/main"), Some(&one));
        assert_eq!(state.packs, vec!["p1".to_string()]);
        assert_eq!((state.epoch, state.refs_version), (0, 1));

        let delete = Txn { ref_updates: vec![update(Some(one), None)], ..Txn::default() };
        store.commit("a/b", delete).await.unwrap();
        assert!(store.get_repo("a/b").await.unwrap().unwrap().refs.is_empty());

        // Compaction swaps packs atomically; whoever loses a race changes nothing.
        let swap = |add: &str, remove: &str| Txn { add_pack: Some(add.into()), remove_packs: vec![remove.into()], ..Txn::default() };
        store.commit("a/b", swap("merged", "p1")).await.unwrap();
        assert!(matches!(store.commit("a/b", swap("other", "p1")).await, Err(MetaError::PackConflict)));
        assert_eq!(store.get_repo("a/b").await.unwrap().unwrap().packs, vec!["merged".to_string()]);

        // The replaced pack waits out its grace period, then is forgotten.
        let garbage = Garbage { pool_id: repo.pool_id.clone(), pack_id: "p1".into() };
        assert!(store.list_garbage(0).await.unwrap().is_empty());
        assert_eq!(store.list_garbage(i64::MAX).await.unwrap(), vec![garbage.clone()]);
        store.purge_garbage(&garbage).await.unwrap();
        assert!(store.list_garbage(i64::MAX).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn versions_guard_against_changes_in_between() {
        let store = SqliteMetaStore::open(":memory:").unwrap();
        store.create_repo("r").await.unwrap();
        let one = Oid::from_bytes(&[1; 20]).unwrap();
        let push = |epoch| Txn {
            ref_updates: vec![RefUpdate { name: "refs/heads/main".into(), old: None, new: Some(one) }],
            expect_epoch: Some(epoch),
            ..Txn::default()
        };

        // Objects were removed after the pusher looked: its checks no longer hold.
        store.commit("r", Txn { bump_epoch: true, ..Txn::default() }).await.unwrap();
        assert!(matches!(store.commit("r", push(0)).await, Err(MetaError::Stale)));
        store.commit("r", push(1)).await.unwrap();

        // Refs moved after a pruning compaction verified them: it must look again.
        let prune = |version| Txn { bump_epoch: true, expect_refs_version: Some(version), ..Txn::default() };
        assert!(matches!(store.commit("r", prune(0)).await, Err(MetaError::Stale)));
        store.commit("r", prune(1)).await.unwrap();
        assert_eq!(store.get_repo("r").await.unwrap().unwrap().epoch, 2);
    }

    #[tokio::test]
    async fn leases_and_tokens() {
        let store = SqliteMetaStore::open(":memory:").unwrap();
        assert!(store.try_lease("compact/r", "node-a", 60).await.unwrap());
        assert!(store.try_lease("compact/r", "node-a", 60).await.unwrap());
        assert!(!store.try_lease("compact/r", "node-b", 60).await.unwrap());
        store.release_lease("compact/r", "node-b").await.unwrap();
        assert!(!store.try_lease("compact/r", "node-b", 60).await.unwrap());
        store.release_lease("compact/r", "node-a").await.unwrap();
        assert!(store.try_lease("compact/r", "node-b", 0).await.unwrap());
        // An expired lease is up for grabs.
        assert!(store.try_lease("compact/r", "node-a", 60).await.unwrap());

        let token = TokenRecord {
            name: "ci".into(),
            hash: "abc".into(),
            grants: vec![Grant { pattern: "acme/*".into(), access: Access::Read }, Grant { pattern: "acme/app".into(), access: Access::Write }],
        };
        store.put_token(token.clone()).await.unwrap();
        assert_eq!(store.find_token("abc").await.unwrap(), Some(token));
        assert_eq!(store.find_token("other").await.unwrap(), None);
        assert!(store.delete_token("ci").await.unwrap());
        assert!(!store.delete_token("ci").await.unwrap());
        assert_eq!(store.find_token("abc").await.unwrap(), None);
    }
}
