//! Push side. Every push is applied as one atomic metadata transaction.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, BodyDataStream};
use bytes::{Bytes, BytesMut};
use strata_git::pack::index_pack;
use strata_git::pktline::{self, Pkt, Reader};
use strata_git::{walk, ObjectSource, Oid, PackData, PackFile, PackSet};
use strata_store::{MetaError, PackStore, RefScope, RefUpdate, RepoState, Txn};
use tokio::io::AsyncWriteExt;
use tokio_stream::StreamExt;

use crate::auth::Granted;
use crate::upload_pack::AGENT;
use crate::{blocking, App, HttpError};

/// Upper bound on the ref-update commands of one push.
const MAX_COMMANDS: usize = 16 << 20;
/// Times a push is re-validated when the pool is pruned underneath it.
const PUSH_ATTEMPTS: usize = 3;

/// `no-thin` is deliberately absent: thin packs are completed on receipt.
fn capabilities() -> String {
    format!("report-status delete-refs ofs-delta atomic object-format=sha1 agent={AGENT}")
}

pub(crate) fn advertise(repo: Option<&RepoState>) -> Vec<u8> {
    let caps = capabilities();
    let mut out = Vec::new();
    pktline::write(&mut out, "# service=git-receive-pack\n");
    pktline::flush(&mut out);

    let mut refs = repo.into_iter().flat_map(|r| &r.refs);
    match refs.next() {
        Some((name, oid)) => pktline::write(&mut out, format!("{oid} {name}\0{caps}\n")),
        None => pktline::write(&mut out, format!("{} capabilities^{{}}\0{caps}\n", Oid::ZERO)),
    }
    for (name, oid) in refs {
        pktline::write(&mut out, format!("{oid} {name}\n"));
    }
    pktline::flush(&mut out);
    out
}

struct Command {
    old: Oid,
    new: Oid,
    name: String,
}

/// Why a push was refused. Pushes are all-or-nothing, so one failure rejects every ref.
enum Rejection {
    Unpack(String),
    Ref { name: String, reason: String },
    All(String),
}

fn parse_commands(body: &[u8]) -> Result<(Vec<Command>, Vec<String>), HttpError> {
    let bad = || HttpError::bad_request("malformed push request");
    let mut reader = Reader::new(body);
    let mut commands = Vec::new();
    let mut caps = Vec::new();
    loop {
        match reader.read().map_err(|_| bad())? {
            Some(Pkt::Data(line)) => {
                let mut parts = pktline::text(line).splitn(2, |b| *b == 0);
                let command = std::str::from_utf8(parts.next().unwrap_or_default()).map_err(|_| bad())?;
                if commands.is_empty() {
                    if let Some(c) = parts.next() {
                        caps = String::from_utf8_lossy(c).split(' ').map(str::to_string).collect();
                    }
                }
                if command.starts_with("shallow ") {
                    // A shallow clone announcing where its history stops. Nothing to
                    // record: the connectivity check below refuses the push unless
                    // the server already has everything the client left out.
                    continue;
                }
                let mut fields = command.splitn(3, ' ');
                let old = fields.next().and_then(Oid::from_hex).ok_or_else(bad)?;
                let new = fields.next().and_then(Oid::from_hex).ok_or_else(bad)?;
                let name = fields.next().ok_or_else(bad)?.to_string();
                commands.push(Command { old, new, name });
            }
            Some(Pkt::Flush) | None => break,
            Some(_) => return Err(bad()),
        }
    }
    Ok((commands, caps))
}

/// A conservative subset of git's ref name rules.
pub(crate) fn valid_ref_name(name: &str) -> bool {
    name.starts_with("refs/")
        && name.len() <= 1024
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && name.split('/').all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
        && !name.bytes().any(|b| b <= b' ' || b == 0x7f || b"~^:?*[\\".contains(&b))
}

/// Finds where the command list ends (just past its flush packet), or
/// `None` if `buf` does not hold all of it yet.
fn commands_end(buf: &[u8]) -> Result<Option<usize>, HttpError> {
    let mut pos = 0;
    loop {
        let Some(header) = buf.get(pos..pos + 4) else { return Ok(None) };
        let len = std::str::from_utf8(header)
            .ok()
            .and_then(|h| usize::from_str_radix(h, 16).ok())
            .ok_or_else(|| HttpError::bad_request("malformed push request"))?;
        match len {
            0 => return Ok(Some(pos + 4)),
            1..=3 => return Err(HttpError::bad_request("malformed push request")),
            _ if buf.len() < pos + len => return Ok(None),
            _ => pos += len,
        }
    }
}

/// A pack spooled to a scratch file and mapped into memory: read wherever
/// the indexer asks, without copying, and paged in and out by the kernel as
/// memory allows.
pub(crate) struct SpooledPack {
    data: Bytes,
}

impl SpooledPack {
    fn open(file: &std::fs::File) -> std::io::Result<SpooledPack> {
        // Safe as long as nobody else writes the file: it has no name, and
        // this request holds the only handle.
        let map = unsafe { memmap2::Mmap::map(file)? };
        Ok(SpooledPack { data: Bytes::from_owner(map) })
    }
}

impl PackData for SpooledPack {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn chunk(&self, offset: u64) -> strata_git::Result<Bytes> {
        let start = (offset as usize).min(self.data.len());
        Ok(self.data.slice(start..))
    }
}

/// Writes the rest of the request to an anonymous scratch file, so that a
/// push of any size costs disk rather than memory. Returns `None` if there
/// was no pack data.
pub(crate) async fn spool(app: &App, first: Bytes, mut body: BodyDataStream) -> Result<Option<SpooledPack>, HttpError> {
    let dir = app.options.scratch_dir.clone();
    let mut file = tokio::fs::File::from_std(blocking(move || tempfile::tempfile_in(dir)).await??);
    let mut size = first.len() as u64;
    file.write_all(&first).await?;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| HttpError::bad_request("push was interrupted"))?;
        size += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    if size == 0 {
        return Ok(None);
    }
    let file = file.into_std().await;
    Ok(Some(blocking(move || SpooledPack::open(&file)).await??))
}

pub(crate) async fn handle(app: &App, granted: &Granted, body: Body) -> Result<Vec<u8>, HttpError> {
    let name = granted.repo.as_str();
    let mut body = body.into_data_stream();
    let mut head = BytesMut::new();
    let end = loop {
        if let Some(end) = commands_end(&head)? {
            break end;
        }
        if head.len() > MAX_COMMANDS {
            return Err(HttpError::bad_request("too many ref updates in one push"));
        }
        match body.next().await {
            Some(chunk) => head.extend_from_slice(&chunk.map_err(|_| HttpError::bad_request("push was interrupted"))?),
            None if head.is_empty() => return Ok(Vec::new()),
            None => return Err(HttpError::bad_request("malformed push request")),
        }
    };
    let head = head.freeze();
    let (commands, caps) = parse_commands(&head[..end])?;
    if commands.is_empty() {
        // git probes with a bare flush packet before sending a large chunked push.
        return Ok(Vec::new());
    }
    // A push is about the refs it names, however many the repository has.
    let scope = RefScope::Names(commands.iter().map(|c| c.name.clone()).collect());
    let repo = app.meta.get_repo(name, &scope).await?;
    if repo.is_none() && !app.options.auto_create {
        return Err(HttpError::not_found());
    }
    let pack: Option<Arc<dyn PackData>> = match spool(app, head.slice(end..), body).await? {
        Some(spooled) => Some(Arc::new(spooled)),
        None => None,
    };
    let mut repo = match repo {
        Some(repo) => repo,
        None => match app.meta.create_repo(name).await {
            Ok(repo) => repo,
            // Lost a race with another first push.
            Err(MetaError::AlreadyExists) => app.meta.get_repo(name, &scope).await?.ok_or(MetaError::NotFound)?,
            Err(e) => return Err(e.into()),
        },
    };

    hold_back(app, &scope, &mut repo).await?;

    // If objects were pruned from the pool while this push was being checked,
    // the checks are void: run them again against the pool as it is now.
    let mut uploaded = None;
    let mut result = Err(Rejection::All("the repository is being reorganised, try again".into()));
    for _ in 0..PUSH_ATTEMPTS {
        match apply(app, &repo, granted.actor.as_deref(), &commands, pack.clone(), &mut uploaded).await? {
            Some(outcome) => {
                result = outcome;
                break;
            }
            None => repo = app.meta.get_repo(name, &scope).await?.ok_or(MetaError::NotFound)?,
        }
    }
    if let Err(Rejection::Unpack(reason) | Rejection::All(reason) | Rejection::Ref { reason, .. }) = &result {
        tracing::info!(repo = name, "push rejected: {reason}");
    }

    let mut out = Vec::new();
    if caps.iter().any(|c| c == "report-status") {
        match &result {
            Err(Rejection::Unpack(reason)) => pktline::write(&mut out, format!("unpack {}\n", one_line(reason))),
            _ => pktline::write(&mut out, "unpack ok\n"),
        }
        for command in &commands {
            let line = match &result {
                Ok(()) => format!("ok {}\n", command.name),
                Err(Rejection::Unpack(_)) => format!("ng {} unpacker error\n", command.name),
                Err(Rejection::All(reason)) => format!("ng {} {}\n", command.name, one_line(reason)),
                Err(Rejection::Ref { name, reason }) if *name == command.name => {
                    format!("ng {} {}\n", command.name, one_line(reason))
                }
                Err(Rejection::Ref { .. }) => format!("ng {} atomic push failed\n", command.name),
            };
            pktline::write(&mut out, line);
        }
        pktline::flush(&mut out);
    }
    Ok(out)
}

/// The longest a push waits for compaction. After that it goes on anyway:
/// slow is better than refused.
const MAX_HOLD: Duration = Duration::from_secs(10);

/// Waits while the pool has more packs than requests should have to go
/// through, see [`crate::Options::wait_at`]. Leaves `repo` as it is then.
async fn hold_back(app: &App, scope: &RefScope, repo: &mut RepoState) -> Result<(), HttpError> {
    let limit = app.options.wait_at;
    let deadline = tokio::time::Instant::now() + MAX_HOLD;
    let mut pause = Duration::from_millis(5);
    while limit > 0 && repo.packs.len() >= limit && tokio::time::Instant::now() < deadline {
        app.request_compaction(&repo.name);
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_millis(100));
        *repo = app.meta.get_repo(&repo.name, scope).await?.ok_or(MetaError::NotFound)?;
    }
    Ok(())
}

/// The application's answer to a pre-receive question.
#[derive(serde::Deserialize)]
struct Verdict {
    allow: bool,
    message: Option<String>,
}

/// What an update does to its ref: `create`, `delete`, `fast-forward` (the
/// old value is in the new one's history) or `forced` (history is replaced,
/// or the ref is not a branch-like pointer to commits).
fn change(src: &dyn strata_git::ObjectSource, old: &Oid, new: &Oid) -> &'static str {
    if old.is_zero() {
        return "create";
    }
    if new.is_zero() {
        return "delete";
    }
    match walk::is_ancestor(src, old, new) {
        Ok(true) => "fast-forward",
        _ => "forced",
    }
}

/// How many commits of one update the pre-receive hook is shown.
const MAX_HOOK_COMMITS: usize = 1000;

/// The commits an update adds to its ref, newest first, and whether there
/// were more than are listed. For a ref that moves, these are the commits its
/// old value lacks; for a new ref, those the repository did not have before
/// this push, which takes no look at the other refs, however many there are.
fn new_commits(all: &PackSet, existing: &PackSet, old: &Oid, new: &Oid) -> Result<(Vec<serde_json::Value>, bool), HttpError> {
    let Some(head) = walk::commit_tips(all, &[*new])?.first().copied().filter(|_| !new.is_zero()) else { return Ok((Vec::new(), false)) };
    let ahead = if old.is_zero() {
        let mut ahead = Vec::new();
        let mut seen = std::collections::HashSet::from([head]);
        let mut queue = std::collections::BinaryHeap::new();
        if !existing.contains(&head) {
            queue.push((i64::MAX, head));
        }
        while let Some((_, oid)) = queue.pop() {
            ahead.push(oid);
            if ahead.len() > MAX_HOOK_COMMITS {
                break;
            }
            for parent in all.commit(&oid)?.map(|commit| commit.parents).unwrap_or_default() {
                if seen.insert(parent) && !existing.contains(&parent) {
                    queue.push((all.commit(&parent)?.map_or(0, |commit| commit.time), parent));
                }
            }
        }
        ahead
    } else {
        crate::merge::diverge(all, &walk::commit_tips(all, &[*old])?, head)?.ahead
    };
    let commits = ahead.iter().take(MAX_HOOK_COMMITS).map(|oid| crate::content::commit_json(all, oid)).collect::<Result<Vec<_>, _>>()?;
    Ok((commits, ahead.len() > MAX_HOOK_COMMITS))
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// Validates and commits a push against one snapshot of the repository.
///
/// Returns `None` if objects were removed from the pool in the meantime (or
/// this push's own upload was), in which case the caller should try again
/// with a fresh snapshot. `uploaded`
/// carries the id of this push's pack from one attempt to the next, so that
/// it is indexed and uploaded only once.
async fn apply(
    app: &App,
    repo: &RepoState,
    actor: Option<&str>,
    commands: &[Command],
    pack: Option<Arc<dyn PackData>>,
    uploaded: &mut Option<String>,
) -> Result<Option<Result<(), Rejection>>, HttpError> {
    if let Some(bad) = commands.iter().find(|c| !valid_ref_name(&c.name)) {
        return Ok(Some(Err(Rejection::Ref { name: bad.name.clone(), reason: "invalid ref name".into() })));
    }

    // Whoever pushes from a stale view loses to the ref's current value anyway;
    // saying so now saves the work, and the hooks a question about a push that cannot land.
    if let Some(stale) = commands.iter().find(|c| repo.refs.get(&c.name).copied().unwrap_or(Oid::ZERO) != c.old) {
        return Ok(Some(Err(Rejection::Ref { name: stale.name.clone(), reason: "ref changed concurrently, fetch first".into() })));
    }

    let existing = app.packs.open(&repo.pool_id, &repo.packs).await?;
    let mut all = existing.clone();
    let mut to_upload = None;
    // What the new pack points at outside itself, when it was indexed just now.
    let mut outside = None;
    // A lot of history arriving where nothing has bitmaps: worth a compaction right after.
    let mut wants_bitmaps = false;
    if let Some(pack_id) = uploaded.as_ref() {
        all = app.packs.open(&repo.pool_id, std::slice::from_ref(pack_id)).await?.merged_with(&existing);
    } else if let Some(spooled) = pack {
        let (received, base) = (spooled.clone(), existing.clone());
        match blocking(move || index_pack(&*received, &base)).await? {
            Err(e) => return Ok(Some(Err(Rejection::Unpack(e.to_string())))),
            // A push that only moves refs to existing objects carries an empty pack.
            Ok(indexed) if indexed.index.is_empty() => {}
            Ok(indexed) => {
                wants_bitmaps = app.options.wants_bitmaps(indexed.index.len()) && existing.packs().iter().all(|pack| pack.bitmaps().is_none());
                outside = Some(indexed.outside.clone());
                let data = indexed.data(spooled);
                all = existing.with_pack(Arc::new(PackFile::new(data.clone(), indexed.index.clone(), None)?));
                to_upload = Some((data, indexed.index, indexed.graph));
            }
        }
    }

    // Refuse refs whose history would be incomplete. Checked before upload so
    // a bad push leaves nothing behind.
    let tips: Vec<Oid> = commands.iter().map(|c| c.new).filter(|oid| !oid.is_zero()).collect();
    let moves: Vec<(Oid, Oid)> = commands.iter().map(|c| (c.old, c.new)).collect();
    let (pushed, before) = (all.clone(), existing.clone());
    let checked = blocking(move || {
        match outside {
            // Indexing saw every object of the pack and noted what each points
            // at: the pack is connected if what lies outside it is here.
            Some(outside) => {
                if let Some(missing) = tips.iter().find(|tip| !all.contains(tip)).or_else(|| outside.iter().find(|oid| !existing.contains(oid))) {
                    return Err(strata_git::Error::Missing(*missing));
                }
            }
            None => walk::check_connected(&all, &existing, &tips)?,
        }
        Ok::<_, strata_git::Error>(moves.iter().map(|(old, new)| change(&all, old, new)).collect::<Vec<_>>())
    })
    .await?;
    let changes = match checked {
        Ok(changes) => changes,
        Err(e) => return Ok(Some(Err(Rejection::All(format!("missing necessary objects: {e}"))))),
    };
    let updates: Vec<serde_json::Value> = commands
        .iter()
        .zip(&changes)
        .map(|(c, change)| {
            let hex = |oid: Oid| (!oid.is_zero()).then(|| oid.to_string());
            serde_json::json!({ "ref": c.name, "old": hex(c.old), "new": hex(c.new), "change": change })
        })
        .collect();

    // The application's rules (protected branches and the like), asked before
    // anything is stored. No answer means no.
    if let Some(url) = &app.options.pre_receive_url {
        // The question also shows the commits each update brings, so that
        // rules can be about them: who signed, who wrote, what they say.
        let moves: Vec<(Oid, Oid)> = commands.iter().map(|c| (c.old, c.new)).collect();
        let brought = blocking(move || moves.iter().map(|(old, new)| new_commits(&pushed, &before, old, new)).collect::<Result<Vec<_>, HttpError>>()).await??;
        let mut asked = updates.clone();
        for (update, (commits, truncated)) in asked.iter_mut().zip(brought) {
            update["commits"] = serde_json::Value::Array(commits);
            update["commits_truncated"] = serde_json::Value::Bool(truncated);
        }
        let question = serde_json::json!({ "repo": repo.name, "actor": actor, "updates": asked });
        match app.hooks.ask::<_, Verdict>(url, &question).await {
            Ok(Verdict { allow: true, .. }) => {}
            Ok(Verdict { message, .. }) => {
                return Ok(Some(Err(Rejection::All(message.unwrap_or_else(|| "rejected by the repository's rules".into())))));
            }
            Err(e) => {
                tracing::error!(repo = repo.name, "pre-receive hook failed: {e:#}");
                return Ok(Some(Err(Rejection::All("the repository's rules could not be checked, try again".into()))));
            }
        }
    }

    if let Some((data, index, graph)) = to_upload {
        // Uploaded but unreferenced until the transaction below commits. If
        // that never happens, the orphan sweep deletes it.
        let pack_id = PackStore::new_pack_id();
        app.packs.put_pack(&repo.pool_id, &pack_id, data, index, graph).await?;
        *uploaded = Some(pack_id);
    }
    let present = |oid: Oid| (!oid.is_zero()).then_some(oid);
    let mut txn = Txn {
        add_pack: uploaded.clone(),
        ref_updates: commands.iter().map(|c| RefUpdate { name: c.name.clone(), old: present(c.old), new: present(c.new) }).collect(),
        // The connectivity check above is only as good as the pool it saw.
        expect_epoch: Some(repo.epoch),
        event: Some(serde_json::json!({ "type": "push", "actor": actor, "updates": updates }).to_string()),
        ..Txn::default()
    };

    // Point HEAD at something real when the default branch does not exist,
    // so that clones of a repository pushed with another branch name work.
    let creates = |name: &str| commands.iter().any(|c| c.name == name && !c.new.is_zero());
    if !repo.refs.contains_key(&repo.head) && !creates(&repo.head) {
        txn.set_head = commands
            .iter()
            .find(|c| c.name.starts_with("refs/heads/") && !c.new.is_zero())
            .map(|c| c.name.clone());
    }

    // The orphan sweep deletes uploads it takes for abandoned. Never let the
    // manifest name a pack that is gone: upload it again instead.
    if let Some(pack_id) = &txn.add_pack {
        if !app.packs.is_stored(&repo.pool_id, pack_id).await? {
            tracing::warn!(repo = repo.name, pack = pack_id, "uploaded pack disappeared before commit, uploading again");
            *uploaded = None;
            return Ok(None);
        }
    }

    let adds_pack = txn.add_pack.is_some();
    match app.meta.commit(&repo.name, txn).await {
        Ok(()) => {
            if adds_pack && (wants_bitmaps || repo.packs.len() + 1 >= app.options.compact_at) {
                app.request_compaction(&repo.name);
            }
            Ok(Some(Ok(())))
        }
        Err(MetaError::Conflict { name }) => {
            Ok(Some(Err(Rejection::Ref { name, reason: "ref changed concurrently, fetch first".into() })))
        }
        Err(MetaError::Stale) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::valid_ref_name;

    #[test]
    fn ref_names() {
        for ok in ["refs/heads/main", "refs/tags/v1.0", "refs/heads/feature/x-1"] {
            assert!(valid_ref_name(ok), "{ok}");
        }
        for bad in ["HEAD", "refs/heads/", "refs/heads/a..b", "refs/heads/.hidden", "refs/heads/a b", "refs/heads/x.lock", "refs/heads/a:b"] {
            assert!(!valid_ref_name(bad), "{bad}");
        }
    }
}
