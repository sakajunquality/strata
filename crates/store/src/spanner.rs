//! Metadata in Cloud Spanner, for several nodes (and regions) sharing one database.
//!
//! A change reads what it depends on inside a read-write transaction and
//! buffers its writes; Spanner aborts and reruns transactions that overlap,
//! which gives the same one-at-a-time behaviour per repository as a row lock.
//! Reads inside such a transaction do not see its own buffered writes, so
//! each change reads first and writes last.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use gcloud_gax::grpc::{Code, Status};
use gcloud_gax::retry::TryAs;
use gcloud_googleapis::spanner::admin::database::v1::UpdateDatabaseDdlRequest;
use gcloud_spanner::admin::client::Client as AdminClient;
use gcloud_spanner::admin::AdminClientConfig;
use gcloud_spanner::client::{Client, ClientConfig};
use gcloud_spanner::key::Key;
use gcloud_spanner::mutation::{delete, insert, insert_or_update, update};
use gcloud_spanner::session::SessionError;
use gcloud_spanner::statement::Statement;
use gcloud_spanner::transaction::Transaction;
use gcloud_spanner::transaction_rw::ReadWriteTransaction;
use strata_git::Oid;

use crate::meta::{
    decode_grants, encode_grants, final_refs, new_repo, unix_now, Garbage, MetaError, MetaStore,
    RepoState, TokenRecord, Txn,
};

/// `refs` is stored inside its repository's row range, so a repository's
/// metadata is read and written in one place. Column names avoid Spanner's
/// reserved words (`key`, `hash`).
const SCHEMA: &[&str] = &[
    "CREATE TABLE repos (
        repo         STRING(MAX) NOT NULL,
        pool_id      STRING(64) NOT NULL,
        head         STRING(MAX) NOT NULL,
        epoch        INT64 NOT NULL,
        refs_version INT64 NOT NULL
    ) PRIMARY KEY (repo)",
    "CREATE TABLE refs (
        repo STRING(MAX) NOT NULL,
        name STRING(MAX) NOT NULL,
        oid  STRING(64) NOT NULL
    ) PRIMARY KEY (repo, name), INTERLEAVE IN PARENT repos ON DELETE CASCADE",
    "CREATE TABLE packs (
        pool_id STRING(64) NOT NULL,
        pack_id STRING(64) NOT NULL,
        seq     INT64 NOT NULL
    ) PRIMARY KEY (pool_id, pack_id)",
    "CREATE TABLE garbage (
        pool_id    STRING(64) NOT NULL,
        pack_id    STRING(64) NOT NULL,
        removed_at INT64 NOT NULL
    ) PRIMARY KEY (pool_id, pack_id)",
    "CREATE TABLE leases (
        lease_key  STRING(MAX) NOT NULL,
        holder     STRING(MAX) NOT NULL,
        expires_at INT64 NOT NULL
    ) PRIMARY KEY (lease_key)",
    "CREATE TABLE tokens (
        name       STRING(MAX) NOT NULL,
        token_hash STRING(128) NOT NULL,
        grants     STRING(MAX) NOT NULL
    ) PRIMARY KEY (name)",
    "CREATE UNIQUE INDEX tokens_by_hash ON tokens (token_hash)",
];

fn backend(e: impl std::fmt::Display) -> MetaError {
    MetaError::Backend(format!("spanner: {e}"))
}

/// What can go wrong inside a transaction: our own verdicts, which end it,
/// and Spanner's, some of which (aborts) make the client run it again.
enum TxError {
    Meta(MetaError),
    Status(Status),
}

impl TryAs<Status> for TxError {
    fn try_as(&self) -> Option<&Status> {
        match self {
            TxError::Status(status) => Some(status),
            TxError::Meta(_) => None,
        }
    }
}

impl From<Status> for TxError {
    fn from(status: Status) -> Self {
        TxError::Status(status)
    }
}

impl From<SessionError> for TxError {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::GRPC(status) => TxError::Status(status),
            other => TxError::Meta(backend(other)),
        }
    }
}

impl From<MetaError> for TxError {
    fn from(e: MetaError) -> Self {
        TxError::Meta(e)
    }
}

impl From<gcloud_spanner::row::Error> for TxError {
    fn from(e: gcloud_spanner::row::Error) -> Self {
        TxError::Meta(backend(e))
    }
}

impl From<TxError> for MetaError {
    fn from(e: TxError) -> Self {
        match e {
            TxError::Meta(e) => e,
            TxError::Status(status) => {
                backend(format!("{:?}: {}", status.code(), status.message()))
            }
        }
    }
}

impl From<Status> for MetaError {
    fn from(status: Status) -> Self {
        TxError::Status(status).into()
    }
}

impl From<gcloud_spanner::client::Error> for MetaError {
    fn from(e: gcloud_spanner::client::Error) -> Self {
        backend(e)
    }
}

impl From<gcloud_spanner::row::Error> for MetaError {
    fn from(e: gcloud_spanner::row::Error) -> Self {
        backend(e)
    }
}

/// How long to keep rerunning a transaction that Spanner aborts because it
/// overlapped another. The client library gives up after a few seconds,
/// which a burst of pushes to one repository can outlast.
const ABORT_PATIENCE: Duration = Duration::from_secs(30);

type TxFuture<'tx, T> = Pin<Box<dyn Future<Output = Result<T, TxError>> + Send + 'tx>>;

pub struct SpannerMetaStore {
    client: Client,
}

impl SpannerMetaStore {
    /// Opens `projects/<p>/instances/<i>/databases/<d>` and creates the
    /// tables if the database has none of them yet.
    ///
    /// Credentials are Google's application default ones. With
    /// `SPANNER_EMULATOR_HOST` set, talks to the emulator there instead.
    pub async fn connect(database: &str) -> Result<Self, MetaError> {
        let emulated = std::env::var_os("SPANNER_EMULATOR_HOST").is_some();
        let mut config = ClientConfig::default();
        if !emulated {
            config = config.with_auth().await.map_err(backend)?;
        }
        let client = Client::new(database, config).await?;
        let store = SpannerMetaStore { client };
        if !store.has_schema().await? {
            tracing::info!(database, "creating metadata tables");
            let mut admin = AdminClientConfig::default();
            if !emulated {
                admin = admin.with_auth().await.map_err(backend)?;
            }
            let admin = AdminClient::new(admin).await.map_err(backend)?;
            let request = UpdateDatabaseDdlRequest {
                database: database.to_string(),
                statements: SCHEMA.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            };
            match admin.database().update_database_ddl(request, None).await {
                Ok(mut operation) => {
                    operation.wait(None).await?;
                }
                // Another node got there first; it is still creating them.
                Err(status)
                    if status.code() == Code::AlreadyExists
                        || status.message().contains("Duplicate name") => {}
                Err(status) => return Err(status.into()),
            }
        }
        Ok(store)
    }

    /// Runs `f` in a read-write transaction, again from the start for as
    /// long as Spanner aborts it.
    async fn transact<T, F>(&self, f: F) -> Result<T, MetaError>
    where
        F: for<'tx> Fn(&'tx mut ReadWriteTransaction) -> TxFuture<'tx, T>,
    {
        let deadline = Instant::now() + ABORT_PATIENCE;
        let mut pause = Duration::from_millis(10);
        loop {
            match self.client.read_write_transaction(&f).await {
                Ok((_, value)) => return Ok(value),
                Err(TxError::Status(status))
                    if status.code() == Code::Aborted && Instant::now() < deadline =>
                {
                    // Spread out, so that the same transactions do not collide again.
                    let jitter = uuid::Uuid::new_v4().as_u128() % pause.as_millis().max(1);
                    tokio::time::sleep(pause + Duration::from_millis(jitter as u64)).await;
                    pause = (pause * 2).min(Duration::from_millis(400));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    async fn has_schema(&self) -> Result<bool, MetaError> {
        let stmt = Statement::new(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = '' AND table_name IN ('repos', 'tokens')",
        );
        let mut tx = self.client.single().await?;
        let mut rows = tx.query(stmt).await?;
        let mut found = 0;
        while rows.next().await?.is_some() {
            found += 1;
        }
        Ok(found == 2)
    }
}

async fn read_refs(
    tx: &mut Transaction,
    repo: &str,
    only: Option<Vec<String>>,
) -> Result<BTreeMap<String, Oid>, TxError> {
    let mut stmt = match &only {
        Some(_) => Statement::new(
            "SELECT name, oid FROM refs WHERE repo = @repo AND name IN UNNEST(@names)",
        ),
        None => Statement::new("SELECT name, oid FROM refs WHERE repo = @repo"),
    };
    stmt.add_param("repo", &repo);
    if let Some(names) = &only {
        stmt.add_param("names", names);
    }
    let mut rows = tx.query(stmt).await?;
    let mut refs = BTreeMap::new();
    while let Some(row) = rows.next().await? {
        let (name, hex): (String, String) = (row.column(0)?, row.column(1)?);
        let oid =
            Oid::from_hex(&hex).ok_or_else(|| backend(format!("bad oid stored for {name}")))?;
        refs.insert(name, oid);
    }
    Ok(refs)
}

/// A pool's packs with their sequence numbers, oldest first.
async fn read_packs(tx: &mut Transaction, pool_id: &str) -> Result<Vec<(String, i64)>, TxError> {
    let mut stmt =
        Statement::new("SELECT pack_id, seq FROM packs WHERE pool_id = @pool ORDER BY seq");
    stmt.add_param("pool", &pool_id);
    let mut rows = tx.query(stmt).await?;
    let mut packs = Vec::new();
    while let Some(row) = rows.next().await? {
        packs.push((row.column(0)?, row.column(1)?));
    }
    Ok(packs)
}

#[async_trait]
impl MetaStore for SpannerMetaStore {
    async fn create_repo(&self, name: &str) -> Result<RepoState, MetaError> {
        let repo = new_repo(name);
        let row = insert(
            "repos",
            &["repo", "pool_id", "head", "epoch", "refs_version"],
            &[&repo.name, &repo.pool_id, &repo.head, &0i64, &0i64],
        );
        match self.client.apply(vec![row]).await {
            Ok(_) => Ok(repo),
            Err(gcloud_spanner::client::Error::GRPC(status))
                if status.code() == Code::AlreadyExists =>
            {
                Err(MetaError::AlreadyExists)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn get_repo(&self, name: &str) -> Result<Option<RepoState>, MetaError> {
        // One snapshot, so that refs and packs belong together.
        let mut tx = self.client.read_only_transaction().await?;
        let Some(row) = tx
            .read_row(
                "repos",
                &["pool_id", "head", "epoch", "refs_version"],
                Key::new(&name),
            )
            .await?
        else {
            return Ok(None);
        };
        let pool_id: String = row.column(0)?;
        let refs = read_refs(&mut tx, name, None).await?;
        let packs = read_packs(&mut tx, &pool_id).await?;
        Ok(Some(RepoState {
            name: name.to_string(),
            pool_id,
            head: row.column(1)?,
            refs,
            packs: packs.into_iter().map(|(id, _)| id).collect(),
            epoch: row.column::<i64>(2)? as u64,
            refs_version: row.column::<i64>(3)? as u64,
        }))
    }

    async fn list_repos(&self) -> Result<Vec<String>, MetaError> {
        let mut tx = self.client.single().await?;
        let mut rows = tx
            .query(Statement::new("SELECT repo FROM repos ORDER BY repo"))
            .await?;
        let mut names = Vec::new();
        while let Some(row) = rows.next().await? {
            names.push(row.column(0)?);
        }
        Ok(names)
    }

    async fn commit(&self, name: &str, txn: Txn) -> Result<(), MetaError> {
        self.transact(|tx| {
            let (name, txn) = (name.to_string(), txn.clone());
            Box::pin(async move {
                let row = tx
                    .read_row(
                        "repos",
                        &["pool_id", "head", "epoch", "refs_version"],
                        Key::new(&name),
                    )
                    .await?
                    .ok_or(MetaError::NotFound)?;
                let (pool_id, head): (String, String) = (row.column(0)?, row.column(1)?);
                let (epoch, refs_version): (i64, i64) = (row.column(2)?, row.column(3)?);
                if txn.expect_epoch.is_some_and(|e| e != epoch as u64)
                    || txn
                        .expect_refs_version
                        .is_some_and(|v| v != refs_version as u64)
                {
                    return Err(MetaError::Stale.into());
                }

                let mut writes = Vec::new();
                if !txn.ref_updates.is_empty() {
                    let names = txn.ref_updates.iter().map(|u| u.name.clone()).collect();
                    let current = read_refs(tx, &name, Some(names)).await?;
                    for update in &txn.ref_updates {
                        if current.get(&update.name) != update.old.as_ref() {
                            return Err(MetaError::Conflict {
                                name: update.name.clone(),
                            }
                            .into());
                        }
                    }
                    for (ref_name, new) in final_refs(&txn.ref_updates) {
                        writes.push(match new {
                            Some(oid) => insert_or_update(
                                "refs",
                                &["repo", "name", "oid"],
                                &[&name, &ref_name, &oid.to_string()],
                            ),
                            None => delete("refs", Key::composite(&[&name, &ref_name])),
                        });
                    }
                }

                if !txn.remove_packs.is_empty() || txn.add_pack.is_some() {
                    let packs: HashMap<String, i64> =
                        read_packs(tx, &pool_id).await?.into_iter().collect();
                    if txn
                        .remove_packs
                        .iter()
                        .any(|pack| !packs.contains_key(pack))
                    {
                        return Err(MetaError::PackConflict.into());
                    }
                    let now = unix_now();
                    for pack in &txn.remove_packs {
                        writes.push(delete("packs", Key::composite(&[&pool_id, pack])));
                        writes.push(insert_or_update(
                            "garbage",
                            &["pool_id", "pack_id", "removed_at"],
                            &[&pool_id, pack, &now],
                        ));
                    }
                    if let Some(pack) = txn
                        .add_pack
                        .as_ref()
                        .filter(|pack| !packs.contains_key(*pack))
                    {
                        let seq = packs.values().max().copied().unwrap_or(0) + 1;
                        writes.push(insert(
                            "packs",
                            &["pool_id", "pack_id", "seq"],
                            &[&pool_id, pack, &seq],
                        ));
                    }
                }

                let refs_changed = !txn.ref_updates.is_empty() || txn.set_head.is_some();
                writes.push(update(
                    "repos",
                    &["repo", "head", "epoch", "refs_version"],
                    &[
                        &name,
                        txn.set_head.as_ref().unwrap_or(&head),
                        &(epoch + i64::from(txn.bump_epoch)),
                        &(refs_version + i64::from(refs_changed)),
                    ],
                ));
                tx.buffer_write(writes);
                Ok(())
            })
        })
        .await
    }

    async fn list_garbage(&self, removed_before: i64) -> Result<Vec<Garbage>, MetaError> {
        let mut stmt =
            Statement::new("SELECT pool_id, pack_id FROM garbage WHERE removed_at < @before");
        stmt.add_param("before", &removed_before);
        let mut tx = self.client.single().await?;
        let mut rows = tx.query(stmt).await?;
        let mut garbage = Vec::new();
        while let Some(row) = rows.next().await? {
            garbage.push(Garbage {
                pool_id: row.column(0)?,
                pack_id: row.column(1)?,
            });
        }
        Ok(garbage)
    }

    async fn purge_garbage(&self, garbage: &Garbage) -> Result<(), MetaError> {
        let row = delete(
            "garbage",
            Key::composite(&[&garbage.pool_id, &garbage.pack_id]),
        );
        self.client.apply(vec![row]).await?;
        Ok(())
    }

    async fn try_lease(&self, key: &str, holder: &str, ttl_secs: i64) -> Result<bool, MetaError> {
        self.transact(|tx| {
            let (key, holder) = (key.to_string(), holder.to_string());
            Box::pin(async move {
                let now = unix_now();
                if let Some(row) = tx
                    .read_row("leases", &["holder", "expires_at"], Key::new(&key))
                    .await?
                {
                    let (current, expires_at): (String, i64) = (row.column(0)?, row.column(1)?);
                    if current != holder && expires_at > now {
                        return Ok(false);
                    }
                }
                let lease = insert_or_update(
                    "leases",
                    &["lease_key", "holder", "expires_at"],
                    &[&key, &holder, &(now + ttl_secs)],
                );
                tx.buffer_write(vec![lease]);
                Ok(true)
            })
        })
        .await
    }

    async fn release_lease(&self, key: &str, holder: &str) -> Result<(), MetaError> {
        self.transact(|tx| {
            let (key, holder) = (key.to_string(), holder.to_string());
            Box::pin(async move {
                if let Some(row) = tx.read_row("leases", &["holder"], Key::new(&key)).await? {
                    if row.column::<String>(0)? == holder {
                        tx.buffer_write(vec![delete("leases", Key::new(&key))]);
                    }
                }
                Ok(())
            })
        })
        .await
    }

    async fn put_token(&self, token: TokenRecord) -> Result<(), MetaError> {
        let row = insert_or_update(
            "tokens",
            &["name", "token_hash", "grants"],
            &[&token.name, &token.hash, &encode_grants(&token.grants)],
        );
        self.client.apply(vec![row]).await?;
        Ok(())
    }

    async fn find_token(&self, hash: &str) -> Result<Option<TokenRecord>, MetaError> {
        let mut stmt = Statement::new("SELECT name, grants FROM tokens WHERE token_hash = @hash");
        stmt.add_param("hash", &hash);
        let mut tx = self.client.single().await?;
        let mut rows = tx.query(stmt).await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let grants: String = row.column(1)?;
        Ok(Some(TokenRecord {
            name: row.column(0)?,
            hash: hash.to_string(),
            grants: decode_grants(&grants),
        }))
    }

    async fn delete_token(&self, name: &str) -> Result<bool, MetaError> {
        self.transact(|tx| {
            let name = name.to_string();
            Box::pin(async move {
                let existed = tx
                    .read_row("tokens", &["name"], Key::new(&name))
                    .await?
                    .is_some();
                if existed {
                    tx.buffer_write(vec![delete("tokens", Key::new(&name))]);
                }
                Ok(existed)
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Needs the emulator: set SPANNER_EMULATOR_HOST, and STRATA_TEST_SPANNER
    /// to an existing database's path (scripts/meta-test.sh does).
    #[tokio::test(flavor = "multi_thread")]
    async fn conforms() {
        let Ok(database) = std::env::var("STRATA_TEST_SPANNER") else {
            eprintln!("skipped: STRATA_TEST_SPANNER is not set");
            return;
        };
        crate::conformance::check(Arc::new(
            SpannerMetaStore::connect(&database).await.unwrap(),
        ))
        .await;
    }
}
