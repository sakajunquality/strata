//! Compaction: rewriting several packs as one.
//!
//! Every object of the source packs is carried over exactly once, by copying
//! its stored bytes, unless the caller asks for unreachable objects to be
//! left behind.

use std::collections::HashMap;
use std::io::Write;

use crate::bitmap::{kind_slot, PackBitmaps};
use crate::bitset::BitSet;
use crate::graph::CommitGraph;
use crate::idx::PackIndex;
use crate::object::CommitInfo;
use crate::pack::PackWriter;
use crate::{corrupt, walk, Error, Object, ObjectKind, ObjectSource, Oid, PackSet, Result};

/// Which commits should get reachability bitmaps.
pub struct BitmapRequest {
    /// Typically every ref of the repository.
    pub tips: Vec<Oid>,
    /// The tip of the default branch. Bitmaps are also placed at intervals
    /// along its history, so that clients which are some way behind can be
    /// served from bitmaps too.
    pub mainline: Option<Oid>,
}

/// Bitmaps are placed on at most this many of the newest tips.
const MAX_TIP_BITMAPS: usize = 64;
/// Distance of the first mainline bitmap from the tip; each next one is four times further.
const MAINLINE_FIRST_STEP: usize = 256;
const MAX_MAINLINE_BITMAPS: usize = 12;

pub struct Compacted {
    pub index: PackIndex,
    pub graph: CommitGraph,
    pub bitmaps: Option<PackBitmaps>,
    /// Size of the pack written, in bytes.
    pub size: u64,
}

/// What a compaction should do beyond copying every object.
#[derive(Default)]
pub struct CompactOptions<'a> {
    /// Carry over only the objects this accepts. It must accept a set closed
    /// under reachability, or the result will have holes.
    pub keep: Option<&'a dyn Fn(&Oid) -> bool>,
    /// For objects stored whole, an object likely to resemble each (the
    /// previous version of the same file) to store it as a delta against.
    pub delta_hints: Option<&'a HashMap<Oid, Oid>>,
    pub bitmaps: Option<&'a BitmapRequest>,
    /// The packs of the pool that are not being merged. Bitmaps of the new
    /// pack build on theirs.
    pub rest: Option<&'a PackSet>,
}

/// Longest chain of deltas a newly computed delta may sit on top of.
const MAX_DELTA_CHAIN: usize = 50;

/// Answers commit lookups from a commit-graph before falling back to objects.
struct WithGraph<'a> {
    objects: &'a PackSet,
    graph: &'a CommitGraph,
}

impl ObjectSource for WithGraph<'_> {
    fn get(&self, oid: &Oid) -> Result<Option<Object>> {
        self.objects.get(oid)
    }

    fn contains(&self, oid: &Oid) -> bool {
        self.objects.contains(oid)
    }

    fn commit(&self, oid: &Oid) -> Result<Option<CommitInfo>> {
        match self.graph.get(oid) {
            Some(info) => Ok(Some(info)),
            None => self.objects.commit(oid),
        }
    }
}

/// Writes the contents of `sources` to `out` as a single pack, together with
/// a commit-graph and, where history allows, reachability bitmaps for it.
pub fn compact<W: Write>(sources: &PackSet, out: W, options: &CompactOptions<'_>) -> Result<(W, Compacted)> {
    let packs = sources.packs();
    let in_pack = |oid: &Oid| sources.contains(oid) && options.keep.is_none_or(|keep| keep(oid));

    // Keep the stored order within each pack: it reflects what was pushed
    // together and tends to be read together. Where an object exists more than
    // once, the copy a lookup would find wins.
    let mut plan: Vec<(usize, u64, u32)> = Vec::new();
    for (pack_number, pack) in packs.iter().enumerate() {
        for &(offset, position) in pack.entries_in_pack_order() {
            let oid = pack.index().oid_at(position as usize);
            let in_newer = packs[..pack_number].iter().any(|newer| newer.index().find(&oid).is_some());
            if !in_newer && pack.index().find(&oid) == Some(offset) && in_pack(&oid) {
                plan.push((pack_number, offset, position));
            }
        }
    }
    let count = u32::try_from(plan.len()).map_err(|_| corrupt("too many objects for one pack"))?;

    let mut writer = PackWriter::recording(out, count)?;
    let mut commits = Vec::new();
    let mut kinds: Vec<(Oid, ObjectKind)> = Vec::with_capacity(plan.len());
    // Deltas computed here, by object. Together with the stored deltas that
    // get reused, these decide what each object will depend on.
    let mut new_bases: HashMap<Oid, Oid> = HashMap::new();
    for (pack_number, offset, position) in plan {
        let pack = &packs[pack_number];
        let oid = pack.index().oid_at(position as usize);
        let kind = pack.kind_at(offset)?;

        let mut written = false;
        if let Some(base) = options.delta_hints.and_then(|hints| hints.get(&oid)) {
            let stored_whole = sources.stored_base(&oid)?.is_none_or(|stored| !in_pack(&stored));
            if kind != ObjectKind::Commit && stored_whole && in_pack(base) && !depends_on(sources, &in_pack, &new_bases, base, &oid)? {
                written = writer.add_new_delta(sources, &oid, base)?;
                if written {
                    new_bases.insert(oid, *base);
                }
            }
        }
        if !written {
            // Every stored delta has its base in the same source pack; it is
            // reused when that base is carried over, rebuilt otherwise.
            writer.add_entry(pack, offset, &oid, &in_pack, sources)?;
        }
        if kind == ObjectKind::Commit {
            commits.push((oid, sources.commit(&oid)?.ok_or(Error::Missing(oid))?));
        }
        kinds.push((oid, kind));
    }
    let finished = writer.finish()?;
    let index = PackIndex::new(finished.entries, finished.checksum);
    let graph = CommitGraph::build(commits);

    let bitmaps = match options.bitmaps {
        Some(request) => build_bitmaps(sources, options.rest, &index, &graph, &kinds, request)?,
        None => None,
    };
    Ok((finished.out, Compacted { index, graph, bitmaps, size: finished.size }))
}

/// Whether `object`, as it will be written, depends on `target` through its
/// chain of delta bases, or sits on a chain too long to extend.
///
/// A new delta of `target` against `object` is only safe if this is false:
/// otherwise the two would need each other to be reconstructed.
fn depends_on(
    sources: &PackSet,
    in_pack: &dyn Fn(&Oid) -> bool,
    new_bases: &HashMap<Oid, Oid>,
    object: &Oid,
    target: &Oid,
) -> Result<bool> {
    let mut current = *object;
    for _ in 0..MAX_DELTA_CHAIN {
        if current == *target {
            return Ok(true);
        }
        let base = match new_bases.get(&current) {
            Some(base) => Some(*base),
            None => sources.stored_base(&current)?.filter(|base| in_pack(base)),
        };
        match base {
            Some(base) => current = base,
            None => return Ok(false),
        }
    }
    Ok(true)
}

fn build_bitmaps(
    sources: &PackSet,
    rest: Option<&PackSet>,
    index: &PackIndex,
    graph: &CommitGraph,
    kinds: &[(Oid, ObjectKind)],
    request: &BitmapRequest,
) -> Result<Option<PackBitmaps>> {
    // Walking may need trees of commits that live in the packs left alone.
    let everything = match rest {
        Some(rest) => sources.merged_with(rest),
        None => sources.clone(),
    };
    let src = WithGraph { objects: &everything, graph };

    let mut tips = walk::commit_tips(&src, &request.tips)?;
    let mut dated = HashMap::new();
    for oid in &tips {
        dated.insert(*oid, src.commit(oid)?.map_or(0, |c| c.time));
    }
    tips.sort_by_key(|oid| std::cmp::Reverse(dated[oid]));
    tips.dedup();
    tips.truncate(MAX_TIP_BITMAPS);

    if let Some(mainline) = request.mainline {
        let mut current = walk::commit_tips(&src, &[mainline])?.pop();
        let (mut distance, mut next) = (0usize, 0usize);
        // Only as far as the new pack reaches: older history has its own bitmaps.
        while let Some(oid) = current.filter(|oid| index.position(oid).is_some()) {
            if distance == next {
                tips.push(oid);
                next = if next == 0 { MAINLINE_FIRST_STEP } else { next * 4 };
                if tips.len() >= MAX_TIP_BITMAPS + MAX_MAINLINE_BITMAPS {
                    break;
                }
            }
            distance += 1;
            current = src.commit(&oid)?.and_then(|commit| commit.parents.first().copied());
        }
    }

    let covered = |bases: &[Oid], oids: &[Oid]| -> Result<bool> {
        let Some(rest) = rest else { return Ok(false) };
        let mut union: HashMap<usize, BitSet> = HashMap::new();
        for base in bases {
            let Some(closure) = rest.closure(base)? else { return Ok(false) };
            for (pack, bits) in closure {
                match union.get_mut(&pack) {
                    Some(have) => have.union_with(&bits),
                    None => {
                        union.insert(pack, bits);
                    }
                }
            }
        }
        let is_covered = |oid: &Oid| {
            union.iter().any(|(pack, bits)| rest.packs()[*pack].index().position(oid).is_some_and(|i| bits.get(i)))
        };
        Ok(oids.iter().all(is_covered))
    };
    let context = walk::BitmapContext {
        objects: index.len(),
        position: &|oid| index.position(oid),
        has_base: &|oid| rest.is_some_and(|rest| rest.has_bitmap(oid)),
        covered: &covered,
    };
    let reachable = walk::reachability_bitmaps(&src, &tips, &context)?;
    if reachable.is_empty() {
        return Ok(None);
    }

    let objects = index.len();
    let mut by_kind = [BitSet::new(objects), BitSet::new(objects), BitSet::new(objects), BitSet::new(objects)];
    for (oid, kind) in kinds {
        let position = index.position(oid).ok_or(Error::Missing(*oid))?;
        by_kind[kind_slot(*kind)].set(position);
    }
    Ok(Some(PackBitmaps::build(objects, by_kind, reachable)))
}

/// Chooses which packs to merge so that sizes stay in geometric progression:
/// after merging, every remaining pack holds at least twice as many objects as
/// all smaller ones together. Repeated compaction then keeps the pack count
/// logarithmic while rewriting each object only a logarithmic number of times.
///
/// Takes the object count of each pack and returns the indices to merge, or
/// nothing if the progression already holds.
pub fn geometric_selection(sizes: &[usize]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by_key(|i| sizes[*i]);
    let mut smaller_total = 0usize;
    let mut merge = 0;
    for (rank, i) in order.iter().enumerate() {
        if rank > 0 && sizes[*i] < 2 * smaller_total {
            merge = rank + 1;
        }
        smaller_total += sizes[*i];
    }
    order.truncate(merge);
    order
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;

    use super::*;
    use crate::pack::tests::{blob, open, pack_of};
    use crate::pack::index_pack;
    use crate::walk::{self, plan_fetch, Filter, ShallowPlan};
    use crate::PackFile;

    #[test]
    fn selection_keeps_a_geometric_progression() {
        let sorted = |mut v: Vec<usize>| {
            v.sort();
            v
        };
        assert!(geometric_selection(&[]).is_empty());
        assert!(geometric_selection(&[10]).is_empty());
        assert!(geometric_selection(&[100, 1]).is_empty());
        assert!(geometric_selection(&[1000, 100, 10, 1]).is_empty());
        assert_eq!(sorted(geometric_selection(&[1, 1])), vec![0, 1]);
        // The small packs are merged; the big one is left alone.
        assert_eq!(sorted(geometric_selection(&[1000, 1, 1, 1])), vec![1, 2, 3]);
        // Once the small ones add up, everything is rewritten.
        assert_eq!(sorted(geometric_selection(&[100, 30, 30])), vec![0, 1, 2]);
    }

    fn tree(entries: &[(&str, &str, Oid)]) -> Object {
        let mut data = Vec::new();
        for (mode, name, oid) in entries {
            data.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            data.extend_from_slice(oid.as_bytes());
        }
        Object { kind: ObjectKind::Tree, data: data.into() }
    }

    fn commit(tree: Oid, parents: &[Oid], time: i64) -> Object {
        let mut text = format!("tree {tree}\n");
        for parent in parents {
            text += &format!("parent {parent}\n");
        }
        text += &format!("author A <a@example.com> {time} +0000\ncommitter A <a@example.com> {time} +0000\n\nmessage\n");
        Object { kind: ObjectKind::Commit, data: text.into_bytes().into() }
    }

    /// Three commits pushed as three packs; the middle one changes one of two files.
    struct History {
        packs: PackSet,
        commits: Vec<Oid>,
        all: Vec<Object>,
    }

    fn history() -> History {
        let (a1, b1, b2, c3) = (blob(b"file a"), blob(b"file b, v1"), blob(b"file b, v2"), blob(b"file c"));
        let t1 = tree(&[("100644", "a", a1.oid()), ("100644", "b", b1.oid())]);
        let t2 = tree(&[("100644", "a", a1.oid()), ("100644", "b", b2.oid())]);
        let t3 = tree(&[("100644", "a", a1.oid()), ("100644", "b", b2.oid()), ("100644", "c", c3.oid())]);
        let c1 = commit(t1.oid(), &[], 100);
        let c2 = commit(t2.oid(), &[c1.oid()], 200);
        let c3c = commit(t3.oid(), &[c2.oid()], 300);

        let pushes = [vec![c1.clone(), t1, a1, b1], vec![c2.clone(), t2, b2], vec![c3c.clone(), t3, c3]];
        let mut files = Vec::new();
        for push in &pushes {
            files.push(open(&pack_of(push), &PackSet::default()));
        }
        History { packs: PackSet::new(files), commits: vec![c1.oid(), c2.oid(), c3c.oid()], all: pushes.concat() }
    }

    fn compacted(history: &History, bitmaps: Option<&BitmapRequest>) -> (PackSet, Compacted) {
        merged(&history.packs, &CompactOptions { bitmaps, ..CompactOptions::default() })
    }

    fn merged(sources: &PackSet, options: &CompactOptions<'_>) -> (PackSet, Compacted) {
        let (bytes, result) = compact(sources, Vec::new(), options).unwrap();
        let bytes = Bytes::from(bytes);
        assert_eq!(result.size, bytes.len() as u64);
        // The index written alongside must be the index of what was written.
        let reindexed = index_pack(&bytes, &PackSet::default()).unwrap().index;
        assert_eq!(reindexed.encoded(), result.index.encoded());
        let file = PackFile::new(Arc::new(bytes), result.index.clone(), None)
            .unwrap()
            .with_sidecars(Some(result.graph.clone()), result.bitmaps.clone());
        (PackSet::new(vec![Arc::new(file)]), result)
    }

    #[test]
    fn merges_packs_and_writes_a_commit_graph() {
        let history = history();
        let (merged, result) = compacted(&history, None);
        assert_eq!(result.index.len(), history.all.len());
        for object in &history.all {
            let got = merged.get(&object.oid()).unwrap().unwrap();
            assert_eq!((got.kind, &got.data), (object.kind, &object.data));
        }
        assert_eq!(result.graph.len(), 3);
        let tip = result.graph.get(&history.commits[2]).unwrap();
        assert_eq!((tip.time, tip.parents), (300, vec![history.commits[1]]));
        assert!(result.bitmaps.is_none());
    }

    #[test]
    fn bitmaps_answer_fetches_without_walking() {
        let history = history();
        let request = BitmapRequest { tips: vec![history.commits[2], history.commits[0]], mainline: Some(history.commits[2]) };
        let (merged, result) = compacted(&history, Some(&request));
        let bitmaps = result.bitmaps.expect("history is complete");
        // The root commit reaches its own four objects; the tip reaches everything.
        assert_eq!(bitmaps.get(&history.commits[0]).unwrap().unwrap().0.count(), 4);
        assert_eq!(bitmaps.get(&history.commits[2]).unwrap().unwrap().0.count(), history.all.len());
        assert_eq!(bitmaps.of_kind(ObjectKind::Commit).unwrap().count(), 3);

        let none = ShallowPlan::default();
        let fetch = |wants: &[Oid], haves: &[Oid], filter: Filter| plan_fetch(&merged, wants, haves, &none, &filter).unwrap();

        // A clone is answered entirely from the tip's bitmap.
        let clone = fetch(&[history.commits[2]], &[], Filter::default());
        assert!(clone.objects.is_empty());
        assert_eq!(clone.count(), history.all.len());

        // A client at the root commit gets everything but the root's objects.
        let update = fetch(&[history.commits[2]], &[history.commits[0]], Filter::default());
        assert_eq!(update.count(), history.all.len() - 4);

        // A blobless clone leaves out the four blobs.
        let blobless = fetch(&[history.commits[2]], &[], Filter::parse("blob:none").unwrap());
        assert_eq!(blobless.count(), history.all.len() - 4);
        // Commits only.
        assert_eq!(fetch(&[history.commits[2]], &[], Filter::parse("tree:0").unwrap()).count(), 3);
        // Depth-limited filters cannot use bitmaps, and must still be right:
        // three commits and their three root trees.
        assert_eq!(fetch(&[history.commits[2]], &[], Filter::parse("tree:1").unwrap()).count(), 6);

        // A commit without a bitmap is walked until a bitmap takes over.
        let middle = fetch(&[history.commits[1]], &[], Filter::default());
        assert_eq!(middle.count(), 4 + 3);
        assert_eq!(middle.objects.len(), 3);
    }

    #[test]
    fn incomplete_history_gets_no_bitmaps() {
        let history = history();
        // Only the newest pack: the tip's parents are not in it, and nothing else covers them.
        let newest = PackSet::new(vec![history.packs.packs()[0].clone()]);
        let request = BitmapRequest { tips: vec![history.commits[2]], mainline: None };
        let (_, result) = merged(&newest, &CompactOptions { bitmaps: Some(&request), ..CompactOptions::default() });
        assert!(result.bitmaps.is_none());
        assert_eq!(result.index.len(), 3);
    }

    #[test]
    fn bitmaps_of_a_partial_merge_build_on_older_ones() {
        let history = history();
        let files = history.packs.packs();
        // The two older pushes are merged first and get complete bitmaps...
        let older = PackSet::new(vec![files[2].clone(), files[1].clone()]);
        let request = BitmapRequest { tips: vec![history.commits[1]], mainline: None };
        let (base, _) = merged(&older, &CompactOptions { bitmaps: Some(&request), ..CompactOptions::default() });
        // ...then the newest push is compacted on its own, next to them.
        let newest = PackSet::new(vec![files[0].clone()]);
        let request = BitmapRequest { tips: vec![history.commits[2]], mainline: Some(history.commits[2]) };
        let options = CompactOptions { bitmaps: Some(&request), rest: Some(&base), ..CompactOptions::default() };
        let (layer, result) = merged(&newest, &options);
        let bitmaps = result.bitmaps.expect("the older pack's bitmaps cover the rest");
        let (bits, bases) = bitmaps.get(&history.commits[2]).unwrap().unwrap();
        assert_eq!((bits.count(), bases), (3, &[history.commits[1]][..]));

        let pool = base.merged_with(&layer);
        let none = ShallowPlan::default();
        let clone = plan_fetch(&pool, &[history.commits[2]], &[], &none, &Filter::default()).unwrap();
        assert!(clone.objects.is_empty());
        assert_eq!((clone.bitmaps.len(), clone.count()), (2, history.all.len()));

        // Without the older pack's bitmaps the layer is unusable, and the walk takes over.
        let stripped = PackSet::new(vec![files[2].clone(), files[1].clone()]).merged_with(&layer);
        let walked = plan_fetch(&stripped, &[history.commits[2]], &[], &none, &Filter::default()).unwrap();
        assert!(walked.bitmaps.is_empty());
        assert_eq!(walked.count(), history.all.len());
    }

    #[test]
    fn pruning_drops_what_is_not_kept_and_rebuilds_orphaned_deltas() {
        let history = history();
        // Keep only what the first commit reaches.
        let none = ShallowPlan::default();
        let reachable = plan_fetch(&history.packs, &[history.commits[0]], &[], &none, &Filter::default()).unwrap();
        let keep: std::collections::HashSet<Oid> = reachable.objects.iter().map(|(oid, _)| *oid).collect();
        assert_eq!(keep.len(), 4);
        let keep_fn = |oid: &Oid| keep.contains(oid);
        let (pruned, result) = merged(&history.packs, &CompactOptions { keep: Some(&keep_fn), ..CompactOptions::default() });
        assert_eq!(result.index.len(), 4);
        for object in &history.all {
            assert_eq!(pruned.contains(&object.oid()), keep.contains(&object.oid()));
        }
        assert!(walk::check_connected(&pruned, &PackSet::default(), &[history.commits[0]]).is_ok());
    }

    #[test]
    fn hinted_objects_are_stored_as_deltas_without_cycles() {
        let v1: Vec<u8> = (0..3000u32).flat_map(|i| format!("line {i}\n").into_bytes()).collect();
        let v2 = [&v1[..], b"appended\n"].concat();
        let (v1, v2) = (blob(&v1), blob(&v2));
        let stored = PackSet::new(vec![open(&pack_of(&[v1.clone(), v2.clone()]), &PackSet::default())]);
        let whole_size = pack_of(&[v1.clone(), v2.clone()]).len();

        // Each version is suggested as the base of the other: only one delta may be made.
        let hints = HashMap::from([(v2.oid(), v1.oid()), (v1.oid(), v2.oid())]);
        let (bytes, result) = compact(&stored, Vec::new(), &CompactOptions { delta_hints: Some(&hints), ..CompactOptions::default() }).unwrap();
        assert!(bytes.len() < whole_size * 2 / 3, "{} vs {whole_size}", bytes.len());
        let reread = PackSet::new(vec![Arc::new(PackFile::new(Arc::new(Bytes::from(bytes)), result.index, None).unwrap())]);
        assert_eq!(reread.get(&v1.oid()).unwrap().unwrap().data, v1.data);
        assert_eq!(reread.get(&v2.oid()).unwrap().unwrap().data, v2.data);
    }
}
