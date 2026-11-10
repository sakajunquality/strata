//! What an application needs for pull requests: how two revisions differ,
//! and merging one into a branch without a working copy.
//!
//! - `GET /api/repos/<id>/compare?base=<rev>&head=<rev>`
//! - `POST /api/repos/<id>/merge`
//!
//! A merge is a three-way merge of trees against the merge base. Files changed
//! on both sides are merged line by line; whatever cannot be (overlapping
//! edits, binary files, one side deleting what the other changed) is reported
//! as a conflict and nothing is written. The result lands like a push: one
//! small pack and one ref update in a single transaction, with an event in
//! the feed. The pre-receive hook is not asked: the application is the caller.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use bytes::Bytes;
use serde_json::json;
use strata_git::object::object_id;
use strata_git::pack::{index_pack, PackWriter};
use strata_git::{ObjectKind, ObjectSource, Oid, PackData, PackSet};
use strata_store::{MetaError, PackStore, RefUpdate, RepoState, Txn};

use crate::content::{changes, commit_json, entries, resolve};
use crate::{blocking, App, HttpError};

/// How many commits a comparison lists.
const MAX_COMMITS: usize = 250;
/// Files larger than this are not merged line by line.
const MAX_MERGED_FILE: usize = 4 << 20;
/// How many conflicting paths are reported.
const MAX_CONFLICTS: usize = 200;
/// Times a merge is redone when the repository changes underneath it.
const ATTEMPTS: usize = 3;

const TREE: u32 = 0o40000;
type Entry = (u32, Oid);

/// How two commits relate.
struct Divergence {
    /// The best common ancestors, newest first. Empty for unrelated histories.
    bases: Vec<Oid>,
    /// Commits only `head` has, newest first.
    ahead: Vec<Oid>,
    /// Number of commits only `base` has.
    behind: usize,
}

/// Walks both histories newest first until everything left is common to
/// both, so the cost is that of what differs, not of the whole history.
fn diverge(src: &dyn ObjectSource, base: Oid, head: Oid) -> Result<Divergence, HttpError> {
    const BASE: u8 = 1;
    const HEAD: u8 = 2;
    const STALE: u8 = 4;
    let mut flags: HashMap<Oid, (u8, i64)> = HashMap::new();
    let mut queue: BinaryHeap<(i64, Oid)> = BinaryHeap::new();
    for (oid, flag) in [(base, BASE), (head, HEAD)] {
        let time = src.commit(&oid)?.ok_or_else(HttpError::not_found)?.time;
        flags.entry(oid).or_insert((0, time)).0 |= flag;
        queue.push((time, oid));
    }
    let mut found = Vec::new();
    while queue.iter().any(|(_, oid)| flags[oid].0 & STALE == 0) {
        let Some((_, oid)) = queue.pop() else { break };
        let mut pass = flags[&oid].0;
        if pass & (BASE | HEAD) == (BASE | HEAD) {
            if !found.contains(&oid) {
                found.push(oid);
            }
            // Everything below a common ancestor is common too, and no better.
            pass |= STALE;
        }
        let Some(commit) = src.commit(&oid)? else { continue };
        for parent in commit.parents {
            // Parents this source lacks (beyond a shallow boundary) end the walk there.
            let Some(info) = src.commit(&parent)? else { continue };
            let entry = flags.entry(parent).or_insert((0, info.time));
            if entry.0 & pass != pass {
                entry.0 |= pass;
                queue.push((info.time, parent));
            }
        }
    }
    let bases = found.into_iter().filter(|oid| flags[oid].0 & STALE == 0).collect();
    let mut ahead: Vec<(i64, Oid)> = flags.iter().filter(|(_, (flag, _))| flag & (BASE | HEAD) == HEAD).map(|(oid, (_, time))| (*time, *oid)).collect();
    ahead.sort_unstable_by(|a, b| b.cmp(a));
    let behind = flags.values().filter(|(flag, _)| flag & (BASE | HEAD) == BASE).count();
    Ok(Divergence { bases, ahead: ahead.into_iter().map(|(_, oid)| oid).collect(), behind })
}

fn tree_of(packs: &PackSet, commit: &Oid) -> Result<Oid, HttpError> {
    Ok(packs.commit(commit)?.ok_or_else(HttpError::not_found)?.tree)
}

/// `compare`: what `head` has that `base` does not, as a pull request shows
/// it. Files are compared against the merge base, so changes made on `base`
/// in the meantime do not show up as reverted.
pub(crate) fn compare(repo: &RepoState, packs: &PackSet, params: &HashMap<String, String>) -> Result<Response, HttpError> {
    let param = |name: &str| params.get(name).map(String::as_str).unwrap_or("");
    let base = resolve(repo, packs, param("base"))?;
    let head = resolve(repo, packs, param("head"))?;
    let divergence = diverge(packs, base, head)?;
    let merge_base = divergence.bases.first().copied();
    let mut files = Vec::new();
    let from = tree_of(packs, &merge_base.unwrap_or(base))?;
    let complete = changes(packs, Some(from), Some(tree_of(packs, &head)?), "", &mut files)?;
    let commits = divergence.ahead.iter().take(MAX_COMMITS).map(|oid| commit_json(packs, oid)).collect::<Result<Vec<_>, _>>()?;
    Ok(crate::json(
        StatusCode::OK,
        json!({
            "base": base.to_string(),
            "head": head.to_string(),
            "merge_base": merge_base.map(|oid| oid.to_string()),
            "ahead": divergence.ahead.len(),
            "behind": divergence.behind,
            "commits": commits,
            "files": files,
            "truncated": !complete,
        }),
    ))
}

fn is_file(mode: u32) -> bool {
    mode == 0o100644 || mode == 0o100755
}

/// Merges trees, collecting the objects that have to be written.
struct Merger<'a> {
    packs: &'a PackSet,
    objects: Vec<(ObjectKind, Vec<u8>)>,
    made: HashSet<Oid>,
    conflicts: Vec<String>,
}

impl Merger<'_> {
    fn keep(&mut self, kind: ObjectKind, data: Vec<u8>) -> Oid {
        let oid = object_id(kind, &data);
        if !self.packs.contains(&oid) && self.made.insert(oid) {
            self.objects.push((kind, data));
        }
        oid
    }

    fn conflict(&mut self, path: String) {
        if self.conflicts.len() < MAX_CONFLICTS {
            self.conflicts.push(path);
        }
    }

    /// The merged directory, or `None` if nothing is left in it.
    fn tree(&mut self, base: Option<Oid>, ours: Option<Oid>, theirs: Option<Oid>, prefix: &str) -> Result<Option<Oid>, HttpError> {
        let (base, ours, theirs) = (entries(self.packs, base)?, entries(self.packs, ours)?, entries(self.packs, theirs)?);
        let names: BTreeSet<&Vec<u8>> = base.keys().chain(ours.keys()).chain(theirs.keys()).collect();
        let mut merged: BTreeMap<Vec<u8>, Entry> = BTreeMap::new();
        for name in names {
            let (b, o, t) = (base.get(name).copied(), ours.get(name).copied(), theirs.get(name).copied());
            let pick = if o == t || b == t {
                o
            } else if b == o {
                t
            } else {
                // Both sides changed it, differently.
                let path = format!("{prefix}{}", String::from_utf8_lossy(name));
                let dir = |side: Option<Entry>| side.filter(|(mode, _)| *mode == TREE).map(|(_, oid)| oid);
                match (o, t) {
                    (Some((TREE, _)), Some((TREE, _))) => self.tree(dir(b), dir(o), dir(t), &format!("{path}/"))?.map(|oid| (TREE, oid)),
                    (Some(ours), Some(theirs)) if is_file(ours.0) && is_file(theirs.0) => {
                        match self.file(b.filter(|(mode, _)| is_file(*mode)), ours, theirs)? {
                            Some(entry) => Some(entry),
                            None => {
                                self.conflict(path);
                                o
                            }
                        }
                    }
                    _ => {
                        self.conflict(path);
                        o
                    }
                }
            };
            if let Some(entry) = pick {
                merged.insert(name.clone(), entry);
            }
        }
        if merged.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.keep(ObjectKind::Tree, encode_tree(merged))))
    }

    /// A file both sides changed, or `None` if the changes cannot be combined.
    fn file(&mut self, base: Option<Entry>, ours: Entry, theirs: Entry) -> Result<Option<Entry>, HttpError> {
        // Without a common version there is nothing to tell the changes apart by.
        let Some(base) = base else { return Ok(None) };
        let mode = match (ours.0 == theirs.0, base.0 == ours.0, base.0 == theirs.0) {
            (true, _, _) | (_, _, true) => ours.0,
            (_, true, _) => theirs.0,
            _ => return Ok(None),
        };
        if ours.1 == theirs.1 || base.1 == theirs.1 {
            return Ok(Some((mode, ours.1)));
        }
        if base.1 == ours.1 {
            return Ok(Some((mode, theirs.1)));
        }
        let mut texts = Vec::new();
        for oid in [base.1, ours.1, theirs.1] {
            let data = self.packs.get(&oid)?.ok_or_else(HttpError::not_found)?.data;
            if data.len() > MAX_MERGED_FILE || data.contains(&0) {
                return Ok(None);
            }
            texts.push(data);
        }
        match diffy::merge_bytes(&texts[0], &texts[1], &texts[2]) {
            Ok(merged) => Ok(Some((mode, self.keep(ObjectKind::Blob, merged)))),
            Err(_) => Ok(None),
        }
    }
}

/// Entries in git's order: by name, a directory sorting as if it ended in `/`.
fn encode_tree(entries: BTreeMap<Vec<u8>, Entry>) -> Vec<u8> {
    let mut sorted: Vec<(Vec<u8>, Entry)> = entries
        .into_iter()
        .map(|(mut name, entry)| {
            if entry.0 == TREE {
                name.push(b'/');
            }
            (name, entry)
        })
        .collect();
    sorted.sort();
    let mut out = Vec::new();
    for (name, (mode, oid)) in sorted {
        let name = if mode == TREE { &name[..name.len() - 1] } else { &name[..] };
        out.extend_from_slice(format!("{mode:o} ").as_bytes());
        out.extend_from_slice(name);
        out.push(0);
        out.extend_from_slice(oid.as_bytes());
    }
    out
}

#[derive(serde::Deserialize)]
struct Person {
    name: String,
    email: String,
}

#[derive(serde::Deserialize)]
struct Request {
    /// The branch to merge into: a ref in full, or a branch name.
    base: String,
    /// What to merge: any revision.
    head: String,
    /// Refuse unless `head` is this commit, so that what is merged is what was looked at.
    expect_head: Option<String>,
    /// `merge` (a commit with both as parents, the default) or `squash` (one commit on top of `base`).
    method: Option<String>,
    message: Option<String>,
    author: Option<Person>,
    /// Who is doing this, for the event feed.
    actor: Option<String>,
    /// Only say whether it would work.
    #[serde(default)]
    dry_run: bool,
}

/// What a merge would do, worked out against one snapshot.
struct Plan {
    name: String,
    base: Oid,
    head: Oid,
    merge_base: Option<Oid>,
    ahead: usize,
    conflicts: Vec<String>,
    /// The new commit and everything it needs that is not stored yet.
    objects: Vec<(ObjectKind, Vec<u8>)>,
    commit: Oid,
}

fn conflict(message: impl Into<String>) -> HttpError {
    HttpError::new(StatusCode::CONFLICT, message)
}

fn plan(repo: &RepoState, packs: &PackSet, request: &Request) -> Result<Plan, HttpError> {
    let name = if request.base.starts_with("refs/") { request.base.clone() } else { format!("refs/heads/{}", request.base) };
    let base = *repo.refs.get(&name).ok_or_else(|| HttpError::new(StatusCode::NOT_FOUND, "no such branch"))?;
    if packs.commit(&base)?.is_none() {
        return Err(HttpError::bad_request("the base is not a branch"));
    }
    let head = resolve(repo, packs, &request.head)?;
    if request.expect_head.as_ref().is_some_and(|expected| *expected != head.to_string()) {
        return Err(conflict("the head has changed since"));
    }
    let squash = match request.method.as_deref() {
        None | Some("merge") => false,
        Some("squash") => true,
        Some(_) => return Err(HttpError::bad_request("method must be merge or squash")),
    };
    let divergence = diverge(packs, base, head)?;
    let merge_base = divergence.bases.first().copied();
    let mut plan = Plan { name, base, head, merge_base, ahead: divergence.ahead.len(), conflicts: Vec::new(), objects: Vec::new(), commit: Oid::ZERO };
    if plan.ahead == 0 {
        return Ok(plan);
    }
    let Some(merge_base) = merge_base else { return Err(conflict("the two have no history in common")) };

    let mut merger = Merger { packs, objects: Vec::new(), made: HashSet::new(), conflicts: Vec::new() };
    let tree = if merge_base == base {
        // Nothing happened on the base since: the result is the head as it is.
        tree_of(packs, &head)?
    } else {
        let merged = merger.tree(Some(tree_of(packs, &merge_base)?), Some(tree_of(packs, &base)?), Some(tree_of(packs, &head)?), "")?;
        merged.unwrap_or_else(|| merger.keep(ObjectKind::Tree, Vec::new()))
    };
    if !merger.conflicts.is_empty() || request.dry_run {
        plan.conflicts = merger.conflicts;
        return Ok(plan);
    }

    let clean = |text: &str| text.chars().filter(|c| !matches!(c, '<' | '>' | '\n' | '\r' | '\0')).collect::<String>().trim().to_string();
    let who = match &request.author {
        Some(person) => format!("{} <{}>", clean(&person.name), clean(&person.email)),
        None => "Strata <strata@localhost>".to_string(),
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0);
    let short = plan.name.strip_prefix("refs/heads/").unwrap_or(&plan.name);
    let message = match request.message.as_deref().map(str::trim).filter(|message| !message.is_empty()) {
        Some(message) => message.replace('\0', ""),
        None => format!("Merge {} into {short}", request.head),
    };
    let mut commit = format!("tree {tree}\nparent {base}\n");
    if !squash {
        commit.push_str(&format!("parent {head}\n"));
    }
    commit.push_str(&format!("author {who} {now} +0000\ncommitter {who} {now} +0000\n\n{message}\n"));
    plan.commit = merger.keep(ObjectKind::Commit, commit.into_bytes());
    plan.objects = merger.objects;
    Ok(plan)
}

/// `POST /api/repos/<id>/merge`, with a JSON body as in [`Request`].
///
/// Answers `{"merged": true, "oid": ...}`, or 409 when there is nothing to
/// merge, the changes conflict or the branch keeps moving. With `dry_run`,
/// answers `{"mergeable": ..., "conflicts": [...]}` and changes nothing.
pub(crate) async fn merge(app: &App, name: &str, body: Body) -> Result<Response, HttpError> {
    let body = axum::body::to_bytes(body, 1 << 20).await.map_err(|_| HttpError::bad_request("request too large"))?;
    let request: Arc<Request> = Arc::new(serde_json::from_slice(&body).map_err(|e| HttpError::bad_request(format!("malformed request: {e}")))?);

    for _ in 0..ATTEMPTS {
        let repo = app.meta.get_repo(name).await?.ok_or_else(HttpError::not_found)?;
        let packs = app.packs.open(&repo.pool_id, &repo.packs).await?;
        let (snapshot, existing, asked) = (repo.clone(), packs.clone(), request.clone());
        let plan = blocking(move || plan(&snapshot, &existing, &asked)).await??;
        let described = json!({
            "base": plan.base.to_string(),
            "head": plan.head.to_string(),
            "merge_base": plan.merge_base.map(|oid| oid.to_string()),
            "ahead": plan.ahead,
        });
        if request.dry_run {
            let mut answer = described;
            answer["mergeable"] = json!(plan.ahead > 0 && plan.conflicts.is_empty());
            answer["conflicts"] = json!(plan.conflicts);
            return Ok(crate::json(StatusCode::OK, answer));
        }
        if plan.ahead == 0 {
            return Err(conflict("nothing to merge"));
        }
        if !plan.conflicts.is_empty() {
            return Err(conflict(format!("conflicting changes in {}", plan.conflicts.join(", "))));
        }

        let objects = plan.objects;
        let indexed = blocking(move || -> strata_git::Result<_> {
            let mut writer = PackWriter::new(Vec::new(), objects.len() as u32)?;
            for (kind, data) in &objects {
                writer.add(*kind, data)?;
            }
            let data = Bytes::from(writer.finish()?.out);
            let indexed = index_pack(&data, &packs)?;
            Ok((data, indexed))
        })
        .await??;
        let (data, indexed) = indexed;
        let pack_id = PackStore::new_pack_id();
        let data: Arc<dyn PackData> = Arc::new(data);
        app.packs.put_pack(&repo.pool_id, &pack_id, data, indexed.index, indexed.graph).await?;

        let update = json!({ "ref": plan.name, "old": plan.base.to_string(), "new": plan.commit.to_string(), "change": "fast-forward" });
        let event = json!({
            "type": "push",
            "actor": request.actor,
            "updates": [update],
            "merge": { "head": plan.head.to_string(), "method": request.method.as_deref().unwrap_or("merge") },
        });
        let txn = Txn {
            add_pack: Some(pack_id),
            ref_updates: vec![RefUpdate { name: plan.name.clone(), old: Some(plan.base), new: Some(plan.commit) }],
            // The merge was worked out from the objects of this epoch.
            expect_epoch: Some(repo.epoch),
            event: Some(event.to_string()),
            ..Txn::default()
        };
        match app.meta.commit(&repo.name, txn).await {
            Ok(()) => {
                if repo.packs.len() + 1 >= app.options.compact_at {
                    app.request_compaction(&repo.name);
                }
                let mut answer = described;
                answer["merged"] = json!(true);
                answer["oid"] = json!(plan.commit.to_string());
                return Ok(crate::json(StatusCode::OK, answer));
            }
            // The branch moved or the pool was reorganised: work it out again.
            // The pack just uploaded is left to the orphan sweep.
            Err(MetaError::Conflict { .. } | MetaError::Stale) => continue,
            Err(MetaError::NotFound) => return Err(HttpError::not_found()),
            Err(e) => return Err(e.into()),
        }
    }
    Err(conflict("the branch kept changing, try again"))
}
