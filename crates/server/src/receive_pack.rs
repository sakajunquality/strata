//! Push side. Every push is applied as one atomic metadata transaction.

use std::sync::Arc;

use axum::body::{Body, BodyDataStream};
use bytes::{Bytes, BytesMut};
use strata_git::pack::index_pack;
use strata_git::pktline::{self, Pkt, Reader};
use strata_git::{walk, Oid, PackData, PackFile};
use strata_store::{MetaError, PackStore, RefUpdate, RepoState, Txn};
use tokio::io::AsyncWriteExt;
use tokio_stream::StreamExt;

use crate::upload_pack::AGENT;
use crate::{blocking, App, HttpError};

/// Upper bound on the ref-update commands of one push.
const MAX_COMMANDS: usize = 16 << 20;
/// Times a push is re-validated when the pool is pruned underneath it.
const PUSH_ATTEMPTS: usize = 3;
/// How much of a spooled pack is read at a time.
const SPOOL_CHUNK: u64 = 256 << 10;

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
fn valid_ref_name(name: &str) -> bool {
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

/// A pack spooled to a scratch file, read back in pieces.
struct SpooledPack {
    file: std::fs::File,
    size: u64,
}

impl PackData for SpooledPack {
    fn size(&self) -> u64 {
        self.size
    }

    fn chunk(&self, offset: u64) -> strata_git::Result<Bytes> {
        if offset >= self.size {
            return Ok(Bytes::new());
        }
        let mut buf = vec![0u8; (self.size - offset).min(SPOOL_CHUNK) as usize];
        read_exact_at(&self.file, &mut buf, offset).map_err(|e| strata_git::Error::Io(format!("reading spooled pack: {e}")))?;
        Ok(buf.into())
    }
}

#[cfg(unix)]
fn read_exact_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &std::fs::File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        match std::os::windows::fs::FileExt::seek_read(file, buf, offset)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

/// Writes the rest of the request to an anonymous scratch file, so that a
/// push of any size costs disk rather than memory. Returns `None` if there
/// was no pack data.
async fn spool(app: &App, first: Bytes, mut body: BodyDataStream) -> Result<Option<SpooledPack>, HttpError> {
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
    Ok((size > 0).then_some(SpooledPack { file: file.into_std().await, size }))
}

pub(crate) async fn handle(app: &App, name: &str, repo: Option<RepoState>, body: Body) -> Result<Vec<u8>, HttpError> {
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
    let pack: Option<Arc<dyn PackData>> = match spool(app, head.slice(end..), body).await? {
        Some(spooled) => Some(Arc::new(spooled)),
        None => None,
    };
    let mut repo = match repo {
        Some(repo) => repo,
        None => match app.meta.create_repo(name).await {
            Ok(repo) => repo,
            // Lost a race with another first push.
            Err(MetaError::AlreadyExists) => app.meta.get_repo(name).await?.ok_or(MetaError::NotFound)?,
            Err(e) => return Err(e.into()),
        },
    };

    // If objects were pruned from the pool while this push was being checked,
    // the checks are void: run them again against the pool as it is now.
    let mut uploaded = None;
    let mut result = Err(Rejection::All("the repository is being reorganised, try again".into()));
    for _ in 0..PUSH_ATTEMPTS {
        match apply(app, &repo, &commands, pack.clone(), &mut uploaded).await? {
            Some(outcome) => {
                result = outcome;
                break;
            }
            None => repo = app.meta.get_repo(name).await?.ok_or(MetaError::NotFound)?,
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
    commands: &[Command],
    pack: Option<Arc<dyn PackData>>,
    uploaded: &mut Option<String>,
) -> Result<Option<Result<(), Rejection>>, HttpError> {
    if let Some(bad) = commands.iter().find(|c| !valid_ref_name(&c.name)) {
        return Ok(Some(Err(Rejection::Ref { name: bad.name.clone(), reason: "invalid ref name".into() })));
    }

    let existing = app.packs.open(&repo.pool_id, &repo.packs).await?;
    let mut all = existing.clone();
    let mut to_upload = None;
    if let Some(pack_id) = uploaded.as_ref() {
        all = app.packs.open(&repo.pool_id, std::slice::from_ref(pack_id)).await?.merged_with(&existing);
    } else if let Some(spooled) = pack {
        let (received, base) = (spooled.clone(), existing.clone());
        match blocking(move || index_pack(&*received, &base)).await? {
            Err(e) => return Ok(Some(Err(Rejection::Unpack(e.to_string())))),
            // A push that only moves refs to existing objects carries an empty pack.
            Ok(indexed) if indexed.index.is_empty() => {}
            Ok(indexed) => {
                let data = indexed.data(spooled);
                all = existing.with_pack(Arc::new(PackFile::new(data.clone(), indexed.index.clone(), None)?));
                to_upload = Some((data, indexed.index, indexed.graph));
            }
        }
    }

    // Refuse refs whose history would be incomplete. Checked before upload so
    // a bad push leaves nothing behind.
    let tips: Vec<Oid> = commands.iter().map(|c| c.new).filter(|oid| !oid.is_zero()).collect();
    if let Err(e) = blocking(move || walk::check_connected(&all, &existing, &tips)).await? {
        return Ok(Some(Err(Rejection::All(format!("missing necessary objects: {e}")))));
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
            if adds_pack && repo.packs.len() + 1 >= app.options.compact_at {
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
