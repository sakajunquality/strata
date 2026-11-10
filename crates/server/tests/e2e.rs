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

    // Filters the server cannot honour are refused rather than silently ignored.
    assert!(!git_in(root, &["clone", "-q", "--filter=object:type=blob", url, "unsupported"]).status.success());
}

async fn pack_count(app: &App, repo: &str) -> usize {
    app.meta.get_repo(repo).await.unwrap().unwrap().packs.len()
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

/// Calls the HTTP API and returns the status code and body.
fn api(method: &str, url: &str, token: Option<&str>) -> (u16, String) {
    let mut command = Command::new("curl");
    command.args(["-s", "-X", method, "-w", "\n%{http_code}", url]);
    if let Some(token) = token {
        command.args(["-H", &format!("Authorization: Bearer {token}")]);
    }
    let out = String::from_utf8(command.output().expect("curl must be installed").stdout).unwrap();
    let (body, status) = out.rsplit_once('\n').unwrap();
    (status.parse().unwrap(), body.trim().to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_gate_access() {
    let options = Options { auto_create: true, compact_at: usize::MAX, admin_token: Some("root-secret".into()), ..Options::default() };
    let (base, _app) = start_with(options).await;
    tokio::task::spawn_blocking(move || {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let with = |secret: &str, repo: &str| format!("{}/{repo}.git", base.replace("http://", &format!("http://git:{secret}@")));

        // The admin secret can do everything, including the first push.
        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file.txt"), "one\n").unwrap();
        commit_on(&work, 1, "one");
        git(&work, &["push", "-q", &with("root-secret", "acme/app"), "main"]);

        // Without credentials, or with wrong ones, nothing is visible: not even whether a repository exists.
        for url in [format!("{base}/acme/app.git"), with("wrong", "acme/app"), format!("{base}/acme/nope.git")] {
            let out = git_in(root, &["ls-remote", &url]);
            let message = String::from_utf8_lossy(&out.stderr).to_string();
            assert!(!out.status.success());
            // Always an authentication problem, never "not found".
            assert!(message.contains("Authentication failed") || message.contains("could not read Username"), "{url}: {message}");
        }

        // Only an administrator issues tokens.
        let issue = format!("{base}/api/tokens?name=ci&grant=acme/*:read");
        assert_eq!(api("POST", &issue, None).0, 401);
        assert_eq!(api("POST", &issue, Some("wrong")).0, 401);
        let (status, reader) = api("POST", &issue, Some("root-secret"));
        assert_eq!(status, 201);
        let (status, writer) = api("POST", &format!("{base}/api/tokens?name=dev&grant=acme/app:write"), Some("root-secret"));
        assert_eq!(status, 201);
        assert_eq!(api("POST", &format!("{base}/api/tokens?name=bad&grant=acme/app:owner"), Some("root-secret")).0, 400);
        // A token is not an administrator, whatever it grants on repositories.
        assert_eq!(api("POST", &issue, Some(&writer)).0, 403);
        assert_eq!(api("POST", &format!("{base}/api/repos/acme/app/compact"), Some(&writer)).0, 403);

        // Read access: fetch yes, push no, other namespaces no.
        git(root, &["clone", "-q", &with(&reader, "acme/app"), "clone"]);
        std::fs::write(work.join("file.txt"), "two\n").unwrap();
        commit_on(&work, 2, "two");
        assert!(!git_in(&work, &["push", "-q", &with(&reader, "acme/app"), "main"]).status.success());
        assert!(!git_in(&work, &["push", "-q", &with(&reader, "other/app"), "main"]).status.success());

        // Write access to one repository: push there, and only there.
        git(&work, &["push", "-q", &with(&writer, "acme/app"), "main"]);
        assert!(!git_in(&work, &["push", "-q", &with(&writer, "acme/other"), "main"]).status.success());
        git(&root.join("clone"), &["pull", "-q", "--ff-only"]);
        assert_eq!(git(&root.join("clone"), &["rev-parse", "HEAD"]), git(&work, &["rev-parse", "HEAD"]));

        // Revoked tokens stop working at once.
        assert_eq!(api("DELETE", &format!("{base}/api/tokens/ci"), Some("root-secret")).0, 200);
        assert!(!git_in(&root.join("clone"), &["fetch", "-q"]).status.success());
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

    let before = app.meta.get_repo("acme/prune").await.unwrap().unwrap();
    let report = compact(&app, "acme/prune", true).await.unwrap().expect("two packs to merge");
    assert!(report.pruned);
    // One commit, one tree, one file.
    assert_eq!(report.objects, 3);
    let after = app.meta.get_repo("acme/prune").await.unwrap().unwrap();
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
