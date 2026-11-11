//! Bringing a repository in whole, from a bundle.
//!
//! A bundle is what `git bundle create <file> --all` writes: the refs with
//! the ids they point at, then one pack with their objects. It is the way in
//! for a repository with very many refs: git's own push compares every ref it
//! sends with every ref it has, and takes hours for a hundred thousand.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use bytes::BytesMut;
use serde_json::json;
use strata_git::pack::index_pack;
use strata_git::{ObjectSource, Oid, PackData, PackFile};
use strata_store::{MetaError, PackStore, RefScope, RefUpdate, Txn};
use tokio_stream::StreamExt;

use crate::receive_pack::{spool, valid_ref_name};
use crate::{blocking, App, HttpError};

/// Upper bound on a bundle's list of refs.
const MAX_HEADER: usize = 64 << 20;
/// Refs set by one metadata transaction. The shared stores limit how much
/// one transaction may change.
const BATCH: usize = 5000;
/// Times the bundle is checked again when the repository changes underneath.
const ATTEMPTS: usize = 3;
/// How long to wait for a compaction in flight before setting refs in several steps.
const LEASE_PATIENCE: Duration = Duration::from_secs(30);
/// Longer than any import should take: the lease is given back when it is done.
const LEASE_SECS: i64 = 3600;

struct Bundle {
    refs: Vec<(String, Oid)>,
    /// What HEAD pointed at where the bundle was made.
    head: Option<Oid>,
    /// Objects the bundle builds on without holding them.
    prerequisites: Vec<Oid>,
}

fn not_a_bundle() -> HttpError {
    HttpError::bad_request("not a git bundle: make one with `git bundle create <file> --all`")
}

fn parse_header(text: &[u8]) -> Result<Bundle, HttpError> {
    let text = std::str::from_utf8(text).map_err(|_| not_a_bundle())?;
    let mut lines = text.lines();
    if !matches!(lines.next(), Some("# v2 git bundle" | "# v3 git bundle")) {
        return Err(not_a_bundle());
    }
    let mut bundle = Bundle { refs: Vec::new(), head: None, prerequisites: Vec::new() };
    for line in lines.filter(|line| !line.is_empty()) {
        if let Some(capability) = line.strip_prefix('@') {
            match capability {
                "object-format=sha1" => continue,
                _ if capability.starts_with("filter=") => return Err(HttpError::bad_request("a bundle that leaves objects out cannot be imported")),
                _ => return Err(HttpError::bad_request(format!("the bundle needs something not supported here: {capability}"))),
            }
        }
        if let Some(rest) = line.strip_prefix('-') {
            bundle.prerequisites.push(rest.get(..40).and_then(Oid::from_hex).ok_or_else(not_a_bundle)?);
            continue;
        }
        let (hex, name) = line.split_once(' ').ok_or_else(not_a_bundle)?;
        let oid = Oid::from_hex(hex).ok_or_else(not_a_bundle)?;
        if name == "HEAD" {
            bundle.head = Some(oid);
        } else if valid_ref_name(name) {
            bundle.refs.push((name.to_string(), oid));
        } else {
            return Err(HttpError::bad_request(format!("the bundle holds a ref with a name not accepted here: {name}")));
        }
    }
    Ok(bundle)
}

/// `POST /api/repos/<id>/import`, with a bundle as the body: stores its
/// objects and sets every ref in it to the value it has there, whatever the
/// ref was before. Refs the bundle does not name are left alone.
///
/// Answers `{"refs": ..., "updated": ..., "objects": ...}`. Refs are set a few
/// thousand at a time, so a failure half way leaves some of them set; sending
/// the bundle again finishes the job. The pre-receive hook is not asked, and
/// the event feed gets one `import` event with the number of refs, not a
/// list of them.
pub(crate) async fn import(app: &App, name: &str, query: &str, body: Body) -> Result<Response, HttpError> {
    let actor = url::form_urlencoded::parse(query.as_bytes()).find(|(key, _)| key == "actor").map(|(_, value)| value.into_owned());

    // The list of refs ends at the first empty line; the pack follows.
    let mut body = body.into_data_stream();
    let mut head = BytesMut::new();
    let mut searched = 0;
    let end = loop {
        if let Some(at) = head[searched..].windows(2).position(|pair| pair == b"\n\n") {
            break searched + at + 2;
        }
        searched = head.len().saturating_sub(1);
        if head.len() > MAX_HEADER {
            return Err(HttpError::bad_request("too many refs in one bundle"));
        }
        match body.next().await {
            Some(chunk) => head.extend_from_slice(&chunk.map_err(|_| HttpError::bad_request("the upload was interrupted"))?),
            None => return Err(not_a_bundle()),
        }
    };
    let head = head.freeze();
    let bundle = Arc::new(parse_header(&head[..end - 1])?);
    if app.meta.get_repo(name, &RefScope::none()).await?.is_none() {
        return Err(HttpError::not_found());
    }
    let pack: Option<Arc<dyn PackData>> = match spool(app, head.slice(end..), body).await? {
        Some(spooled) => Some(Arc::new(spooled)),
        None => None,
    };

    // Under a name of its own: a node's compactions all hold the lease as the
    // node, and would take this one for theirs.
    let lease = Lease { key: format!("compact/{name}"), holder: format!("import-{}", PackStore::new_pack_id()), held: false };
    let mut lease = lease;
    let result = apply(app, name, actor.as_deref(), &bundle, pack, &mut lease).await;
    if lease.held {
        if let Err(e) = app.meta.release_lease(&lease.key, &lease.holder).await {
            tracing::warn!(repo = name, "releasing the compaction lease after an import failed: {e}");
        }
    }
    result
}

/// The lease that keeps compactions of the repository from starting.
struct Lease {
    key: String,
    holder: String,
    held: bool,
}

async fn apply(app: &App, name: &str, actor: Option<&str>, bundle: &Arc<Bundle>, pack: Option<Arc<dyn PackData>>, lease: &mut Lease) -> Result<Response, HttpError> {
    let mut uploaded: Option<String> = None;
    // What the pack points at outside itself, and how many objects it holds.
    let mut outside: Arc<Vec<Oid>> = Arc::default();
    let mut objects = 0;
    let mut updated = 0;
    'attempts: for _ in 0..ATTEMPTS {
        let repo = app.meta.get_repo(name, &RefScope::All).await?.ok_or_else(HttpError::not_found)?;
        let existing = app.packs.open(&repo.pool_id, &repo.packs).await?;
        let lacks_bitmaps = existing.packs().iter().all(|pack| pack.bitmaps().is_none());
        // Once the refs are as the bundle has them: the answer, and a compaction where one is due.
        let done = |uploaded: &Option<String>, objects: usize, updated: usize| {
            if uploaded.is_some() && ((lacks_bitmaps && app.options.wants_bitmaps(objects)) || repo.packs.len() + 1 >= app.options.compact_at) {
                app.request_compaction(name);
            }
            crate::json(StatusCode::OK, json!({ "refs": bundle.refs.len(), "updated": updated, "objects": if uploaded.is_some() { objects } else { 0 } }))
        };
        let mut to_upload = None;
        let mut all = existing.clone();
        match (&uploaded, &pack) {
            (Some(pack_id), _) if repo.packs.contains(pack_id) => {}
            (Some(pack_id), _) => all = app.packs.open(&repo.pool_id, std::slice::from_ref(pack_id)).await?.merged_with(&existing),
            (None, Some(spooled)) => {
                let (received, base) = (spooled.clone(), existing.clone());
                match blocking(move || index_pack(&*received, &base)).await? {
                    Err(e) => return Err(HttpError::bad_request(format!("the bundle's pack cannot be read: {e}"))),
                    Ok(indexed) if indexed.index.is_empty() => {}
                    Ok(indexed) => {
                        outside = Arc::new(indexed.outside.clone());
                        objects = indexed.index.len();
                        let data = indexed.data(spooled.clone());
                        all = existing.with_pack(Arc::new(PackFile::new(data.clone(), indexed.index.clone(), None)?));
                        to_upload = Some((data, indexed.index, indexed.graph));
                    }
                }
            }
            (None, None) => {}
        }

        // Everything the refs point at, and everything that points at, must be here.
        let (checked, pointed_at, before, with_pack) = (bundle.clone(), outside.clone(), existing.clone(), all.clone());
        let missing = blocking(move || {
            let tips = checked.refs.iter().map(|(_, oid)| oid).find(|oid| !with_pack.contains(oid));
            tips.or_else(|| pointed_at.iter().chain(&checked.prerequisites).find(|oid| !before.contains(oid))).copied()
        })
        .await?;
        if let Some(missing) = missing {
            return Err(HttpError::bad_request(format!("the bundle is not complete: {missing} is neither in it nor in the repository")));
        }

        // A ref named twice ends up as its last line says.
        let wanted: BTreeMap<&str, Oid> = bundle.refs.iter().map(|(name, oid)| (name.as_str(), *oid)).collect();
        let updates: Vec<RefUpdate> = wanted
            .into_iter()
            .filter(|(name, oid)| repo.refs.get(*name) != Some(oid))
            .map(|(name, oid)| RefUpdate { name: name.to_string(), old: repo.refs.get(name).copied(), new: Some(oid) })
            .collect();
        if updates.is_empty() {
            return Ok(done(&uploaded, objects, updated));
        }
        if let Some((data, index, graph)) = to_upload {
            // Unreferenced until the first transaction below commits; the orphan sweep deletes it if none does.
            let pack_id = PackStore::new_pack_id();
            app.packs.put_pack(&repo.pool_id, &pack_id, data, index, graph).await?;
            uploaded = Some(pack_id);
        }
        let mut add_pack = uploaded.clone().filter(|pack_id| !repo.packs.contains(pack_id));
        if let Some(pack_id) = &add_pack {
            if !app.packs.is_stored(&repo.pool_id, pack_id).await? {
                tracing::warn!(repo = name, pack = pack_id, "uploaded pack disappeared before commit, uploading again");
                uploaded = None;
                continue;
            }
        }

        // Set in several steps, the refs not yet set leave their objects
        // unreachable for a moment: a compaction that prunes must not start
        // in between. Holding its lease keeps it out.
        if updates.len() > BATCH && !lease.held {
            let deadline = tokio::time::Instant::now() + LEASE_PATIENCE;
            while !app.meta.try_lease(&lease.key, &lease.holder, LEASE_SECS).await? {
                if tokio::time::Instant::now() > deadline {
                    return Err(HttpError::new(StatusCode::CONFLICT, "the repository is being compacted, try again"));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            lease.held = true;
        }

        // HEAD pointing at nothing is pointed at what it was where the bundle
        // comes from, as far as a bundle tells: a branch at the same commit.
        let mut set_head = None;
        if !repo.refs.contains_key(&repo.head) && !bundle.refs.iter().any(|(name, _)| *name == repo.head) {
            let branches = || bundle.refs.iter().filter(|(name, _)| name.starts_with("refs/heads/"));
            let at_head = |name: &str| branches().any(|(other, oid)| other == name && Some(*oid) == bundle.head);
            set_head = ["refs/heads/main", "refs/heads/master"]
                .into_iter()
                .find(|name| at_head(name))
                .map(str::to_string)
                .or_else(|| branches().find(|(_, oid)| Some(*oid) == bundle.head).map(|(name, _)| name.clone()))
                .or_else(|| branches().next().map(|(name, _)| name.clone()));
        }

        let mut event = Some(json!({ "type": "import", "actor": actor, "refs": updates.len() }).to_string());
        for batch in updates.chunks(BATCH) {
            let txn = Txn {
                add_pack: add_pack.take(),
                ref_updates: batch.to_vec(),
                set_head: set_head.take(),
                // The check above is only as good as the pool it saw.
                expect_epoch: Some(repo.epoch),
                event: event.take(),
                ..Txn::default()
            };
            match app.meta.commit(name, txn).await {
                Ok(()) => updated += batch.len(),
                // Something moved in between: look again, and set what is still to be set.
                Err(MetaError::Conflict { .. } | MetaError::Stale) => continue 'attempts,
                Err(e) => return Err(e.into()),
            }
        }
        return Ok(done(&uploaded, objects, updated));
    }
    Err(HttpError::new(StatusCode::CONFLICT, "the repository kept changing, send the bundle again"))
}
