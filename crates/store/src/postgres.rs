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
    decode_grants, encode_grants, final_refs, new_repo, unix_now, Garbage, MetaError, MetaStore,
    RepoState, TokenRecord, Txn,
};

/// Run under an advisory lock, so that nodes starting together do not trip
/// over each other creating the same tables.
const SCHEMA: &str = "
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
    name TEXT NOT NULL,
    oid  TEXT NOT NULL,
    PRIMARY KEY (repo, name)
);
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
CREATE TABLE IF NOT EXISTS tokens (
    name   TEXT PRIMARY KEY,
    hash   TEXT NOT NULL UNIQUE,
    grants TEXT NOT NULL
);
COMMIT;
";

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

    async fn get_repo(&self, name: &str) -> Result<Option<RepoState>, MetaError> {
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
        let mut refs = BTreeMap::new();
        for row in tx
            .query("SELECT name, oid FROM refs WHERE repo = $1", &[&name])
            .await?
        {
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
            head: row.get(1),
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
        tx.commit().await?;
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

    async fn put_token(&self, token: TokenRecord) -> Result<(), MetaError> {
        self.pool
            .get()
            .await?
            .execute(
                "INSERT INTO tokens (name, hash, grants) VALUES ($1, $2, $3)
                 ON CONFLICT (name) DO UPDATE SET hash = excluded.hash, grants = excluded.grants",
                &[&token.name, &token.hash, &encode_grants(&token.grants)],
            )
            .await?;
        Ok(())
    }

    async fn find_token(&self, hash: &str) -> Result<Option<TokenRecord>, MetaError> {
        let row = self
            .pool
            .get()
            .await?
            .query_opt("SELECT name, grants FROM tokens WHERE hash = $1", &[&hash])
            .await?;
        Ok(row.map(|r| TokenRecord {
            name: r.get(0),
            hash: hash.to_string(),
            grants: decode_grants(r.get(1)),
        }))
    }

    async fn delete_token(&self, name: &str) -> Result<bool, MetaError> {
        Ok(self
            .pool
            .get()
            .await?
            .execute("DELETE FROM tokens WHERE name = $1", &[&name])
            .await?
            > 0)
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
