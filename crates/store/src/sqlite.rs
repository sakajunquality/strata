//! Single-node metadata store, in a file. Several nodes need one of the
//! shared backends instead.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use strata_git::Oid;

use crate::meta::{new_repo, prefix_end, unix_now, Event, Garbage, MetaError, MetaStore, RefScope, RepoState, Txn};

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
CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    repo       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    payload    TEXT NOT NULL
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

fn load_repo(conn: &Connection, name: &str, scope: &RefScope) -> Result<Option<RepoState>, MetaError> {
    let row = conn
        .query_row("SELECT pool_id, head, epoch, refs_version FROM repos WHERE name = ?1", [name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
        })
        .optional()?;
    let Some((pool_id, head, epoch, refs_version)) = row else { return Ok(None) };

    let mut refs = BTreeMap::new();
    let mut read = |condition: &str, values: &[&str]| -> Result<(), MetaError> {
        let mut stmt = conn.prepare_cached(&format!("SELECT name, oid FROM refs WHERE repo = ?1{condition}"))?;
        let rows = stmt.query_map(rusqlite::params_from_iter([name].iter().chain(values)), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (ref_name, hex) = row?;
            let oid = Oid::from_hex(&hex).ok_or_else(|| MetaError::Backend(format!("bad oid stored for {ref_name}")))?;
            refs.insert(ref_name, oid);
        }
        Ok(())
    };
    match scope {
        RefScope::All => read("", &[])?,
        RefScope::Names(names) => {
            for wanted in names.iter().chain([&head]) {
                read(" AND name = ?2", &[wanted.as_str()])?;
            }
        }
        RefScope::Prefixes(prefixes) => {
            read(" AND name = ?2", &[head.as_str()])?;
            // Names compare byte by byte here, so a prefix is a range of the primary key.
            for prefix in prefixes {
                match prefix_end(prefix) {
                    Some(end) => read(" AND name >= ?2 AND name < ?3", &[prefix.as_str(), end.as_str()])?,
                    None => read(" AND name >= ?2", &[prefix.as_str()])?,
                }
            }
        }
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

    async fn get_repo(&self, name: &str, refs: &RefScope) -> Result<Option<RepoState>, MetaError> {
        let (name, refs) = (name.to_string(), refs.clone());
        self.run(move |conn| {
            // One read transaction so refs and packs come from the same snapshot.
            let tx = conn.transaction()?;
            load_repo(&tx, &name, &refs)
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
            if let Some(payload) = &txn.event {
                tx.execute("INSERT INTO events (repo, created_at, payload) VALUES (?1, ?2, ?3)", params![name, unix_now(), payload])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    async fn delete_repo(&self, name: &str) -> Result<bool, MetaError> {
        let name = name.to_string();
        self.run(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let pool_id: Option<String> = tx.query_row("SELECT pool_id FROM repos WHERE name = ?1", [&name], |r| r.get(0)).optional()?;
            let Some(pool_id) = pool_id else { return Ok(false) };
            tx.execute(
                "INSERT OR REPLACE INTO garbage (pool_id, pack_id, removed_at) SELECT pool_id, pack_id, ?2 FROM packs WHERE pool_id = ?1",
                params![pool_id, unix_now()],
            )?;
            tx.execute("DELETE FROM packs WHERE pool_id = ?1", [&pool_id])?;
            tx.execute("DELETE FROM refs WHERE repo = ?1", [&name])?;
            tx.execute("DELETE FROM repos WHERE name = ?1", [&name])?;
            tx.commit()?;
            Ok(true)
        })
        .await
    }

    async fn list_events(&self, after: Option<&str>, limit: usize) -> Result<Vec<Event>, MetaError> {
        let after: i64 = after.map_or(Ok(0), str::parse).map_err(|_| MetaError::BadCursor)?;
        self.run(move |conn| {
            let mut stmt = conn.prepare("SELECT id, repo, created_at, payload FROM events WHERE id > ?1 ORDER BY id LIMIT ?2")?;
            let rows = stmt.query_map(params![after, limit as i64], |r| {
                Ok(Event { cursor: r.get::<_, i64>(0)?.to_string(), repo: r.get(1)?, created_at: r.get(2)?, payload: r.get(3)? })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    async fn purge_events(&self, created_before: i64) -> Result<(), MetaError> {
        self.run(move |conn| {
            conn.execute("DELETE FROM events WHERE created_at < ?1", [created_before])?;
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn conforms() {
        crate::conformance::check(Arc::new(SqliteMetaStore::open(":memory:").unwrap())).await;
    }
}
