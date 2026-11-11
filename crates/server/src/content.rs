//! Read-only views of what a repository holds, for an application that shows
//! code: refs, history, directory listings, file contents and what a commit
//! changed. All of it is behind the admin token, like the rest of `/api`;
//! who may see which repository is the application's business.
//!
//! - `GET /api/repos/<id>/refs`
//! - `GET /api/repos/<id>/commits?rev=<rev>&skip=<n>&limit=<1..100>`
//! - `GET /api/repos/<id>/commit?rev=<rev>`
//! - `GET /api/repos/<id>/compare?base=<rev>&head=<rev>`, see [`crate::merge`]
//! - `GET /api/repos/<id>/tree?rev=<rev>&path=<dir>`
//! - `GET /api/repos/<id>/blob?rev=<rev>&path=<file>` or `?oid=<id>`
//!
//! A `<rev>` is a ref (in full, or a branch or tag name) or a commit id, and
//! defaults to HEAD.

use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use strata_git::object::{parse_tree, CommitInfo};
use strata_git::walk::commit_tips;
use strata_git::{ObjectKind, ObjectSource, Oid, PackSet};
use strata_store::RepoState;

use crate::upload_pack::resolve_ref;
use crate::{blocking, App, HttpError};

/// How many changed files a commit view lists before it gives up.
const MAX_CHANGES: usize = 1000;

pub(crate) const VIEWS: [&str; 6] = ["refs", "commits", "commit", "compare", "tree", "blob"];

pub(crate) async fn handle(app: &App, repo: RepoState, view: &str, query: &str) -> Result<Response, HttpError> {
    let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    let packs = app.packs.open(&repo.pool_id, &repo.packs).await?;
    let view = view.to_string();
    blocking(move || respond(&repo, &packs, &view, &params)).await?
}

fn respond(repo: &RepoState, packs: &PackSet, view: &str, params: &HashMap<String, String>) -> Result<Response, HttpError> {
    let param = |name: &str| params.get(name).map(String::as_str).unwrap_or("");
    let number = |name: &str, default: usize| match params.get(name) {
        None => Ok(default),
        Some(n) => n.parse::<usize>().map_err(|_| HttpError::bad_request(format!("{name} must be a number"))),
    };
    match view {
        "refs" => {
            let refs: Vec<Value> = repo.refs.iter().map(|(name, oid)| json!({ "name": name, "oid": oid.to_string() })).collect();
            Ok(crate::json(StatusCode::OK, json!({ "head": repo.head, "refs": refs })))
        }
        "commits" => {
            let limit = number("limit", 30)?;
            if !(1..=100).contains(&limit) {
                return Err(HttpError::bad_request("limit must be 1 to 100"));
            }
            let skip = number("skip", 0)?;
            let tip = resolve(repo, packs, param("rev"))?;
            let (oids, more) = history(packs, tip, skip, limit)?;
            let commits = oids.iter().map(|oid| commit_json(packs, oid)).collect::<Result<Vec<_>, _>>()?;
            Ok(crate::json(StatusCode::OK, json!({ "commits": commits, "next": more.then_some(skip + limit) })))
        }
        "commit" => {
            let oid = resolve(repo, packs, param("rev"))?;
            let info = packs.commit(&oid)?.ok_or_else(HttpError::not_found)?;
            // Against the first parent, as `git show` does for an ordinary commit.
            let before = match info.parents.first() {
                Some(parent) => packs.commit(parent)?.map(|parent| parent.tree),
                None => None,
            };
            let mut files = Vec::new();
            let complete = changes(packs, before, Some(info.tree), "", &mut files)?;
            let mut commit = commit_json(packs, &oid)?;
            commit["files"] = Value::Array(files);
            commit["truncated"] = Value::Bool(!complete);
            Ok(crate::json(StatusCode::OK, commit))
        }
        "compare" => crate::merge::compare(repo, packs, params),
        "tree" => {
            let oid = resolve(repo, packs, param("rev"))?;
            let info = packs.commit(&oid)?.ok_or_else(HttpError::not_found)?;
            let path = param("path").trim_matches('/');
            let tree = match find(packs, &info, path)? {
                Some((0o40000, tree)) => tree,
                Some(_) => return Err(HttpError::bad_request("not a directory")),
                None => return Err(HttpError::new(StatusCode::NOT_FOUND, "no such path")),
            };
            let data = packs.get(&tree)?.ok_or_else(HttpError::not_found)?.data;
            let entries: Vec<Value> = parse_tree(&data)?
                .iter()
                .map(|entry| {
                    json!({
                        "name": String::from_utf8_lossy(entry.name),
                        "mode": format!("{:06o}", entry.mode),
                        "type": kind_of(entry.mode),
                        "oid": entry.oid.to_string(),
                    })
                })
                .collect();
            Ok(crate::json(StatusCode::OK, json!({ "commit": oid.to_string(), "path": path, "entries": entries })))
        }
        "blob" => {
            let oid = match params.get("oid") {
                Some(hex) => Oid::from_hex(hex).ok_or_else(|| HttpError::bad_request("malformed object id"))?,
                None => {
                    let commit = resolve(repo, packs, param("rev"))?;
                    let info = packs.commit(&commit)?.ok_or_else(HttpError::not_found)?;
                    match find(packs, &info, param("path").trim_matches('/'))? {
                        Some((mode, oid)) if kind_of(mode) == "blob" => oid,
                        Some(_) => return Err(HttpError::bad_request("not a file")),
                        None => return Err(HttpError::new(StatusCode::NOT_FOUND, "no such path")),
                    }
                }
            };
            match packs.get(&oid)? {
                Some(object) if object.kind == ObjectKind::Blob => Ok((
                    [(header::CONTENT_TYPE, "application/octet-stream".to_string()), (header::ETAG, format!("\"{oid}\""))],
                    object.data,
                )
                    .into_response()),
                _ => Err(HttpError::new(StatusCode::NOT_FOUND, "no such blob")),
            }
        }
        _ => Err(HttpError::not_found()),
    }
}

fn kind_of(mode: u32) -> &'static str {
    match mode {
        0o40000 => "tree",
        // A submodule: the commit lives in another repository.
        0o160000 => "commit",
        _ => "blob",
    }
}

/// The commit a revision names.
pub(crate) fn resolve(repo: &RepoState, packs: &PackSet, rev: &str) -> Result<Oid, HttpError> {
    let unknown = || HttpError::new(StatusCode::NOT_FOUND, "unknown revision");
    let named = match rev {
        "" | "HEAD" => repo.refs.get(&repo.head).copied(),
        _ => resolve_ref(repo, rev).or_else(|| if rev.len() == 40 { Oid::from_hex(rev) } else { None }),
    };
    let oid = named.ok_or_else(unknown)?;
    if !packs.contains(&oid) {
        return Err(unknown());
    }
    commit_tips(packs, &[oid])?.first().copied().ok_or_else(unknown)
}

/// The history of `tip`, newest first: `limit` commits after the first
/// `skip`, and whether there are more. Costs as much as the commits passed over.
fn history(packs: &PackSet, tip: Oid, skip: usize, limit: usize) -> Result<(Vec<Oid>, bool), HttpError> {
    let mut seen = HashSet::from([tip]);
    let mut queue = BinaryHeap::new();
    let time = packs.commit(&tip)?.map(|commit| commit.time).unwrap_or(0);
    queue.push((time, tip));
    let mut out = Vec::new();
    let mut passed = 0;
    while let Some((_, oid)) = queue.pop() {
        if out.len() == limit {
            return Ok((out, true));
        }
        if passed < skip {
            passed += 1;
        } else {
            out.push(oid);
        }
        let Some(commit) = packs.commit(&oid)? else { continue };
        for parent in commit.parents {
            if seen.insert(parent) {
                if let Some(info) = packs.commit(&parent)? {
                    queue.push((info.time, parent));
                }
            }
        }
    }
    Ok((out, false))
}

/// The entry at `path` below a commit's root: its mode and id.
pub(crate) fn find(packs: &PackSet, commit: &CommitInfo, path: &str) -> Result<Option<(u32, Oid)>, HttpError> {
    let mut at = (0o40000, commit.tree);
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        if at.0 != 0o40000 {
            return Ok(None);
        }
        let Some(tree) = packs.get(&at.1)? else { return Ok(None) };
        match parse_tree(&tree.data)?.iter().find(|entry| entry.name == segment.as_bytes()) {
            Some(entry) => at = (entry.mode, entry.oid),
            None => return Ok(None),
        }
    }
    Ok(Some(at))
}

pub(crate) fn entries(packs: &PackSet, tree: Option<Oid>) -> Result<BTreeMap<Vec<u8>, (u32, Oid)>, HttpError> {
    let Some(tree) = tree else { return Ok(BTreeMap::new()) };
    let Some(object) = packs.get(&tree)? else { return Ok(BTreeMap::new()) };
    Ok(parse_tree(&object.data)?.iter().map(|entry| (entry.name.to_vec(), (entry.mode, entry.oid))).collect())
}

/// Lists the files that differ between two trees. Returns false if the list
/// was cut off at [`MAX_CHANGES`]. Only subtrees that differ are read.
pub(crate) fn changes(packs: &PackSet, old: Option<Oid>, new: Option<Oid>, prefix: &str, out: &mut Vec<Value>) -> Result<bool, HttpError> {
    let (before, after) = (entries(packs, old)?, entries(packs, new)?);
    let names: std::collections::BTreeSet<&Vec<u8>> = before.keys().chain(after.keys()).collect();
    for name in names {
        let (a, b) = (before.get(name).copied(), after.get(name).copied());
        if a == b {
            continue;
        }
        let path = format!("{prefix}{}", String::from_utf8_lossy(name));
        let subtree = |side: Option<(u32, Oid)>| side.filter(|(mode, _)| *mode == 0o40000).map(|(_, oid)| oid);
        let file = |side: Option<(u32, Oid)>| side.filter(|(mode, _)| *mode != 0o40000);
        let descend = subtree(a).is_some() || subtree(b).is_some();
        if descend && !changes(packs, subtree(a), subtree(b), &format!("{path}/"), out)? {
            return Ok(false);
        }
        let (a, b) = (file(a), file(b));
        if a.is_none() && b.is_none() {
            continue;
        }
        if out.len() == MAX_CHANGES {
            return Ok(false);
        }
        let side = |side: Option<(u32, Oid)>| match side {
            Some((mode, oid)) => json!({ "oid": oid.to_string(), "mode": format!("{mode:06o}") }),
            None => Value::Null,
        };
        let status = match (a, b) {
            (None, _) => "added",
            (_, None) => "deleted",
            _ => "modified",
        };
        out.push(json!({ "path": path, "status": status, "old": side(a), "new": side(b) }));
    }
    Ok(true)
}

/// A commit's signature, and the bytes it signs: the commit without the
/// signature's own header. Checking it takes the signer's key, which is the
/// application's to know.
fn signature(raw: &[u8]) -> Option<Value> {
    use base64::Engine;
    let end = raw.windows(2).position(|pair| pair == b"\n\n").map_or(raw.len(), |at| at + 1);
    let (headers, message) = raw.split_at(end);
    let (mut signed, mut text, mut inside) = (Vec::with_capacity(raw.len()), Vec::new(), false);
    for line in headers.split_inclusive(|byte| *byte == b'\n') {
        // A line starting with a space continues the header above it.
        match line.strip_prefix(b" ") {
            Some(more) if inside => text.extend_from_slice(more),
            Some(_) => signed.extend_from_slice(line),
            None => {
                let first = line.strip_prefix(b"gpgsig ").or_else(|| line.strip_prefix(b"gpgsig-sha256 "));
                inside = first.is_some();
                match first {
                    Some(first) => text.extend_from_slice(first),
                    None => signed.extend_from_slice(line),
                }
            }
        }
    }
    if text.is_empty() {
        return None;
    }
    signed.extend_from_slice(message);
    let text = String::from_utf8_lossy(&text).trim().to_string();
    let format = match &text {
        text if text.starts_with("-----BEGIN PGP SIGNATURE") => "openpgp",
        text if text.starts_with("-----BEGIN SSH SIGNATURE") => "ssh",
        text if text.starts_with("-----BEGIN SIGNED MESSAGE") => "x509",
        _ => "unknown",
    };
    Some(json!({ "format": format, "signature": text, "payload": base64::engine::general_purpose::STANDARD.encode(signed) }))
}

/// `Name <email> seconds zone`, as it stands in a commit header.
fn person(line: &str) -> Value {
    let (who, when) = match line.rfind('>') {
        Some(end) => (&line[..=end], line[end + 1..].trim()),
        None => (line, ""),
    };
    let (name, email) = match who.rfind('<') {
        Some(start) => (who[..start].trim(), who[start + 1..].trim_end_matches('>')),
        None => (who.trim(), ""),
    };
    let (time, zone) = when.split_once(' ').unwrap_or((when, ""));
    json!({ "name": name, "email": email, "time": time.parse::<i64>().unwrap_or(0), "zone": zone })
}

pub(crate) fn commit_json(packs: &PackSet, oid: &Oid) -> Result<Value, HttpError> {
    let object = packs.get(oid)?.filter(|object| object.kind == ObjectKind::Commit).ok_or_else(HttpError::not_found)?;
    let text = String::from_utf8_lossy(&object.data);
    let (headers, message) = text.split_once("\n\n").unwrap_or((&text, ""));
    let mut commit = json!({ "oid": oid.to_string(), "parents": [], "message": message });
    let mut parents = Vec::new();
    for line in headers.lines() {
        match line.split_once(' ') {
            Some(("tree", tree)) => commit["tree"] = json!(tree),
            Some(("parent", parent)) => parents.push(json!(parent)),
            Some(("author", author)) => commit["author"] = person(author),
            Some(("committer", committer)) => commit["committer"] = person(committer),
            // Signatures and other multi-line headers are not shown.
            _ => {}
        }
    }
    commit["parents"] = Value::Array(parents);
    commit["signature"] = signature(&object.data).unwrap_or(Value::Null);
    Ok(commit)
}

#[cfg(test)]
mod tests {
    use super::signature;
    use base64::Engine;

    #[test]
    fn signatures_are_told_from_what_they_sign() {
        let plain = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n\nmessage\n\n with a gap\n";
        assert_eq!(signature(plain), None);
        let signed = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\ngpgsig -----BEGIN SSH SIGNATURE-----\n AAAA\n BBBB\n -----END SSH SIGNATURE-----\nencoding latin1\n\nmessage\n\n with a gap\n";
        let found = signature(signed).unwrap();
        assert_eq!(found["format"], "ssh");
        assert_eq!(found["signature"], "-----BEGIN SSH SIGNATURE-----\nAAAA\nBBBB\n-----END SSH SIGNATURE-----");
        let payload = base64::engine::general_purpose::STANDARD.decode(found["payload"].as_str().unwrap()).unwrap();
        let expected = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\nencoding latin1\n\nmessage\n\n with a gap\n";
        assert_eq!(payload, expected);
    }
}
