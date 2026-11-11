//! Object graph traversal.
//!
//! Commits are read through [`ObjectSource::commit`], which is answered from a
//! commit-graph when the pack has one.

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::bitmap::{kind_slot, Reachable};
use crate::bitset::BitSet;
use crate::object::{parse_tag, parse_tree, CommitInfo};
use crate::sparse::Patterns;
use crate::{Error, ObjectKind, ObjectSource, Oid, OidMap, OidSet, PackFile, PackSet, Result};

/// Follows annotated tags from `oid`. Returns the first non-tag object, or
/// `None` if `oid` is not a tag.
pub fn peel(src: &dyn ObjectSource, oid: &Oid) -> Result<Option<Oid>> {
    let mut current = *oid;
    let mut peeled = None;
    loop {
        let object = src.get(&current)?.ok_or(Error::Missing(current))?;
        if object.kind != ObjectKind::Tag {
            return Ok(peeled);
        }
        current = parse_tag(&object.data)?;
        peeled = Some(current);
    }
}

/// Resolves each id to the commit it denotes, dropping anything that is not
/// (a tag of) a commit present in `src`.
pub fn commit_tips(src: &dyn ObjectSource, oids: &[Oid]) -> Result<Vec<Oid>> {
    let mut tips = Vec::new();
    for oid in oids {
        let mut current = *oid;
        loop {
            if src.commit(&current)?.is_some() {
                tips.push(current);
                break;
            }
            match src.get(&current)? {
                Some(object) if object.kind == ObjectKind::Tag => current = parse_tag(&object.data)?,
                _ => break,
            }
        }
    }
    Ok(tips)
}

/// Whether commit `ancestor` is `descendant` or part of its history.
///
/// Walks newest first and stops as soon as the answer is yes, so the usual
/// case (a branch moved forward by a few commits) reads a few commits. A
/// "no" has to look at all of `descendant`'s history.
pub fn is_ancestor(src: &dyn ObjectSource, ancestor: &Oid, descendant: &Oid) -> Result<bool> {
    let mut seen = HashSet::new();
    let mut queue = BinaryHeap::new();
    let commit = src.commit(descendant)?.ok_or(Error::Missing(*descendant))?;
    queue.push((commit.time, *descendant));
    seen.insert(*descendant);
    while let Some((_, oid)) = queue.pop() {
        if oid == *ancestor {
            return Ok(true);
        }
        let commit = src.commit(&oid)?.ok_or(Error::Missing(oid))?;
        for parent in commit.parents {
            if seen.insert(parent) {
                // Parents this source lacks (beyond a shallow boundary) end the walk there.
                if let Some(info) = src.commit(&parent)? {
                    queue.push((info.time, parent));
                }
            }
        }
    }
    Ok(false)
}

/// Collects every commit reachable from `tips` that is present in `src`.
fn reachable_commits(src: &dyn ObjectSource, tips: &[Oid]) -> Result<HashSet<Oid>> {
    let mut commits = HashSet::new();
    let mut stack = commit_tips(src, tips)?;
    while let Some(oid) = stack.pop() {
        if !commits.insert(oid) {
            continue;
        }
        if let Some(commit) = src.commit(&oid)? {
            stack.extend(commit.parents);
        }
    }
    Ok(commits)
}

/// Which objects a partial clone asked the server to leave out. Conditions
/// add up: an object is sent only if every one of them lets it through,
/// which is what `combine:` asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// `blob:none`
    blob_none: bool,
    /// `blob:limit=<n>`: omit blobs of at least `n` bytes.
    blob_limit: Option<u64>,
    /// `tree:<n>`: omit trees and blobs `n` or more levels below a commit's
    /// root tree, which is level 0. `tree:0` omits all of them.
    tree_depth: Option<u32>,
    /// `sparse:oid=<blob>`: the blobs named, to be read with [`Filter::load_sparse`].
    sparse_names: Vec<String>,
    /// Their patterns: only files they include are sent. Trees always are.
    sparse: Vec<Arc<Patterns>>,
    /// `object:type=<kind>`: the kinds that are not sent, by [`kind_slot`].
    /// What a fetch asks for by name is sent whatever its kind.
    omitted_kinds: [bool; 4],
}

/// More sub-filters than anyone has a use for.
const MAX_COMBINED: usize = 16;

impl Filter {
    /// Parses a filter spec. Returns `None` for specs this server cannot honour.
    pub fn parse(spec: &str) -> Option<Filter> {
        let mut filter = Filter::default();
        filter.add(spec, 0)?;
        (filter.sparse_names.len() <= MAX_COMBINED).then_some(filter)
    }

    fn add(&mut self, spec: &str, nesting: usize) -> Option<()> {
        if let Some(parts) = spec.strip_prefix("combine:") {
            if nesting > 2 {
                return None;
            }
            // Each part is URL-encoded, so that a part can itself be a combination.
            for part in parts.split('+') {
                self.add(&percent_decode(part)?, nesting + 1)?;
            }
        } else if spec == "blob:none" {
            self.blob_none = true;
        } else if let Some(depth) = spec.strip_prefix("tree:") {
            let depth = depth.parse().ok()?;
            self.tree_depth = Some(self.tree_depth.map_or(depth, |other| other.min(depth)));
        } else if let Some(kind) = spec.strip_prefix("object:type=") {
            let kind = match kind {
                "commit" => ObjectKind::Commit,
                "tree" => ObjectKind::Tree,
                "blob" => ObjectKind::Blob,
                "tag" => ObjectKind::Tag,
                _ => return None,
            };
            for slot in 0..4 {
                self.omitted_kinds[slot] |= slot != kind_slot(kind);
            }
        } else if let Some(name) = spec.strip_prefix("sparse:oid=") {
            if name.is_empty() {
                return None;
            }
            self.sparse_names.push(name.to_string());
        } else {
            let limit = spec.strip_prefix("blob:limit=")?;
            let (digits, unit) = match limit.as_bytes().last()? {
                b'k' | b'K' => (&limit[..limit.len() - 1], 1 << 10),
                b'm' | b'M' => (&limit[..limit.len() - 1], 1 << 20),
                b'g' | b'G' => (&limit[..limit.len() - 1], 1 << 30),
                _ => (limit, 1),
            };
            let limit = digits.parse::<u64>().ok()?.checked_mul(unit)?;
            self.blob_limit = Some(self.blob_limit.map_or(limit, |other| other.min(limit)));
        }
        Some(())
    }

    /// Whether everything is sent.
    pub fn is_none(&self) -> bool {
        *self == Filter::default()
    }

    /// The blobs `sparse:oid=` named, as the client wrote them.
    pub fn sparse_names(&self) -> &[String] {
        &self.sparse_names
    }

    /// Gives the filter the contents of the blobs in [`Filter::sparse_names`], in that order.
    pub fn load_sparse(&mut self, contents: &[impl AsRef<[u8]>]) {
        self.sparse = contents.iter().map(|text| Arc::new(Patterns::parse(text.as_ref()))).collect();
    }

    /// Whether objects of this kind are sent at all.
    fn keeps_kind(&self, kind: ObjectKind) -> bool {
        !self.omitted_kinds[kind_slot(kind)]
    }

    /// Whether objects `depth` levels below a root tree are kept.
    fn keeps_depth(&self, depth: u32) -> bool {
        self.tree_depth.is_none_or(|max| depth < max)
    }

    /// Whether the blob `oid`, found at `path`, is kept.
    fn keeps_blob(&self, src: &dyn ObjectSource, oid: &Oid, path: &[u8]) -> Result<bool> {
        if self.blob_none || !self.keeps_kind(ObjectKind::Blob) || !self.sparse.iter().all(|patterns| patterns.includes(path)) {
            return Ok(false);
        }
        match self.blob_limit {
            None => Ok(true),
            Some(limit) => Ok(src.size(oid)?.ok_or(Error::Missing(*oid))? < limit),
        }
    }

    /// Whether where a blob is decides if it is sent.
    fn by_path(&self) -> bool {
        !self.sparse.is_empty()
    }

    /// Whether the filter says nothing that bitmaps cannot answer: they
    /// record what kind an object is, but not its size, depth or path.
    fn fits_bitmaps(&self) -> bool {
        self.blob_limit.is_none() && self.sparse_names.is_empty() && self.tree_depth.is_none_or(|depth| depth == 0)
    }

    /// Whether nothing of this kind is sent, wherever it is: what is taken
    /// out of a bitmap, which knows kinds and nothing else.
    fn omits_all(&self, kind: ObjectKind) -> bool {
        !self.keeps_kind(kind)
            || match kind {
                ObjectKind::Blob => self.blob_none || self.tree_depth == Some(0),
                ObjectKind::Tree => self.tree_depth == Some(0),
                ObjectKind::Commit | ObjectKind::Tag => false,
            }
    }
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The shallow part of a fetch request.
#[derive(Clone, Debug, Default)]
pub struct ShallowRequest {
    /// Commits the client already treats as having no parents.
    pub client_shallow: Vec<Oid>,
    /// `deepen <n>`: keep `n` commits along each line of history.
    pub depth: Option<u32>,
    /// `deepen-relative`: count `depth` from the client's current boundary instead of from the wants.
    pub relative: bool,
    /// `deepen-since <timestamp>`: keep commits at least this recent.
    pub since: Option<i64>,
    /// `deepen-not <ref>`: drop everything reachable from these.
    pub not: Vec<Oid>,
}

impl ShallowRequest {
    /// Whether the request changes how deep the client's history goes.
    pub fn deepens(&self) -> bool {
        self.depth.is_some() || self.since.is_some() || !self.not.is_empty()
    }
}

/// How a fetch must be cut short to honour a [`ShallowRequest`].
#[derive(Default)]
pub struct ShallowPlan {
    /// Commits the client must newly record as shallow.
    pub shallow: Vec<Oid>,
    /// Commits the client must stop treating as shallow; their parents are being sent.
    pub unshallow: Vec<Oid>,
    /// Whether the response needs a shallow-info section at all.
    pub active: bool,
    /// Commits whose parents are not sent.
    boundary: HashSet<Oid>,
    /// Commits beyond which the client has nothing, whatever it claims to have.
    client_shallow: HashSet<Oid>,
    /// Parents of unshallowed commits: extra starting points for the walk.
    extra_tips: Vec<Oid>,
}

/// Commits within `limit` steps of `seeds` (a seed is at step 1), and those
/// among them where history is cut.
fn depth_limited(src: &dyn ObjectSource, seeds: Vec<Oid>, limit: u32) -> Result<(HashSet<Oid>, HashSet<Oid>)> {
    let mut included = HashSet::new();
    let mut boundary = HashSet::new();
    let mut queue: VecDeque<(Oid, u32)> = seeds.into_iter().map(|oid| (oid, 1)).collect();
    // Breadth-first, so each commit is first seen at its minimum depth.
    while let Some((oid, depth)) = queue.pop_front() {
        if included.contains(&oid) {
            continue;
        }
        let Some(commit) = src.commit(&oid)? else { continue };
        included.insert(oid);
        if depth < limit {
            queue.extend(commit.parents.into_iter().map(|p| (p, depth + 1)));
        } else if !commit.parents.is_empty() {
            boundary.insert(oid);
        }
    }
    Ok((included, boundary))
}

/// Commits reachable from `tips` that are no older than `since` and not
/// reachable from `not`, and those among them with an excluded parent.
fn rev_limited(src: &dyn ObjectSource, tips: Vec<Oid>, since: Option<i64>, not: &[Oid]) -> Result<(HashSet<Oid>, HashSet<Oid>)> {
    let excluded = reachable_commits(src, not)?;
    let mut parents_of: HashMap<Oid, Vec<Oid>> = HashMap::new();
    let mut stack = tips;
    while let Some(oid) = stack.pop() {
        if excluded.contains(&oid) || parents_of.contains_key(&oid) {
            continue;
        }
        let Some(commit) = src.commit(&oid)? else { continue };
        if since.is_some_and(|since| commit.time < since) {
            continue;
        }
        stack.extend(commit.parents.iter().copied());
        parents_of.insert(oid, commit.parents);
    }
    let boundary = parents_of
        .iter()
        .filter(|(_, parents)| parents.iter().any(|p| !parents_of.contains_key(p)))
        .map(|(oid, _)| *oid)
        .collect();
    Ok((parents_of.into_keys().collect(), boundary))
}

/// Works out where history is cut for a shallow fetch of `wants`.
pub fn plan_shallow(src: &dyn ObjectSource, wants: &[Oid], request: &ShallowRequest) -> Result<ShallowPlan> {
    let client: HashSet<Oid> = request.client_shallow.iter().copied().collect();
    let mut plan = ShallowPlan {
        active: !client.is_empty() || request.deepens(),
        boundary: client.clone(),
        client_shallow: client.clone(),
        ..ShallowPlan::default()
    };
    if !request.deepens() {
        return Ok(plan);
    }

    let (included, boundary) = match request.depth {
        Some(_) if request.since.is_some() || !request.not.is_empty() => {
            return Err(Error::Request("deepen cannot be combined with deepen-since or deepen-not".into()));
        }
        Some(0) => return Err(Error::Request("invalid deepen depth".into())),
        Some(depth) if request.relative => {
            depth_limited(src, commit_tips(src, &request.client_shallow)?, depth.saturating_add(1))?
        }
        Some(depth) => depth_limited(src, commit_tips(src, wants)?, depth)?,
        None => {
            let selected = rev_limited(src, commit_tips(src, wants)?, request.since, &request.not)?;
            if selected.0.is_empty() {
                return Err(Error::Request("no commits selected for shallow request".into()));
            }
            selected
        }
    };

    for oid in &request.client_shallow {
        // Now inside the kept range: the client gets this commit's parents.
        if included.contains(oid) && !boundary.contains(oid) && plan.boundary.remove(oid) {
            plan.unshallow.push(*oid);
            if let Some(commit) = src.commit(oid)? {
                plan.extra_tips.extend(commit.parents);
            }
        }
    }
    plan.shallow = boundary.iter().copied().filter(|oid| !client.contains(oid)).collect();
    plan.shallow.sort();
    plan.boundary.extend(boundary);
    Ok(plan)
}

/// A tree to examine, together with the trees at the same path in commits
/// whose content is already accounted for.
struct TreeJob {
    tree: Oid,
    known: Vec<Oid>,
    /// Levels below the root tree.
    depth: u32,
    /// Where the tree is, with a trailing slash unless it is the root. Only
    /// kept track of for filters that go by path.
    path: Vec<u8>,
}

/// An object a commit brought, with, where one exists, the object it
/// replaced at that path (a good delta base).
type Found = (Oid, Option<Oid>);
/// What some of the jobs handed to [`introduced_by`] found, each with its number.
type Numbered = Vec<(usize, Vec<Found>)>;

/// Trees that have been gone through from top to bottom with nothing known
/// at their path. Wherever one of them turns up again with nothing known, it
/// has nothing more to give: this is what keeps a directory that moves from
/// being read in full each time.
struct Walked {
    whole: Mutex<OidSet>,
    /// Not with filters that go by depth or by path: what a tree gives then
    /// depends on where it is.
    usable: bool,
}

/// Collects what a commit's tree holds that the known trees do not: only
/// paths that differ are visited, so a commit that touches three files costs
/// three paths, however large the repository is.
///
/// Nothing is remembered from one commit to the next, apart from [`Walked`].
/// An object reached from several commits is found several times, and
/// commits can be looked at in any order, and at the same time. (An earlier
/// version remembered every tree it had seen. That made the order matter,
/// and lost objects when it was wrong.)
fn introduced(src: &dyn ObjectSource, job: TreeJob, filter: &Filter, walked: &Walked, out: &mut Vec<Found>) -> Result<()> {
    enum Step {
        Enter(TreeJob),
        /// Everything below this tree has been collected.
        Whole(Oid),
    }
    let by_path = filter.by_path();
    let mut steps = vec![Step::Enter(job)];
    while let Some(step) = steps.pop() {
        let TreeJob { tree: oid, known, depth, path } = match step {
            Step::Enter(job) => job,
            Step::Whole(oid) => {
                walked.whole.lock().expect("no panics while held").insert(oid);
                continue;
            }
        };
        if !filter.keeps_depth(depth) || known.contains(&oid) {
            continue;
        }
        if known.is_empty() && walked.usable {
            if walked.whole.lock().expect("no panics while held").contains(&oid) {
                continue;
            }
            // Taken once everything pushed after it has been.
            steps.push(Step::Whole(oid));
        }
        if filter.keeps_kind(ObjectKind::Tree) {
            out.push((oid, known.first().copied()));
        }
        let tree = src.get(&oid)?.ok_or(Error::Missing(oid))?;
        let mut known_trees = Vec::with_capacity(known.len());
        for known_oid in &known {
            if let Some(object) = src.get(known_oid)? {
                known_trees.push(object);
            }
        }
        // Trees keep their entries sorted, so the known trees are read side
        // by side with this one, each with a place of its own. Should a tree
        // not be sorted, an entry may be taken for new when it is not: more
        // is sent then, never less.
        let mut known_entries = Vec::with_capacity(known_trees.len());
        for known_tree in &known_trees {
            known_entries.push((parse_tree(&known_tree.data)?, 0));
        }
        for entry in parse_tree(&tree.data)? {
            if entry.is_gitlink() {
                continue;
            }
            // What the known trees have under this name, where it is something else.
            let mut same_kind = Vec::new();
            let mut unchanged = false;
            for (entries, at) in &mut known_entries {
                while entries.get(*at).is_some_and(|other| other.order(&entry).is_lt()) {
                    *at += 1;
                }
                if let Some(other) = entries.get(*at).filter(|other| other.order(&entry).is_eq()) {
                    if other.oid == entry.oid {
                        unchanged = true;
                        break;
                    }
                    same_kind.push(other.oid);
                }
            }
            if unchanged {
                continue;
            }
            let mut entry_path = Vec::new();
            if by_path {
                entry_path = [&path[..], entry.name].concat();
            }
            if entry.is_tree() {
                entry_path.push(b'/');
                steps.push(Step::Enter(TreeJob { tree: entry.oid, known: same_kind, depth: depth + 1, path: entry_path }));
            } else if filter.keeps_depth(depth + 1) && filter.keeps_blob(src, &entry.oid, &entry_path)? {
                out.push((entry.oid, same_kind.into_iter().next()));
            }
        }
    }
    Ok(())
}

/// Below this many trees to look into, threads cost more than they save.
const PARALLEL_FROM: usize = 64;
/// More threads than this mostly wait for one another's cache entries.
const MAX_WALK_THREADS: usize = 8;

/// Runs [`introduced`] for each job, on several threads when there are many,
/// and returns what each found, in the order of the jobs.
fn introduced_by(src: &(dyn ObjectSource + Sync), jobs: Vec<TreeJob>, filter: &Filter) -> Result<Vec<Vec<Found>>> {
    let walked = Walked { whole: Mutex::new(OidSet::default()), usable: !filter.by_path() && filter.tree_depth.is_none() };
    let count = jobs.len();
    let threads = match count {
        0..PARALLEL_FROM => 1,
        _ => std::thread::available_parallelism().map_or(1, usize::from).min(MAX_WALK_THREADS),
    };
    let queue = Mutex::new(jobs.into_iter().enumerate());
    let failed = AtomicBool::new(false);
    let work = || -> Result<Numbered> {
        let mut done = Vec::new();
        loop {
            let next = queue.lock().expect("no panics while held").next();
            let Some((number, job)) = next.filter(|_| !failed.load(Ordering::Relaxed)) else { return Ok(done) };
            let mut found = Vec::new();
            if let Err(e) = introduced(src, job, filter, &walked, &mut found) {
                failed.store(true, Ordering::Relaxed);
                return Err(e);
            }
            done.push((number, found));
        }
    };
    let parts: Vec<Result<Numbered>> = match threads {
        1 => vec![work()],
        _ => std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads).map(|_| scope.spawn(work)).collect();
            workers.into_iter().map(|worker| worker.join().expect("a walk does not panic")).collect()
        }),
    };
    let mut ordered: Vec<Vec<Found>> = (0..count).map(|_| Vec::new()).collect();
    for part in parts {
        for (number, found) in part? {
            ordered[number] = found;
        }
    }
    Ok(ordered)
}

const QUEUED: u8 = 1;
const POPPED: u8 = 2;
const UNINTERESTING: u8 = 4;

/// Bits per pack, keyed by the pack's position in [`PackSet::packs`].
type PackBits = HashMap<usize, BitSet>;

fn merge_bits(into: &mut PackBits, from: PackBits) {
    for (pack, bits) in from {
        match into.get_mut(&pack) {
            Some(have) => have.union_with(&bits),
            None => {
                into.insert(pack, bits);
            }
        }
    }
}

/// Walks commits newest first, separating what the client needs from what it
/// already has, the way `git rev-list wants --not haves` does. The walk ends
/// as soon as every commit still queued is known to the client, so its cost
/// follows the size of the difference, not the size of history.
///
/// A commit with reachability bitmaps ends the walk along its line of
/// history: the bitmaps already say what lies beyond it.
struct CommitWalk<'a> {
    src: &'a dyn ObjectSource,
    /// Where to look for bitmaps, if they may be used.
    bitmaps: Option<&'a PackSet>,
    info: OidMap<CommitInfo>,
    flags: OidMap<u8>,
    queue: BinaryHeap<(i64, Oid)>,
    /// Queued commits not (yet) known to be uninteresting.
    pending: usize,
    /// Union of the bitmaps of needed commits.
    want_bits: PackBits,
    /// Union of the bitmaps of commits the client has.
    have_bits: PackBits,
    /// Commits the client has that no bitmap stands for.
    passed: Vec<Oid>,
}

impl CommitWalk<'_> {
    fn load(&mut self, oid: Oid) -> Result<bool> {
        if self.info.contains_key(&oid) {
            return Ok(true);
        }
        let Some(commit) = self.src.commit(&oid)? else { return Ok(false) };
        self.info.insert(oid, commit);
        Ok(true)
    }

    fn enqueue(&mut self, oid: Oid, uninteresting: bool) -> Result<()> {
        if !self.load(oid)? {
            return Ok(());
        }
        let flags = self.flags.entry(oid).or_default();
        if uninteresting && *flags & UNINTERESTING == 0 {
            if *flags & QUEUED != 0 && *flags & POPPED == 0 {
                self.pending -= 1;
            }
            *flags |= UNINTERESTING;
            if *flags & POPPED != 0 {
                // Already handed out as needed (commit dates out of order):
                // visit it again so its ancestors are marked as well.
                *flags &= !(QUEUED | POPPED);
            }
        }
        if *flags & QUEUED == 0 {
            if *flags & UNINTERESTING == 0 {
                self.pending += 1;
            }
            *flags |= QUEUED;
            self.queue.push((self.info[&oid].time, oid));
        }
        Ok(())
    }

    fn bitmap(&self, oid: &Oid) -> Result<Option<PackBits>> {
        match self.bitmaps {
            Some(packs) if packs.has_bitmap(oid) => packs.closure(oid),
            _ => Ok(None),
        }
    }

    fn pop(&mut self) -> Option<(Oid, bool)> {
        let (_, oid) = self.queue.pop()?;
        let flags = self.flags.get_mut(&oid).expect("queued commits have flags");
        *flags |= POPPED;
        Some((oid, *flags & UNINTERESTING != 0))
    }

    /// Whether a bitmap of a commit the client has includes this object.
    fn client_has(&self, oid: &Oid) -> bool {
        let Some(packs) = self.bitmaps else { return false };
        self.have_bits.iter().any(|(number, bits)| packs.packs()[*number].index().position(oid).is_some_and(|at| bits.get(at)))
    }

    /// Handles a commit the client has. Returns false if bitmaps covered it.
    fn visit_have(&mut self, oid: Oid, shallow: &ShallowPlan) -> Result<bool> {
        if let Some(bits) = self.bitmap(&oid)? {
            merge_bits(&mut self.have_bits, bits);
            return Ok(false);
        }
        self.passed.push(oid);
        // The client has nothing beyond its own shallow boundary.
        if !shallow.client_shallow.contains(&oid) {
            for parent in self.info[&oid].parents.clone() {
                self.enqueue(parent, true)?;
            }
        }
        Ok(true)
    }

    /// Returns the commits to send that are not covered by `want_bits`.
    fn run(&mut self, shallow: &ShallowPlan) -> Result<Vec<Oid>> {
        let mut needed = Vec::new();
        while self.pending > 0 {
            let Some((oid, uninteresting)) = self.pop() else { break };
            if uninteresting {
                self.visit_have(oid, shallow)?;
                continue;
            }
            self.pending -= 1;
            if self.client_has(&oid) {
                // Behind a bitmap of the client's: it has this commit and all
                // beyond it, though no path from a commit of its own led here.
                *self.flags.get_mut(&oid).expect("queued commits have flags") |= UNINTERESTING;
                continue;
            }
            if let Some(bits) = self.bitmap(&oid)? {
                merge_bits(&mut self.want_bits, bits);
                continue;
            }
            needed.push(oid);
            if !shallow.boundary.contains(&oid) {
                for parent in self.info[&oid].parents.clone() {
                    self.enqueue(parent, false)?;
                }
            }
        }
        needed.retain(|oid| self.flags[oid] & UNINTERESTING == 0);
        Ok(needed)
    }

    /// Follows the client's history down to bitmapped commits, so that
    /// everything it has can be subtracted from `want_bits`: the commits
    /// passed on the way are added to `passed`.
    ///
    /// Only needed when bitmaps are about to be sent: otherwise the walk has
    /// already stopped at exactly what the client lacks.
    fn finish_haves(&mut self, shallow: &ShallowPlan) -> Result<()> {
        while let Some((oid, uninteresting)) = self.pop() {
            if uninteresting {
                self.visit_have(oid, shallow)?;
            }
        }
        Ok(())
    }
}

/// Objects taken wholesale from one pack, as selected by reachability bitmaps.
pub struct BitmapSelection {
    pub pack: Arc<PackFile>,
    /// One bit per object of `pack`, in index order.
    pub bits: BitSet,
}

impl BitmapSelection {
    pub fn contains(&self, oid: &Oid) -> bool {
        self.pack.index().position(oid).is_some_and(|i| self.bits.get(i))
    }
}

/// What to send in response to a fetch.
pub struct FetchPlan {
    /// Objects found by walking, each with a suggested delta base if one is known.
    pub objects: Vec<(Oid, Option<Oid>)>,
    /// Objects found through bitmaps. No object appears twice, here or in `objects`.
    pub bitmaps: Vec<BitmapSelection>,
}

impl FetchPlan {
    pub fn count(&self) -> usize {
        self.objects.len() + self.bitmaps.iter().map(|b| b.bits.count()).sum::<usize>()
    }

    /// Whether `oid` is selected through a bitmap.
    pub fn in_bitmaps(&self, oid: &Oid) -> bool {
        self.bitmaps.iter().any(|selection| selection.contains(oid))
    }
}

/// How many of the client's commits to compare against where a shallow
/// boundary hides the real parents.
const MAX_HAVE_TREES: usize = 4;

/// Works out the objects a client needs to obtain `wants`, given that it
/// already has everything reachable from `haves`.
///
/// Objects named directly in `wants` are always included, even if `filter`
/// would omit them: that is how partial clones fetch what they left out.
///
/// The result may include objects the client already has (for example a file
/// restored to an old version); it never omits one it lacks.
pub fn plan_fetch(packs: &PackSet, wants: &[Oid], haves: &[Oid], shallow: &ShallowPlan, filter: &Filter) -> Result<FetchPlan> {
    // Bitmaps describe complete history, and record kinds but neither sizes nor depths.
    let use_bitmaps = !shallow.active && filter.fits_bitmaps();
    let mut walk = CommitWalk {
        src: packs,
        bitmaps: use_bitmaps.then_some(packs),
        info: OidMap::default(),
        flags: OidMap::default(),
        queue: BinaryHeap::new(),
        pending: 0,
        want_bits: HashMap::new(),
        have_bits: HashMap::new(),
        passed: Vec::new(),
    };
    let have_tips = commit_tips(packs, haves)?;
    for oid in &have_tips {
        walk.enqueue(*oid, true)?;
    }

    let mut objects: Vec<(Oid, Option<Oid>)> = Vec::new();
    let mut sent = OidSet::default();
    let mut jobs = Vec::new();
    let mut wanted_blobs = Vec::new();
    // Everything the request names, down through tags to what they point at.
    let mut named = Vec::new();
    for oid in wants.iter().chain(&shallow.extra_tips) {
        let mut current = *oid;
        loop {
            named.push(current);
            let object = packs.get(&current)?.ok_or(Error::Missing(current))?;
            match object.kind {
                ObjectKind::Commit => walk.enqueue(current, false)?,
                ObjectKind::Blob => wanted_blobs.push(current),
                ObjectKind::Tree => {
                    // A tree asked for by name is sent whatever the filter
                    // says; the filter then applies to what is below it.
                    if sent.insert(current) {
                        objects.push((current, None));
                    }
                    for entry in parse_tree(&object.data)? {
                        // Where a tree asked for by name sits is not known: it is taken for the root.
                        if entry.is_tree() {
                            jobs.push(TreeJob { tree: entry.oid, known: Vec::new(), depth: 0, path: [entry.name, b"/"].concat() });
                        } else if !entry.is_gitlink() && filter.keeps_depth(0) && filter.keeps_blob(packs, &entry.oid, entry.name)? {
                            wanted_blobs.push(entry.oid);
                        }
                    }
                }
                ObjectKind::Tag => {
                    if sent.insert(current) {
                        objects.push((current, None));
                    }
                    current = parse_tag(&object.data)?;
                    continue;
                }
            }
            break;
        }
    }

    let commits = walk.run(shallow)?;
    sent.extend(commits.iter().copied());
    objects.extend(commits.iter().filter(|oid| filter.keeps_kind(ObjectKind::Commit) || named.contains(*oid)).map(|oid| (*oid, None)));

    let have_trees: Vec<Oid> = have_tips.iter().take(MAX_HAVE_TREES).map(|oid| walk.info[oid].tree).collect();
    // Oldest first, as the objects will stand in the pack.
    for oid in commits.iter().rev() {
        let info = &walk.info[oid];
        let known = if shallow.boundary.contains(oid) {
            // The real parents are cut off, so the best guess at what the
            // client has here is the commits it told us about.
            have_trees.clone()
        } else {
            // Every parent is either being sent, already with the client,
            // or covered by a bitmap: its tree is accounted for.
            info.parents.iter().filter_map(|p| walk.info.get(p)).map(|p| p.tree).collect()
        };
        jobs.push(TreeJob { tree: info.tree, known, depth: 0, path: Vec::new() });
    }
    for found in introduced_by(packs, jobs, filter)? {
        objects.extend(found.into_iter().filter(|(oid, _)| sent.insert(*oid)));
    }
    objects.extend(wanted_blobs.into_iter().filter(|oid| sent.insert(*oid)).map(|oid| (oid, None)));

    let mut bitmaps = Vec::new();
    // What the client has without a bitmap saying so: the commits passed on
    // the way to its bitmaps, and what each of them brought.
    let mut passed = OidSet::default();
    if !walk.want_bits.is_empty() {
        walk.finish_haves(shallow)?;
        let brought: Vec<TreeJob> = walk
            .passed
            .iter()
            .map(|oid| {
                let info = &walk.info[oid];
                // A parent that was not loaded lies beyond the client's
                // shallow boundary: the whole tree counts as brought then.
                let known = info.parents.iter().filter_map(|p| walk.info.get(p)).map(|p| p.tree).collect();
                TreeJob { tree: info.tree, known, depth: 0, path: Vec::new() }
            })
            .collect();
        passed.extend(walk.passed.iter().copied());
        passed.extend(introduced_by(packs, brought, &Filter::default())?.into_iter().flatten().map(|(oid, _)| oid));
        for (number, mut bits) in std::mem::take(&mut walk.want_bits) {
            let pack = &packs.packs()[number];
            if let Some(have) = walk.have_bits.get(&number) {
                bits.subtract(have);
            }
            for oid in &passed {
                if let Some(position) = pack.index().position(oid) {
                    bits.clear(position);
                }
            }
            let kinds = pack.bitmaps().expect("bits come from this pack's bitmaps");
            for kind in [ObjectKind::Commit, ObjectKind::Tree, ObjectKind::Blob, ObjectKind::Tag] {
                if filter.omits_all(kind) {
                    bits.subtract(&kinds.of_kind(kind)?);
                }
            }
            bitmaps.push(BitmapSelection { pack: pack.clone(), bits });
        }
        // An object can sit in two packs and be selected in both: keep the
        // copy in the larger selection, so the lookups are done for the smaller.
        bitmaps.sort_by_key(|selection| std::cmp::Reverse(selection.bits.count()));
        for later in 1..bitmaps.len() {
            let (earlier, rest) = bitmaps.split_at_mut(later);
            let selection = &mut rest[0];
            for position in 0..selection.pack.index().len() {
                if selection.bits.get(position) {
                    let oid = selection.pack.index().oid_at(position);
                    if earlier.iter().any(|other| other.contains(&oid)) {
                        selection.bits.clear(position);
                    }
                }
            }
        }
    }
    let plan = FetchPlan { objects: Vec::new(), bitmaps };
    objects.retain(|(oid, _)| !plan.in_bitmaps(oid));
    if !walk.have_bits.is_empty() || !passed.is_empty() {
        // The walk compares each commit with its parents only, so it also
        // finds what the client has by another way: history it reached on
        // its way to an older bitmap, or a change made twice. What is asked
        // for by name is sent all the same.
        let named: OidSet = named.iter().copied().collect();
        objects.retain(|(oid, _)| !(walk.client_has(oid) || passed.contains(oid)) || named.contains(oid));
    }
    if filter.omitted_kinds.contains(&true) {
        // What was asked for by name comes whatever its kind, also where a
        // bitmap would have brought it and no longer does.
        let listed: OidSet = objects.iter().map(|(oid, _)| *oid).collect();
        objects.extend(named.iter().filter(|oid| !listed.contains(*oid) && !plan.in_bitmaps(oid)).map(|oid| (*oid, None)));
    }
    Ok(FetchPlan { objects, ..plan })
}

/// Decides whether the bitmaps of some base commits cover some objects.
pub type CoverageCheck<'a> = dyn Fn(&[Oid], &[Oid]) -> Result<bool> + 'a;

/// What [`reachability_bitmaps`] needs to know about the pack it describes
/// and about the rest of the pool.
pub struct BitmapContext<'a> {
    /// Number of objects in the pack.
    pub objects: usize,
    /// Position of an object in the pack's index, if it is in the pack.
    pub position: &'a dyn Fn(&Oid) -> Option<usize>,
    /// Whether a commit has a bitmap entry in another pack.
    pub has_base: &'a dyn Fn(&Oid) -> bool,
    /// Whether the bitmaps of the given base commits cover all the given objects.
    pub covered: &'a CoverageCheck<'a>,
}

/// Computes, for each of `tips`, which objects of the pack are reachable from
/// it, and which commits with bitmaps elsewhere account for the rest.
///
/// A tip is left out when the pack and the available base bitmaps do not
/// account for its whole history, or when it is fully covered elsewhere.
pub fn reachability_bitmaps(src: &(dyn ObjectSource + Sync), tips: &[Oid], context: &BitmapContext<'_>) -> Result<Vec<Reachable>> {
    // Oldest first, so that each bitmap builds on those of its ancestors.
    let tips = dated_tips(src, tips)?;

    let mut done: HashMap<Oid, (BitSet, Vec<Oid>)> = HashMap::new();
    let mut out = Vec::new();
    for tip in tips {
        let mut bits = BitSet::new(context.objects);
        let mut bases = Vec::new();
        // Objects reached that are not in the pack: the bases must cover them.
        let mut outside = Vec::new();
        // History not behind an existing bitmap, with the bitmaps it leads to folded in.
        let mut fresh: Vec<(Oid, usize, CommitInfo)> = Vec::new();
        let mut visited = HashSet::new();
        let mut stack = vec![tip];
        while let Some(oid) = stack.pop() {
            if !visited.insert(oid) {
                continue;
            }
            if let Some((known, known_bases)) = done.get(&oid) {
                bits.union_with(known);
                bases.extend(known_bases);
            } else if (context.has_base)(&oid) {
                bases.push(oid);
            } else if let Some(index) = (context.position)(&oid) {
                let Some(commit) = src.commit(&oid)? else { continue };
                stack.extend(commit.parents.iter().copied());
                fresh.push((oid, index, commit));
            } else {
                outside.push(oid);
            }
        }
        if bases.contains(&tip) {
            continue;
        }

        let trees: HashMap<Oid, Oid> = fresh.iter().map(|(oid, _, commit)| (*oid, commit.tree)).collect();
        let mut jobs = Vec::new();
        for (_, index, commit) in &fresh {
            if bits.get(*index) {
                // Reachable from a commit that already has a bitmap.
                continue;
            }
            bits.set(*index);
            let mut known = Vec::with_capacity(commit.parents.len());
            for parent in &commit.parents {
                match trees.get(parent) {
                    Some(tree) => known.push(*tree),
                    None => known.extend(src.commit(parent)?.map(|p| p.tree)),
                }
            }
            jobs.push(TreeJob { tree: commit.tree, known, depth: 0, path: Vec::new() });
        }

        for (oid, _) in introduced_by(src, jobs, &Filter::default())?.into_iter().flatten() {
            match (context.position)(&oid) {
                Some(index) => bits.set(index),
                // Not ours to describe; if a base covers it, it covers everything below it too.
                None => outside.push(oid),
            }
        }

        bases.sort();
        bases.dedup();
        if !outside.is_empty() && !(context.covered)(&bases, &outside)? {
            continue;
        }
        done.insert(tip, (bits.clone(), bases.clone()));
        out.push(Reachable { commit: tip, bits, bases });
    }
    Ok(out)
}

/// The commits `tips` denote, oldest first, without duplicates.
fn dated_tips(src: &dyn ObjectSource, tips: &[Oid]) -> Result<Vec<Oid>> {
    let mut dated = Vec::new();
    for oid in commit_tips(src, tips)? {
        if let Some(commit) = src.commit(&oid)? {
            dated.push((commit.time, oid));
        }
    }
    dated.sort();
    dated.dedup();
    Ok(dated.into_iter().map(|(_, oid)| oid).collect())
}

/// Verifies that everything reachable from `tips` is present in `all`.
///
/// Objects in `existing` are assumed to already have complete history, so the
/// walk stops there.
pub fn check_connected(all: &dyn ObjectSource, existing: &dyn ObjectSource, tips: &[Oid]) -> Result<()> {
    let mut seen = HashSet::new();
    let mut stack = tips.to_vec();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) || existing.contains(&oid) {
            continue;
        }
        let object = all.get(&oid)?.ok_or(Error::Missing(oid))?;
        match object.kind {
            ObjectKind::Commit => {
                let commit = crate::object::commit_info(&object.data)?;
                stack.push(commit.tree);
                stack.extend(commit.parents);
            }
            ObjectKind::Tag => stack.push(parse_tag(&object.data)?),
            ObjectKind::Tree => {
                for entry in parse_tree(&object.data)? {
                    if entry.is_gitlink() {
                        continue;
                    }
                    if entry.is_tree() {
                        stack.push(entry.oid);
                    } else if seen.insert(entry.oid) && !all.contains(&entry.oid) {
                        return Err(Error::Missing(entry.oid));
                    }
                }
            }
            ObjectKind::Blob => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Filter;

    #[test]
    fn filter_specs() {
        let parsed = |spec: &str| Filter::parse(spec).unwrap_or_else(|| panic!("{spec} should parse"));
        assert!(parsed("blob:none").blob_none);
        assert_eq!(parsed("tree:0").tree_depth, Some(0));
        assert_eq!(parsed("tree:3").tree_depth, Some(3));
        assert_eq!(parsed("blob:limit=1024").blob_limit, Some(1024));
        assert_eq!(parsed("blob:limit=2k").blob_limit, Some(2048));
        assert_eq!(parsed("blob:limit=1m").blob_limit, Some(1 << 20));
        assert_eq!(parsed("sparse:oid=main:.sparse").sparse_names(), ["main:.sparse"]);
        // Parts are URL-encoded, so one can be a combination itself; the strictest of each kind holds.
        let combined = parsed("combine:blob:limit=1k+tree:5+combine:tree:2%2Bblob:limit=4k+sparse:oid=abc%2Bdef");
        assert_eq!((combined.blob_limit, combined.tree_depth), (Some(1024), Some(2)));
        assert_eq!(combined.sparse_names(), ["abc+def"]);
        assert!(!combined.fits_bitmaps());
        assert!(parsed("combine:blob:none+tree:0").fits_bitmaps());
        assert!(parsed("object:type=blob").keeps_kind(crate::ObjectKind::Blob));
        assert!(!parsed("object:type=blob").keeps_kind(crate::ObjectKind::Tree));
        // Two kinds at once leave nothing.
        assert!(!parsed("combine:object:type=blob+object:type=tree").keeps_kind(crate::ObjectKind::Blob));
        for unsupported in ["tree:x", "sparse:oid=", "sparse:path=x", "blob:limit=", "blob:limit=x", "object:type=file", "combine:blob:none+nonsense", "combine:%zz"] {
            assert_eq!(Filter::parse(unsupported), None, "{unsupported}");
        }
    }
}
