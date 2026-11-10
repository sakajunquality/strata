//! What every metadata backend must do, checked the same way for each.
//!
//! Shared databases keep their contents between runs, so every name used
//! here is unique to the run.

use std::sync::Arc;

use strata_git::Oid;

use crate::meta::{Garbage, MetaError, MetaStore, RefUpdate, Txn};

fn unique(what: &str) -> String {
    format!("test-{}/{what}", uuid::Uuid::new_v4().simple())
}

fn oid(byte: u8) -> Oid {
    Oid::from_bytes(&[byte; 20]).unwrap()
}

fn main_ref(old: Option<Oid>, new: Option<Oid>) -> RefUpdate {
    RefUpdate {
        name: "refs/heads/main".into(),
        old,
        new,
    }
}

pub async fn check(store: Arc<dyn MetaStore>) {
    cas_and_manifest(&*store).await;
    versions_guard_against_changes_in_between(&*store).await;
    leases(&*store).await;
    events_and_deletion(&*store).await;
    concurrent_commits(store).await;
}

async fn cas_and_manifest(store: &dyn MetaStore) {
    let name = unique("repo");
    assert!(store.get_repo(&name).await.unwrap().is_none());
    assert!(matches!(
        store.commit(&name, Txn::default()).await,
        Err(MetaError::NotFound)
    ));
    let repo = store.create_repo(&name).await.unwrap();
    assert!(matches!(
        store.create_repo(&name).await,
        Err(MetaError::AlreadyExists)
    ));
    assert!(store.list_repos().await.unwrap().contains(&name));

    let (one, two) = (oid(1), oid(2));
    let txn = Txn {
        ref_updates: vec![main_ref(None, Some(one))],
        add_pack: Some("p1".into()),
        ..Txn::default()
    };
    store.commit(&name, txn).await.unwrap();

    // A stale expectation must change nothing, including the manifest.
    let stale = Txn {
        ref_updates: vec![main_ref(None, Some(two))],
        add_pack: Some("p2".into()),
        ..Txn::default()
    };
    assert!(
        matches!(store.commit(&name, stale).await, Err(MetaError::Conflict { name }) if name == "refs/heads/main")
    );

    let state = store.get_repo(&name).await.unwrap().unwrap();
    assert_eq!(
        (state.name.as_str(), &state.pool_id, state.head.as_str()),
        (name.as_str(), &repo.pool_id, "refs/heads/main")
    );
    assert_eq!(state.refs.get("refs/heads/main"), Some(&one));
    assert_eq!(state.packs, vec!["p1".to_string()]);
    assert_eq!((state.epoch, state.refs_version), (0, 1));

    // Several refs at once, HEAD moved with them; all or nothing.
    let many = |main_old| Txn {
        ref_updates: vec![
            main_ref(main_old, Some(two)),
            RefUpdate {
                name: "refs/heads/dev".into(),
                old: None,
                new: Some(one),
            },
            RefUpdate {
                name: "refs/tags/v1".into(),
                old: None,
                new: Some(two),
            },
        ],
        set_head: Some("refs/heads/dev".into()),
        add_pack: Some("p2".into()),
        ..Txn::default()
    };
    assert!(matches!(
        store.commit(&name, many(None)).await,
        Err(MetaError::Conflict { .. })
    ));
    assert_eq!(store.get_repo(&name).await.unwrap().unwrap().refs.len(), 1);
    store.commit(&name, many(Some(one))).await.unwrap();
    let state = store.get_repo(&name).await.unwrap().unwrap();
    assert_eq!(state.refs.len(), 3);
    assert_eq!(
        (
            state.refs["refs/heads/main"],
            state.head.as_str(),
            state.refs_version
        ),
        (two, "refs/heads/dev", 2)
    );
    // Packs come back oldest first; adding one twice changes nothing.
    store
        .commit(
            &name,
            Txn {
                add_pack: Some("p2".into()),
                ..Txn::default()
            },
        )
        .await
        .unwrap();
    store
        .commit(
            &name,
            Txn {
                add_pack: Some("a-later-pack".into()),
                ..Txn::default()
            },
        )
        .await
        .unwrap();
    let state = store.get_repo(&name).await.unwrap().unwrap();
    assert_eq!(state.packs, ["p1", "p2", "a-later-pack"]);
    assert_eq!(state.refs_version, 2);

    let delete = Txn {
        ref_updates: vec![
            RefUpdate {
                name: "refs/heads/dev".into(),
                old: Some(one),
                new: None,
            },
            RefUpdate {
                name: "refs/tags/v1".into(),
                old: Some(two),
                new: None,
            },
        ],
        ..Txn::default()
    };
    store.commit(&name, delete).await.unwrap();
    assert_eq!(store.get_repo(&name).await.unwrap().unwrap().refs.len(), 1);

    // Compaction swaps packs atomically; whoever loses a race changes nothing.
    let swap = |add: &str| Txn {
        add_pack: Some(add.into()),
        remove_packs: vec!["p1".into(), "p2".into()],
        ..Txn::default()
    };
    store.commit(&name, swap("merged")).await.unwrap();
    assert!(matches!(
        store.commit(&name, swap("other")).await,
        Err(MetaError::PackConflict)
    ));
    assert_eq!(
        store.get_repo(&name).await.unwrap().unwrap().packs,
        ["a-later-pack", "merged"]
    );

    // The replaced packs wait out their grace period, then are forgotten.
    let ours = |all: Vec<Garbage>| {
        let mut ours: Vec<String> = all
            .into_iter()
            .filter(|g| g.pool_id == repo.pool_id)
            .map(|g| g.pack_id)
            .collect();
        ours.sort();
        ours
    };
    assert!(ours(store.list_garbage(0).await.unwrap()).is_empty());
    assert_eq!(
        ours(store.list_garbage(i64::MAX).await.unwrap()),
        ["p1", "p2"]
    );
    store
        .purge_garbage(&Garbage {
            pool_id: repo.pool_id.clone(),
            pack_id: "p1".into(),
        })
        .await
        .unwrap();
    assert_eq!(ours(store.list_garbage(i64::MAX).await.unwrap()), ["p2"]);
}

async fn versions_guard_against_changes_in_between(store: &dyn MetaStore) {
    let name = unique("versions");
    store.create_repo(&name).await.unwrap();
    let push = |epoch| Txn {
        ref_updates: vec![main_ref(None, Some(oid(1)))],
        expect_epoch: Some(epoch),
        ..Txn::default()
    };

    // Objects were removed after the pusher looked: its checks no longer hold.
    store
        .commit(
            &name,
            Txn {
                bump_epoch: true,
                ..Txn::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        store.commit(&name, push(0)).await,
        Err(MetaError::Stale)
    ));
    store.commit(&name, push(1)).await.unwrap();

    // Refs moved after a pruning compaction verified them: it must look again.
    let prune = |version| Txn {
        bump_epoch: true,
        expect_refs_version: Some(version),
        ..Txn::default()
    };
    assert!(matches!(
        store.commit(&name, prune(0)).await,
        Err(MetaError::Stale)
    ));
    store.commit(&name, prune(1)).await.unwrap();
    let state = store.get_repo(&name).await.unwrap().unwrap();
    assert_eq!((state.epoch, state.refs_version), (2, 1));
}

async fn leases(store: &dyn MetaStore) {
    let key = unique("lease");
    assert!(store.try_lease(&key, "node-a", 60).await.unwrap());
    assert!(store.try_lease(&key, "node-a", 60).await.unwrap());
    assert!(!store.try_lease(&key, "node-b", 60).await.unwrap());
    store.release_lease(&key, "node-b").await.unwrap();
    assert!(!store.try_lease(&key, "node-b", 60).await.unwrap());
    store.release_lease(&key, "node-a").await.unwrap();
    assert!(store.try_lease(&key, "node-b", 0).await.unwrap());
    // An expired lease is up for grabs.
    assert!(store.try_lease(&key, "node-a", 60).await.unwrap());
}

async fn events_and_deletion(store: &dyn MetaStore) {
    let name = unique("events");
    let repo = store.create_repo(&name).await.unwrap();
    // Other tests (and other runs) write to the same feed: start at its end.
    let mut start = None;
    loop {
        let page = store.list_events(start.as_deref(), 500).await.unwrap();
        match page.last() {
            Some(last) => start = Some(last.cursor.clone()),
            None => break,
        }
    }
    let ours = |events: Vec<crate::meta::Event>| events.into_iter().filter(|e| e.repo == name).collect::<Vec<_>>();

    let push = |old, new, event: &str| Txn {
        ref_updates: vec![main_ref(old, new)],
        add_pack: Some(format!("pack-{event}")),
        event: Some(event.to_string()),
        ..Txn::default()
    };
    store.commit(&name, push(None, Some(oid(1)), "first")).await.unwrap();
    // A change that fails records nothing.
    assert!(store.commit(&name, push(None, Some(oid(2)), "lost")).await.is_err());
    store.commit(&name, push(Some(oid(1)), Some(oid(2)), "second")).await.unwrap();
    store.commit(&name, push(Some(oid(2)), Some(oid(3)), "third")).await.unwrap();

    let all = ours(store.list_events(start.as_deref(), 500).await.unwrap());
    assert_eq!(all.iter().map(|e| e.payload.as_str()).collect::<Vec<_>>(), ["first", "second", "third"]);
    assert!(all.iter().all(|e| e.created_at > 1_600_000_000));
    // Reading on from any event gives exactly what followed it.
    let rest = ours(store.list_events(Some(&all[0].cursor), 500).await.unwrap());
    assert_eq!(rest, all[1..]);
    assert!(ours(store.list_events(Some(&all[2].cursor), 500).await.unwrap()).is_empty());
    assert!(store.list_events(start.as_deref(), 1).await.unwrap().len() <= 1);
    assert!(matches!(store.list_events(Some("not a cursor"), 10).await, Err(MetaError::BadCursor)));

    // Nothing is old enough to purge yet; then everything is.
    store.purge_events(1_600_000_000).await.unwrap();
    assert_eq!(ours(store.list_events(start.as_deref(), 500).await.unwrap()).len(), 3);

    // A deleted repository is gone at once; its packs wait as garbage.
    assert!(store.delete_repo(&name).await.unwrap());
    assert!(!store.delete_repo(&name).await.unwrap());
    assert!(store.get_repo(&name).await.unwrap().is_none());
    assert!(!store.list_repos().await.unwrap().contains(&name));
    assert!(matches!(store.commit(&name, Txn::default()).await, Err(MetaError::NotFound)));
    let garbage = store.list_garbage(i64::MAX).await.unwrap();
    assert_eq!(garbage.iter().filter(|g| g.pool_id == repo.pool_id).count(), 3);
    // The name can be used again, for a repository that shares nothing with the old one.
    let again = store.create_repo(&name).await.unwrap();
    assert_ne!(again.pool_id, repo.pool_id);
    assert!(store.get_repo(&name).await.unwrap().unwrap().refs.is_empty());
}

/// Several nodes at once: every race has exactly one winner.
async fn concurrent_commits(store: Arc<dyn MetaStore>) {
    const WRITERS: u8 = 12;
    let name = unique("race");
    store.create_repo(&name).await.unwrap();

    // The same branch, created by everyone at once, each with its own pack.
    let mut tasks = Vec::new();
    for i in 0..WRITERS {
        let (store, name) = (store.clone(), name.clone());
        tasks.push(tokio::spawn(async move {
            let txn = Txn {
                ref_updates: vec![main_ref(None, Some(oid(i + 1)))],
                add_pack: Some(format!("race-{i}")),
                ..Txn::default()
            };
            store.commit(&name, txn).await.map(|()| i)
        }));
    }
    let mut winners = Vec::new();
    for task in tasks {
        match task.await.unwrap() {
            Ok(i) => winners.push(i),
            Err(MetaError::Conflict { .. }) => {}
            Err(e) => panic!("unexpected failure: {e}"),
        }
    }
    assert_eq!(winners.len(), 1, "exactly one writer creates the branch");
    let state = store.get_repo(&name).await.unwrap().unwrap();
    assert_eq!(state.refs["refs/heads/main"], oid(winners[0] + 1));
    assert_eq!(state.packs, [format!("race-{}", winners[0])]);
    assert_eq!(state.refs_version, 1);

    // Changes that do not conflict all land, and none is lost.
    let mut tasks = Vec::new();
    for i in 0..WRITERS {
        let (store, name) = (store.clone(), name.clone());
        tasks.push(tokio::spawn(async move {
            let update = RefUpdate {
                name: format!("refs/heads/topic-{i}"),
                old: None,
                new: Some(oid(i + 1)),
            };
            store
                .commit(
                    &name,
                    Txn {
                        ref_updates: vec![update],
                        add_pack: Some(format!("topic-{i}")),
                        event: Some(format!("topic-{i}")),
                        ..Txn::default()
                    },
                )
                .await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let state = store.get_repo(&name).await.unwrap().unwrap();
    assert_eq!(
        (state.refs.len(), state.packs.len()),
        (1 + WRITERS as usize, 1 + WRITERS as usize)
    );
    assert_eq!(state.refs_version, 1 + WRITERS as u64);
    assert_eq!(
        state.packs[0],
        format!("race-{}", winners[0]),
        "the oldest pack stays first"
    );
    assert_eq!(
        store.get_repo(&name).await.unwrap().unwrap().packs,
        state.packs,
        "the order is stable"
    );

    // Each of them left exactly one event.
    let (mut seen, mut after) = (0, None);
    loop {
        let page = store.list_events(after.as_deref(), 100).await.unwrap();
        let Some(last) = page.last() else { break };
        after = Some(last.cursor.clone());
        seen += page.iter().filter(|e| e.repo == name).count();
    }
    assert_eq!(seen, WRITERS as usize);

    // One lease, many nodes.
    let key = unique("lease");
    let mut tasks = Vec::new();
    for i in 0..WRITERS {
        let (store, key) = (store.clone(), key.clone());
        tasks.push(tokio::spawn(async move {
            store
                .try_lease(&key, &format!("node-{i}"), 60)
                .await
                .unwrap()
        }));
    }
    let mut holders = 0;
    for task in tasks {
        holders += usize::from(task.await.unwrap());
    }
    assert_eq!(holders, 1, "exactly one node holds the lease");
}
