//! Background maintenance: merging packs, dropping unreachable objects, and
//! deleting packs nothing refers to any more.

use std::collections::{HashMap, HashSet};
use std::io::BufWriter;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use strata_git::compact::{self, BitmapRequest, CompactOptions, Compacted};
use strata_git::walk::{self, Filter, ShallowPlan};
use strata_git::{Oid, PackSet};
use strata_store::{MetaError, PackStore, RepoState, Txn};
use tokio::sync::Notify;

use crate::App;

/// How long a node may hold a repository's compaction to itself. A compaction
/// that outlives this can be duplicated by another node; one of the two then
/// loses at commit and discards its work.
const COMPACTION_LEASE_SECS: i64 = 3600;
const SWEEP_LEASE_SECS: i64 = 600;
/// Times a pruning compaction re-checks refs that keep moving before giving up.
const COMMIT_ATTEMPTS: usize = 5;

/// Repositories waiting to be compacted. A repository pushed to many times
/// while it waits is still compacted once.
#[derive(Default)]
pub(crate) struct Queue {
    pending: Mutex<HashSet<String>>,
    wake: Notify,
}

impl App {
    pub(crate) fn request_compaction(&self, repo: &str) {
        self.compactions.pending.lock().unwrap().insert(repo.to_string());
        self.compactions.wake.notify_one();
    }
}

pub struct CompactionReport {
    /// Number of packs replaced by one.
    pub merged: usize,
    /// Number of objects in the new pack.
    pub objects: usize,
    /// Whether the whole pool was rewritten.
    pub full: bool,
    /// Whether unreachable objects were left out.
    pub pruned: bool,
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Merges packs of a repository's pool into one.
///
/// With `full`, every pack is merged. Otherwise only as many of the smallest
/// packs as needed to keep pack sizes in geometric progression, so that the
/// bulk of a large repository is rewritten rarely.
///
/// A merge of the whole pool also drops objects no ref reaches (unless
/// pruning is switched off) and stores changed files as deltas against their
/// previous versions.
///
/// Returns `None` when there was nothing to do or when the attempt was
/// abandoned because the repository changed underneath; neither is an error.
pub async fn compact(app: &App, name: &str, full: bool) -> anyhow::Result<Option<CompactionReport>> {
    let lease = format!("compact/{name}");
    if !app.meta.try_lease(&lease, &app.node_id, COMPACTION_LEASE_SECS).await? {
        return Ok(None);
    }
    let result = compact_leased(app, name, full).await;
    if let Err(e) = app.meta.release_lease(&lease, &app.node_id).await {
        tracing::warn!(repo = name, "releasing compaction lease failed: {e}");
    }
    result
}

async fn compact_leased(app: &App, name: &str, full: bool) -> anyhow::Result<Option<CompactionReport>> {
    let Some(repo) = app.meta.get_repo(name).await? else { return Ok(None) };
    let files = app.packs.open_each(&repo.pool_id, &repo.packs).await?;

    let mut chosen: Vec<usize> = if full {
        (0..files.len()).collect()
    } else {
        let sizes: Vec<usize> = files.iter().map(|file| file.index().len()).collect();
        compact::geometric_selection(&sizes)
    };
    chosen.sort_unstable();
    let whole_pool = chosen.len() == files.len();
    // A lone pack is still worth rewriting once, to prune it and give it bitmaps.
    let lone_without_bitmaps = full && files.len() == 1 && files[0].bitmaps().is_none();
    if chosen.len() < 2 && !lone_without_bitmaps {
        return Ok(None);
    }

    let sources = PackSet::new(chosen.iter().map(|i| files[*i].clone()).collect());
    let rest = PackSet::new((0..files.len()).filter(|i| !chosen.contains(i)).map(|i| files[i].clone()).collect());
    let replaced: Vec<String> = chosen.iter().map(|i| repo.packs[*i].clone()).collect();
    let tips: Vec<Oid> = repo.refs.values().copied().collect();
    let bitmaps = BitmapRequest { tips: tips.clone(), mainline: repo.refs.get(&repo.head).copied() };
    let try_prune = whole_pool && app.options.prune;

    let pack_id = PackStore::new_pack_id();
    let upload = app.packs.upload(&repo.pool_id, &pack_id).await?;
    let (compacted, pruned) = tokio::task::spawn_blocking(move || -> anyhow::Result<(Compacted, bool)> {
        // What the refs reach, found the same way a clone of every ref would be.
        let reachable = if try_prune {
            match walk::plan_fetch(&sources, &tips, &[], &ShallowPlan::default(), &Filter::None) {
                Ok(plan) => Some(plan),
                Err(e) => {
                    // A ref without its history: nothing safe to conclude, so keep everything.
                    tracing::warn!("not pruning, reachability walk failed: {e}");
                    None
                }
            }
        } else {
            None
        };
        let walked: HashSet<Oid> = reachable.iter().flat_map(|plan| &plan.objects).map(|(oid, _)| *oid).collect();
        let hints: HashMap<Oid, Oid> =
            reachable.iter().flat_map(|plan| &plan.objects).filter_map(|(oid, base)| Some((*oid, (*base)?))).collect();
        let keep = |oid: &Oid| walked.contains(oid) || reachable.as_ref().is_some_and(|plan| plan.in_bitmaps(oid));
        let options = CompactOptions {
            keep: reachable.is_some().then_some(&keep as &dyn Fn(&Oid) -> bool),
            delta_hints: reachable.is_some().then_some(&hints),
            bitmaps: Some(&bitmaps),
            rest: (!whole_pool).then_some(&rest),
        };
        let out = BufWriter::with_capacity(1 << 20, upload);
        let (out, compacted) = compact::compact(&sources, out, &options)?;
        out.into_inner().map_err(|e| e.into_error()).context("flushing pack upload")?.finish()?;
        Ok((compacted, reachable.is_some()))
    })
    .await??;

    let objects = compacted.index.len();
    app.packs
        .finish_pack(&repo.pool_id, &pack_id, compacted.size, compacted.index, Some(compacted.graph), compacted.bitmaps)
        .await?;

    for _ in 0..COMMIT_ATTEMPTS {
        let mut txn = Txn { add_pack: Some(pack_id.clone()), remove_packs: replaced.clone(), ..Txn::default() };
        if pruned {
            // Objects are about to disappear. Pushes that validated themselves
            // against the old pool must notice (the epoch), and refs that moved
            // since the reachability walk must not depend on anything dropped.
            let Some(current) = app.meta.get_repo(name).await? else { break };
            if current.epoch != repo.epoch || !still_complete(app, &repo, &current, &pack_id).await? {
                tracing::warn!(repo = name, "abandoning pruning compaction: the repository moved onto pruned objects");
                break;
            }
            txn.bump_epoch = true;
            txn.expect_epoch = Some(repo.epoch);
            txn.expect_refs_version = Some(current.refs_version);
        }
        match app.meta.commit(name, txn).await {
            Ok(()) => {
                let merged = replaced.len();
                tracing::info!(repo = name, merged, objects, full = whole_pool, pruned, "compacted");
                return Ok(Some(CompactionReport { merged, objects, full: whole_pool, pruned }));
            }
            // Refs moved between the check and the commit: check again.
            Err(MetaError::Stale) => continue,
            // Someone else replaced these packs first.
            Err(MetaError::PackConflict) => break,
            Err(e) => return Err(e.into()),
        }
    }
    // The pack just written was never visible.
    app.packs.delete(&repo.pool_id, &pack_id).await?;
    Ok(None)
}

/// Whether every ref that changed since `before` is fully present in the
/// pruned pack plus the packs pushed since.
async fn still_complete(app: &App, before: &RepoState, current: &RepoState, pruned_pack: &str) -> anyhow::Result<bool> {
    let known: HashSet<&Oid> = before.refs.values().collect();
    let moved: Vec<Oid> = current.refs.values().filter(|oid| !known.contains(oid)).copied().collect();
    if moved.is_empty() {
        return Ok(true);
    }
    let since: Vec<String> = current.packs.iter().filter(|id| !before.packs.contains(id)).cloned().collect();
    let pruned = app.packs.open(&before.pool_id, &[pruned_pack.to_string()]).await?;
    let all = app.packs.open(&before.pool_id, &since).await?.merged_with(&pruned);
    // The pruned pack is closed under reachability, so the walk can stop there.
    Ok(tokio::task::spawn_blocking(move || walk::check_connected(&all, &pruned, &moved).is_ok()).await?)
}

/// Deletes packs that compaction replaced longer ago than the grace period.
/// Returns how many were deleted.
pub async fn sweep(app: &App) -> anyhow::Result<usize> {
    let cutoff = unix_now() - app.options.gc_grace.as_secs() as i64 + 1;
    let garbage = app.meta.list_garbage(cutoff).await?;
    for pack in &garbage {
        app.packs.delete(&pack.pool_id, &pack.pack_id).await?;
        app.meta.purge_garbage(pack).await?;
    }
    Ok(garbage.len())
}

/// Deletes stored packs that no manifest and no garbage list mentions: what
/// pushes and compactions leave behind when they fail before committing.
/// Returns how many were deleted.
///
/// Such a pack is indistinguishable from one still being written, so only
/// packs older than [`crate::Options::orphan_grace`] are touched.
pub async fn sweep_orphans(app: &App) -> anyhow::Result<usize> {
    let cutoff = unix_now() - app.options.orphan_grace.as_secs() as i64 + 1;
    let garbage = app.meta.list_garbage(i64::MAX).await?;
    let mut deleted = 0;
    for name in app.meta.list_repos().await? {
        let Some(repo) = app.meta.get_repo(&name).await? else { continue };
        let stored = app.packs.list_stored(&repo.pool_id).await?;
        // Read the manifest after the listing: a pack committed in between is then seen as known.
        let Some(repo) = app.meta.get_repo(&name).await? else { continue };
        for (pack_id, written) in stored {
            let known = repo.packs.contains(&pack_id) || garbage.iter().any(|g| g.pool_id == repo.pool_id && g.pack_id == pack_id);
            if !known && written < cutoff {
                app.packs.delete(&repo.pool_id, &pack_id).await?;
                deleted += 1;
            }
        }
    }
    Ok(deleted)
}

/// Starts the maintenance loops: compaction of repositories that pushes have
/// flagged, and periodic deletion of packs nothing refers to.
///
/// Safe to run on every node: leases in the metadata store keep nodes from
/// duplicating each other's work.
pub fn spawn_background(app: Arc<App>, sweep_every: Duration) {
    let compactor = app.clone();
    tokio::spawn(async move {
        loop {
            compactor.compactions.wake.notified().await;
            let repos: Vec<String> = compactor.compactions.pending.lock().unwrap().drain().collect();
            for repo in repos {
                if let Err(e) = compact(&compactor, &repo, false).await {
                    tracing::error!(repo, "compaction failed: {e:#}");
                }
            }
        }
    });
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(sweep_every);
        loop {
            ticks.tick().await;
            match app.meta.try_lease("sweep", &app.node_id, SWEEP_LEASE_SECS).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::error!("claiming the sweep lease failed: {e}");
                    continue;
                }
            }
            match sweep(&app).await {
                Ok(0) => {}
                Ok(deleted) => tracing::info!(deleted, "deleted replaced packs"),
                Err(e) => tracing::error!("sweeping replaced packs failed: {e:#}"),
            }
            match sweep_orphans(&app).await {
                Ok(0) => {}
                Ok(deleted) => tracing::info!(deleted, "deleted orphaned packs"),
                Err(e) => tracing::error!("sweeping orphaned packs failed: {e:#}"),
            }
        }
    });
}
