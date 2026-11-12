//! Drives the server with the real `git` client.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use object_store::memory::InMemory;
use strata_server::{compact, router, spawn_background, sweep, sweep_orphans, App, Options};
use strata_store::{CacheConfig, MemoryMetaStore, PackStore};

fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
    git_with(dir, args, &[])
}

fn git_with(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .envs(env.iter().copied())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git must be installed")
}

#[track_caller]
fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_in(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Deterministic bytes that zlib cannot shrink, to push the request past
/// git's 1 MiB `http.postBuffer` and exercise chunked uploads.
fn incompressible(len: usize) -> Vec<u8> {
    let mut state = 0x9e3779b97f4a7c15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 32) as u8
        })
        .collect()
}

async fn start_with(options: Options) -> (String, Arc<App>) {
    // Tiny blocks and caches, so that reads straddle block boundaries and
    // entries are evicted even with test-sized repositories.
    let packs = PackStore::new(
        Arc::new(InMemory::new()),
        CacheConfig { block_size: 4096, block_cache_bytes: 256 << 10, object_cache_bytes: 64 << 10, index_cache_bytes: 1 << 20 },
    );
    let app = Arc::new(App::new(Arc::new(MemoryMetaStore::default()), packs, options));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, router(served)).await.unwrap() });
    (format!("http://{addr}"), app)
}

/// A server that never compacts on its own.
async fn start(auto_create: bool) -> String {
    start_with(Options { auto_create, compact_at: usize::MAX, ..Options::default() }).await.0
}

#[tokio::test(flavor = "multi_thread")]
async fn push_clone_fetch() {
    let url = format!("{}/acme/demo.git", start(true).await);
    tokio::task::spawn_blocking(move || scenario(&url)).await.unwrap();
}

fn scenario(url: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let work = root.join("work");
    std::fs::create_dir(&work).unwrap();

    // First push: a branch that is not the server's default name, plus an annotated tag.
    git(&work, &["init", "-q", "-b", "trunk"]);
    let text: String = (0..400).map(|i| format!("line {i} of the file\n")).collect();
    std::fs::write(work.join("notes.txt"), &text).unwrap();
    std::fs::create_dir(work.join("dir")).unwrap();
    std::fs::write(work.join("dir/nested.txt"), "nested\n").unwrap();
    std::fs::write(work.join("big.bin"), incompressible(2 << 20)).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first"]);
    git(&work, &["tag", "-a", "v1", "-m", "version one"]);
    git(&work, &["remote", "add", "origin", url]);
    git(&work, &["push", "-q", "origin", "trunk", "v1"]);
    let first = git(&work, &["rev-parse", "HEAD"]);

    // Clone must check out the pushed branch and receive the tag.
    git(root, &["clone", "-q", url, "clone1"]);
    let clone1 = root.join("clone1");
    assert_eq!(git(&clone1, &["rev-parse", "HEAD"]), first);
    assert_eq!(git(&clone1, &["symbolic-ref", "HEAD"]), "refs/heads/trunk");
    assert_eq!(git(&clone1, &["rev-parse", "v1"]), git(&work, &["rev-parse", "v1"]));
    assert_eq!(std::fs::read(clone1.join("big.bin")).unwrap(), incompressible(2 << 20));
    git(&clone1, &["fsck", "--strict"]);

    // Second push: a small edit, which git sends as a thin pack of deltas.
    std::fs::write(work.join("notes.txt"), text.replace("line 200 ", "line two hundred ")).unwrap();
    git(&work, &["commit", "-q", "-am", "second"]);
    git(&work, &["push", "-q", "origin", "trunk"]);
    let second = git(&work, &["rev-parse", "HEAD"]);

    // Incremental fetch into the existing clone.
    git(&clone1, &["pull", "-q", "--ff-only"]);
    assert_eq!(git(&clone1, &["rev-parse", "HEAD"]), second);
    assert_eq!(std::fs::read(clone1.join("notes.txt")).unwrap(), std::fs::read(work.join("notes.txt")).unwrap());
    git(&clone1, &["fsck", "--strict"]);
    // Exactly the new commit, its root tree and the edited file arrived (git
    // unpacks small fetches into loose objects): not the 2 MiB file next to it.
    assert_eq!(git(&clone1, &["count-objects"]).split(' ').next(), Some("3"));

    // A fresh clone reads across both packs.
    git(root, &["clone", "-q", url, "clone2"]);
    let clone2 = root.join("clone2");
    assert_eq!(git(&clone2, &["rev-parse", "HEAD"]), second);
    git(&clone2, &["fsck", "--strict"]);
    // The deltas git pushed for the edited file and its tree are passed on as deltas, not re-expanded.
    let pack_dir = clone2.join(".git/objects/pack");
    let idx = std::fs::read_dir(&pack_dir).unwrap().map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|e| e == "idx"));
    let listing = git(&pack_dir, &["verify-pack", "-v", idx.unwrap().to_str().unwrap()]);
    assert!(listing.contains("chain length = 1: 2 objects"), "{listing}");

    // A push based on stale history is refused and leaves the ref alone.
    git(&clone2, &["reset", "-q", "--hard", &first]);
    std::fs::write(clone2.join("other.txt"), "diverged\n").unwrap();
    git(&clone2, &["add", "."]);
    git(&clone2, &["commit", "-q", "-m", "diverged"]);
    assert!(!git_in(&clone2, &["push", "-q", "origin", "trunk"]).status.success());
    assert!(git(&work, &["ls-remote", "origin", "refs/heads/trunk"]).starts_with(&second));

    // Branch create (no new objects) and delete.
    git(&work, &["push", "-q", "origin", "trunk:refs/heads/topic"]);
    assert!(git(&work, &["ls-remote", "origin", "refs/heads/topic"]).starts_with(&second));
    git(&work, &["push", "-q", "origin", ":refs/heads/topic"]);
    assert_eq!(git(&work, &["ls-remote", "origin", "refs/heads/topic"]), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_repository_is_not_created_implicitly() {
    let base = start(false).await;
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("{base}/nobody/nothing.git");
        assert!(!git_in(tmp.path(), &["ls-remote", &url]).status.success());
        assert!(!git_in(tmp.path(), &["clone", "-q", &url, "x"]).status.success());
    })
    .await
    .unwrap();
}

/// Commits everything in `dir` with a fixed date in January 2020 and returns the commit id.
fn commit_on(dir: &Path, day: u32, message: &str) -> String {
    let date = format!("2020-01-{day:02}T12:00:00+00:00");
    git(dir, &["add", "."]);
    let out = git_with(dir, &["commit", "-q", "-m", message], &[("GIT_AUTHOR_DATE", &date), ("GIT_COMMITTER_DATE", &date)]);
    assert!(out.status.success(), "commit failed: {}", String::from_utf8_lossy(&out.stderr));
    git(dir, &["rev-parse", "HEAD"])
}

fn commit_count(dir: &Path) -> String {
    git(dir, &["rev-list", "--count", "HEAD"])
}

fn is_shallow(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--is-shallow-repository"]) == "true"
}

fn has_object(dir: &Path, oid: &str) -> bool {
    // Keep a partial clone from fetching the object just because we asked about it.
    git_with(dir, &["cat-file", "-e", oid], &[("GIT_NO_LAZY_FETCH", "1")]).status.success()
}

/// Objects reachable from HEAD that a partial clone has not downloaded.
fn missing_objects(dir: &Path) -> usize {
    git(dir, &["rev-list", "--objects", "--missing=print", "HEAD"]).lines().filter(|l| l.starts_with('?')).count()
}

#[tokio::test(flavor = "multi_thread")]
async fn shallow_and_partial_clones() {
    let url = format!("{}/acme/ci.git", start(true).await);
    tokio::task::spawn_blocking(move || shallow_scenario(&url)).await.unwrap();
}

fn shallow_scenario(url: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let work = root.join("work");
    std::fs::create_dir_all(work.join("src")).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["remote", "add", "origin", url]);

    // Five commits, one per day, each touching one of two files.
    let body = |n: u32| -> String { (0..200).map(|i| format!("revision {n} line {i}\n")).collect() };
    let mut commits = Vec::new();
    for n in 1..=5 {
        std::fs::write(work.join("src/code.txt"), body(n)).unwrap();
        std::fs::write(work.join(format!("file{n}.txt")), format!("file {n}\n")).unwrap();
        commits.push(commit_on(&work, n, &format!("commit {n}")));
        if n == 2 {
            git(&work, &["tag", "-a", "v1", "-m", "version one"]);
        }
    }
    git(&work, &["push", "-q", "origin", "main", "v1"]);

    // --depth 1, the usual CI clone.
    git(root, &["clone", "-q", "--depth", "1", url, "s1"]);
    let s1 = root.join("s1");
    assert!(is_shallow(&s1));
    assert_eq!(commit_count(&s1), "1");
    assert_eq!(git(&s1, &["rev-parse", "HEAD"]), commits[4]);
    assert_eq!(std::fs::read_to_string(s1.join("src/code.txt")).unwrap(), body(5));
    git(&s1, &["fsck", "--strict"]);
    // The server must really have left the older history out, not just labelled the clone shallow.
    assert!(!has_object(&s1, &commits[3]));
    assert!(!has_object(&s1, &git(&work, &["rev-parse", &format!("{}:src/code.txt", commits[3])])));
    assert_eq!(git(&s1, &["count-objects", "-v"]).lines().find(|l| l.starts_with("in-pack")), Some("in-pack: 9"));

    // Deepen step by step, then all the way.
    git(&s1, &["fetch", "-q", "--deepen=1"]);
    assert_eq!(commit_count(&s1), "2");
    git(&s1, &["fetch", "-q", "--depth=4"]);
    assert_eq!(commit_count(&s1), "4");
    git(&s1, &["fsck", "--strict"]);
    git(&s1, &["fetch", "-q", "--unshallow"]);
    assert!(!is_shallow(&s1));
    assert_eq!(commit_count(&s1), "5");
    git(&s1, &["fsck", "--strict"]);

    // Fetching one commit by id into an empty repository, as CI checkout actions do.
    let ci = root.join("ci");
    std::fs::create_dir(&ci).unwrap();
    git(&ci, &["init", "-q"]);
    git(&ci, &["fetch", "-q", "--depth=1", url, &commits[2]]);
    git(&ci, &["checkout", "-q", "FETCH_HEAD"]);
    assert_eq!(git(&ci, &["rev-parse", "HEAD"]), commits[2]);
    assert_eq!(commit_count(&ci), "1");
    assert!(!has_object(&ci, &commits[1]) && !has_object(&ci, &commits[4]));
    assert_eq!(std::fs::read_to_string(ci.join("src/code.txt")).unwrap(), body(3));
    assert!(!ci.join("file4.txt").exists());

    // An ordinary pull in a shallow clone picks up new commits without deepening.
    git(root, &["clone", "-q", "--depth", "2", url, "s2"]);
    let s2 = root.join("s2");
    std::fs::write(work.join("src/code.txt"), body(6)).unwrap();
    commits.push(commit_on(&work, 6, "commit 6"));
    git(&work, &["push", "-q", "origin", "main"]);
    git(&s2, &["pull", "-q", "--ff-only"]);
    assert_eq!(git(&s2, &["rev-parse", "HEAD"]), commits[5]);
    assert_eq!(commit_count(&s2), "3");
    git(&s2, &["fsck", "--strict"]);

    // Pushing from a shallow clone works when the server has the rest of the history.
    std::fs::write(s2.join("from-shallow.txt"), "pushed from a shallow clone\n").unwrap();
    commits.push(commit_on(&s2, 7, "commit 7"));
    git(&s2, &["push", "-q", "origin", "main"]);
    assert!(git(&work, &["ls-remote", "origin", "refs/heads/main"]).starts_with(&commits[6]));

    // Cut by date and by ref instead of by depth.
    git(root, &["clone", "-q", "--shallow-since=2020-01-05T00:00:00+00:00", url, "since"]);
    assert_eq!(commit_count(&root.join("since")), "3");
    git(&root.join("since"), &["fsck", "--strict"]);
    git(root, &["clone", "-q", "--shallow-exclude=v1", url, "exclude"]);
    assert_eq!(commit_count(&root.join("exclude")), "5");
    git(&root.join("exclude"), &["fsck", "--strict"]);

    // Blobless clone: full history, file contents fetched on checkout.
    git(root, &["clone", "-q", "--filter=blob:none", "--no-checkout", url, "blobless"]);
    let blobless = root.join("blobless");
    assert_eq!(commit_count(&blobless), "7");
    assert!(missing_objects(&blobless) > 0);
    git(&blobless, &["checkout", "-q", "main"]);
    assert_eq!(std::fs::read_to_string(blobless.join("src/code.txt")).unwrap(), body(6));
    // Only the checked-out revision was downloaded; older file versions are still absent.
    assert!(missing_objects(&blobless) > 0);

    // Size-limited: small files come with the clone, large ones on demand.
    git(root, &["clone", "-q", "--filter=blob:limit=1k", "--no-checkout", url, "limited"]);
    let limited = root.join("limited");
    let absent = git(&limited, &["rev-list", "--objects", "--missing=print", "HEAD"]);
    let code_blob = git(&work, &["rev-parse", "HEAD:src/code.txt"]);
    let small_blob = git(&work, &["rev-parse", "HEAD:file1.txt"]);
    assert!(absent.contains(&format!("?{code_blob}")));
    assert!(!absent.contains(&format!("?{small_blob}")));

    // The smallest possible checkout: one commit, trees and blobs on demand.
    git(root, &["clone", "-q", "--depth", "1", "--filter=tree:0", url, "treeless"]);
    let treeless = root.join("treeless");
    assert_eq!(commit_count(&treeless), "1");
    assert_eq!(std::fs::read_to_string(treeless.join("src/code.txt")).unwrap(), body(6));
    assert!(treeless.join("from-shallow.txt").exists());

    // Depth-limited trees: commits and root trees arrive, the rest on demand.
    git(root, &["clone", "-q", "--filter=tree:1", "--no-checkout", url, "roots"]);
    let roots = root.join("roots");
    assert_eq!(commit_count(&roots), "7");
    assert!(has_object(&roots, &git(&work, &["rev-parse", "HEAD^{tree}"])));
    assert!(!has_object(&roots, &git(&work, &["rev-parse", "HEAD:src"])));
    git(&roots, &["checkout", "-q", "main"]);
    assert_eq!(std::fs::read_to_string(roots.join("src/code.txt")).unwrap(), body(6));

    // Several filters at once: an object comes only if all of them let it through.
    git(root, &["clone", "-q", "--filter=tree:3", "--filter=blob:limit=1k", "--no-checkout", url, "combined"]);
    let combined = root.join("combined");
    assert_eq!(commit_count(&combined), "7");
    assert!(has_object(&combined, &git(&work, &["rev-parse", "HEAD:src"])));
    assert!(has_object(&combined, &small_blob));
    assert!(!has_object(&combined, &code_blob));
    git(root, &["clone", "-q", "--filter=tree:1", "--filter=blob:limit=1k", "--no-checkout", url, "stricter"]);
    assert!(!has_object(&root.join("stricter"), &small_blob));

    // Sparse: only the files that a list of patterns kept in the repository
    // names, in every revision. Trees all come, so the clone knows what it left out.
    std::fs::write(work.join(".sparse"), "# the code, and one more file\n/src/\nfile2.*\n").unwrap();
    git(&work, &["checkout", "-q", "-b", "patterns"]);
    commit_on(&work, 8, "patterns");
    git(&work, &["push", "-q", "origin", "patterns"]);
    git(&work, &["checkout", "-q", "main"]);
    git(root, &["clone", "-q", "--filter=sparse:oid=patterns:.sparse", "--no-checkout", "--branch", "main", url, "sparse"]);
    let sparse = root.join("sparse");
    assert_eq!(commit_count(&sparse), "7");
    assert!(has_object(&sparse, &code_blob));
    assert!(has_object(&sparse, &git(&work, &["rev-parse", &format!("{}:src/code.txt", commits[0])])));
    assert!(has_object(&sparse, &git(&work, &["rev-parse", "HEAD:file2.txt"])));
    assert!(has_object(&sparse, &git(&work, &["rev-parse", "HEAD^{tree}"])));
    assert!(!has_object(&sparse, &small_blob));
    assert!(!has_object(&sparse, &git(&s2, &["rev-parse", "HEAD:from-shallow.txt"])));
    // What was left out still comes when asked for.
    git(&sparse, &["checkout", "-q", "main"]);
    assert_eq!(std::fs::read_to_string(sparse.join("file1.txt")).unwrap(), "file 1\n");
    // The patterns can be named by the blob's id too, and combined with other filters.
    let list = git(&work, &["rev-parse", "patterns:.sparse"]);
    git(root, &["clone", "-q", &format!("--filter=sparse:oid={list}"), "--filter=blob:limit=1k", "--no-checkout", url, "both"]);
    assert!(has_object(&root.join("both"), &git(&work, &["rev-parse", "HEAD:file2.txt"])));
    assert!(!has_object(&root.join("both"), &code_blob) && !has_object(&root.join("both"), &small_blob));
    assert!(!git_in(root, &["clone", "-q", "--filter=sparse:oid=patterns:nope", url, "nolist"]).status.success());

    // By kind of object: all the commits and nothing else, or the files alone
    // (with the commit that was asked for, which comes whatever the filter says).
    git(root, &["clone", "-q", "--filter=object:type=commit", "--no-checkout", url, "commits-only"]);
    let only = root.join("commits-only");
    assert_eq!(commit_count(&only), "7");
    assert!(has_object(&only, &commits[0]));
    assert!(!has_object(&only, &git(&s2, &["rev-parse", "HEAD^{tree}"])) && !has_object(&only, &small_blob));
    git(root, &["clone", "-q", "--filter=object:type=blob", "--no-checkout", url, "blobs-only"]);
    let only = root.join("blobs-only");
    assert!(has_object(&only, &git(&s2, &["rev-parse", "HEAD"])));
    assert!(has_object(&only, &small_blob) && has_object(&only, &code_blob));
    assert!(!has_object(&only, &commits[0]) && !has_object(&only, &git(&s2, &["rev-parse", "HEAD^{tree}"])));
}

async fn pack_count(app: &App, repo: &str) -> usize {
    app.meta.get_repo(repo, &strata_store::RefScope::All).await.unwrap().unwrap().packs.len()
}

/// Clones `url` into `root/name` and checks the clone is complete and at `head`.
fn clone_and_check(root: &Path, url: &str, name: &str, head: &str, commits: &str) {
    git(root, &["clone", "-q", url, name]);
    let clone = root.join(name);
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
    assert_eq!(commit_count(&clone), commits);
    git(&clone, &["fsck", "--strict"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_preserves_the_repository() {
    let options = Options { auto_create: true, compact_at: usize::MAX, gc_grace: Duration::ZERO, ..Options::default() };
    let (base, app) = start_with(options).await;
    let url = format!("{base}/acme/compact.git");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let work = root.join("work");

    // Six pushes with new objects make six packs; later ones are thin packs
    // against earlier ones. (The tag push carries commit 2, so the branch push
    // after it has nothing left to send.)
    let (dir, remote) = (root.clone(), url.clone());
    let mut commits: Vec<String> = tokio::task::spawn_blocking(move || {
        let work = dir.join("work");
        std::fs::create_dir_all(work.join("src")).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["remote", "add", "origin", &remote]);
        let mut commits = Vec::new();
        for n in 1..=6 {
            let body: String = (0..300).map(|i| format!("revision {n} line {i}\n")).collect();
            std::fs::write(work.join("src/code.txt"), body).unwrap();
            std::fs::write(work.join(format!("file{n}.txt")), format!("file {n}\n")).unwrap();
            commits.push(commit_on(&work, n, &format!("commit {n}")));
            if n == 2 {
                git(&work, &["tag", "-a", "v1", "-m", "version one"]);
                git(&work, &["push", "-q", "origin", "v1"]);
            }
            git(&work, &["push", "-q", "origin", "main"]);
        }
        // A clone from before any compaction, to pull into later.
        git(&dir, &["clone", "-q", &remote, "early"]);
        commits
    })
    .await
    .unwrap();
    assert_eq!(pack_count(&app, "acme/compact").await, 6);

    // Equal-sized packs break the geometric progression: all of them merge.
    let report = compact(&app, "acme/compact", false).await.unwrap().expect("something to merge");
    assert_eq!(report.merged, 6);
    assert!(report.full);
    assert_eq!(pack_count(&app, "acme/compact").await, 1);
    assert!(compact(&app, "acme/compact", false).await.unwrap().is_none());

    // The replaced packs are deleted; everything must now come from the merged one.
    assert_eq!(sweep(&app).await.unwrap(), 6);
    assert_eq!(sweep(&app).await.unwrap(), 0);

    let (dir, remote, head) = (root.clone(), url.clone(), commits[5].clone());
    let newer = tokio::task::spawn_blocking(move || {
        // Served from the tip's bitmap.
        clone_and_check(&dir, &remote, "after-compaction", &head, "6");
        assert_eq!(git(&dir.join("after-compaction"), &["rev-parse", "v1^{commit}"]).len(), 40);

        git(&dir, &["clone", "-q", "--depth", "1", &remote, "shallow"]);
        assert_eq!(commit_count(&dir.join("shallow")), "1");
        git(&dir.join("shallow"), &["fsck", "--strict"]);

        git(&dir, &["clone", "-q", "--filter=blob:none", "--no-checkout", &remote, "blobless"]);
        assert_eq!(commit_count(&dir.join("blobless")), "6");
        assert!(missing_objects(&dir.join("blobless")) > 0);
        git(&dir.join("blobless"), &["checkout", "-q", "main"]);

        // A push on top of the merged pack, then a clone that needs both the
        // new pack (walked) and the old history (from the bitmap).
        let work = dir.join("work");
        std::fs::write(work.join("file7.txt"), "file 7\n").unwrap();
        let newer = commit_on(&work, 7, "commit 7");
        git(&work, &["push", "-q", "origin", "main"]);
        clone_and_check(&dir, &remote, "mixed", &newer, "7");

        // A client that is behind catches up.
        git(&dir.join("early"), &["pull", "-q", "--ff-only"]);
        assert_eq!(git(&dir.join("early"), &["rev-parse", "HEAD"]), newer);
        git(&dir.join("early"), &["fsck", "--strict"]);
        newer
    })
    .await
    .unwrap();
    commits.push(newer);

    // One big pack and one small one: nothing to merge unless forced.
    assert_eq!(pack_count(&app, "acme/compact").await, 2);
    assert!(compact(&app, "acme/compact", false).await.unwrap().is_none());
    let report = compact(&app, "acme/compact", true).await.unwrap().expect("forced");
    assert!(report.full);
    assert_eq!(sweep(&app).await.unwrap(), 2);

    let (dir, remote, head) = (root.clone(), url.clone(), commits[6].clone());
    tokio::task::spawn_blocking(move || {
        clone_and_check(&dir, &remote, "final", &head, "7");
        // A client holding old history fetches against the bitmaps of both ends.
        git(&dir.join("after-compaction"), &["pull", "-q", "--ff-only"]);
        assert_eq!(git(&dir.join("after-compaction"), &["rev-parse", "HEAD"]), head);
        git(&dir.join("after-compaction"), &["fsck", "--strict"]);
    })
    .await
    .unwrap();
    assert!(work.exists());
}

/// A push must bring, or find here already, everything its commits lead to.
/// git would not send less; this request is made by hand.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_that_leaves_objects_out_is_refused() {
    let base = start(true).await;
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("a.txt"), "a\n").unwrap();
        let commit = commit_on(&work, 1, "first");
        let tree = git(&work, &["rev-parse", "HEAD^{tree}"]);
        let blob = git(&work, &["rev-parse", "HEAD:a.txt"]);
        let push = |name: &str, objects: &[&str]| -> String {
            let list = tmp.path().join("objects");
            std::fs::write(&list, objects.join("\n") + "\n").unwrap();
            let packed = Command::new("sh").current_dir(&work).args(["-c", "git pack-objects -q --stdout < \"$0\"", list.to_str().unwrap()]).output().unwrap();
            let line = format!("{} {commit} refs/heads/{name}\0report-status\n", "0".repeat(40));
            let request = [format!("{:04x}{line}0000", line.len() + 4).as_bytes(), &packed.stdout[..]].concat();
            let body = tmp.path().join("request");
            std::fs::write(&body, request).unwrap();
            let url = format!("{base}/acme/partial.git/git-receive-pack");
            let out = Command::new("curl")
                .args(["-s", "-X", "POST", "-H", "Content-Type: application/x-git-receive-pack-request", "--data-binary", &format!("@{}", body.display()), &url])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        // The commit without its tree, then with the tree but not the file in it.
        assert!(push("one", &[&commit]).contains("ng refs/heads/one missing necessary objects"));
        assert!(push("two", &[&commit, &tree]).contains("ng refs/heads/two missing necessary objects"));
        let whole = push("three", &[&commit, &tree, &blob]);
        assert!(whole.contains("ok refs/heads/three"), "{whole}");
        // Only the whole one landed. And what is here already need not come again.
        let refs = git(tmp.path(), &["ls-remote", "--heads", &format!("{base}/acme/partial.git")]);
        assert_eq!(refs, format!("{commit}\trefs/heads/three"));
        assert!(push("four", &[]).contains("ok refs/heads/four"));
    })
    .await
    .unwrap();
}

/// Two directories trade contents, in commits that a merge and unordered
/// dates make the server look at before the commit both contents come from.
/// Each new tree then shares a file with a tree the server has yet to walk,
/// and that file must not fall through the gap.
#[tokio::test(flavor = "multi_thread")]
async fn files_shared_by_swapped_directories_are_not_lost() {
    let options = Options { auto_create: true, compact_at: usize::MAX, ..Options::default() };
    let (base, app) = start_with(options).await;
    let url = format!("{base}/acme/swap.git");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let (dir, remote) = (root.clone(), url.clone());
    let head = tokio::task::spawn_blocking(move || {
        let work = dir.join("work");
        for (name, version) in [("q", "1"), ("b", "2")] {
            std::fs::create_dir_all(work.join(name)).unwrap();
            std::fs::write(work.join(name).join("keep.txt"), "in both\n").unwrap();
            std::fs::write(work.join(name).join("v.txt"), version).unwrap();
        }
        git(&work, &["init", "-q", "-b", "main"]);
        commit_on(&work, 20, "start");
        git(&work, &["checkout", "-q", "-b", "side"]);
        std::fs::write(work.join("z.txt"), "z\n").unwrap();
        commit_on(&work, 10, "side work");
        git(&work, &["checkout", "-q", "main"]);
        std::fs::write(work.join("q/v.txt"), "2").unwrap();
        commit_on(&work, 3, "q becomes what b is");
        std::fs::write(work.join("b/v.txt"), "1").unwrap();
        commit_on(&work, 4, "b becomes what q was");
        let date = [("GIT_AUTHOR_DATE", "2020-01-11T12:00:00+00:00"), ("GIT_COMMITTER_DATE", "2020-01-11T12:00:00+00:00")];
        assert!(git_with(&work, &["merge", "-q", "--no-ff", "-m", "merge side", "side"], &date).status.success());
        git(&work, &["push", "-q", &remote, "main"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        // Walked commit by commit.
        clone_and_check(&dir, &remote, "walked", &head, "5");
        head
    })
    .await
    .unwrap();

    compact(&app, "acme/swap", true).await.unwrap().expect("a pack to rewrite");
    let (dir, remote) = (root.clone(), url.clone());
    // Served from bitmaps, which were built by the same kind of walk.
    tokio::task::spawn_blocking(move || clone_and_check(&dir, &remote, "from-bitmaps", &head, "5")).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn pushes_trigger_compaction_in_the_background() {
    let options = Options { auto_create: true, compact_at: 3, gc_grace: Duration::ZERO, ..Options::default() };
    let (base, app) = start_with(options).await;
    spawn_background(app.clone(), Duration::from_millis(50));
    let url = format!("{base}/acme/auto.git");
    let tmp = tempfile::tempdir().unwrap();

    let (dir, remote) = (tmp.path().to_path_buf(), url.clone());
    let head = tokio::task::spawn_blocking(move || {
        let work = dir.join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["remote", "add", "origin", &remote]);
        let mut head = String::new();
        for n in 1..=5 {
            std::fs::write(work.join("file.txt"), format!("version {n}\n")).unwrap();
            head = commit_on(&work, n, &format!("commit {n}"));
            git(&work, &["push", "-q", "origin", "main"]);
        }
        head
    })
    .await
    .unwrap();

    // Compaction runs behind the pushes; give it a moment to catch up.
    let mut packs = usize::MAX;
    for _ in 0..100 {
        packs = pack_count(&app, "acme/auto").await;
        if packs < 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(packs < 3, "still {packs} packs");

    let (dir, remote) = (tmp.path().to_path_buf(), url.clone());
    tokio::task::spawn_blocking(move || clone_and_check(&dir, &remote, "clone", &head, "5")).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_push_gets_bitmaps_without_being_asked() {
    // "Large" set low enough for a test: four commits bring twelve objects.
    let options = Options { auto_create: true, compact_at: usize::MAX, bitmaps_from: 10, gc_grace: Duration::ZERO, ..Options::default() };
    let (base, app) = start_with(options).await;
    spawn_background(app.clone(), Duration::from_millis(50));
    let tmp = tempfile::tempdir().unwrap();
    let has_bitmaps = |name: &'static str| {
        let app = app.clone();
        async move {
            let repo = app.meta.get_repo(name, &strata_store::RefScope::none()).await.unwrap().unwrap();
            let packs = app.packs.open_each(&repo.pool_id, &repo.packs).await.unwrap();
            (packs.len(), packs.iter().any(|pack| pack.bitmaps().is_some()))
        }
    };

    let (dir, big, small) = (tmp.path().to_path_buf(), format!("{base}/acme/big.git"), format!("{base}/acme/small.git"));
    let head = tokio::task::spawn_blocking(move || {
        let work = dir.join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "version 1\n").unwrap();
        commit_on(&work, 1, "commit 1");
        // One commit is three objects: too few to be worth it.
        git(&work, &["push", "-q", &small, "main"]);
        let mut head = String::new();
        for n in 2..=4 {
            std::fs::write(work.join("file.txt"), format!("version {n}\n")).unwrap();
            head = commit_on(&work, n, &format!("commit {n}"));
        }
        git(&work, &["push", "-q", &big, "main"]);
        head
    })
    .await
    .unwrap();

    let mut state = (0, false);
    for _ in 0..100 {
        state = has_bitmaps("acme/big").await;
        if state.1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state, (1, true), "the pushed pack was not rewritten with bitmaps");
    assert_eq!(has_bitmaps("acme/small").await, (1, false));

    let (dir, remote) = (tmp.path().to_path_buf(), format!("{base}/acme/big.git"));
    tokio::task::spawn_blocking(move || clone_and_check(&dir, &remote, "clone", &head, "4")).await.unwrap();
}

/// Calls the HTTP API and returns the status code and body.
fn api(method: &str, url: &str, token: Option<&str>) -> (u16, String) {
    api_with(method, url, token, None)
}

fn api_with(method: &str, url: &str, token: Option<&str>, body: Option<&str>) -> (u16, String) {
    let mut command = Command::new("curl");
    command.args(["-s", "-X", method, "-w", "\n%{http_code}", url]);
    if let Some(token) = token {
        command.args(["-H", &format!("Authorization: Bearer {token}")]);
    }
    if let Some(body) = body {
        command.args(["--data-binary", body]);
    }
    let out = String::from_utf8(command.output().expect("curl must be installed").stdout).unwrap();
    let (body, status) = out.rsplit_once('\n').unwrap();
    (status.parse().unwrap(), body.trim().to_string())
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

/// Without an application behind it, the admin token is the only credential.
#[tokio::test(flavor = "multi_thread")]
async fn the_admin_token_gates_access() {
    let options = Options { auto_create: true, compact_at: usize::MAX, admin_token: Some("root-secret".into()), ..Options::default() };
    let (base, _app) = start_with(options).await;
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let with = |secret: &str, repo: &str| format!("{}/{repo}.git", base.replace("http://", &format!("http://git:{secret}@")));

        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "one\n").unwrap();
        commit_on(&work, 1, "one");
        git(&work, &["push", "-q", &with("root-secret", "acme/app"), "main"]);
        git(root, &["clone", "-q", &with("root-secret", "acme/app"), "clone"]);

        // Without credentials, or with wrong ones, nothing is visible: not even whether a repository exists.
        for url in [format!("{base}/acme/app.git"), with("wrong", "acme/app"), format!("{base}/acme/nope.git")] {
            let out = git_in(root, &["ls-remote", &url]);
            let message = String::from_utf8_lossy(&out.stderr).to_string();
            assert!(!out.status.success());
            // Always an authentication problem, never "not found".
            assert!(message.contains("Authentication failed") || message.contains("could not read Username"), "{url}: {message}");
        }
        for (method, path) in [("GET", "api/repos/acme/app"), ("DELETE", "api/repos/acme/app"), ("GET", "api/events")] {
            assert_eq!(api(method, &format!("{base}/{path}"), None).0, 401, "{method} {path}");
            assert_eq!(api(method, &format!("{base}/{path}"), Some("wrong")).0, 401, "{method} {path}");
        }
        assert_eq!(api("GET", &format!("{base}/api/repos/acme/app"), Some("root-secret")).0, 200);
        // Load balancers need no secret to ask whether the node is up.
        assert_eq!(api("GET", &format!("{base}/healthz"), None), (200, "ok".to_string()));
    })
    .await
    .unwrap();
}

/// A stand-in for the application Strata serves. It knows two users and one
/// repository, which it calls `r-1` whatever name people reach it under.
mod application {
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{json, Value};

    #[derive(Default)]
    pub struct Seen {
        pub authz_questions: usize,
        pub pushes: Vec<Value>,
        /// Whose SSH keys the application knows: the key, as an `authorized_keys` line, and the user.
        pub ssh_keys: Vec<(String, &'static str)>,
    }

    type Shared = Arc<Mutex<Seen>>;

    fn from_strata(headers: &HeaderMap) -> bool {
        headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer hook-secret")
    }

    async fn authz(State(seen): State<Shared>, headers: HeaderMap, Json(q): Json<Value>) -> Json<Value> {
        seen.lock().unwrap().authz_questions += 1;
        if !from_strata(&headers) {
            return Json(json!({ "allow": false, "status": 403, "message": "who is asking?" }));
        }
        let by_key = q["ssh_key"].as_str().map(|key| seen.lock().unwrap().ssh_keys.iter().find(|(known, _)| known == key).map(|(_, user)| *user));
        if q["access"] == "identify" {
            return Json(json!({ "allow": matches!(by_key, Some(Some(_))) }));
        }
        // "Basic " + base64("git:<token>")
        let user = match (q["authorization"].as_str(), by_key) {
            (_, Some(Some(user))) => user,
            (_, Some(None)) => return Json(json!({ "allow": false, "status": 401 })),
            (Some("Basic Z2l0OmFsaWNlLXRva2Vu"), _) => "alice",
            (Some("Basic Z2l0OmJvYi10b2tlbg=="), _) => "bob",
            (Some(_), _) => return Json(json!({ "allow": false, "status": 401 })),
            (None, _) => return Json(json!({ "allow": false })),
        };
        // The repository was renamed once; both names lead to it.
        if !matches!(q["path"].as_str(), Some("acme/app" | "acme/renamed")) {
            return Json(json!({ "allow": false, "status": 404, "message": "no such repository" }));
        }
        if q["access"] == "write" && user != "alice" {
            return Json(json!({ "allow": false, "message": "bob may only read" }));
        }
        Json(json!({ "allow": true, "repo": "r-1", "actor": user }))
    }

    /// `main` is protected: it only moves forward.
    async fn pre_receive(State(seen): State<Shared>, headers: HeaderMap, Json(q): Json<Value>) -> Json<Value> {
        assert!(from_strata(&headers));
        seen.lock().unwrap().pushes.push(q.clone());
        let protected = q["updates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["ref"] == "refs/heads/main" && !matches!(u["change"].as_str(), Some("create" | "fast-forward")));
        match protected {
            true => Json(json!({ "allow": false, "message": "main is protected" })),
            false => Json(json!({ "allow": true })),
        }
    }

    pub async fn start() -> (String, Shared) {
        let seen = Shared::default();
        let router = Router::new().route("/authz", post(authz)).route("/pre-receive", post(pre_receive)).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://{addr}"), seen)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_application_decides_who_may_do_what() {
    let (application, seen) = application::start().await;
    let options = Options {
        compact_at: usize::MAX,
        admin_token: Some("root-secret".into()),
        authz_url: Some(format!("{application}/authz")),
        authz_cache: Duration::ZERO,
        pre_receive_url: Some(format!("{application}/pre-receive")),
        hook_secret: Some("hook-secret".into()),
        ..Options::default()
    };
    let (base, app) = start_with(options).await;
    // The application creates repositories, under ids of its own.
    app.meta.create_repo("r-1").await.unwrap();

    let strata = base.clone();
    let (first, second) = tokio::task::spawn_blocking(move || {
        let base = strata;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let with = |token: &str, repo: &str| format!("{}/{repo}.git", base.replace("http://", &format!("http://git:{token}@")));
        let stderr = |out: std::process::Output| String::from_utf8_lossy(&out.stderr).to_string();

        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "one\n").unwrap();
        let first = commit_on(&work, 1, "one");

        // Bob reads, Alice writes, strangers get nothing, and the admin
        // token is no way around the application.
        let refused = stderr(git_in(&work, &["push", &with("bob-token", "acme/app"), "main"]));
        assert!(refused.contains("403"), "{refused}");
        assert!(!git_in(&work, &["push", "-q", &with("root-secret", "acme/app"), "main"]).status.success());
        assert!(!git_in(&work, &["push", "-q", &format!("{base}/acme/app.git"), "main"]).status.success());
        git(&work, &["push", "-q", &with("alice-token", "acme/app"), "main"]);
        git(root, &["clone", "-q", &with("bob-token", "acme/app"), "clone"]);
        assert!(!git_in(root, &["ls-remote", &with("mallory-token", "acme/app")]).status.success());
        let missing = stderr(git_in(root, &["ls-remote", &with("alice-token", "acme/other")]));
        assert!(missing.contains("not found"), "{missing}");

        // The new name reaches the same repository; nothing moved inside Strata.
        std::fs::write(work.join("file.txt"), "two\n").unwrap();
        let second = commit_on(&work, 2, "two");
        git(&work, &["push", "-q", &with("alice-token", "acme/renamed"), "main", "main:refs/heads/topic"]);
        git(&root.join("clone"), &["pull", "-q", "--ff-only"]);
        assert_eq!(git(&root.join("clone"), &["rev-parse", "HEAD"]), second);

        // The application's rules: main only moves forward, other branches are free.
        git(&work, &["reset", "-q", "--hard", &first]);
        let refused = stderr(git_in(&work, &["push", "--force", &with("alice-token", "acme/app"), "main"]));
        assert!(refused.contains("main is protected"), "{refused}");
        let refused = stderr(git_in(&work, &["push", &with("alice-token", "acme/app"), ":main"]));
        assert!(refused.contains("main is protected"), "{refused}");
        git(&work, &["push", "-q", "--force", &with("alice-token", "acme/app"), "main:topic"]);
        git(&work, &["push", "-q", &with("alice-token", "acme/app"), ":topic"]);
        assert!(git(root, &["ls-remote", &with("bob-token", "acme/app"), "refs/heads/main"]).starts_with(&second));
        (first, second)
    })
    .await
    .unwrap();

    // Everything landed in the repository the application named, and only there.
    let repo = app.meta.get_repo("r-1", &strata_store::RefScope::All).await.unwrap().unwrap();
    assert_eq!(repo.refs.keys().collect::<Vec<_>>(), ["refs/heads/main"]);
    assert_eq!(app.meta.list_repos().await.unwrap(), ["r-1"]);

    // The rules were asked about every push that could have landed, with what it would do.
    let asked: Vec<(String, String)> = seen
        .lock()
        .unwrap()
        .pushes
        .iter()
        .map(|q| {
            assert_eq!((q["repo"].as_str(), q["actor"].as_str()), (Some("r-1"), Some("alice")));
            let update = q["updates"].as_array().unwrap().last().unwrap();
            (update["ref"].as_str().unwrap().to_string(), update["change"].as_str().unwrap().to_string())
        })
        .collect();
    let expected = [
        ("refs/heads/main", "create"),
        ("refs/heads/topic", "create"),
        ("refs/heads/main", "forced"),
        ("refs/heads/main", "delete"),
        ("refs/heads/topic", "forced"),
        ("refs/heads/topic", "delete"),
    ];
    assert_eq!(asked, expected.map(|(name, change)| (name.to_string(), change.to_string())));
    // Each question also showed the commits the update would bring to its ref, newest first.
    {
        let seen = seen.lock().unwrap();
        let created = &seen.pushes[0]["updates"][0];
        assert_eq!(created["commits"][0]["oid"], created["new"]);
        assert_eq!(created["commits"][0]["author"]["name"], "Test");
        assert_eq!(created["commits"][0]["signature"], serde_json::Value::Null);
        assert_eq!(created["commits_truncated"], false);
        let deleted = &seen.pushes[3]["updates"][0];
        assert_eq!(deleted["commits"], serde_json::json!([]));
    }

    // The feed has the pushes that did land, in order, and nothing else.
    let feed = tokio::task::spawn_blocking(move || {
        let (status, page) = api("GET", &format!("{base}/api/events?limit=3"), Some("root-secret"));
        assert_eq!(status, 200);
        let page = json(&page);
        let mut events = page["events"].as_array().unwrap().clone();
        assert_eq!(events.len(), 3);
        let next = page["next"].as_str().unwrap().to_string();
        let (_, rest) = api("GET", &format!("{base}/api/events?after={next}"), Some("root-secret"));
        let rest = json(&rest);
        events.extend(rest["events"].as_array().unwrap().iter().cloned());
        // At the end of the feed: the same position, to poll again from.
        let end = rest["next"].as_str().unwrap().to_string();
        let (_, empty) = api("GET", &format!("{base}/api/events?after={end}"), Some("root-secret"));
        assert_eq!(json(&empty), serde_json::json!({ "events": [], "next": end }));
        assert_eq!(api("GET", &format!("{base}/api/events?after=nonsense"), Some("root-secret")).0, 400);
        events
    })
    .await
    .unwrap();
    let summary: Vec<Vec<(String, String)>> = feed
        .iter()
        .map(|e| {
            assert_eq!((e["repo"].as_str(), e["event"]["type"].as_str(), e["event"]["actor"].as_str()), (Some("r-1"), Some("push"), Some("alice")));
            assert!(e["time"].as_i64().unwrap() > 1_600_000_000);
            let updates = e["event"]["updates"].as_array().unwrap();
            updates.iter().map(|u| (u["ref"].as_str().unwrap().to_string(), u["change"].as_str().unwrap().to_string())).collect()
        })
        .collect();
    let pair = |name: &str, change: &str| (name.to_string(), change.to_string());
    assert_eq!(
        summary,
        [
            vec![pair("refs/heads/main", "create")],
            vec![pair("refs/heads/main", "fast-forward"), pair("refs/heads/topic", "create")],
            vec![pair("refs/heads/topic", "forced")],
            vec![pair("refs/heads/topic", "delete")],
        ]
    );
    let moved = &feed[1]["event"]["updates"][0];
    assert_eq!((moved["old"].as_str(), moved["new"].as_str()), (Some(first.as_str()), Some(second.as_str())));
    assert!(feed[0]["event"]["updates"][0]["old"].is_null());
    assert!(seen.lock().unwrap().authz_questions > 10);
}

/// With the application out of reach, nobody gets in. Answers are
/// remembered briefly, so that one git command is one question, not several.
/// A key pair made by `ssh-keygen`, and the public half as the server will see it.
fn ssh_key(dir: &Path, name: &str, kind: &str) -> String {
    let path = dir.join(name);
    let made = Command::new("ssh-keygen").args(["-q", "-t", kind, "-N", "", "-C", "", "-f"]).arg(&path).output().expect("ssh-keygen must be installed");
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    std::fs::read_to_string(path.with_extension("pub")).unwrap().trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn git_is_served_over_ssh_to_those_whose_key_is_known() {
    let (application, seen) = application::start().await;
    let options = Options {
        compact_at: usize::MAX,
        admin_token: Some("root-secret".into()),
        authz_url: Some(format!("{application}/authz")),
        authz_cache: Duration::ZERO,
        pre_receive_url: Some(format!("{application}/pre-receive")),
        hook_secret: Some("hook-secret".into()),
        ..Options::default()
    };
    let (base, app) = start_with(options).await;
    app.meta.create_repo("r-1").await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    // Made on first use, and the same one from then on.
    let host_key = strata_server::host_key(&root.join("host_key")).unwrap();
    assert_eq!(strata_server::host_key(&root.join("host_key")).unwrap().public_key(), host_key.public_key());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(strata_server::serve_ssh(app.clone(), listener, host_key, std::future::pending()));

    seen.lock().unwrap().ssh_keys = vec![(ssh_key(&root, "alice", "ed25519"), "alice"), (ssh_key(&root, "bob", "rsa"), "bob")];
    ssh_key(&root, "mallory", "ed25519");

    let pushes = tokio::task::spawn_blocking(move || {
        let ssh = |keys: &[&str]| {
            let keys: String = keys.iter().map(|key| format!(" -i {}", root.join(key).display())).collect();
            format!("ssh -F /dev/null{keys} -o IdentitiesOnly=yes -o IdentityAgent=none -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes")
        };
        let url = format!("ssh://git@127.0.0.1:{port}/acme/app.git");
        let as_user = |dir: &Path, keys: &[&str], args: &[&str]| git_with(dir, args, &[("GIT_SSH_COMMAND", &ssh(keys))]);
        #[track_caller]
        fn ok(out: std::process::Output) -> String {
            assert!(out.status.success(), "stdout: {}\nstderr: {}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        #[track_caller]
        fn refused(out: std::process::Output) -> String {
            assert!(!out.status.success());
            String::from_utf8_lossy(&out.stderr).to_string()
        }

        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "one\n").unwrap();
        // Larger than one SSH packet many times over, and not to be shrunk.
        std::fs::write(work.join("large.bin"), incompressible(700 << 10)).unwrap();
        let first = commit_on(&work, 1, "one");

        // A key nobody knows gets nowhere; Bob's reads and Alice's writes.
        let denied = refused(as_user(&work, &["mallory"], &["push", &url, "main"]));
        assert!(denied.contains("Permission denied"), "{denied}");
        let denied = refused(as_user(&work, &["bob"], &["push", &url, "main"]));
        assert!(denied.contains("bob may only read"), "{denied}");
        ok(as_user(&work, &["alice"], &["push", "-q", &url, "main"]));
        let missing = refused(as_user(&work, &["alice"], &["ls-remote", &format!("ssh://git@127.0.0.1:{port}/acme/other.git")]));
        assert!(missing.contains("no such repository"), "{missing}");

        // A client with several keys is let in with the one that is known.
        ok(as_user(&root, &["mallory", "bob"], &["clone", "-q", &url, "clone"]));
        let clone = root.join("clone");
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), first);
        git(&clone, &["fsck", "--strict"]);

        // What came in over SSH goes out over HTTP, and the other way round.
        let http = format!("{}/acme/app.git", base.replace("http://", "http://git:alice-token@"));
        assert!(git(&root, &["ls-remote", &http, "refs/heads/main"]).starts_with(&first));
        std::fs::write(work.join("file.txt"), "two\n").unwrap();
        let second = commit_on(&work, 2, "two");
        git(&work, &["push", "-q", &http, "main"]);
        ok(as_user(&clone, &["bob"], &["pull", "-q", "--ff-only"]));
        assert_eq!(git(&clone, &["rev-parse", "HEAD"]), second);

        // Several refs at once, a push that only deletes, and one that brings nothing new.
        std::fs::write(work.join("file.txt"), "three\n").unwrap();
        let third = commit_on(&work, 3, "three");
        ok(as_user(&work, &["alice"], &["push", "-q", &url, "main", "main:refs/heads/topic", "main:refs/tags/v1"]));
        ok(as_user(&work, &["alice"], &["push", "-q", &url, ":topic"]));
        ok(as_user(&work, &["alice"], &["push", "-q", &url, &format!("{second}:refs/heads/older")]));
        let listed = ok(as_user(&root, &["bob"], &["ls-remote", &url]));
        assert!(listed.contains(&format!("{third}\trefs/heads/main")) && listed.contains(&format!("{second}\trefs/heads/older")), "{listed}");
        assert!(listed.contains("refs/tags/v1") && !listed.contains("topic"), "{listed}");

        // The application's push rules hold here too.
        git(&work, &["reset", "-q", "--hard", &first]);
        let denied = refused(as_user(&work, &["alice"], &["push", "--force", &url, "main"]));
        assert!(denied.contains("main is protected"), "{denied}");

        // Shallow and partial clones.
        ok(as_user(&root, &["bob"], &["clone", "-q", "--depth", "1", &url, "shallow"]));
        assert_eq!(commit_count(&root.join("shallow")), "1");
        ok(as_user(&root, &["bob"], &["clone", "-q", "--filter=blob:none", "--no-checkout", &url, "partial"]));
        assert!(missing_objects(&root.join("partial")) > 0);

        // The older protocol is turned down in words, as is asking for a shell or another program.
        let old = refused(as_user(&root, &["bob"], &["-c", "protocol.version=0", "ls-remote", &url]));
        assert!(old.contains("protocol version 2"), "{old}");
        let run = |command: &[&str]| {
            let mut parts = ssh(&["bob"]);
            parts.push_str(&format!(" -p {port} git@127.0.0.1"));
            let mut words = parts.split(' ');
            Command::new(words.next().unwrap()).args(words).args(command).output().unwrap()
        };
        assert!(String::from_utf8_lossy(&run(&[]).stderr).contains("no shell"));
        assert!(String::from_utf8_lossy(&run(&["cat", "/etc/passwd"]).stderr).contains("only git-upload-pack"));

        let metrics = api("GET", &format!("{base}/metrics"), Some("root-secret")).1;
        assert!(metrics.contains("strata_ssh_commands_total{result=\"ok\",service=\"receive-pack\"}"), "{metrics}");
        assert!(metrics.contains("strata_ssh_commands_total{result=\"refused\",service=\"other\"} 2"), "{metrics}");
        (first, second, third)
    })
    .await
    .unwrap();

    // Pushes over SSH are told to the application like any other, with who made them.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.pushes[0]["actor"], "alice");
    assert_eq!(seen.pushes[0]["updates"][0]["new"], pushes.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_application_means_no() {
    let (application, seen) = application::start().await;
    let options = |url: String, cache: u64| Options {
        compact_at: usize::MAX,
        admin_token: Some("root-secret".into()),
        authz_url: Some(url),
        authz_cache: Duration::from_secs(cache),
        hook_secret: Some("hook-secret".into()),
        ..Options::default()
    };
    // A port nothing listens on.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let (down, _) = start_with(options(format!("http://{closed}/authz"), 0)).await;
    let (cached, app) = start_with(options(format!("{application}/authz"), 60)).await;
    app.meta.create_repo("r-1").await.unwrap();

    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let alice = |base: &str| format!("{}/acme/app.git", base.replace("http://", "http://git:alice-token@"));
        let out = git_in(tmp.path(), &["ls-remote", &alice(&down)]);
        assert!(String::from_utf8_lossy(&out.stderr).contains("503"), "{}", String::from_utf8_lossy(&out.stderr));

        git(tmp.path(), &["ls-remote", &alice(&cached)]);
        let asked = seen.lock().unwrap().authz_questions;
        git(tmp.path(), &["ls-remote", &alice(&cached)]);
        git(tmp.path(), &["ls-remote", &alice(&cached)]);
        assert_eq!(seen.lock().unwrap().authz_questions, asked, "remembered answers are not asked for again");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn repositories_are_managed_through_the_api() {
    let options = Options { compact_at: usize::MAX, gc_grace: Duration::ZERO, orphan_grace: Duration::ZERO, ..Options::default() };
    let (base, app) = start_with(options).await;
    let server = base.clone();
    tokio::task::spawn_blocking(move || {
        let base = server;
        let tmp = tempfile::tempdir().unwrap();
        let repo = format!("{base}/api/repos/r-42");
        assert_eq!(api("GET", &repo, None).0, 404);
        assert_eq!(api("DELETE", &repo, None).0, 404);
        assert_eq!(api_with("PUT", &format!("{repo}/head"), None, Some("refs/heads/trunk")).0, 404);
        assert_eq!(api("PUT", &repo, None).0, 201);
        assert_eq!(api("PUT", &repo, None).0, 409);
        let empty = serde_json::json!({ "repo": "r-42", "head": "refs/heads/main", "refs": 0, "packs": 0, "objects": 0, "bytes": 0 });
        assert_eq!(json(&api("GET", &repo, None).1), empty);

        let work = tmp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "trunk"]);
        std::fs::write(work.join("file.txt"), "one\n").unwrap();
        commit_on(&work, 1, "one");
        git(&work, &["push", "-q", &format!("{base}/r-42.git"), "trunk", "trunk:refs/heads/other"]);
        let info = json(&api("GET", &repo, None).1);
        assert_eq!((info["refs"].as_u64(), info["packs"].as_u64(), info["objects"].as_u64()), (Some(2), Some(1), Some(3)));
        assert!(info["bytes"].as_u64().unwrap() > 100);

        // The default branch is the application's to choose.
        assert_eq!(api_with("PUT", &format!("{repo}/head"), None, Some("not-a-ref")).0, 400);
        assert_eq!(api_with("PUT", &format!("{repo}/head"), None, Some("refs/heads/other\n")).0, 200);
        assert_eq!(json(&api("GET", &repo, None).1)["head"], "refs/heads/other");
        git(tmp.path(), &["clone", "-q", &format!("{base}/r-42.git"), "clone"]);
        assert_eq!(git(&tmp.path().join("clone"), &["symbolic-ref", "--short", "HEAD"]), "other");

        assert_eq!(api("DELETE", &repo, None).0, 200);
        assert_eq!(api("GET", &repo, None).0, 404);
        assert!(!git_in(tmp.path(), &["ls-remote", &format!("{base}/r-42.git")]).status.success());
    })
    .await
    .unwrap();

    // The deleted repository's pack goes after the grace period (none, here)...
    assert!(app.meta.list_repos().await.unwrap().is_empty());
    assert_eq!(sweep(&app).await.unwrap(), 1);
    // ...and so does what a push still in flight at that moment leaves behind.
    let mut stray = app.packs.upload("a-pool-nobody-has", "0000stray").await.unwrap();
    tokio::task::spawn_blocking(move || {
        std::io::Write::write_all(&mut stray, b"not really a pack").unwrap();
        stray.finish().unwrap();
    })
    .await
    .unwrap();
    assert_eq!(sweep_orphans(&app).await.unwrap(), 1);
    assert_eq!(sweep_orphans(&app).await.unwrap(), 0);
}

/// What an application needs to show code: refs, history, listings, files
/// and what a commit changed.
#[tokio::test(flavor = "multi_thread")]
async fn content_can_be_read_through_the_api() {
    let base = start(false).await;
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let repo = format!("{base}/api/repos/r-7");
        let get = |path: &str| api("GET", &format!("{repo}/{path}"), None);
        assert_eq!(api("PUT", &repo, None).0, 201);
        // Nothing pushed yet: no refs, and HEAD names nothing.
        assert_eq!(json(&get("refs").1), serde_json::json!({ "head": "refs/heads/main", "refs": [] }));
        assert_eq!(get("commits").0, 404);

        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("src/deep")).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("README.md"), "# hello\n").unwrap();
        std::fs::write(work.join("src/deep/a.txt"), "one\n").unwrap();
        std::fs::write(work.join("src/b.txt"), "b\n").unwrap();
        let first = commit_on(&work, 1, "first");
        std::fs::write(work.join("src/deep/a.txt"), "one\ntwo\n").unwrap();
        std::fs::remove_file(work.join("src/b.txt")).unwrap();
        std::fs::write(work.join("new file.txt"), "new\n").unwrap();
        let second = commit_on(&work, 2, "second\n\nwith a body");
        git(&work, &["tag", "-a", "-m", "release", "v1", &first]);
        git(&work, &["push", "-q", &format!("{base}/r-7.git"), "main", "main:refs/heads/topic/x", "v1"]);

        let refs = json(&get("refs").1);
        let names: Vec<&str> = refs["refs"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["refs/heads/main", "refs/heads/topic/x", "refs/tags/v1"]);
        assert_eq!(refs["refs"][0]["oid"], second.as_str());
        // Only the refs under a prefix, for a repository with too many to list.
        let listed = |path: &str| -> Vec<String> {
            json(&get(path).1)["refs"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(listed("refs?prefix=refs/heads/topic/"), ["refs/heads/topic/x"]);
        assert_eq!(listed("refs?prefix=refs/tags/"), ["refs/tags/v1"]);
        assert_eq!(listed("refs?prefix=refs/heads/m"), ["refs/heads/main"]);
        assert!(listed("refs?prefix=refs/nothing/").is_empty());
        // git asks the same way: about the refs it is after, or about all of them.
        let remote = format!("{base}/r-7.git");
        let lines = |args: &[&str]| git(tmp.path(), &[&["ls-remote", remote.as_str()], args].concat()).lines().map(|l| l.split('\t').nth(1).unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(lines(&["refs/heads/topic/x"]), ["refs/heads/topic/x"]);
        assert_eq!(lines(&["v1"]), ["refs/tags/v1"]);
        assert_eq!(lines(&["HEAD"]), ["HEAD"]);
        assert_eq!(lines(&[]), ["HEAD", "refs/heads/main", "refs/heads/topic/x", "refs/tags/v1", "refs/tags/v1^{}"]);

        // History, newest first, a page at a time.
        let page = json(&get("commits?limit=1").1);
        assert_eq!(page["commits"].as_array().unwrap().len(), 1);
        assert_eq!(page["commits"][0]["oid"], second.as_str());
        assert_eq!(page["commits"][0]["message"], "second\n\nwith a body\n");
        assert_eq!(page["commits"][0]["parents"], serde_json::json!([first]));
        assert_eq!(page["commits"][0]["author"]["name"], "Test");
        assert_eq!(page["commits"][0]["author"]["email"], "test@example.com");
        assert_eq!(page["next"], 1);
        let page = json(&get("commits?limit=1&skip=1&rev=topic%2Fx").1);
        assert_eq!(page["commits"][0]["oid"], first.as_str());
        assert_eq!(page["next"], serde_json::Value::Null);
        // An annotated tag names the commit it points at.
        assert_eq!(json(&get("commits?rev=v1").1)["commits"][0]["oid"], first.as_str());
        assert_eq!(get("commits?rev=nope").0, 404);
        assert_eq!(get("commits?limit=0").0, 400);

        let root = json(&get("tree").1);
        let listing: Vec<(&str, &str)> =
            root["entries"].as_array().unwrap().iter().map(|e| (e["name"].as_str().unwrap(), e["type"].as_str().unwrap())).collect();
        assert_eq!(listing, [("README.md", "blob"), ("new file.txt", "blob"), ("src", "tree")]);
        assert_eq!(root["commit"], second.as_str());
        let deep = json(&get(&format!("tree?rev={first}&path=src")).1);
        assert_eq!(deep["entries"].as_array().unwrap().len(), 2);
        assert_eq!(get("tree?path=README.md").0, 400);
        assert_eq!(get("tree?path=missing").0, 404);

        assert_eq!(get("blob?path=src/deep/a.txt").1, "one\ntwo");
        assert_eq!(get(&format!("blob?rev={first}&path=src/deep/a.txt")).1, "one");
        assert_eq!(get("blob?path=new%20file.txt").1, "new");
        assert_eq!(get("blob?path=src").0, 400);
        assert_eq!(get("blob?path=src/b.txt").0, 404);
        let oid = root["entries"][0]["oid"].as_str().unwrap();
        assert_eq!(get(&format!("blob?oid={oid}")).1, "# hello");

        let shown = json(&get("commit").1);
        assert_eq!(shown["oid"], second.as_str());
        assert_eq!(shown["truncated"], false);
        let files: Vec<(&str, &str)> =
            shown["files"].as_array().unwrap().iter().map(|f| (f["path"].as_str().unwrap(), f["status"].as_str().unwrap())).collect();
        assert_eq!(files, [("new file.txt", "added"), ("src/b.txt", "deleted"), ("src/deep/a.txt", "modified")]);
        assert_eq!(shown["files"][1]["new"], serde_json::Value::Null);
        // The first commit added everything it holds.
        let shown = json(&get(&format!("commit?rev={first}")).1);
        assert_eq!(shown["files"].as_array().unwrap().len(), 3);
        assert!(shown["files"].as_array().unwrap().iter().all(|f| f["status"] == "added"));

        assert_eq!(api("POST", &format!("{repo}/refs"), None).0, 405);
        // What an orchestrator asks before sending requests here.
        assert_eq!(api("GET", &format!("{base}/readyz"), None), (200, "ready".to_string()));
        assert_eq!(api("GET", &format!("{base}/api/repos/r-8/refs"), None).0, 404);
    })
    .await
    .unwrap();
}

/// The commit git itself puts each line of a file down to.
fn git_blame(dir: &Path, path: &str) -> Vec<String> {
    git(dir, &["blame", "-l", "-s", "--root", path]).lines().map(|line| line[..40].to_string()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn code_can_be_blamed_searched_and_followed() {
    let base = start(false).await;
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let repo = format!("{base}/api/repos/r-code");
        let get = |path: &str| api("GET", &format!("{repo}/{path}"), None);
        assert_eq!(api("PUT", &repo, None).0, 201);

        // A file that grows, has lines changed and removed on two branches, and is merged.
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("src")).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        let write = |lines: &[String]| std::fs::write(work.join("src/lib.rs"), lines.join("\n") + "\n").unwrap();
        let mut lines: Vec<String> = (1..=40).map(|n| format!("fn line_{n}() {{}}")).collect();
        write(&lines);
        std::fs::write(work.join("README.md"), "# Code\n\nSearch me: Needle in a haystack.\n").unwrap();
        std::fs::write(work.join("blob.bin"), [0u8, 1, 2, b'n', b'e', b'e', b'd', b'l', b'e']).unwrap();
        let first = commit_on(&work, 1, "first");
        lines[4] = "fn line_5_changed() {}".into();
        lines.insert(10, "fn inserted_by_second() { needle(); }".into());
        write(&lines);
        let second = commit_on(&work, 2, "second");
        git(&work, &["checkout", "-q", "-b", "side"]);
        lines.truncate(35);
        lines.push("fn added_on_side() {}".into());
        write(&lines);
        commit_on(&work, 3, "on the side");
        git(&work, &["checkout", "-q", "main"]);
        let mut ours: Vec<String> = git(&work, &["show", "HEAD:src/lib.rs"]).lines().map(str::to_string).collect();
        ours[0] = "fn line_1_on_main() {}".into();
        ours.remove(20);
        write(&ours);
        std::fs::write(work.join("other.txt"), "untouched by the rest\n").unwrap();
        let fourth = commit_on(&work, 4, "on main");
        git(&work, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
        std::fs::write(work.join("src/lib.rs"), std::fs::read_to_string(work.join("src/lib.rs")).unwrap() + "// the end").unwrap();
        commit_on(&work, 5, "no newline at the end");
        git(&work, &["push", "-q", &format!("{base}/r-code.git"), "main", "side"]);

        // Every line is put down to the commit git puts it down to.
        let blame = |rev: &str| {
            let answer = json(&get(&format!("blame?rev={rev}&path=src/lib.rs")).1);
            let mut by_line = Vec::new();
            for hunk in answer["hunks"].as_array().unwrap() {
                assert_eq!(hunk["start"].as_u64().unwrap() as usize, by_line.len() + 1);
                assert!(answer["commits"][hunk["oid"].as_str().unwrap()]["message"].is_string());
                by_line.extend(std::iter::repeat_n(hunk["oid"].as_str().unwrap().to_string(), hunk["lines"].as_u64().unwrap() as usize));
            }
            assert_eq!(answer["lines"].as_u64().unwrap() as usize, by_line.len());
            assert_eq!(answer["truncated"], false);
            by_line
        };
        assert_eq!(blame("main"), git_blame(&work, "src/lib.rs"));
        git(&work, &["checkout", "-q", "side"]);
        assert_eq!(blame("side"), git_blame(&work, "src/lib.rs"));
        git(&work, &["checkout", "-q", &second]);
        let at_second = blame(&second);
        assert_eq!(at_second, git_blame(&work, "src/lib.rs"));
        assert_eq!(at_second.iter().filter(|oid| **oid == second).count(), 2);
        assert_eq!(get("blame?path=src").0, 404);
        assert_eq!(get("blame?path=nope.rs").0, 404);
        assert_eq!(get("blame?path=blob.bin").0, 422);

        // The history of one path: the commits that changed it, and no merge that only took a side.
        let touched = |path: &str| -> Vec<String> {
            json(&get(&format!("commits?path={path}&limit=100")).1)["commits"].as_array().unwrap().iter().map(|c| c["message"].as_str().unwrap().trim().to_string()).collect()
        };
        assert_eq!(touched("src/lib.rs"), ["no newline at the end", "merge side", "on main", "on the side", "second", "first"]);
        assert_eq!(touched("other.txt"), ["on main"]);
        assert_eq!(touched("src"), touched("src/lib.rs"));
        assert_eq!(touched("README.md"), ["first"]);
        assert!(touched("nope").is_empty());
        let page = json(&get("commits?path=src/lib.rs&limit=2&skip=2").1);
        assert_eq!(page["commits"][0]["oid"], fourth.as_str());
        assert_eq!(page["next"], 4);

        // Text is found wherever it stands, whatever its case; binary files are passed over.
        let found = json(&get("search?q=needle").1);
        let hits: Vec<(String, u64)> = found["files"].as_array().unwrap().iter().flat_map(|f| f["matches"].as_array().unwrap().iter().map(|m| (f["path"].as_str().unwrap().to_string(), m["line"].as_u64().unwrap())).collect::<Vec<_>>()).collect();
        assert_eq!(hits, [("README.md".to_string(), 3), ("src/lib.rs".to_string(), 11)]);
        assert_eq!(found["files"][0]["matches"][0]["text"], "Search me: Needle in a haystack.");
        assert_eq!(found["truncated"], false);
        assert_eq!(json(&get("search?q=needle&case=1").1)["matches"], 1);
        assert_eq!(json(&get("search?q=needle&path=src").1)["matches"], 1);
        assert_eq!(json(&get(&format!("search?q=needle&rev={first}")).1)["matches"], 1);
        // A pattern, a limit, and files found by their names.
        let pattern = json(&get("search?regex=1&q=fn%20line_%5B0-9%5D%2B%5C(").1);
        assert_eq!(pattern["files"][0]["matches"].as_array().unwrap().len(), 20);
        assert_eq!(pattern["files"][0]["more"], true);
        let limited = json(&get("search?q=fn&limit=3").1);
        assert_eq!((limited["matches"].as_u64(), limited["truncated"].as_bool()), (Some(3), Some(true)));
        assert_eq!(json(&get("search?q=lib.rs").1)["paths"], serde_json::json!(["src/lib.rs"]));
        assert_eq!(json(&get("search?q=a.c").1)["matches"], 0);
        assert_eq!(json(&get("search?regex=1&q=%5Efn%20line_7%5C(%5C)%20%5C%7B%5C%7D%24").1)["matches"], 1);
        assert_eq!(get("search?q=").0, 400);
        assert_eq!(get("search?regex=1&q=(").0, 400);
        assert_eq!(get("search?q=x&path=README.md").0, 400);
    })
    .await
    .unwrap();
}

fn curl(args: &[&str]) -> (u16, String) {
    let out = Command::new("curl").args(["-s", "-w", "\n%{http_code}"]).args(args).output().expect("curl must be installed");
    let out = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, status) = out.rsplit_once('\n').unwrap();
    (status.parse().unwrap(), body.to_string())
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// Git LFS: large files go beside the packs, to those who may push, and come back to those who may fetch.
#[tokio::test(flavor = "multi_thread")]
async fn large_files_are_kept_beside_the_repository() {
    let (application, _seen) = application::start().await;
    let options = Options {
        compact_at: usize::MAX,
        admin_token: Some("root-secret".into()),
        authz_url: Some(format!("{application}/authz")),
        authz_cache: Duration::ZERO,
        hook_secret: Some("hook-secret".into()),
        ..Options::default()
    };
    let (base, app) = start_with(options).await;
    app.meta.create_repo("r-1").await.unwrap();
    app.meta.create_repo("r-2").await.unwrap();

    let served = base.clone();
    tokio::task::spawn_blocking(move || {
        let base = served;
        let tmp = tempfile::tempdir().unwrap();
        let lfs = format!("{base}/acme/app.git/info/lfs");
        let data = incompressible(3 << 20);
        let oid = sha256_hex(&data);
        let file = tmp.path().join("large.bin");
        std::fs::write(&file, &data).unwrap();
        let batch = |user: &str, operation: &str, oid: &str| {
            let body = serde_json::json!({ "operation": operation, "transfers": ["basic"], "objects": [{ "oid": oid, "size": data.len() }], "hash_algo": "sha256" }).to_string();
            let (status, text) = curl(&["-X", "POST", "-u", &format!("git:{user}-token"), "-H", "Content-Type: application/vnd.git-lfs+json", "-d", &body, &format!("{lfs}/objects/batch")]);
            (status, json(&text))
        };

        // Bob may only read: he is told so in the way LFS clients read.
        let (status, refused) = batch("bob", "upload", &oid);
        assert_eq!((status, refused["message"].as_str()), (403, Some("bob may only read")));
        assert_eq!(curl(&["-X", "POST", "-d", "{}", &format!("{lfs}/objects/batch")]).0, 401);
        assert_eq!(batch("alice", "download", &oid).1["objects"][0]["error"]["code"], 404);

        // Alice is told where to send it, and what to show there.
        let (status, answer) = batch("alice", "upload", &oid);
        assert_eq!(status, 200);
        let upload = &answer["objects"][0]["actions"]["upload"];
        let href = upload["href"].as_str().unwrap().to_string();
        assert_eq!(href, format!("{lfs}/objects/{oid}"));
        let authorization = format!("Authorization: {}", upload["header"]["Authorization"].as_str().unwrap());
        // Not without that, not as Bob, and not content that is something else.
        let put = |header: &str, path: &Path, href: &str| curl(&["-X", "PUT", "-H", header, "-H", "Content-Type: application/octet-stream", "--data-binary", &format!("@{}", path.display()), href]).0;
        assert_eq!(put("X-Nothing: 1", &file, &href), 401);
        let other = tmp.path().join("other.bin");
        std::fs::write(&other, b"something else").unwrap();
        assert_eq!(put(&authorization, &other, &href), 422);
        assert_eq!(batch("alice", "download", &oid).1["objects"][0]["error"]["code"], 404);
        assert_eq!(put(&authorization, &file, &href), 200);
        assert_eq!(put(&authorization, &file, &href), 200);
        // Now there: nothing more to send, and Bob can fetch it.
        assert!(batch("alice", "upload", &oid).1["objects"][0]["actions"].is_null());
        let (_, answer) = batch("bob", "download", &oid);
        let download = &answer["objects"][0]["actions"]["download"];
        let fetched = tmp.path().join("fetched.bin");
        let got = curl(&["-H", &format!("Authorization: {}", download["header"]["Authorization"].as_str().unwrap()), "-o", &fetched.display().to_string(), download["href"].as_str().unwrap()]);
        assert_eq!(got.0, 200);
        assert_eq!(sha256_hex(&std::fs::read(&fetched).unwrap()), oid);
        assert_eq!(curl(&[&href]).0, 401);
        // An empty file is a file too.
        let empty = tmp.path().join("empty");
        std::fs::write(&empty, b"").unwrap();
        let none = sha256_hex(b"");
        assert_eq!(put(&authorization, &empty, &format!("{lfs}/objects/{none}")), 200);
        assert_eq!(curl(&["-H", &authorization, &format!("{lfs}/objects/{none}")]), (200, String::new()));

        // The application reads it with the admin token; another repository does not have it.
        let admin = |path: &str| curl(&["-H", "Authorization: Bearer root-secret", "-o", "/dev/null", &format!("{base}/api/repos/{path}")]).0;
        assert_eq!(admin(&format!("r-1/lfs/{oid}")), 200);
        assert_eq!(admin(&format!("r-2/lfs/{oid}")), 404);
        assert_eq!(admin("r-1/lfs/not-an-id"), 400);
        let metrics = api("GET", &format!("{base}/metrics"), Some("root-secret")).1;
        assert!(metrics.contains("strata_lfs_objects_total{op=\"upload\"} 2"), "{metrics}");
        assert!(metrics.contains(&format!("strata_lfs_bytes_total{{op=\"upload\"}} {}", data.len())), "{metrics}");

        // With the repository, its large files go.
        assert_eq!(api("DELETE", &format!("{base}/api/repos/r-1"), Some("root-secret")).0, 200);
    })
    .await
    .unwrap();
    assert_eq!(app.packs.lfs_delete_all("anything").await.unwrap(), 0);
    let left = app.packs.list_pools().await.unwrap();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn branches_can_be_compared_and_merged() {
    let base = start(false).await;
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let repo = format!("{base}/api/repos/r-9");
        let url = format!("{base}/r-9.git");
        let get = |path: &str| api("GET", &format!("{repo}/{path}"), None);
        let merge = |body: serde_json::Value| api_with("POST", &format!("{repo}/merge"), None, Some(&body.to_string()));
        let lines = |changed: &[(usize, &str)]| {
            let mut lines: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
            for (n, text) in changed {
                lines[n - 1] = text.to_string();
            }
            lines.join("\n") + "\n"
        };
        assert_eq!(api("PUT", &repo, None).0, 201);

        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("dir")).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("a.txt"), lines(&[])).unwrap();
        std::fs::write(work.join("dir/b.txt"), "b\n").unwrap();
        std::fs::write(work.join("gone.txt"), "gone\n").unwrap();
        let first = commit_on(&work, 1, "first");
        // A topic that edits the top of a file, adds one and removes one.
        git(&work, &["checkout", "-q", "-b", "topic"]);
        std::fs::write(work.join("a.txt"), lines(&[(2, "two")])).unwrap();
        std::fs::write(work.join("dir/new.txt"), "new\n").unwrap();
        std::fs::remove_file(work.join("gone.txt")).unwrap();
        let topic = commit_on(&work, 2, "topic work");
        // Meanwhile main edits the bottom of the same file and adds another.
        git(&work, &["checkout", "-q", "main"]);
        std::fs::write(work.join("a.txt"), lines(&[(9, "nine")])).unwrap();
        std::fs::write(work.join("c.txt"), "c\n").unwrap();
        let main = commit_on(&work, 3, "main work");
        // And a third branch edits that bottom line differently.
        git(&work, &["checkout", "-q", "-b", "clash", &first]);
        std::fs::write(work.join("a.txt"), lines(&[(9, "NINE")])).unwrap();
        std::fs::write(work.join("dir/b.txt"), "\0binary\n").unwrap();
        commit_on(&work, 4, "clashing work");
        git(&work, &["push", "-q", &url, "main", "topic", "clash"]);

        // The comparison is against where the two parted, so main's own work is not in it.
        let compared = json(&get("compare?base=main&head=topic").1);
        assert_eq!(compared["base"], main.as_str());
        assert_eq!(compared["head"], topic.as_str());
        assert_eq!(compared["merge_base"], first.as_str());
        assert_eq!((&compared["ahead"], &compared["behind"]), (&serde_json::json!(1), &serde_json::json!(1)));
        assert_eq!(compared["commits"][0]["oid"], topic.as_str());
        let files: Vec<(&str, &str)> =
            compared["files"].as_array().unwrap().iter().map(|f| (f["path"].as_str().unwrap(), f["status"].as_str().unwrap())).collect();
        assert_eq!(files, [("a.txt", "modified"), ("dir/new.txt", "added"), ("gone.txt", "deleted")]);
        assert_eq!(get("compare?base=main&head=nope").0, 404);

        // Asking first changes nothing.
        let asked = json(&merge(serde_json::json!({ "base": "main", "head": "topic", "dry_run": true })).1);
        assert_eq!(asked["mergeable"], true);
        assert_eq!(json(&get("refs").1)["refs"][1]["oid"], main.as_str());
        // What was looked at must be what is merged.
        assert_eq!(merge(serde_json::json!({ "base": "main", "head": "topic", "expect_head": first })).0, 409);

        let merged = merge(serde_json::json!({
            "base": "main", "head": "topic", "expect_head": topic, "actor": "alice",
            "message": "Merge pull request #1\n\nTopic work",
            "author": { "name": "Alice <A>", "email": "alice@example.com" },
        }));
        assert_eq!(merged.0, 200, "{}", merged.1);
        let merged = json(&merged.1);
        let oid = merged["oid"].as_str().unwrap().to_string();
        let copy = tmp.path().join("copy");
        git(tmp.path(), &["clone", "-q", &url, "copy"]);
        assert_eq!(git(&copy, &["rev-parse", "HEAD"]), oid);
        git(&copy, &["fsck", "--strict"]);
        assert_eq!(git(&copy, &["log", "-1", "--format=%P"]), format!("{main} {topic}"));
        assert_eq!(git(&copy, &["log", "-1", "--format=%an <%ae>|%s|%b"]), "Alice A <alice@example.com>|Merge pull request #1|Topic work");
        // Both sides' edits to the same file, and each side's own files.
        assert_eq!(std::fs::read_to_string(copy.join("a.txt")).unwrap(), lines(&[(2, "two"), (9, "nine")]));
        assert_eq!(git(&copy, &["ls-files"]), "a.txt\nc.txt\ndir/b.txt\ndir/new.txt");
        // The merge is in the feed like any push.
        let events = json(&api("GET", &format!("{base}/api/events"), None).1);
        let last = events["events"].as_array().unwrap().last().unwrap();
        assert_eq!(last["event"]["actor"], "alice");
        assert_eq!(last["event"]["updates"][0], serde_json::json!({ "ref": "refs/heads/main", "old": main, "new": oid, "change": "fast-forward" }));
        assert_eq!(last["event"]["merge"]["head"], topic.as_str());

        // Merged: nothing is left to compare or to merge.
        assert_eq!(json(&get("compare?base=main&head=topic").1)["ahead"], 0);
        let again = merge(serde_json::json!({ "base": "main", "head": "topic" }));
        assert_eq!(again, (409, "nothing to merge".to_string()));

        // Overlapping edits and binary files are conflicts, and leave the branch alone.
        let asked = json(&merge(serde_json::json!({ "base": "main", "head": "clash", "dry_run": true })).1);
        assert_eq!(asked["mergeable"], false);
        assert_eq!(asked["conflicts"], serde_json::json!(["a.txt"]));
        let refused = merge(serde_json::json!({ "base": "main", "head": "clash" }));
        assert_eq!(refused, (409, "conflicting changes in a.txt".to_string()));
        assert_eq!(json(&get("refs").1)["refs"][1]["oid"], oid.as_str());

        // A squash puts a branch's commits on top as one. Here main has not moved since.
        git(&copy, &["checkout", "-q", "-b", "two-steps"]);
        std::fs::write(copy.join("d.txt"), "d\n").unwrap();
        commit_on(&copy, 5, "step one");
        std::fs::create_dir_all(copy.join("deep/er")).unwrap();
        std::fs::write(copy.join("deep/er/e.txt"), "e\n").unwrap();
        commit_on(&copy, 6, "step two");
        git(&copy, &["push", "-q", "origin", "two-steps"]);
        let squashed = merge(serde_json::json!({ "base": "refs/heads/main", "head": "two-steps", "method": "squash" }));
        assert_eq!(squashed.0, 200, "{}", squashed.1);
        git(&copy, &["fetch", "-q", "origin"]);
        assert_eq!(git(&copy, &["log", "-1", "--format=%P|%s", "origin/main"]), format!("{oid}|Merge two-steps into main"));
        assert_eq!(git(&copy, &["diff", "--stat", "origin/main", "two-steps"]), "");
        git(&copy, &["fsck", "--strict"]);

        // A rebase makes each commit again on top of the base. This branch left
        // main two merges ago and holds a merge of its own, which is left out.
        git(&copy, &["checkout", "-q", "-b", "replay", &main]);
        std::fs::write(copy.join("a.txt"), lines(&[(5, "five"), (9, "nine")])).unwrap();
        let five = commit_on(&copy, 7, "five");
        git(&copy, &["merge", "-q", "--no-edit", &topic]);
        std::fs::write(copy.join("f.txt"), "f\n").unwrap();
        commit_on(&copy, 8, "add f\n\nwith a body");
        git(&copy, &["push", "-q", "origin", "replay"]);
        let before = git(&copy, &["rev-parse", "origin/main"]);
        let asked = json(&merge(serde_json::json!({ "base": "main", "head": "replay", "method": "rebase", "dry_run": true })).1);
        assert_eq!(asked["mergeable"], true);
        let rebased = merge(serde_json::json!({
            "base": "main", "head": "replay", "method": "rebase",
            "author": { "name": "Bob", "email": "bob@example.com" },
        }));
        assert_eq!(rebased.0, 200, "{}", rebased.1);
        git(&copy, &["fetch", "-q", "origin"]);
        // Two commits on top, in order: the topic's own commit is in main already and adds nothing.
        assert_eq!(git(&copy, &["log", "--format=%s|%an|%cn", &format!("{before}..origin/main")]), "add f|Test|Bob\nfive|Test|Bob");
        assert_eq!(git(&copy, &["log", "-1", "--format=%b", "origin/main"]), "with a body");
        assert_eq!(git(&copy, &["rev-list", "--merges", "--count", &format!("{before}..origin/main")]), "0");
        assert_eq!(git(&copy, &["log", "-1", "--format=%ad", "--date=short", "origin/main~1"]), "2020-01-07");
        assert_ne!(git(&copy, &["rev-parse", "origin/main~1"]), five);
        assert_eq!(git(&copy, &["show", "origin/main:a.txt"]) + "\n", lines(&[(2, "two"), (5, "five"), (9, "nine")]));
        assert_eq!(git(&copy, &["show", "origin/main:f.txt"]), "f");
        git(&copy, &["fsck", "--strict"]);
        // A straight line on top of the base is taken as it is: the branch moves up.
        git(&copy, &["checkout", "-q", "-B", "line", "origin/main"]);
        std::fs::write(copy.join("g.txt"), "g\n").unwrap();
        let line = commit_on(&copy, 9, "add g");
        git(&copy, &["push", "-q", "origin", "line"]);
        let moved = json(&merge(serde_json::json!({ "base": "main", "head": "line", "method": "rebase" })).1);
        assert_eq!(moved["oid"], line.as_str());
        assert_eq!(merge(serde_json::json!({ "base": "main", "head": "line", "method": "rebase" })).0, 409);
        // Replaying meets the same conflicts a merge does.
        let refused = merge(serde_json::json!({ "base": "main", "head": "clash", "method": "rebase" }));
        assert_eq!(refused, (409, "conflicting changes in a.txt".to_string()));

        assert_eq!(merge(serde_json::json!({ "base": "nope", "head": "topic" })).0, 404);
        assert_eq!(merge(serde_json::json!({ "base": "main", "head": "topic", "method": "octopus" })).0, 400);
        assert_eq!(api("GET", &format!("{repo}/merge"), None).0, 405);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_is_brought_in_from_a_bundle() {
    let base = start(false).await;
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let repo = format!("{base}/api/repos/in-1");
        let path = |name: &str| tmp.path().join(name).to_str().unwrap().to_string();
        let send = |bundle: &str, to: &str| api_with("POST", &format!("{to}/import?actor=mover"), None, Some(&format!("@{}", path(bundle))));

        // Branches, a branch off the main line, and tags of both kinds.
        let work = tmp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "trunk"]);
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        commit_on(&work, 1, "first");
        git(&work, &["tag", "-a", "-m", "release", "v1"]);
        git(&work, &["checkout", "-q", "-b", "side"]);
        std::fs::write(work.join("b.txt"), "side\n").unwrap();
        commit_on(&work, 2, "on the side");
        git(&work, &["checkout", "-q", "trunk"]);
        std::fs::write(work.join("a.txt"), "two\n").unwrap();
        let second = commit_on(&work, 3, "second");
        git(&work, &["tag", "light"]);
        git(&work, &["bundle", "create", "-q", &path("all.bundle"), "--all"]);
        let local = git(&work, &["for-each-ref", "--format=%(objectname) %(refname)"]);

        // Into a repository that exists: the import does not make one.
        assert_eq!(send("all.bundle", &repo).0, 404);
        assert_eq!(api("PUT", &repo, None).0, 201);
        let (status, answer) = send("all.bundle", &repo);
        assert_eq!(status, 200, "{answer}");
        assert_eq!(json(&answer), serde_json::json!({ "refs": 4, "updated": 4, "objects": 10 }));

        // Every ref is where it was, and HEAD names the branch it was on.
        let remote = format!("{base}/in-1.git");
        let listed = git(tmp.path(), &["ls-remote", "--refs", &remote]).replace('\t', " ");
        assert_eq!(listed, local);
        assert_eq!(json(&api("GET", &format!("{repo}/refs"), None).1)["head"], "refs/heads/trunk");
        clone_and_check(tmp.path(), &remote, "clone", &second, "2");
        git(&tmp.path().join("clone"), &["fsck", "--strict"]);

        // Again: nothing to do, and nothing stored a second time.
        assert_eq!(json(&send("all.bundle", &repo).1), serde_json::json!({ "refs": 4, "updated": 0, "objects": 0 }));
        assert_eq!(json(&api("GET", &repo, None).1)["packs"], 1);

        // A bundle of what came after builds on what is here, and moves refs whatever they were.
        std::fs::write(work.join("a.txt"), "three\n").unwrap();
        let third = commit_on(&work, 4, "third");
        git(&work, &["branch", "-f", "side", &third]);
        git(&work, &["bundle", "create", "-q", &path("more.bundle"), "trunk", "side", &format!("^{second}")]);
        assert_eq!(json(&send("more.bundle", &repo).1), serde_json::json!({ "refs": 2, "updated": 2, "objects": 3 }));
        let refs = json(&api("GET", &format!("{repo}/refs"), None).1);
        let at = |name: &str| refs["refs"].as_array().unwrap().iter().find(|r| r["name"] == name).unwrap()["oid"].as_str().unwrap().to_string();
        assert_eq!((at("refs/heads/trunk"), at("refs/heads/side")), (third.clone(), third.clone()));
        // The tags it did not name are still there.
        assert_eq!(refs["refs"].as_array().unwrap().len(), 4);

        // A mirror also drops what is gone at its source: asked to, the import
        // deletes the refs under a prefix that the bundle does not name.
        git(&work, &["bundle", "create", "-q", &path("trunk.bundle"), "trunk"]);
        let (status, answer) = api_with("POST", &format!("{repo}/import?actor=mover&prune=refs/heads/"), None, Some(&format!("@{}", path("trunk.bundle"))));
        assert_eq!((status, json(&answer)["updated"].clone()), (200, serde_json::json!(1)), "{answer}");
        let refs = json(&api("GET", &format!("{repo}/refs"), None).1);
        let names: Vec<&str> = refs["refs"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["refs/heads/trunk", "refs/tags/light", "refs/tags/v1"]);
        assert_eq!(api_with("POST", &format!("{repo}/import?prune=heads"), None, Some(&format!("@{}", path("trunk.bundle")))).0, 400);

        // The same bundle where its base is missing is refused, and leaves nothing behind.
        let empty = format!("{base}/api/repos/in-2");
        assert_eq!(api("PUT", &empty, None).0, 201);
        let (status, reason) = send("more.bundle", &empty);
        assert_eq!(status, 400);
        assert!(reason.contains("not complete"), "{reason}");
        assert_eq!(json(&api("GET", &format!("{empty}/refs"), None).1)["refs"], serde_json::json!([]));
        assert_eq!(json(&api("GET", &empty, None).1)["packs"], 0);

        // Not a bundle at all.
        std::fs::write(tmp.path().join("junk"), "hello\n\nworld").unwrap();
        assert_eq!(send("junk", &repo).0, 400);

        // The feed tells of each import, without listing its refs.
        let events = json(&api("GET", &format!("{base}/api/events"), None).1);
        let told: Vec<&serde_json::Value> = events["events"].as_array().unwrap().iter().filter(|e| e["repo"] == "in-1").map(|e| &e["event"]).collect();
        assert_eq!(told, [&serde_json::json!({ "type": "import", "actor": "mover", "refs": 4 }), &serde_json::json!({ "type": "import", "actor": "mover", "refs": 2 }), &serde_json::json!({ "type": "import", "actor": "mover", "refs": 1 })]);
    })
    .await
    .unwrap();
}

/// What the node did can be read at /metrics, by those with the admin token.
#[tokio::test(flavor = "multi_thread")]
async fn the_node_tells_prometheus_what_it_does() {
    let metrics = Arc::new(strata_server::Metrics::new());
    let packs = PackStore::new(metrics.metered_store(Arc::new(InMemory::new())), CacheConfig::default());
    let options = Options { auto_create: true, compact_at: usize::MAX, admin_token: Some("root-secret".into()), ..Options::default() };
    let app = Arc::new(App::with_metrics(Arc::new(MemoryMetaStore::default()), packs, options, metrics));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, router(served)).await.unwrap() });
    compact(&app, "nothing/here", true).await.unwrap();

    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "one\n").unwrap();
        commit_on(&work, 1, "one");
        let remote = format!("{}/acme/app.git", base.replace("http://", "http://git:root-secret@"));
        git(&work, &["push", "-q", &remote, "main"]);
        git(tmp.path(), &["clone", "-q", &remote, "clone"]);

        let url = format!("{base}/metrics");
        assert_eq!(api("GET", &url, None).0, 401);
        assert_eq!(api("GET", &url, Some("wrong")).0, 401);
        let (status, text) = api("GET", &url, Some("root-secret"));
        assert_eq!(status, 200);
        // The value of the series whose name and labels are exactly these.
        let value = |series: &str| -> f64 {
            let line = text.lines().find(|line| line.rsplit_once(' ').is_some_and(|(name, _)| name == series));
            line.unwrap_or_else(|| panic!("no {series} in:\n{text}")).rsplit_once(' ').unwrap().1.parse().unwrap()
        };
        assert_eq!(value(r#"strata_pushes_total{result="accepted"}"#), 1.0);
        assert!(value("strata_push_bytes_total") > 0.0);
        assert!(value(r#"strata_http_requests_total{handler="receive-pack",status="200"}"#) >= 1.0);
        // git asks without credentials first.
        assert!(value(r#"strata_http_requests_total{handler="info-refs",status="401"}"#) >= 1.0);
        assert!(value(r#"strata_fetches_total{result="sent"}"#) >= 1.0);
        assert!(value("strata_fetch_bytes_total") > 0.0);
        assert!(value(r#"strata_http_request_duration_seconds_count{handler="upload-pack"}"#) >= 1.0);
        assert!(value(r#"strata_meta_requests_total{op="commit",result="ok"}"#) >= 1.0);
        assert!(value(r#"strata_object_store_requests_total{op="put",result="ok"}"#) + value(r#"strata_object_store_requests_total{op="complete_multipart",result="ok"}"#) >= 1.0);
        assert!(value(r#"strata_object_store_bytes_total{direction="written"}"#) > 0.0);
        assert_eq!(value(r#"strata_compactions_total{result="nothing"}"#), 1.0);
        assert_eq!(value("strata_compactions_pending"), 0.0);
        assert!(text.contains("# TYPE strata_http_request_duration_seconds histogram"));
        // The scrapes themselves are not counted.
        assert!(!text.contains(r#"handler="other""#), "{text}");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn older_protocol_versions_can_fetch() {
    let url = format!("{}/acme/legacy.git", start(true).await);
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["remote", "add", "origin", &url]);
        let mut commits = Vec::new();
        for n in 1..=4 {
            let body: String = (0..200).map(|i| format!("revision {n} line {i}\n")).collect();
            std::fs::write(work.join("code.txt"), body).unwrap();
            commits.push(commit_on(&work, n, &format!("commit {n}")));
        }
        git(&work, &["tag", "-a", "v1", "-m", "version one"]);
        git(&work, &["push", "-q", "origin", "main", "v1"]);

        let mut head = commits[3].clone();
        for version in ["0", "1"] {
            let v = format!("protocol.version={version}");
            let name = format!("clone-v{version}");
            git(root, &["-c", &v, "clone", "-q", &url, &name]);
            let clone = root.join(&name);
            assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
            assert_eq!(git(&clone, &["symbolic-ref", "HEAD"]), "refs/heads/main");
            assert_eq!(git(&clone, &["rev-parse", "v1^{commit}"]), commits[3]);
            git(&clone, &["fsck", "--strict"]);

            // A fetch with history in common: haves are negotiated, only the new commit travels.
            std::fs::write(work.join(format!("extra{version}.txt")), format!("extra for protocol {version}\n")).unwrap();
            let newer = commit_on(&work, 5, "newer");
            git(&work, &["push", "-q", "origin", "main"]);
            // (A clone over the old protocol leaves small packs unpacked, so count the difference.)
            let loose = |dir: &Path| git(dir, &["count-objects"]).split(' ').next().unwrap().parse::<usize>().unwrap();
            let before = loose(&clone);
            git(&clone, &["-c", &v, "pull", "-q", "--ff-only"]);
            assert_eq!(git(&clone, &["rev-parse", "HEAD"]), newer);
            assert_eq!(loose(&clone) - before, 3);
            head = newer;
            git(&clone, &["fsck", "--strict"]);

            let shallow = format!("shallow-v{version}");
            git(root, &["-c", &v, "clone", "-q", "--depth", "1", &url, &shallow]);
            assert_eq!(commit_count(&root.join(&shallow)), "1");
            assert!(!has_object(&root.join(&shallow), &commits[0]));
            git(&root.join(&shallow), &["-c", &v, "fetch", "-q", "--unshallow"]);
            assert!(!is_shallow(&root.join(&shallow)));
            git(&root.join(&shallow), &["fsck", "--strict"]);
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_objects_and_stray_packs_are_removed() {
    let options = Options {
        auto_create: true,
        compact_at: usize::MAX,
        gc_grace: Duration::ZERO,
        orphan_grace: Duration::ZERO,
        ..Options::default()
    };
    let (base, app) = start_with(options).await;
    let url = format!("{base}/acme/prune.git");
    let tmp = tempfile::tempdir().unwrap();

    let (dir, remote) = (tmp.path().to_path_buf(), url.clone());
    let (kept, dropped) = tokio::task::spawn_blocking(move || {
        let work = dir.join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["remote", "add", "origin", &remote]);
        std::fs::write(work.join("file.txt"), "kept\n").unwrap();
        let kept = commit_on(&work, 1, "kept");
        git(&work, &["push", "-q", "origin", "main"]);

        // A branch that is pushed and then deleted leaves its objects behind.
        git(&work, &["checkout", "-q", "-b", "topic"]);
        std::fs::write(work.join("secret.txt"), "should not survive\n").unwrap();
        let dropped = commit_on(&work, 2, "dropped");
        git(&work, &["push", "-q", "origin", "topic"]);
        git(&work, &["push", "-q", "origin", ":topic"]);
        git(&work, &["checkout", "-q", "main"]);
        (kept, dropped)
    })
    .await
    .unwrap();

    let before = app.meta.get_repo("acme/prune", &strata_store::RefScope::All).await.unwrap().unwrap();
    let report = compact(&app, "acme/prune", true).await.unwrap().expect("two packs to merge");
    assert!(report.pruned);
    // One commit, one tree, one file.
    assert_eq!(report.objects, 3);
    let after = app.meta.get_repo("acme/prune", &strata_store::RefScope::All).await.unwrap().unwrap();
    assert_eq!(after.epoch, before.epoch + 1);
    assert_eq!(sweep(&app).await.unwrap(), 2);

    // A pack that was uploaded but never committed is cleaned up as well.
    let mut stray = app.packs.upload(&after.pool_id, "0000stray").await.unwrap();
    tokio::task::spawn_blocking(move || {
        std::io::Write::write_all(&mut stray, b"not really a pack").unwrap();
        stray.finish().unwrap();
    })
    .await
    .unwrap();
    assert_eq!(sweep_orphans(&app).await.unwrap(), 1);
    assert_eq!(sweep_orphans(&app).await.unwrap(), 0);

    let (dir, remote) = (tmp.path().to_path_buf(), url.clone());
    tokio::task::spawn_blocking(move || {
        clone_and_check(&dir, &remote, "clone", &kept, "1");
        // The deleted branch's commit is gone from the server for good.
        let probe = dir.join("probe");
        std::fs::create_dir(&probe).unwrap();
        git(&probe, &["init", "-q"]);
        assert!(git_in(&probe, &["fetch", "-q", &remote, &kept]).status.success());
        assert!(!git_in(&probe, &["fetch", "-q", &remote, &dropped]).status.success());

        // The repository still takes pushes, including ones that bring the dropped commit back.
        let work = dir.join("work");
        git(&work, &["push", "-q", "origin", "topic"]);
        assert!(git(&work, &["ls-remote", "origin", "refs/heads/topic"]).starts_with(&dropped));
        git(&dir, &["clone", "-q", "-b", "topic", &remote, "topic-clone"]);
        git(&dir.join("topic-clone"), &["fsck", "--strict"]);
    })
    .await
    .unwrap();
}
