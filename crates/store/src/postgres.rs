//! Metadata in PostgreSQL, for several nodes sharing one database.
//!
//! Every change to a repository first locks its row in `repos`, so changes
//! to one repository happen one at a time and changes to different
//! repositories do not wait for each other.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use strata_git::Oid;
use tokio_postgres::IsolationLevel;

use crate::meta::{
    final_refs, new_repo, prefix_end, unix_now, Event, Garbage, MetaError, MetaStore, RefScope, RepoState, Txn,
};

/// Run under an advisory lock, so that nodes starting together do not trip
/// over each other creating the same tables.
const SCHEMA: &str = r#"
BEGIN;
SELECT pg_advisory_xact_lock(7305881);
CREATE TABLE IF NOT EXISTS repos (
    name         TEXT PRIMARY KEY,
    pool_id      TEXT NOT NULL,
    head         TEXT NOT NULL,
    epoch        BIGINT NOT NULL DEFAULT 0,
    refs_version BIGINT NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS refs (
    repo TEXT NOT NULL REFERENCES repos (name) ON DELETE CASCADE,
    name TEXT COLLATE "C" NOT NULL,
    oid  TEXT NOT NULL,
    PRIMARY KEY (repo, name)
);
-- Ref names are ordered byte by byte, so that the refs under a prefix are a
-- range of the primary key. Tables created before this are changed over once.
DO $$ BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = current_schema() AND table_name = 'refs' AND column_name = 'name'
            AND collation_name IS DISTINCT FROM 'C'
    ) THEN
        ALTER TABLE refs ALTER COLUMN name TYPE TEXT COLLATE "C";
    END IF;
END $$;
CREATE TABLE IF NOT EXISTS packs (
    pool_id TEXT NOT NULL,
    pack_id TEXT NOT NULL,
    seq     BIGINT NOT NULL,
    PRIMARY KEY (pool_id, pack_id)
);
CREATE TABLE IF NOT EXISTS garbage (
    pool_id    TEXT NOT NULL,
    pack_id    TEXT NOT NULL,
    removed_at BIGINT NOT NULL,
    PRIMARY KEY (pool_id, pack_id)
);
CREATE TABLE IF NOT EXISTS leases (
    key        TEXT PRIMARY KEY,
    holder     TEXT NOT NULL,
    expires_at BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    xid        xid8 NOT NULL DEFAULT pg_current_xact_id(),
    repo       TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    payload    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_feed ON events (xid, id);
COMMIT;
"#;

/// Connections kept open per node.
const POOL_SIZE: usize = 16;

fn backend(e: impl std::fmt::Display) -> MetaError {
    MetaError::Backend(e.to_string())
}

impl From<tokio_postgres::Error> for MetaError {
    fn from(e: tokio_postgres::Error) -> Self {
        // The server's own message, where there is one: the default text is just "db error".
        match e.as_db_error() {
            Some(db) => MetaError::Backend(format!("postgres: {}", db.message())),
            None => MetaError::Backend(format!("postgres: {e}")),
        }
    }
}

impl From<deadpool_postgres::PoolError> for MetaError {
    fn from(e: deadpool_postgres::PoolError) -> Self {
        MetaError::Backend(format!("postgres connection: {e}"))
    }
}

/// TLS that trusts the system's certificate authorities (and `SSL_CERT_FILE`).
fn tls() -> Result<tokio_postgres_rustls::MakeRustlsConnect, MetaError> {
    let mut roots = rustls::RootCertStore::empty();
    let found = rustls_native_certs::load_native_certs();
    for cert in found.certs {
        // Unusable certificates in the system store are not our problem.
        let _ = roots.add(cert);
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(backend)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

pub struct PostgresMetaStore {
    pool: Pool,
}

impl PostgresMetaStore {
    /// Connects to a `postgres://user:password@host/database` URL (or a
    /// `key=value` connection string) and creates the tables if they are missing.
    ///
    /// The server's certificate is verified whenever TLS is used; add
    /// `sslmode=disable` for a server without TLS, `sslmode=require` to insist on it.
    pub async fn connect(location: &str) -> Result<Self, MetaError> {
        let config = tokio_postgres::Config::from_str(location)
            .map_err(|e| backend(format!("postgres settings: {e}")))?;
        let manager = Manager::from_config(
            config,
            tls()?,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(POOL_SIZE)
            .build()
            .map_err(backend)?;
        pool.get().await?.batch_execute(SCHEMA).await?;
        Ok(PostgresMetaStore { pool })
    }
}

#[async_trait]
impl MetaStore for PostgresMetaStore {
    async fn create_repo(&self, name: &str) -> Result<RepoState, MetaError> {
        let repo = new_repo(name);
        let inserted = self
            .pool
            .get()
            .await?
            .execute(
                "INSERT INTO repos (name, pool_id, head) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                &[&repo.name, &repo.pool_id, &repo.head],
            )
            .await?;
        if inserted == 0 {
            return Err(MetaError::AlreadyExists);
        }
        Ok(repo)
    }

    async fn get_repo(&self, name: &str, scope: &RefScope) -> Result<Option<RepoState>, MetaError> {
        let mut client = self.pool.get().await?;
        // One snapshot, so that refs and packs belong together.
        let tx = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await?;
        let Some(row) = tx
            .query_opt(
                "SELECT pool_id, head, epoch, refs_version FROM repos WHERE name = $1",
                &[&name],
            )
            .await?
        else {
            return Ok(None);
        };
        let pool_id: String = row.get(0);
        let head: String = row.get(1);
        let mut found = Vec::new();
        match scope {
            RefScope::All => {
                found.push(tx.query("SELECT name, oid FROM refs WHERE repo = $1", &[&name]).await?);
            }
            RefScope::Names(names) => {
                let names: Vec<&str> = names.iter().chain([&head]).map(String::as_str).collect();
                found.push(tx.query("SELECT name, oid FROM refs WHERE repo = $1 AND name = ANY($2)", &[&name, &names]).await?);
            }
            RefScope::Prefixes(prefixes) => {
                found.push(tx.query("SELECT name, oid FROM refs WHERE repo = $1 AND name = $2", &[&name, &head]).await?);
                for prefix in prefixes {
                    found.push(match prefix_end(prefix) {
                        Some(end) => {
                            tx.query("SELECT name, oid FROM refs WHERE repo = $1 AND name >= $2 AND name < $3", &[&name, prefix, &end]).await?
                        }
                        None => tx.query("SELECT name, oid FROM refs WHERE repo = $1 AND name >= $2", &[&name, prefix]).await?,
                    });
                }
            }
        }
        let mut refs = BTreeMap::new();
        for row in found.iter().flatten() {
            let (ref_name, hex): (String, &str) = (row.get(0), row.get(1));
            let oid = Oid::from_hex(hex)
                .ok_or_else(|| backend(format!("bad oid stored for {ref_name}")))?;
            refs.insert(ref_name, oid);
        }
        let packs = tx
            .query(
                "SELECT pack_id FROM packs WHERE pool_id = $1 ORDER BY seq",
                &[&pool_id],
            )
            .await?;
        tx.commit().await?;
        Ok(Some(RepoState {
            name: name.to_string(),
            pool_id,
            head,
            refs,
            packs: packs.iter().map(|r| r.get(0)).collect(),
            epoch: row.get::<_, i64>(2) as u64,
            refs_version: row.get::<_, i64>(3) as u64,
        }))
    }

    async fn list_repos(&self) -> Result<Vec<String>, MetaError> {
        let rows = self
            .pool
            .get()
            .await?
            .query("SELECT name FROM repos ORDER BY name", &[])
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    async fn commit(&self, name: &str, txn: Txn) -> Result<(), MetaError> {
        let mut client = self.pool.get().await?;
        // Dropping `tx` on any early return rolls everything back.
        let tx = client.transaction().await?;
        let row = tx
            .query_opt(
                "SELECT pool_id, epoch, refs_version FROM repos WHERE name = $1 FOR UPDATE",
                &[&name],
            )
            .await?
            .ok_or(MetaError::NotFound)?;
        let (pool_id, epoch, refs_version): (String, i64, i64) =
            (row.get(0), row.get(1), row.get(2));
        if txn.expect_epoch.is_some_and(|e| e != epoch as u64)
            || txn
                .expect_refs_version
                .is_some_and(|v| v != refs_version as u64)
        {
            return Err(MetaError::Stale);
        }

        if !txn.ref_updates.is_empty() {
            let names: Vec<&str> = txn.ref_updates.iter().map(|u| u.name.as_str()).collect();
            let rows = tx
                .query(
                    "SELECT name, oid FROM refs WHERE repo = $1 AND name = ANY($2)",
                    &[&name, &names],
                )
                .await?;
            let current: HashMap<String, String> =
                rows.iter().map(|r| (r.get(0), r.get(1))).collect();
            for update in &txn.ref_updates {
                if current.get(&update.name) != update.old.map(|oid| oid.to_string()).as_ref() {
                    return Err(MetaError::Conflict {
                        name: update.name.clone(),
                    });
                }
            }
            let (mut set_names, mut set_oids, mut deleted) = (Vec::new(), Vec::new(), Vec::new());
            for (ref_name, new) in final_refs(&txn.ref_updates) {
                match new {
                    Some(oid) => {
                        set_names.push(ref_name);
                        set_oids.push(oid.to_string());
                    }
                    None => deleted.push(ref_name),
                }
            }
            if !set_names.is_empty() {
                tx.execute(
                    "INSERT INTO refs (repo, name, oid) SELECT $1, * FROM unnest($2::text[], $3::text[])
                     ON CONFLICT (repo, name) DO UPDATE SET oid = excluded.oid",
                    &[&name, &set_names, &set_oids],
                )
                .await?;
            }
            if !deleted.is_empty() {
                tx.execute(
                    "DELETE FROM refs WHERE repo = $1 AND name = ANY($2)",
                    &[&name, &deleted],
                )
                .await?;
            }
        }

        if !txn.remove_packs.is_empty() {
            let mut removed = txn.remove_packs.clone();
            removed.sort();
            removed.dedup();
            let gone = tx
                .execute(
                    "DELETE FROM packs WHERE pool_id = $1 AND pack_id = ANY($2)",
                    &[&pool_id, &removed],
                )
                .await?;
            if gone != removed.len() as u64 {
                return Err(MetaError::PackConflict);
            }
            tx.execute(
                "INSERT INTO garbage (pool_id, pack_id, removed_at) SELECT $1, pack_id, $3 FROM unnest($2::text[]) AS pack_id
                 ON CONFLICT (pool_id, pack_id) DO UPDATE SET removed_at = excluded.removed_at",
                &[&pool_id, &removed, &unix_now()],
            )
            .await?;
        }
        if let Some(pack) = &txn.add_pack {
            // The repository's row is locked, so nobody else is numbering packs of this pool.
            tx.execute(
                "INSERT INTO packs (pool_id, pack_id, seq)
                 SELECT $1, $2, COALESCE(MAX(seq), 0) + 1 FROM packs WHERE pool_id = $1
                 ON CONFLICT DO NOTHING",
                &[&pool_id, pack],
            )
            .await?;
        }
        let refs_changed = !txn.ref_updates.is_empty() || txn.set_head.is_some();
        tx.execute(
            "UPDATE repos SET head = COALESCE($1, head), epoch = epoch + $2, refs_version = refs_version + $3 WHERE name = $4",
            &[&txn.set_head, &i64::from(txn.bump_epoch), &i64::from(refs_changed), &name],
        )
        .await?;
        if let Some(payload) = &txn.event {
            tx.execute("INSERT INTO events (repo, created_at, payload) VALUES ($1, $2, $3)", &[&name, &unix_now(), payload]).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn delete_repo(&self, name: &str) -> Result<bool, MetaError> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        // The same lock commits take, so none is half-way through.
        let Some(row) = tx.query_opt("SELECT pool_id FROM repos WHERE name = $1 FOR UPDATE", &[&name]).await? else {
            return Ok(false);
        };
        let pool_id: String = row.get(0);
        tx.execute(
            "INSERT INTO garbage (pool_id, pack_id, removed_at) SELECT pool_id, pack_id, $2 FROM packs WHERE pool_id = $1
             ON CONFLICT (pool_id, pack_id) DO UPDATE SET removed_at = excluded.removed_at",
            &[&pool_id, &unix_now()],
        )
        .await?;
        tx.execute("DELETE FROM packs WHERE pool_id = $1", &[&pool_id]).await?;
        // Its refs go with it (ON DELETE CASCADE).
        tx.execute("DELETE FROM repos WHERE name = $1", &[&name]).await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn list_events(&self, after: Option<&str>, limit: usize) -> Result<Vec<Event>, MetaError> {
        // A cursor is "<transaction id>.<row id>".
        let (after_xid, after_id) = match after {
            None => (String::from("0"), 0i64),
            Some(cursor) => {
                let (xid, id) = cursor.split_once('.').ok_or(MetaError::BadCursor)?;
                let xid: u64 = xid.parse().map_err(|_| MetaError::BadCursor)?;
                (xid.to_string(), id.parse().map_err(|_| MetaError::BadCursor)?)
            }
        };
        // Row ids are handed out before commit, so a row can appear behind one
        // already read. Transactions older than every one still running are
        // settled, though: reading only those, ordered by transaction, never
        // skips an event. A long-running transaction elsewhere delays the feed.
        let rows = self
            .pool
            .get()
            .await?
            .query(
                "SELECT xid::text, id, repo, created_at, payload FROM events
                 WHERE xid < pg_snapshot_xmin(pg_current_snapshot()) AND (xid, id) > ($1::text::xid8, $2)
                 ORDER BY xid, id LIMIT $3",
                &[&after_xid, &after_id, &(limit as i64)],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| Event {
                cursor: format!("{}.{}", r.get::<_, String>(0), r.get::<_, i64>(1)),
                repo: r.get(2),
                created_at: r.get(3),
                payload: r.get(4),
            })
            .collect())
    }

    async fn purge_events(&self, created_before: i64) -> Result<(), MetaError> {
        self.pool.get().await?.execute("DELETE FROM events WHERE created_at < $1", &[&created_before]).await?;
        Ok(())
    }

    async fn list_garbage(&self, removed_before: i64) -> Result<Vec<Garbage>, MetaError> {
        let rows = self
            .pool
            .get()
            .await?
            .query(
                "SELECT pool_id, pack_id FROM garbage WHERE removed_at < $1",
                &[&removed_before],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| Garbage {
                pool_id: r.get(0),
                pack_id: r.get(1),
            })
            .collect())
    }

    async fn purge_garbage(&self, garbage: &Garbage) -> Result<(), MetaError> {
        self.pool
            .get()
            .await?
            .execute(
                "DELETE FROM garbage WHERE pool_id = $1 AND pack_id = $2",
                &[&garbage.pool_id, &garbage.pack_id],
            )
            .await?;
        Ok(())
    }

    async fn try_lease(&self, key: &str, holder: &str, ttl_secs: i64) -> Result<bool, MetaError> {
        let now = unix_now();
        // Takes the lease if it is free, expired, or already ours.
        let taken = self
            .pool
            .get()
            .await?
            .execute(
                "INSERT INTO leases (key, holder, expires_at) VALUES ($1, $2, $3)
                 ON CONFLICT (key) DO UPDATE SET holder = excluded.holder, expires_at = excluded.expires_at
                 WHERE leases.holder = excluded.holder OR leases.expires_at <= $4",
                &[&key, &holder, &(now + ttl_secs), &now],
            )
            .await?;
        Ok(taken > 0)
    }

    async fn release_lease(&self, key: &str, holder: &str) -> Result<(), MetaError> {
        self.pool
            .get()
            .await?
            .execute(
                "DELETE FROM leases WHERE key = $1 AND holder = $2",
                &[&key, &holder],
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a server: set STRATA_TEST_POSTGRES to its URL (scripts/meta-test.sh does).
    #[tokio::test(flavor = "multi_thread")]
    async fn conforms() {
        let Ok(location) = std::env::var("STRATA_TEST_POSTGRES") else {
            eprintln!("skipped: STRATA_TEST_POSTGRES is not set");
            return;
        };
        crate::conformance::check(Arc::new(
            PostgresMetaStore::connect(&location).await.unwrap(),
        ))
        .await;
    }
}
