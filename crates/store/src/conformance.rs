//! What every metadata backend must do, checked the same way for each.
//!
//! Shared databases keep their contents between runs, so every name used
//! here is unique to the run.

use std::sync::Arc;

use strata_git::Oid;

use crate::meta::{Access, Garbage, Grant, MetaError, MetaStore, RefUpdate, TokenRecord, Txn};

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
    leases_and_tokens(&*store).await;
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

async fn leases_and_tokens(store: &dyn MetaStore) {
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

    let (name, hash) = (unique("token"), unique("hash"));
    let token = TokenRecord {
        name: name.clone(),
        hash: hash.clone(),
        grants: vec![
            Grant {
                pattern: "acme/*".into(),
                access: Access::Read,
            },
            Grant {
                pattern: "acme/app".into(),
                access: Access::Write,
            },
        ],
    };
    store.put_token(token.clone()).await.unwrap();
    assert_eq!(store.find_token(&hash).await.unwrap(), Some(token));
    assert_eq!(store.find_token(&unique("other")).await.unwrap(), None);

    // Reissuing a token under the same name retires the old secret.
    let rehash = unique("hash");
    let reissued = TokenRecord {
        name: name.clone(),
        hash: rehash.clone(),
        grants: vec![],
    };
    store.put_token(reissued.clone()).await.unwrap();
    assert_eq!(store.find_token(&hash).await.unwrap(), None);
    assert_eq!(store.find_token(&rehash).await.unwrap(), Some(reissued));

    assert!(store.delete_token(&name).await.unwrap());
    assert!(!store.delete_token(&name).await.unwrap());
    assert_eq!(store.find_token(&rehash).await.unwrap(), None);
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
