//! Object graph traversal.
//!
//! Commits are read through [`ObjectSource::commit`], which is answered from a
//! commit-graph when the pack has one.

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::bitmap::Reachable;
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

    /// Whether objects `depth` levels below a root tree are kept.
    fn keeps_depth(&self, depth: u32) -> bool {
        self.tree_depth.is_none_or(|max| depth < max)
    }

    /// Whether the blob `oid`, found at `path`, is kept.
    fn keeps_blob(&self, src: &dyn ObjectSource, oid: &Oid, path: &[u8]) -> Result<bool> {
        if self.blob_none || !self.sparse.iter().all(|patterns| patterns.includes(path)) {
            return Ok(false);
        }
        match self.blob_limit {
            None => Ok(true),
            Some(limit) => Ok(src.size(oid)?.ok_or(Error::Missing(*oid))? < limit),
        }
    }

    /// Whether where a blob is decides if it is sent. Then a tree seen at one
    /// path has to be looked into again at another.
    fn by_path(&self) -> bool {
        !self.sparse.is_empty()
    }

    /// Whether the filter says nothing that bitmaps cannot answer: they
    /// record what kind an object is, but not its size, depth or path.
    fn fits_bitmaps(&self) -> bool {
        self.blob_limit.is_none() && self.sparse_names.is_empty() && self.tree_depth.is_none_or(|depth| depth == 0)
    }

    fn omits_all_blobs(&self) -> bool {
        self.blob_none || self.tree_depth == Some(0)
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

/// Descends into the trees of `jobs`, visiting only what differs from the
/// known trees at the same path. A commit that touches three files costs three
/// paths, however large the repository is.
///
/// `mark` records an object as handled and reports whether it was new;
/// `emit` receives each new object with, where one exists, the object it
/// replaced at that path (a good delta base).
///
/// Jobs are taken from the end of `jobs`, and a known tree is trusted to be
/// accounted for in full. That holds for trees of commits outside the walk,
/// and for those of commits inside it only once their own jobs are done: so
/// the jobs of a commit must come after (be taken before) those of its
/// children. See [`parents_last`]. Otherwise a tree met first where most of
/// it is known is marked as handled with entries that nobody has looked at.
fn diff_trees(
    src: &dyn ObjectSource,
    mut jobs: Vec<TreeJob>,
    filter: &Filter,
    mark: &mut dyn FnMut(&Oid) -> Result<bool>,
    emit: &mut dyn FnMut(Oid, Option<Oid>),
) -> Result<()> {
    let by_path = filter.by_path();
    // With a filter that goes by path, the same tree can hold wanted files
    // in one place and none in another: each place is looked into once.
    let mut places: HashSet<(Oid, Vec<u8>)> = HashSet::new();
    while let Some(TreeJob { tree: oid, known, depth, path }) = jobs.pop() {
        if !filter.keeps_depth(depth) || known.contains(&oid) {
            continue;
        }
        let new = mark(&oid)?;
        if !new && !(by_path && places.insert((oid, path.clone()))) {
            continue;
        }
        if new {
            if by_path {
                places.insert((oid, path.clone()));
            }
            emit(oid, known.first().copied());
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
                jobs.push(TreeJob { tree: entry.oid, known: same_kind, depth: depth + 1, path: entry_path });
            } else if filter.keeps_depth(depth + 1) && filter.keeps_blob(src, &entry.oid, &entry_path)? && mark(&entry.oid)? {
                emit(entry.oid, same_kind.into_iter().next());
            }
        }
    }
    Ok(())
}

/// Orders commits so that each comes before its parents among them: the
/// order in which to hand their trees to [`diff_trees`], which works from
/// the end. Dates do not give this order; they can be anything.
fn parents_last<'a>(commits: &[Oid], parents: impl Fn(&Oid) -> &'a [Oid]) -> Vec<Oid> {
    let among: HashSet<Oid> = commits.iter().copied().collect();
    let mut placed = HashSet::with_capacity(commits.len());
    let mut order = Vec::with_capacity(commits.len());
    for start in commits {
        // Depth first, a commit placed once all its parents are.
        let mut stack = vec![(*start, 0)];
        while let Some((oid, next)) = stack.pop() {
            if next == 0 && placed.contains(&oid) {
                continue;
            }
            match parents(&oid).get(next) {
                Some(parent) => {
                    stack.push((oid, next + 1));
                    if among.contains(parent) && !placed.contains(parent) {
                        stack.push((*parent, 0));
                    }
                }
                None => {
                    if placed.insert(oid) {
                        order.push(oid);
                    }
                }
            }
        }
    }
    order.reverse();
    order
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

    /// Handles a commit the client has. Returns false if bitmaps covered it.
    fn visit_have(&mut self, oid: Oid, shallow: &ShallowPlan) -> Result<bool> {
        if let Some(bits) = self.bitmap(&oid)? {
            merge_bits(&mut self.have_bits, bits);
            return Ok(false);
        }
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
    /// everything it has can be subtracted from `want_bits`. Returns the
    /// commits passed on the way.
    ///
    /// Only needed when bitmaps are about to be sent: otherwise the walk has
    /// already stopped at exactly what the client lacks.
    fn finish_haves(&mut self, shallow: &ShallowPlan) -> Result<Vec<Oid>> {
        let mut passed = Vec::new();
        while let Some((oid, uninteresting)) = self.pop() {
            if uninteresting && self.visit_have(oid, shallow)? {
                passed.push(oid);
            }
        }
        Ok(passed)
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
    };
    let have_tips = commit_tips(packs, haves)?;
    for oid in &have_tips {
        walk.enqueue(*oid, true)?;
    }

    let mut objects: Vec<(Oid, Option<Oid>)> = Vec::new();
    let mut sent = OidSet::default();
    let mut jobs = Vec::new();
    let mut wanted_blobs = Vec::new();
    for oid in wants.iter().chain(&shallow.extra_tips) {
        let mut current = *oid;
        loop {
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
    objects.extend(commits.iter().map(|oid| (*oid, None)));

    let have_trees: Vec<Oid> = have_tips.iter().take(MAX_HAVE_TREES).map(|oid| walk.info[oid].tree).collect();
    for oid in &parents_last(&commits, |oid| &walk.info[oid].parents) {
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
    let mut found = Vec::new();
    diff_trees(packs, jobs, filter, &mut |oid| Ok(sent.insert(*oid)), &mut |oid, base| found.push((oid, base)))?;
    objects.extend(found);
    objects.extend(wanted_blobs.into_iter().filter(|oid| sent.insert(*oid)).map(|oid| (oid, None)));

    let mut bitmaps = Vec::new();
    if !walk.want_bits.is_empty() {
        let passed = walk.finish_haves(shallow)?;
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
            if filter.omits_all_blobs() {
                bits.subtract(&kinds.of_kind(ObjectKind::Blob)?);
            }
            if filter.tree_depth == Some(0) {
                bits.subtract(&kinds.of_kind(ObjectKind::Tree)?);
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
pub fn reachability_bitmaps(src: &dyn ObjectSource, tips: &[Oid], context: &BitmapContext<'_>) -> Result<Vec<Reachable>> {
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
        let by_oid: HashMap<Oid, &(Oid, usize, CommitInfo)> = fresh.iter().map(|entry| (entry.0, entry)).collect();
        let order = parents_last(&fresh.iter().map(|(oid, _, _)| *oid).collect::<Vec<_>>(), |oid| &by_oid[oid].2.parents);
        let mut jobs = Vec::new();
        for (_, index, commit) in order.iter().map(|oid| by_oid[oid]) {
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

        let mut mark = |oid: &Oid| -> Result<bool> {
            let Some(index) = (context.position)(oid) else {
                // Not ours to describe; if a base covers it, it covers everything below it too.
                outside.push(*oid);
                return Ok(false);
            };
            let new = !bits.get(index);
            bits.set(index);
            Ok(new)
        };
        diff_trees(src, jobs, &Filter::default(), &mut mark, &mut |_, _| {})?;

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
        for unsupported in ["tree:x", "sparse:oid=", "sparse:path=x", "blob:limit=", "blob:limit=x", "object:type=blob", "combine:blob:none+nonsense", "combine:%zz"] {
            assert_eq!(Filter::parse(unsupported), None, "{unsupported}");
        }
    }
}
