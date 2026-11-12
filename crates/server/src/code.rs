//! Questions about code that take reading a lot of it: who last changed each
//! line of a file, which commits touched a path, and where some text occurs.
//!
//! - `GET /api/repos/<id>/blame?rev=<rev>&path=<file>`
//! - `GET /api/repos/<id>/search?rev=<rev>&q=<text>[&regex=1][&case=1][&path=<dir>][&limit=<n>]`
//! - `GET /api/repos/<id>/commits?...&path=<path>`: only the commits that changed it
//!
//! Nothing here is indexed: each answer is worked out from the objects, and
//! gives up after [`BUDGET`], saying so. Renames are not followed.

use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use strata_git::object::{parse_tree, CommitInfo};
use strata_git::{ObjectKind, ObjectSource, Oid, PackSet};
use strata_store::RepoState;

use crate::content::{commit_json, find, resolve};
use crate::HttpError;

/// How long an answer is worked on before what there is so far is given.
const BUDGET: Duration = Duration::from_secs(10);
/// Files larger than this are not blamed, and not searched.
const MAX_FILE: usize = 2 << 20;
/// What a search stops at, unless asked for less.
const MAX_MATCHES: usize = 500;
/// Matches shown of one file.
const MAX_PER_FILE: usize = 20;
/// How much of a matching line is shown.
const MAX_LINE: usize = 400;

fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

fn blob_at(packs: &PackSet, commit: &CommitInfo, path: &str) -> Result<Option<Oid>, HttpError> {
    Ok(find(packs, commit, path)?.filter(|(mode, _)| *mode != 0o40000 && *mode != 0o160000).map(|(_, oid)| oid))
}

/// Whether a commit changed what is at `path`: it differs from what every
/// parent has there. A merge that took one side's version did not change it.
pub(crate) fn touches(packs: &PackSet, commit: &CommitInfo, path: &str) -> Result<bool, HttpError> {
    let here = find(packs, commit, path)?;
    if commit.parents.is_empty() {
        return Ok(here.is_some());
    }
    for parent in &commit.parents {
        let Some(parent) = packs.commit(parent)? else { continue };
        if find(packs, &parent, path)? == here {
            return Ok(false);
        }
    }
    Ok(true)
}

/// For each line of `new`, the line of `old` it was left unchanged from, if any. Lines count from 0.
fn unchanged(old: &[u8], new: &[u8]) -> Vec<Option<u32>> {
    let lines = |data: &[u8]| data.split_inclusive(|b| *b == b'\n').count();
    let mut from = vec![None; lines(new)];
    // One hunk with every line in it: context enough to span both files.
    let mut options = diffy::DiffOptions::new();
    options.set_context_len(lines(old) + lines(new) + 1);
    let patch = options.create_patch_bytes(old, new);
    let (mut a, mut b) = (0u32, 0usize);
    for hunk in patch.hunks() {
        for line in hunk.lines() {
            match line {
                diffy::Line::Context(_) => {
                    if let Some(slot) = from.get_mut(b) {
                        *slot = Some(a);
                    }
                    a += 1;
                    b += 1;
                }
                diffy::Line::Delete(_) => a += 1,
                diffy::Line::Insert(_) => b += 1,
            }
        }
    }
    from
}

/// Lines of a file still looking for the commit that wrote them: where each
/// stands in the file as this commit has it, and in the file that was asked about.
struct Pending {
    blob: Oid,
    lines: Vec<(u32, u32)>,
}

pub(crate) fn blame(repo: &RepoState, packs: &PackSet, rev: &str, path: &str) -> Result<Response, HttpError> {
    let started = Instant::now();
    let tip = resolve(repo, packs, rev)?;
    let info = packs.commit(&tip)?.ok_or_else(HttpError::not_found)?;
    let path = path.trim_matches('/');
    let blob = blob_at(packs, &info, path)?.ok_or_else(|| HttpError::new(StatusCode::NOT_FOUND, "no such file"))?;
    let data = packs.get(&blob)?.ok_or_else(HttpError::not_found)?.data;
    if data.len() > MAX_FILE || is_binary(&data) {
        return Err(HttpError::new(StatusCode::UNPROCESSABLE_ENTITY, "this file is too large, or not text"));
    }
    let count = data.split_inclusive(|b| *b == b'\n').count() as u32;

    // Commits are taken newest first, so that all the lines a commit is
    // handed, by whichever of its children, are there when its turn comes.
    let mut waiting: HashMap<Oid, Pending> = HashMap::from([(tip, Pending { blob, lines: (0..count).map(|line| (line, line)).collect() })]);
    let mut queue = BinaryHeap::from([(info.time, tip)]);
    let mut written_by: Vec<Option<Oid>> = vec![None; count as usize];
    let mut truncated = false;
    while let Some((_, oid)) = queue.pop() {
        let Some(mut pending) = waiting.remove(&oid) else { continue };
        let commit = packs.commit(&oid)?.ok_or_else(HttpError::not_found)?;
        // Out of time: what is left is put down to the oldest commits reached.
        let spent = started.elapsed() > BUDGET;
        truncated |= spent;
        for parent in commit.parents.iter().filter(|_| !spent) {
            if pending.lines.is_empty() {
                break;
            }
            let Some(parent_info) = packs.commit(parent)? else { continue };
            let Some(theirs) = blob_at(packs, &parent_info, path)? else { continue };
            let handed: Vec<(u32, u32)> = if theirs == pending.blob {
                std::mem::take(&mut pending.lines)
            } else {
                let Some(old) = packs.get(&theirs)?.filter(|object| object.kind == ObjectKind::Blob) else { continue };
                if old.data.len() > MAX_FILE {
                    continue;
                }
                let new = packs.get(&pending.blob)?.ok_or_else(HttpError::not_found)?;
                let from = unchanged(&old.data, &new.data);
                let (kept, handed): (Vec<_>, Vec<_>) = pending.lines.iter().partition(|(line, _)| from.get(*line as usize).copied().flatten().is_none());
                pending.lines = kept;
                handed.into_iter().map(|(line, asked)| (from[line as usize].expect("handed lines have a source"), asked)).collect()
            };
            if handed.is_empty() {
                continue;
            }
            match waiting.get_mut(parent) {
                Some(already) => already.lines.extend(handed),
                None => {
                    waiting.insert(*parent, Pending { blob: theirs, lines: handed });
                    queue.push((parent_info.time, *parent));
                }
            }
        }
        for (_, asked) in pending.lines {
            written_by[asked as usize] = Some(oid);
        }
    }

    // Runs of lines by the same commit, and each of those commits once.
    let mut hunks: Vec<Value> = Vec::new();
    let mut commits = serde_json::Map::new();
    let mut start = 0;
    while start < written_by.len() {
        let oid = written_by[start].unwrap_or(tip);
        let end = (start..written_by.len()).find(|line| written_by[*line].unwrap_or(tip) != oid).unwrap_or(written_by.len());
        hunks.push(json!({ "start": start + 1, "lines": end - start, "oid": oid.to_string() }));
        if !commits.contains_key(&oid.to_string()) {
            let mut commit = commit_json(packs, &oid)?;
            // Who signed it is not what is asked here.
            commit.as_object_mut().map(|fields| fields.remove("signature"));
            commits.insert(oid.to_string(), commit);
        }
        start = end;
    }
    Ok(crate::json(StatusCode::OK, json!({ "commit": tip.to_string(), "path": path, "lines": count, "hunks": hunks, "commits": commits, "truncated": truncated })))
}

struct Search<'a> {
    packs: &'a PackSet,
    pattern: regex::bytes::Regex,
    limit: usize,
    started: Instant,
    files: Vec<Value>,
    paths: Vec<Value>,
    matches: usize,
    truncated: bool,
}

impl Search<'_> {
    fn full(&mut self) -> bool {
        if self.matches >= self.limit || self.started.elapsed() > BUDGET {
            self.truncated = true;
        }
        self.truncated
    }

    fn walk(&mut self, tree: Oid, prefix: &str) -> Result<(), HttpError> {
        let Some(object) = self.packs.get(&tree)? else { return Ok(()) };
        for entry in parse_tree(&object.data)? {
            if self.full() {
                return Ok(());
            }
            let path = format!("{prefix}{}", String::from_utf8_lossy(entry.name));
            if entry.is_tree() {
                self.walk(entry.oid, &format!("{path}/"))?;
                continue;
            }
            if entry.is_gitlink() {
                continue;
            }
            // A file is also found by its name.
            if self.pattern.is_match(path.as_bytes()) && self.paths.len() < 100 {
                self.paths.push(json!(path));
            }
            let Some(blob) = self.packs.get(&entry.oid)? else { continue };
            if blob.data.len() > MAX_FILE || is_binary(&blob.data) || !self.pattern.is_match(&blob.data) {
                continue;
            }
            let mut found = Vec::new();
            let mut more = false;
            for (number, line) in blob.data.split(|b| *b == b'\n').enumerate() {
                let Some(hit) = self.pattern.find(line) else { continue };
                if found.len() == MAX_PER_FILE || self.matches >= self.limit {
                    more = true;
                    self.truncated |= self.matches >= self.limit;
                    break;
                }
                // Long lines are shown around what was found in them.
                let from = hit.start().saturating_sub(MAX_LINE / 4);
                let shown = &line[from..line.len().min(from + MAX_LINE)];
                found.push(json!({ "line": number + 1, "text": String::from_utf8_lossy(shown).trim_end_matches('\r'), "cut": from > 0 || shown.len() < line.len() - from }));
                self.matches += 1;
            }
            if !found.is_empty() {
                self.files.push(json!({ "path": path, "matches": found, "more": more }));
            }
        }
        Ok(())
    }
}

pub(crate) fn search(repo: &RepoState, packs: &PackSet, params: &HashMap<String, String>) -> Result<Response, HttpError> {
    let param = |name: &str| params.get(name).map(String::as_str).unwrap_or("");
    let on = |name: &str| matches!(param(name), "1" | "true");
    let query = param("q");
    if query.is_empty() || query.len() > 500 {
        return Err(HttpError::bad_request("q is the text to look for, up to 500 characters"));
    }
    let source = if on("regex") { query.to_string() } else { regex::escape(query) };
    let pattern = regex::bytes::RegexBuilder::new(&source)
        .case_insensitive(!on("case"))
        // A file is searched whole first: ^ and $ are the ends of its lines.
        .multi_line(true)
        .size_limit(1 << 20)
        .build()
        .map_err(|_| HttpError::bad_request("q is not a regular expression this server takes"))?;
    let limit = match params.get("limit") {
        None => 200,
        Some(limit) => limit.parse::<usize>().ok().filter(|n| (1..=MAX_MATCHES).contains(n)).ok_or_else(|| HttpError::bad_request("limit must be 1 to 500"))?,
    };
    let tip = resolve(repo, packs, param("rev"))?;
    let info = packs.commit(&tip)?.ok_or_else(HttpError::not_found)?;
    let below = param("path").trim_matches('/');
    let root = match find(packs, &info, below)? {
        Some((0o40000, tree)) => tree,
        Some(_) => return Err(HttpError::bad_request("path is not a directory")),
        None => return Err(HttpError::new(StatusCode::NOT_FOUND, "no such path")),
    };
    let mut search = Search { packs, pattern, limit, started: Instant::now(), files: Vec::new(), paths: Vec::new(), matches: 0, truncated: false };
    search.walk(root, &if below.is_empty() { String::new() } else { format!("{below}/") })?;
    Ok(crate::json(
        StatusCode::OK,
        json!({ "commit": tip.to_string(), "files": search.files, "paths": search.paths, "matches": search.matches, "truncated": search.truncated }),
    ))
}

#[cfg(test)]
mod tests {
    use super::unchanged;

    #[test]
    fn lines_are_traced_to_where_they_stood_before() {
        let old = b"one\ntwo\nthree\nfour\n";
        assert_eq!(unchanged(old, b"zero\none\ntwo\nTHREE\nfour\n"), vec![None, Some(0), Some(1), None, Some(3)]);
        assert_eq!(unchanged(old, b"two\nfour\n"), vec![Some(1), Some(3)]);
        assert_eq!(unchanged(b"", b"new\n"), vec![None]);
        // A last line that gained its newline is a changed line.
        assert_eq!(unchanged(b"a\nb", b"a\nb\n"), vec![Some(0), None]);
    }
}
