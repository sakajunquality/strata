//! Metadata in PostgreSQL, for several nodes sharing one database.
//!
//! Every change to a repository first locks its row in `repos`, so changes
//! to one repository happen one at a time and changes to different
//! repositories do not wait for each other.

use std::collections::BTreeMap;
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
-- One change to a repository, checked and made in one call: its row stays
-- locked for as long as the statements below take, not for as many trips
-- between a node and the database as there are statements. Every check
-- comes before the first write, so an answer other than 'ok' changed nothing.
CREATE OR REPLACE FUNCTION strata_commit(
    p_repo text, p_expect_epoch bigint, p_expect_refs_version bigint,
    p_names text[], p_old text[], p_set_names text[], p_set_oids text[], p_deleted text[],
    p_remove_packs text[], p_add_pack text, p_set_head text, p_bump_epoch boolean,
    p_refs_changed boolean, p_event text, p_now bigint
) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    v_pool text;
    v_epoch bigint;
    v_refs_version bigint;
    v_name text;
BEGIN
    SELECT pool_id, epoch, refs_version INTO v_pool, v_epoch, v_refs_version FROM repos WHERE name = p_repo FOR UPDATE;
    IF NOT FOUND THEN
        RETURN 'missing';
    END IF;
    IF (p_expect_epoch IS NOT NULL AND p_expect_epoch <> v_epoch)
        OR (p_expect_refs_version IS NOT NULL AND p_expect_refs_version <> v_refs_version) THEN
        RETURN 'stale';
    END IF;
    SELECT u.name INTO v_name
    FROM unnest(p_names, p_old) WITH ORDINALITY AS u(name, old, at)
    LEFT JOIN refs r ON r.repo = p_repo AND r.name = u.name
    WHERE r.oid IS DISTINCT FROM u.old
    ORDER BY u.at LIMIT 1;
    IF FOUND THEN
        RETURN 'conflict ' || v_name;
    END IF;
    IF (SELECT count(*) FROM packs WHERE pool_id = v_pool AND pack_id = ANY(p_remove_packs)) <> cardinality(p_remove_packs) THEN
        RETURN 'packs';
    END IF;

    INSERT INTO refs (repo, name, oid) SELECT p_repo, * FROM unnest(p_set_names, p_set_oids)
        ON CONFLICT (repo, name) DO UPDATE SET oid = excluded.oid;
    DELETE FROM refs WHERE repo = p_repo AND name = ANY(p_deleted);
    DELETE FROM packs WHERE pool_id = v_pool AND pack_id = ANY(p_remove_packs);
    INSERT INTO garbage (pool_id, pack_id, removed_at) SELECT v_pool, pack_id, p_now FROM unnest(p_remove_packs) AS pack_id
        ON CONFLICT (pool_id, pack_id) DO UPDATE SET removed_at = excluded.removed_at;
    IF p_add_pack IS NOT NULL THEN
        -- The repository's row is locked, so nobody else is numbering packs of this pool.
        INSERT INTO packs (pool_id, pack_id, seq)
            SELECT v_pool, p_add_pack, COALESCE(MAX(seq), 0) + 1 FROM packs WHERE pool_id = v_pool
            ON CONFLICT DO NOTHING;
    END IF;
    UPDATE repos SET head = COALESCE(p_set_head, head), epoch = epoch + p_bump_epoch::int, refs_version = refs_version + p_refs_changed::int
        WHERE name = p_repo;
    IF p_event IS NOT NULL THEN
        INSERT INTO events (repo, created_at, payload) VALUES (p_repo, p_now, p_event);
    END IF;
    RETURN 'ok';
END $$;
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
        // The three are sent together and answered together. The ref HEAD
        // points at comes whatever was asked for.
        const HEAD: &str = "(SELECT head FROM repos WHERE name = $1)";
        let (names, low, high);
        let (sql, params): (String, Vec<&(dyn tokio_postgres::types::ToSql + Sync)>) = match scope {
            RefScope::All => ("SELECT name, oid FROM refs WHERE repo = $1".into(), vec![&name]),
            RefScope::Names(wanted) => {
                names = wanted.iter().map(String::as_str).collect::<Vec<&str>>();
                (format!("SELECT name, oid FROM refs WHERE repo = $1 AND (name = ANY($2) OR name = {HEAD})"), vec![&name, &names])
            }
            RefScope::Prefixes(prefixes) => {
                // Names are ordered byte by byte, so a prefix is a range of the primary key.
                low = prefixes.iter().map(String::as_str).collect::<Vec<&str>>();
                high = prefixes.iter().map(|prefix| prefix_end(prefix)).collect::<Vec<Option<String>>>();
                let sql = format!(
                    "SELECT r.name, r.oid FROM unnest($2::text[], $3::text[]) AS p(low, high)
                     JOIN refs r ON r.repo = $1 AND r.name >= p.low AND (p.high IS NULL OR r.name < p.high)
                     UNION ALL SELECT name, oid FROM refs WHERE repo = $1 AND name = {HEAD}"
                );
                (sql, vec![&name, &low, &high])
            }
        };
        let by_name: [&(dyn tokio_postgres::types::ToSql + Sync); 1] = [&name];
        let (row, refs, packs) = tokio::try_join!(
            tx.query_opt("SELECT pool_id, head, epoch, refs_version FROM repos WHERE name = $1", &by_name),
            tx.query(&sql, &params),
            tx.query("SELECT pack_id FROM packs WHERE pool_id = (SELECT pool_id FROM repos WHERE name = $1) ORDER BY seq", &by_name),
        )?;
        let Some(row) = row else { return Ok(None) };
        let mut found = BTreeMap::new();
        for row in &refs {
            let (ref_name, hex): (String, &str) = (row.get(0), row.get(1));
            let oid = Oid::from_hex(hex).ok_or_else(|| backend(format!("bad oid stored for {ref_name}")))?;
            found.insert(ref_name, oid);
        }
        tx.commit().await?;
        Ok(Some(RepoState {
            name: name.to_string(),
            pool_id: row.get(0),
            head: row.get(1),
            refs: found,
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
        let names: Vec<&str> = txn.ref_updates.iter().map(|u| u.name.as_str()).collect();
        let old: Vec<Option<String>> = txn.ref_updates.iter().map(|u| u.old.map(|oid| oid.to_string())).collect();
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
        let mut removed = txn.remove_packs.clone();
        removed.sort();
        removed.dedup();
        let refs_changed = !txn.ref_updates.is_empty() || txn.set_head.is_some();
        let row = self
            .pool
            .get()
            .await?
            .query_one(
                "SELECT strata_commit($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
                &[
                    &name,
                    &txn.expect_epoch.map(|epoch| epoch as i64),
                    &txn.expect_refs_version.map(|version| version as i64),
                    &names,
                    &old,
                    &set_names,
                    &set_oids,
                    &deleted,
                    &removed,
                    &txn.add_pack,
                    &txn.set_head,
                    &txn.bump_epoch,
                    &refs_changed,
                    &txn.event,
                    &unix_now(),
                ],
            )
            .await?;
        match row.get::<_, &str>(0) {
            "ok" => Ok(()),
            "missing" => Err(MetaError::NotFound),
            "stale" => Err(MetaError::Stale),
            "packs" => Err(MetaError::PackConflict),
            other => match other.strip_prefix("conflict ") {
                Some(name) => Err(MetaError::Conflict { name: name.to_string() }),
                None => Err(backend(format!("unexpected answer to a commit: {other}"))),
            },
        }
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
