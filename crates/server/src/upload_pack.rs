//! Fetch side: git protocol v2 `ls-refs` and `fetch`.

use std::collections::HashSet;
use std::io::Write;

use axum::body::Body;
use bytes::Bytes;
use strata_git::pack::PackWriter;
use strata_git::pktline::{self, Pkt, Reader, SidebandWriter};
use strata_git::walk::{self, Filter, ShallowPlan, ShallowRequest};
use strata_git::{Error, ObjectSource, Oid, PackSet};
use strata_store::{RefScope, RepoState};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::{blocking, App, HttpError};

pub(crate) const AGENT: &str = "strata/0.1";

/// The v2 capability advertisement.
pub(crate) fn advertise() -> Vec<u8> {
    let mut out = Vec::new();
    pktline::write(&mut out, "version 2\n");
    pktline::write(&mut out, format!("agent={AGENT}\n"));
    pktline::write(&mut out, "ls-refs=unborn\n");
    pktline::write(&mut out, "fetch=shallow filter\n");
    pktline::write(&mut out, "object-format=sha1\n");
    pktline::flush(&mut out);
    out
}

struct Request<'a> {
    command: &'a [u8],
    args: Vec<&'a [u8]>,
}

fn parse(body: &[u8]) -> Result<Request<'_>, HttpError> {
    let bad = |_| HttpError::bad_request("malformed request");
    let mut reader = Reader::new(body);
    let mut command = None;
    let mut args = Vec::new();

    let has_args = loop {
        match reader.read().map_err(bad)? {
            Some(Pkt::Data(line)) => {
                let line = pktline::text(line);
                if let Some(c) = line.strip_prefix(b"command=") {
                    command = Some(c);
                } else if let Some(format) = line.strip_prefix(b"object-format=") {
                    if format != b"sha1" {
                        return Err(HttpError::bad_request("only object-format=sha1 is supported"));
                    }
                }
            }
            Some(Pkt::Delim) => break true,
            Some(Pkt::Flush) | None => break false,
            Some(Pkt::ResponseEnd) => return Err(HttpError::bad_request("malformed request")),
        }
    };
    if has_args {
        loop {
            match reader.read().map_err(bad)? {
                Some(Pkt::Data(line)) => args.push(pktline::text(line)),
                Some(Pkt::Flush) | None => break,
                Some(_) => return Err(HttpError::bad_request("malformed request")),
            }
        }
    }
    let command = command.ok_or_else(|| HttpError::bad_request("missing command"))?;
    Ok(Request { command, args })
}

pub(crate) async fn handle(app: &App, name: &str, body: Bytes) -> Result<Body, HttpError> {
    let request = parse(&body)?;
    match request.command {
        b"ls-refs" => Ok(ls_refs(app, name, &request.args).await?.into()),
        b"fetch" => {
            // A fetch names objects, not refs: none are read unless it turns out to name one.
            let repo = app.meta.get_repo(name, &RefScope::none()).await?.ok_or_else(HttpError::not_found)?;
            fetch(app, &repo, &request.args).await
        }
        _ => Err(HttpError::bad_request("unsupported command")),
    }
}

/// Beyond this many prefixes, reading every ref and picking is the cheaper way.
const MAX_REF_PREFIXES: usize = 32;

async fn ls_refs(app: &App, name: &str, args: &[&[u8]]) -> Result<Vec<u8>, HttpError> {
    let (mut peel, mut symrefs, mut unborn) = (false, false, false);
    let mut prefixes = Vec::new();
    for arg in args {
        match *arg {
            b"peel" => peel = true,
            b"symrefs" => symrefs = true,
            b"unborn" => unborn = true,
            _ => {
                if let Some(prefix) = arg.strip_prefix(b"ref-prefix ") {
                    prefixes.push(prefix.to_vec());
                }
            }
        }
    }
    let matches = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.as_bytes().starts_with(p));
    // Only the refs asked about are read: git names the few it is after, and
    // a repository can have hundreds of thousands.
    let scope = if prefixes.is_empty() || prefixes.len() > MAX_REF_PREFIXES {
        RefScope::All
    } else {
        // A prefix that is not text is the start of no ref's name.
        RefScope::Prefixes(prefixes.iter().filter_map(|p| String::from_utf8(p.clone()).ok()).collect())
    };
    let repo = &app.meta.get_repo(name, &scope).await?.ok_or_else(HttpError::not_found)?;

    let mut out = Vec::new();
    if matches("HEAD") {
        let target = if symrefs { format!(" symref-target:{}", repo.head) } else { String::new() };
        match repo.refs.get(&repo.head) {
            Some(oid) => pktline::write(&mut out, format!("{oid} HEAD{target}\n")),
            None if unborn => pktline::write(&mut out, format!("unborn HEAD{target}\n")),
            None => {}
        }
    }

    let selected: Vec<(String, Oid)> =
        repo.refs.iter().filter(|(name, _)| matches(name)).map(|(name, oid)| (name.clone(), *oid)).collect();
    // TODO: store peeled values next to refs instead of reading each tip here.
    let lines = if peel && !selected.is_empty() {
        let packs = app.packs.open(&repo.pool_id, &repo.packs).await?;
        blocking(move || {
            selected
                .into_iter()
                .map(|(name, oid)| {
                    Ok(match walk::peel(&packs, &oid)? {
                        Some(peeled) => format!("{oid} {name} peeled:{peeled}\n"),
                        None => format!("{oid} {name}\n"),
                    })
                })
                .collect::<Result<Vec<_>, Error>>()
        })
        .await??
    } else {
        selected.into_iter().map(|(name, oid)| format!("{oid} {name}\n")).collect()
    };
    for line in lines {
        pktline::write(&mut out, line);
    }
    pktline::flush(&mut out);
    Ok(out)
}

#[derive(Default)]
struct Fetch {
    wants: Vec<Oid>,
    haves: Vec<Oid>,
    done: bool,
    /// The client accepts deltas against objects it already has.
    thin: bool,
    shallow: ShallowRequest,
    filter: Filter,
}

async fn fetch(app: &App, repo: &RepoState, args: &[&[u8]]) -> Result<Body, HttpError> {
    let oid = |hex: &[u8]| Oid::from_hex(hex).ok_or_else(|| HttpError::bad_request("malformed object id"));
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    let mut fetch = Fetch::default();
    for arg in args {
        if let Some(hex) = arg.strip_prefix(b"want ") {
            fetch.wants.push(oid(hex)?);
        } else if let Some(hex) = arg.strip_prefix(b"have ") {
            fetch.haves.push(oid(hex)?);
        } else if *arg == b"done" {
            fetch.done = true;
        } else if *arg == b"thin-pack" {
            fetch.thin = true;
        } else if let Some(hex) = arg.strip_prefix(b"shallow ") {
            fetch.shallow.client_shallow.push(oid(hex)?);
        } else if let Some(depth) = arg.strip_prefix(b"deepen ") {
            let depth = text(depth).parse().map_err(|_| HttpError::bad_request("malformed deepen"))?;
            fetch.shallow.depth = Some(depth);
        } else if *arg == b"deepen-relative" {
            fetch.shallow.relative = true;
        } else if let Some(since) = arg.strip_prefix(b"deepen-since ") {
            let since = text(since).parse().map_err(|_| HttpError::bad_request("malformed deepen-since"))?;
            fetch.shallow.since = Some(since);
        } else if let Some(name) = arg.strip_prefix(b"deepen-not ") {
            let name = text(name);
            match lookup_ref(app, &repo.name, &name).await? {
                Some(oid) => fetch.shallow.not.push(oid),
                None => return Ok(error_response(&format!("deepen-not is not a ref: {name}")).into()),
            }
        } else if let Some(spec) = arg.strip_prefix(b"filter ") {
            let spec = text(spec);
            match Filter::parse(&spec) {
                Some(filter) => fetch.filter = filter,
                None => return Ok(error_response(&format!("filter '{spec}' is not supported")).into()),
            }
        }
        // Hints such as ofs-delta and no-progress are safe to ignore: the
        // response below is valid without them.
    }
    if fetch.wants.is_empty() {
        return Err(HttpError::bad_request("fetch without wants"));
    }

    let packs = app.packs.open(&repo.pool_id, &repo.packs).await?;
    let repo_name = repo.name.clone();
    let (fetch, packs) = match load_sparse(app, repo, packs, fetch).await? {
        Ok(loaded) => loaded,
        Err(name) => return Ok(error_response(&format!("filter 'sparse:oid={name}' does not name a file of patterns")).into()),
    };

    // The response is produced on a blocking thread and streamed through a
    // bounded channel, so a slow client holds back the producer instead of
    // letting the pack pile up in memory.
    let (tx, rx) = mpsc::channel(4);
    tokio::task::spawn_blocking(move || {
        let mut out = ChannelWriter { tx, buf: Vec::with_capacity(CHANNEL_CHUNK) };
        match fetch_response(&packs, &fetch, &mut out).and_then(|()| out.flush().map_err(io_error)) {
            Ok(()) => {}
            // The client hung up; nobody is left to tell.
            Err(Error::Io(reason)) => tracing::debug!(repo = repo_name, "fetch aborted: {reason}"),
            Err(e) => tracing::error!(repo = repo_name, "fetch failed: {e}"),
        }
    });
    Ok(Body::from_stream(ReceiverStream::new(rx)))
}

const CHANNEL_CHUNK: usize = 64 << 10;

/// Forwards written bytes to the HTTP response body in chunks.
struct ChannelWriter {
    tx: mpsc::Sender<std::io::Result<Bytes>>,
    buf: Vec<u8>,
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHANNEL_CHUNK {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(CHANNEL_CHUNK)));
        self.tx.blocking_send(Ok(chunk)).map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client disconnected"))
    }
}

/// A file of sparse-checkout patterns larger than this is refused.
const MAX_SPARSE_FILE: usize = 1 << 20;

/// Reads the pattern files a `sparse:oid=` filter names: a blob by id, or
/// `<revision>:<path>`. Hands back the name that could not be read, if any.
async fn load_sparse(app: &App, repo: &RepoState, packs: PackSet, mut fetch: Fetch) -> Result<Result<(Fetch, PackSet), String>, HttpError> {
    if fetch.filter.sparse_names().is_empty() {
        return Ok(Ok((fetch, packs)));
    }
    let revs: Vec<&str> = fetch.filter.sparse_names().iter().filter_map(|name| name.split_once(':')).map(|(rev, _)| rev).collect();
    let repo = if revs.is_empty() { repo.clone() } else { repo_with_revs(app, &repo.name, &revs).await?.ok_or_else(HttpError::not_found)? };
    crate::blocking(move || {
        let mut contents = Vec::new();
        for name in fetch.filter.sparse_names() {
            let oid = match name.split_once(':') {
                None => Oid::from_hex(name).filter(|_| name.len() == 40),
                Some((rev, path)) => (|| {
                    let commit = packs.commit(&crate::content::resolve(&repo, &packs, rev).ok()?).ok()??;
                    Some(crate::content::find(&packs, &commit, path).ok()??.1)
                })(),
            };
            match oid.and_then(|oid| packs.get(&oid).ok().flatten()) {
                Some(object) if object.kind == strata_git::ObjectKind::Blob && object.data.len() <= MAX_SPARSE_FILE => contents.push(object.data),
                _ => return Err(name.clone()),
            }
        }
        fetch.filter.load_sparse(&contents);
        Ok((fetch, packs))
    })
    .await
}

fn io_error(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}

/// The refs a user may mean by `name`: it in full, or a tag or branch called that.
pub(crate) fn ref_candidates(name: &str) -> impl Iterator<Item = String> + '_ {
    ["", "refs/", "refs/tags/", "refs/heads/"].iter().map(move |prefix| format!("{prefix}{name}"))
}

/// Resolves a ref the way a user would name it. `repo` must have been read
/// with the refs `name` may mean, as [`repo_with_revs`] does.
pub(crate) fn resolve_ref(repo: &RepoState, name: &str) -> Option<Oid> {
    ref_candidates(name).find_map(|candidate| repo.refs.get(&candidate).copied())
}

/// The repository with the refs that the given revisions may name, and no others.
pub(crate) async fn repo_with_revs(app: &App, name: &str, revs: &[&str]) -> Result<Option<RepoState>, HttpError> {
    let scope = RefScope::Names(revs.iter().flat_map(|rev| ref_candidates(rev)).collect());
    Ok(app.meta.get_repo(name, &scope).await?)
}

async fn lookup_ref(app: &App, repo: &str, name: &str) -> Result<Option<Oid>, HttpError> {
    Ok(repo_with_revs(app, repo, &[name]).await?.and_then(|repo| resolve_ref(&repo, name)))
}

/// A response that makes the git client abort and print `message`.
fn error_response(message: &str) -> Vec<u8> {
    let mut out = Vec::new();
    pktline::write(&mut out, format!("ERR upload-pack: {message}\n"));
    out
}

fn fetch_response(packs: &PackSet, fetch: &Fetch, out: &mut impl Write) -> Result<(), Error> {
    // TODO: once pools are shared between forks, wants must be checked for
    // reachability from this repository's refs, not mere existence.
    if let Some(missing) = fetch.wants.iter().find(|oid| !packs.contains(oid)) {
        return out.write_all(&error_response(&format!("not our ref {missing}"))).map_err(io_error);
    }

    let mut head = Vec::new();
    let common: Vec<Oid> = fetch.haves.iter().copied().filter(|oid| packs.contains(oid)).collect();
    if !fetch.done && common.is_empty() {
        // Nothing in common yet: let the client send more haves or give up with `done`.
        pktline::write(&mut head, "acknowledgments\n");
        pktline::write(&mut head, "NAK\n");
        pktline::flush(&mut head);
        return out.write_all(&head).map_err(io_error);
    }

    let plan = match walk::plan_shallow(packs, &fetch.wants, &fetch.shallow) {
        Ok(plan) => plan,
        Err(Error::Request(message)) => return out.write_all(&error_response(&message)).map_err(io_error),
        Err(e) => {
            // Nothing has been sent yet, so this can still be a clean protocol error.
            let _ = out.write_all(&error_response("internal error"));
            return Err(e);
        }
    };

    if !fetch.done {
        pktline::write(&mut head, "acknowledgments\n");
        for oid in &common {
            pktline::write(&mut head, format!("ACK {oid}\n"));
        }
        // Any common commit is enough to build a correct (if not minimal) pack.
        pktline::write(&mut head, "ready\n");
        pktline::delim(&mut head);
    }
    if plan.active {
        pktline::write(&mut head, "shallow-info\n");
        for oid in &plan.shallow {
            pktline::write(&mut head, format!("shallow {oid}\n"));
        }
        for oid in &plan.unshallow {
            pktline::write(&mut head, format!("unshallow {oid}\n"));
        }
        pktline::delim(&mut head);
    }
    pktline::write(&mut head, "packfile\n");
    // Get the negotiation result to the client before the expensive part starts.
    // TODO: send sideband keepalives while a long object walk is running.
    out.write_all(&head).and_then(|()| out.flush()).map_err(io_error)?;

    if let Err(e) = send_pack(packs, fetch, &common, &plan, out) {
        // The stream is already inside the packfile section: report on the error band.
        let mut tail = Vec::new();
        pktline::write_sideband(&mut tail, 3, b"internal error while building the pack\n");
        let _ = out.write_all(&tail);
        return Err(e);
    }
    out.write_all(b"0000").map_err(io_error)
}

fn send_pack(packs: &PackSet, fetch: &Fetch, common: &[Oid], shallow: &ShallowPlan, out: &mut impl Write) -> Result<(), Error> {
    let mut band = SidebandWriter::new(out, 1);
    write_pack(packs, fetch, common, shallow, &mut band)?;
    band.finish().map_err(io_error)?;
    Ok(())
}

fn write_pack(packs: &PackSet, fetch: &Fetch, common: &[Oid], shallow: &ShallowPlan, out: &mut impl Write) -> Result<(), Error> {
    let plan = walk::plan_fetch(packs, &fetch.wants, common, shallow, &fetch.filter)?;
    let count = u32::try_from(plan.count()).map_err(|_| Error::Request("too many objects for one pack".into()))?;
    let walked: HashSet<Oid> = plan.objects.iter().map(|(oid, _)| *oid).collect();
    let in_pack = |oid: &Oid| walked.contains(oid) || plan.in_bitmaps(oid);
    // A partial clone may have left out the very object a delta would build on.
    let thin = fetch.thin && fetch.filter.is_none();

    let mut writer = PackWriter::new(out, count)?;
    for (oid, delta_base) in &plan.objects {
        writer.add_best(packs, oid, delta_base.as_ref(), &in_pack, thin)?;
    }
    for selection in &plan.bitmaps {
        // In stored order: a clone becomes a sequential pass over each pack.
        let pack = &selection.pack;
        for &(offset, position) in pack.entries_in_pack_order() {
            if selection.bits.get(position as usize) {
                writer.add_entry(pack, offset, &pack.index().oid_at(position as usize), &in_pack, packs)?;
            }
        }
    }
    writer.finish()?;
    Ok(())
}

// ---- Protocol v0/v1, for clients that do not speak v2 (libgit2, go-git, old git) ----

/// How the client wants common commits acknowledged.
#[derive(Clone, Copy, PartialEq)]
enum AckMode {
    Single,
    Multi,
    Detailed,
}

/// The ref advertisement that opens a v0 fetch.
pub(crate) async fn advertise_v0(app: &App, repo: &RepoState) -> Result<Vec<u8>, HttpError> {
    let caps = format!(
        "multi_ack_detailed multi_ack side-band-64k thin-pack ofs-delta shallow deepen-since deepen-not \
         deepen-relative no-progress allow-tip-sha1-in-want allow-reachable-sha1-in-want filter \
         symref=HEAD:{} object-format=sha1 agent={AGENT}",
        repo.head
    );
    let mut lines: Vec<(Oid, String)> = Vec::new();
    if let Some(oid) = repo.refs.get(&repo.head) {
        lines.push((*oid, "HEAD".to_string()));
    }
    let refs: Vec<(String, Oid)> = repo.refs.iter().map(|(name, oid)| (name.clone(), *oid)).collect();
    if !refs.is_empty() {
        let packs = app.packs.open(&repo.pool_id, &repo.packs).await?;
        lines.extend(
            blocking(move || -> Result<Vec<(Oid, String)>, Error> {
                let mut lines = Vec::new();
                for (name, oid) in refs {
                    let peeled = walk::peel(&packs, &oid)?;
                    lines.push((oid, name.clone()));
                    lines.extend(peeled.map(|target| (target, format!("{name}^{{}}"))));
                }
                Ok(lines)
            })
            .await??,
        );
    }

    let mut out = Vec::new();
    pktline::write(&mut out, "# service=git-upload-pack\n");
    pktline::flush(&mut out);
    match lines.split_first() {
        Some(((oid, name), rest)) => {
            pktline::write(&mut out, format!("{oid} {name}\0{caps}\n"));
            for (oid, name) in rest {
                pktline::write(&mut out, format!("{oid} {name}\n"));
            }
        }
        None => pktline::write(&mut out, format!("{} capabilities^{{}}\0{caps}\n", Oid::ZERO)),
    }
    pktline::flush(&mut out);
    Ok(out)
}

/// One round of a v0 fetch over stateless HTTP: the client repeats its wants
/// and everything it has said so far, and gets acknowledgements or the pack.
pub(crate) async fn handle_v0(app: &App, repo: RepoState, body: Bytes) -> Result<Body, HttpError> {
    let bad = || HttpError::bad_request("malformed request");
    let oid = |hex: &str| Oid::from_hex(hex).ok_or_else(bad);
    let mut reader = Reader::new(&body);
    let mut fetch = Fetch::default();
    let mut caps: Vec<String> = Vec::new();
    let mut flushes = 0;
    while let Some(pkt) = reader.read().map_err(|_| bad())? {
        let Pkt::Data(line) = pkt else {
            flushes += 1;
            continue;
        };
        let line = std::str::from_utf8(pktline::text(line)).map_err(|_| bad())?;
        if let Some(rest) = line.strip_prefix("want ") {
            let (hex, first_caps) = rest.split_once(' ').unwrap_or((rest, ""));
            fetch.wants.push(oid(hex)?);
            caps.extend(first_caps.split(' ').filter(|c| !c.is_empty()).map(str::to_string));
        } else if let Some(hex) = line.strip_prefix("have ") {
            fetch.haves.push(oid(hex)?);
        } else if line == "done" {
            fetch.done = true;
        } else if let Some(hex) = line.strip_prefix("shallow ") {
            fetch.shallow.client_shallow.push(oid(hex)?);
        } else if let Some(depth) = line.strip_prefix("deepen ") {
            fetch.shallow.depth = Some(depth.parse().map_err(|_| bad())?);
        } else if let Some(since) = line.strip_prefix("deepen-since ") {
            fetch.shallow.since = Some(since.parse().map_err(|_| bad())?);
        } else if let Some(name) = line.strip_prefix("deepen-not ") {
            fetch.shallow.not.push(lookup_ref(app, &repo.name, name).await?.ok_or_else(|| HttpError::bad_request("deepen-not is not a ref"))?);
        } else if let Some(spec) = line.strip_prefix("filter ") {
            fetch.filter = Filter::parse(spec).ok_or_else(|| HttpError::bad_request("filter is not supported"))?;
        }
    }
    if fetch.wants.is_empty() {
        // The client only wanted the advertisement.
        return Ok(Body::empty());
    }
    let has = |cap: &str| caps.iter().any(|c| c == cap);
    fetch.thin = has("thin-pack");
    fetch.shallow.relative = has("deepen-relative");
    let ack = if has("multi_ack_detailed") {
        AckMode::Detailed
    } else if has("multi_ack") {
        AckMode::Multi
    } else {
        AckMode::Single
    };
    let sideband = has("side-band-64k");
    // The first flush ends the wants; a second one ends a batch of haves and
    // asks for acknowledgements. A request that stops after its wants (the
    // opening round of a shallow fetch) gets the shallow list and nothing else.
    let negotiating = flushes >= 2;
    if has("side-band") && !sideband {
        return Err(HttpError::bad_request("only side-band-64k is supported"));
    }

    let packs = app.packs.open(&repo.pool_id, &repo.packs).await?;
    let repo_name = repo.name.clone();
    let (fetch, packs) = load_sparse(app, &repo, packs, fetch).await?.map_err(|_| HttpError::bad_request("the sparse filter does not name a file of patterns"))?;
    let (tx, rx) = mpsc::channel(4);
    tokio::task::spawn_blocking(move || {
        let mut out = ChannelWriter { tx, buf: Vec::with_capacity(CHANNEL_CHUNK) };
        match respond_v0(&packs, &fetch, ack, sideband, negotiating, &mut out).and_then(|()| out.flush().map_err(io_error)) {
            Ok(()) => {}
            Err(Error::Io(reason)) => tracing::debug!(repo = repo_name, "fetch aborted: {reason}"),
            Err(e) => tracing::error!(repo = repo_name, "fetch failed: {e}"),
        }
    });
    Ok(Body::from_stream(ReceiverStream::new(rx)))
}

fn respond_v0(
    packs: &PackSet,
    fetch: &Fetch,
    ack: AckMode,
    sideband: bool,
    negotiating: bool,
    out: &mut impl Write,
) -> Result<(), Error> {
    if let Some(missing) = fetch.wants.iter().find(|oid| !packs.contains(oid)) {
        return out.write_all(&error_response(&format!("not our ref {missing}"))).map_err(io_error);
    }
    let plan = match walk::plan_shallow(packs, &fetch.wants, &fetch.shallow) {
        Ok(plan) => plan,
        Err(Error::Request(message)) => return out.write_all(&error_response(&message)).map_err(io_error),
        Err(e) => return Err(e),
    };

    let mut head = Vec::new();
    if fetch.shallow.deepens() {
        for oid in &plan.shallow {
            pktline::write(&mut head, format!("shallow {oid}\n"));
        }
        for oid in &plan.unshallow {
            pktline::write(&mut head, format!("unshallow {oid}\n"));
        }
        pktline::flush(&mut head);
    }

    if !fetch.done && !negotiating {
        return out.write_all(&head).map_err(io_error);
    }

    let common: Vec<Oid> = fetch.haves.iter().copied().filter(|oid| packs.contains(oid)).collect();
    match ack {
        AckMode::Detailed => common.iter().for_each(|oid| pktline::write(&mut head, format!("ACK {oid} common\n"))),
        AckMode::Multi => common.iter().for_each(|oid| pktline::write(&mut head, format!("ACK {oid} continue\n"))),
        AckMode::Single => common.first().into_iter().for_each(|oid| pktline::write(&mut head, format!("ACK {oid}\n"))),
    }
    if !fetch.done {
        // More haves may follow; the pack is sent once the client says `done`.
        if ack == AckMode::Detailed {
            if let Some(oid) = common.last() {
                pktline::write(&mut head, format!("ACK {oid} ready\n"));
            }
        }
        if ack != AckMode::Single || common.is_empty() {
            pktline::write(&mut head, "NAK\n");
        }
        return out.write_all(&head).map_err(io_error);
    }
    match common.last() {
        Some(oid) if ack != AckMode::Single => pktline::write(&mut head, format!("ACK {oid}\n")),
        Some(_) => {}
        None => pktline::write(&mut head, "NAK\n"),
    }
    out.write_all(&head).and_then(|()| out.flush()).map_err(io_error)?;

    if !sideband {
        return write_pack(packs, fetch, &common, &plan, out);
    }
    if let Err(e) = send_pack(packs, fetch, &common, &plan, out) {
        let mut tail = Vec::new();
        pktline::write_sideband(&mut tail, 3, b"internal error while building the pack\n");
        let _ = out.write_all(&tail);
        return Err(e);
    }
    out.write_all(b"0000").map_err(io_error)
}
