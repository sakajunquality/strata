# strata

A git engine with storage and compute separated. Server processes are
stateless; refs live in a transactional metadata store and objects live as
immutable packfiles in object storage.

Strata stores and serves repositories, and stops there. Users, organisations,
permissions, repository names, webhooks and a UI belong to an application
built on top, which Strata asks and informs over HTTP (see
[Building on it](#building-on-it)).

## Layout

| Crate | Role |
|---|---|
| `crates/git` | Git formats, independent of storage: pkt-line, objects, delta apply/encode, pack read/write/index, `.idx` v2, commit-graph, bitmaps, graph walks, compaction |
| `crates/store` | `MetaStore` (refs, pack manifest, garbage list, event feed, leases; compare-and-swap commit) on SQLite, PostgreSQL or Cloud Spanner, and `PackStore` (packs in object storage, bounded caches) |
| `crates/server` | Smart HTTP: fetch (protocol v2, and v0/v1 for older clients), push, the hooks and management API for the application, background compaction and sweeping |

A push is spooled to a scratch file, validated, and uploaded as one pack with
a commit-graph; then a single metadata transaction adds the pack to the
manifest and moves the refs. Until that commit the pack is invisible, so a
failed push leaves only garbage, never a broken repository.

Reads never load a whole pack. Pack data is fetched from object storage in
fixed-size blocks through a bounded cache (`--block-cache-mb`), with further
bounded caches for reconstructed commits and trees (`--object-cache-mb`) and
for pack indexes (`--index-cache-mb`). Fetch responses are streamed, copy
stored compressed entries and deltas as they are, and are computed by
comparing trees path by path, so the cost of an incremental fetch follows the
size of the change rather than of the repository. Where the client has the
previous version of a changed file, only a delta against it is sent.

A request reads only the refs it is about: the ones a fetch asks to have
listed, the ones a push updates, the one a revision names. A repository with
hundreds of thousands of refs answers as fast as one with ten, except where
every ref is asked for.

Compaction runs behind pushes (`--compact-at`). It merges the smallest packs
so that pack sizes stay in geometric progression, which keeps the pack count
logarithmic while rewriting the bulk of a repository only rarely. Every merged
pack gets reachability bitmaps: complete ones when the whole pool is merged,
otherwise a layer that builds on the bitmaps of the packs left alone. With
bitmaps a clone is a sequential copy of the packs, and a fetch knows what the
client has beyond the commits it names. A push that brings a lot of history
into a repository where nothing has bitmaps yet (an import) has the whole
pool merged right behind it (`--bitmaps-from`, 20,000 objects by default)
rather than leaving that to the pushes that follow. A merge of the whole pool
also drops objects no ref reaches (`--no-prune` keeps them) and stores changed
files as deltas against their previous versions. Replaced packs are deleted
after a grace period (`--gc-grace-secs`), as are packs left behind by pushes
that never committed (`--orphan-grace-secs`). A push or compaction checks that
its pack is still in storage right before committing it, so a pack the orphan
sweep removed is uploaded again rather than listed. The check and the commit
are not atomic: keep the orphan grace far above the time a commit can take.

Every push adds a pack, and every request costs as much as the repository
has packs. Pushes that come faster than compaction merges their packs would
pile them up without end, each making the next one slower, so a push that
finds 64 packs or more (`--wait-at`) asks for a compaction and waits for it,
up to ten seconds, before it goes on. Under a steady stream of pushes that
keeps the pack count, and with it the time a push takes, level.

Pruning is safe against concurrent pushes: a push commits only if no objects
were removed since it checked its history, and re-checks otherwise; a pruning
compaction commits only after verifying that refs which moved meanwhile do not
depend on anything it dropped.

## Run

```sh
cargo run -- --auto-create          # listens on 127.0.0.1:8080, state in ./data
git push http://127.0.0.1:8080/me/project.git main
git clone http://127.0.0.1:8080/me/project.git
```

Without `--auto-create`, create repositories with `curl -X PUT http://127.0.0.1:8080/api/repos/me/project`.
`POST /api/repos/<name>/compact` forces a full merge.

Git over SSH is off until asked for: `--ssh-listen 0.0.0.0:22`. The host key
is read from `--ssh-host-key` (OpenSSH's format; made on first start if it is
not there, by default in the data directory). Nodes behind one name must all
be given the same key. See "SSH" below for who is let in.

To keep packs in a bucket: `--object-store s3://bucket/prefix` (or `gs://`,
`az://`), with credentials in the usual `AWS_*`, `GOOGLE_*` or `AZURE_*`
environment variables. Give the bucket a lifecycle rule that aborts incomplete
multipart uploads.

### Several nodes

Refs and pack lists live in a metadata store, chosen with `--meta` (or
`STRATA_META`, which keeps a password out of the process list):

| | |
|---|---|
| `sqlite:///path/to/file` | One node. The default, as a file in `--data-dir`. |
| `postgres://user:password@host/database` | Several nodes. |
| `spanner://projects/<p>/instances/<i>/databases/<d>` | Several nodes, several regions. |

Point every node at the same metadata store and the same bucket, and put any
load balancer in front: nodes keep nothing of their own. The shared stores
create their tables on first start (on Spanner that needs permission to change
the schema, once).

- PostgreSQL: the server's certificate is verified against the system's
  authorities whenever TLS is used; set `SSL_CERT_FILE` for a private one, or
  add `?sslmode=disable` for a server without TLS.
- Spanner: Google's application default credentials, or the emulator when
  `SPANNER_EMULATOR_HOST` is set.

Node clocks time leases and grace periods; keep them within seconds of each other.

## Building on it

Strata talks to one application, which owns everything about people. There
are four points of contact; all are optional, and without them Strata is a
plain git server guarded by one admin token.

**The admin token** (`STRATA_ADMIN_TOKEN`) is the application's credential
for the management API below. Started without one, the server allows
everything: use that only on a trusted network. Without an authorization
hook it is also the only credential git clients can use (`--anonymous-read`
lets anyone fetch).

**The authorization hook** (`--authz-url`) decides every git request. Strata
POSTs what was asked for, with the client's `Authorization` header untouched,
so the application can use any credential scheme it likes:

```
-> {"path": "acme/app", "access": "write", "authorization": "Basic Z2l0Oi4uLg=="}
<- {"allow": true, "repo": "r-8f3a", "actor": "alice"}
<- {"allow": false, "status": 404, "message": "no such repository"}   # status: 401, 403 or 404
```

`repo` is the repository Strata then serves. Let it be an id the application
never changes: a rename, or a move to another organisation, is then a change
to the application's own table, and nothing moves inside Strata. `actor` is
recorded with pushes. Answers are reused for `--authz-cache-secs` (10 by
default), which is how long a revoked permission keeps working. If the hook
cannot be reached, the request is refused.

**SSH.** A client over SSH shows a key instead of a header, and the same
hook is asked, with the key as one `authorized_keys` line:

```
-> {"path": "", "access": "identify", "ssh_key": "ssh-ed25519 AAAA..."}     # when the key is offered
<- {"allow": true}
-> {"path": "acme/app", "access": "write", "ssh_key": "ssh-ed25519 AAAA..."}  # for each command
<- {"allow": true, "repo": "r-8f3a", "actor": "alice"}
```

The first question is put before the client is let in, so that one with
several keys goes on to its next: answer `allow` for keys the application
knows. The user name the client connects as (`git@`) means nothing. Nothing
but `git-upload-pack` and `git-receive-pack` runs: there is no shell.
Fetches over SSH speak protocol version 2 only, which git has asked for by
itself since 2.26; older clients fetch over HTTP. Without a hook, SSH serves
what HTTP serves without credentials: everything if there is no admin token,
fetches with `--anonymous-read`, nothing otherwise.

**Large files (Git LFS)** are served by the node itself, from
`<repo>.git/info/lfs`, which is where git-lfs looks. A file is kept whole in
object storage beside the repository's packs, under the SHA-256 of its
content, and stored only once that content has been checked to hash to it.
Who may do what is decided as for git: a download is a fetch, an upload is
a push, and the authorization hook is asked the same two questions. A
client that came in over SSH is given a ticket for the HTTP routes instead,
good for an hour and signed with a key made from the admin token, so every
node honours it; that needs `--public-url https://host`, for the node to
say where those routes are. Only the basic transfer is spoken, without
locks. Uploads are spooled to the scratch directory first, like pushes. A
file stays until its repository is deleted, whether or not a commit still
points at it. `GET /api/repos/<id>/lfs/<oid>` reads one for the application.

**The pre-receive hook** (`--pre-receive-url`) can veto a push before
anything is stored, which is where protected branches go:

```
-> {"repo": "r-8f3a", "actor": "alice", "updates": [
     {"ref": "refs/heads/main", "old": "1f3c...", "new": "9a0b...", "change": "forced",
      "commits": [{"oid": "9a0b...", "author": {...}, "message": "...", "signature": null}],
      "commits_truncated": false}]}
<- {"allow": false, "message": "main is protected"}      # shown to the person pushing
```

`change` is `create`, `delete`, `fast-forward` or `forced`. `commits` are the
commits the update brings to its ref, newest first: for a ref that moves,
those its old value lacks, and for a new one, those the repository did not
have before the push. Up to
1000 are listed. Each is described as the content API describes a commit,
which includes its `signature`: null, or the signature itself with its
`format` (`openpgp`, `ssh` or `x509`) and the `payload` it signs, in base64.
That is what a rule about signed commits needs; whose key made a signature
is the application's to know. Files are not shown: the hook cannot fetch
the pushed objects, which are not visible until the push commits.

Both hooks receive `--hook-secret` as a Bearer token, to tell Strata from
anyone else who finds the URL.

**The event feed** tells the application what happened. Each push that lands
is recorded in the same transaction that moves the refs, so the feed has
exactly the pushes that happened, in order. Poll it, and keep `next`:

```
GET /api/events?after=<next>&limit=100
<- {"events": [{"cursor": "...", "repo": "r-8f3a", "time": 1794311777,
     "event": {"type": "push", "actor": "alice", "updates": [...]}}], "next": "..."}
```

Events are kept for `--event-retention-secs` (a week by default). Webhooks,
CI triggers and audit logs are built from this, with whatever retries they need.

**The management API**, all under the admin token:

| | |
|---|---|
| `PUT /api/repos/<id>` | Create a repository. |
| `GET /api/repos/<id>` | Its HEAD, and counts of refs, packs, objects and bytes stored. |
| `PUT /api/repos/<id>/head` | Point HEAD (the default branch) at the ref named in the body. |
| `DELETE /api/repos/<id>` | Remove it. It is gone at once; its packs are deleted after the grace period. |
| `POST /api/repos/<id>/compact` | Rewrite its packs into one now. |
| `GET /api/repos/<id>/refs` | HEAD and every ref with the id it points at. `prefix=refs/heads/` lists only the refs under a prefix. |
| `GET /api/repos/<id>/commits?rev=&skip=&limit=&path=` | History of a revision, newest first, up to 100 at a time. With `path`, only the commits that changed that file or directory. |
| `GET /api/repos/<id>/blame?rev=&path=` | For each run of lines of a file, the commit that last changed them. |
| `GET /api/repos/<id>/search?rev=&q=` | Lines of the revision's files that hold `q`, and files whose names do. `regex=1` takes a regular expression, `case=1` minds case, `path=` looks below a directory, `limit=` stops sooner than at 200 lines. |
| `GET /api/repos/<id>/lfs/<oid>` | A large file (Git LFS), by the SHA-256 of its content. |
| `GET /api/repos/<id>/commit?rev=` | One commit and the files it changed against its first parent. |
| `GET /api/repos/<id>/tree?rev=&path=` | A directory listing. |
| `GET /api/repos/<id>/blob?rev=&path=` | A file's bytes (or `?oid=` for a blob by id). |
| `GET /api/repos/<id>/compare?base=&head=` | What `head` has that `base` does not: the merge base, the commits, and the files changed since the two parted. |
| `POST /api/repos/<id>/merge` | Merge a revision into a branch: as a merge commit, squashed into one commit, or rebased. |
| `POST /api/repos/<id>/import` | Take in a bundle: its objects, and every ref in it. |
| `GET /api/events` | The feed above. |
| `GET /healthz` | Liveness, without credentials. |
| `GET /readyz` | Readiness, without credentials: 503 while the metadata store cannot be reached. |
| `GET /metrics` | What the node counts and times, for Prometheus. See below. |

A `rev` is a ref (in full, or a branch or tag name) or a commit id, and
defaults to HEAD. These read calls are what a code browser is built from;
they do no access control of their own, which is the application's to do
before it asks.

An import brings in a repository that exists elsewhere, with the bundle git
makes of it as the body:

```sh
git bundle create all.bundle --all
curl -X POST -T all.bundle -H "Authorization: Bearer $TOKEN" "https://host/api/repos/<id>/import?actor=alice"
<- {"refs": 150001, "updated": 150001, "objects": 58000}
```

The objects are stored as a push stores them, and every ref in the bundle is
set to the value it has there, whatever it was before; refs the bundle does
not name are left alone, unless they are under a prefix given as `prune`
(`?prune=refs/heads/&prune=refs/tags/`): those are deleted, which is what
keeping a mirror of another repository takes. A bundle of what came after an earlier one
(`git bundle create more.bundle main ^<commit>`) is taken in the same way, as
long as what it builds on is here. This is the way in for a repository with
very many refs: git's own push over HTTP compares every ref it sends with
every ref it has, and takes hours for a hundred thousand, where this takes a
second. Refs are set five thousand at a time, so an import that fails half
way leaves some of them set; sending the bundle again finishes it. The
pre-receive hook is not asked, and the feed gets one event,
`{"type": "import", "actor": "alice", "refs": 150001}`, without the list.

A merge takes a JSON body:

```json
{ "base": "main", "head": "topic", "method": "merge", "message": "Merge topic",
  "author": { "name": "Alice", "email": "alice@example.com" }, "actor": "alice",
  "expect_head": "<commit id>", "dry_run": false }
```

Only `base` (a branch) and `head` (any revision) are required. It is a
three-way merge against the merge base, done without a working copy: files
changed on both sides are merged line by line, and overlapping edits, binary
files or a change to something the other side deleted are conflicts, answered
with 409 and nothing written. `method` is `merge` (a commit with both
parents, even where a fast-forward would do), `squash` (one commit on top of
the branch) or `rebase` (each commit made again on top of the branch, with
its author and message; merge commits are left out, commits whose change the
branch already has are skipped, and a branch that is already a straight line
on top just moves up). `expect_head`
refuses the merge if the head is no longer that commit, and `dry_run` only
says whether it would work (`{"mergeable": ..., "conflicts": [...]}`). A
merge appears in the event feed as a push by `actor`, with a `merge` field.
The pre-receive hook is not asked: the application is the one calling.

### Metrics

`GET /metrics` answers in the text format Prometheus scrapes. With an admin
token it takes that token, like the management API:

```yaml
scrape_configs:
  - job_name: strata
    authorization: { credentials: "<admin token>" }
    static_configs: [{ targets: ["strata:8080"] }]
```

| | |
|---|---|
| `strata_http_requests_total{handler,status}`, `strata_http_request_duration_seconds{handler}`, `strata_http_requests_in_flight` | Requests by what they asked for: `info-refs`, `upload-pack`, `receive-pack`, `content`, `merge`, `import`, `events`, `repos`, `compact`. Timed until the answer starts. |
| `strata_fetches_total{result}`, `strata_fetch_bytes_total`, `strata_fetch_duration_seconds` | Fetch responses to their end, at the pace the client reads: `sent`, `aborted` (the client left) or `error`. |
| `strata_ssh_commands_total{service,result}`, `strata_ssh_command_duration_seconds{service}` | Commands over SSH (`upload-pack`, `receive-pack`, `other`): `ok`, `refused` or `aborted` (the client left). Their fetches and pushes are counted in the rows around this one too. |
| `strata_lfs_objects_total{op}`, `strata_lfs_bytes_total{op}` | Large files sent out (`download`) and taken in (`upload`). |
| `strata_pushes_total{result}`, `strata_push_bytes_total`, `strata_push_wait_seconds` | Pushes `accepted` or `rejected`, the pack data they brought, and how long they were held back for compaction (`--wait-at`). |
| `strata_compactions_total{result}`, `strata_compaction_duration_seconds`, `strata_compaction_objects_total`, `strata_compactions_pending` | Compaction: runs that `merged` packs, found `nothing` to do or ended in `error`, and the repositories waiting. |
| `strata_hook_requests_total{hook,result}`, `strata_hook_duration_seconds{hook}`, `strata_authz_cache_total{result}` | The application's `authz` and `pre-receive` hooks, and how often an authorization was answered from memory. |
| `strata_meta_requests_total{op,result}`, `strata_meta_duration_seconds{op}` | Calls to the metadata store. `conflict` is a change that lost to a concurrent one. |
| `strata_object_store_requests_total{op,result}`, `strata_object_store_duration_seconds{op}`, `strata_object_store_bytes_total{direction}` | Calls to object storage, and the bytes `read` and `written`. |
| `strata_cache_bytes{cache}` | Memory held by the caches of pack `blocks`, `objects` and `indexes`. |
| `strata_build_info{version}`, `process_*` | The version, and on Linux the process's CPU time, memory and open files. |

No label names a repository: there may be millions, and each would be a time
series of its own. Which repository was slow is in the log.

The server speaks plain HTTP: put TLS in front of it.

[`examples/k8s`](examples/k8s) runs several nodes on Kubernetes, as a plain
`Deployment` over one database and one bucket, with a script that tries it in
a local kind cluster: pods are deleted, replaced and added while pushes go on.

[`examples/hub`](examples/hub) is such an application, small but whole:
accounts, organizations, private repositories, a protected default branch,
signed commits, a code browser, pull requests, an activity feed, mirrors of
repositories on GitHub, and sign-in with Entra ID, Google Workspace or
GitHub, in Bun, Hono and React.

## Test

```sh
cargo test --workspace   # includes end-to-end tests driving the real git client
./scripts/smoke.sh       # push, restart the server, clone
./scripts/s3-smoke.sh    # the same through the S3 API, against LocalStack in Docker
./scripts/meta-test.sh   # PostgreSQL and the Spanner emulator in Docker: backend tests, then two nodes sharing each
cargo build --release && FILES=50000 COMMITS=100 ./scripts/bench.sh   # timings and server memory on a synthetic repository
./scripts/bench-repo.sh /path/to/bare.git master   # the same against a real repository
./scripts/bench-refs.sh  # the requests git makes, against a repository with 150,000 refs
./scripts/bench-push.py http://127.0.0.1:8080/bench/load.git --clients 32   # many clients pushing to one repository at once
```

Against git's own repository (83k commits, 412k objects, a 301 MiB pack), on
an Apple M-series laptop with everything on local disk:

| | |
|---|---|
| Push of all history | 6 s (`git index-pack` takes 6 s for the same pack, on as many threads) |
| Full clone before compaction | 9 s (much of it git indexing what it received) |
| Compaction, with commit-graph and bitmaps | 4 s |
| Full clone after compaction | 7.5 s (server 0.4 s) |
| `clone --depth 1` | 0.6 s |
| `clone --filter=blob:none` | 3.3 s |
| `clone --filter=blob:limit=10k` | 5 s |
| Fetch of 500 new commits | 0.2 s |

The same history pushed in 100 pieces and then compacted takes the same
271 MB as when pushed at once. Memory follows the caches, which default to
1 GiB of pack blocks and 256 MiB of objects (`--block-cache-mb`,
`--object-cache-mb`).

Pushes and walks through history use up to eight threads each.

With 100,000 branches and 50,000 annotated tags in one repository
(`./scripts/bench-refs.sh`), the server's time per request, with the metadata
in SQLite and in PostgreSQL 17 on the same machine:

| | SQLite | PostgreSQL |
|---|---|---|
| Fetch: list one branch | 0.3 ms | 3 ms |
| Fetch: list the 200 branches under a prefix | 0.5 ms | 3 ms |
| Fetch: list everything, as a clone does (13 MB) | 100 ms | 150 ms |
| Push: the opening list of every ref (11 MB) | 65 ms | 105 ms |
| Push of one new branch, after that list | 0.4 ms | not measured |
| API: newest commit of the default branch | 0.3 ms | 2 ms |

Before requests read only their own refs, each of these took at least 40 ms
with SQLite and 90 ms with PostgreSQL.

Many clients pushing to one repository at once, each to a branch of its own,
one commit with one changed file a push (`./scripts/bench-push.py`, which
speaks the push protocol directly). One node, packs on local disk, everything
on the same laptop:

| | Pushes a second | Median | 99% |
|---|---|---|---|
| SQLite, 1 client | 1,080 | 0.8 ms | 3 ms |
| SQLite, 8 clients | 2,100 | 3 ms | 10 ms |
| SQLite, 32 clients, 30 s | 1,540 | 18 ms | 69 ms |
| PostgreSQL 17, 32 clients, 15 s | 1,110 | 25 ms | 75 ms |

No push was refused, and a clone afterwards had every commit. Two things made
these numbers: pushes waiting for compaction (without it the 30 s run fell to
460 a second with 13,800 packs piled up, and kept falling), and PostgreSQL
making each change in one call (with a statement at a time, the repository's
row stayed locked for seven trips to the database: 240 a second).

## Not done

- **The shared metadata stores are tested against local servers only.** PostgreSQL 17 in Docker and the Cloud Spanner emulator, each with two nodes on one database (`./scripts/meta-test.sh`). Not tested: Cloud Spanner itself (the emulator runs one transaction at a time, so real contention and Google credentials are unexercised), managed PostgreSQL services, and more than two nodes or any load.
- **Real cloud buckets are untested.** The S3 API path is exercised against LocalStack (`./scripts/s3-smoke.sh`); AWS S3 itself, GCS and Azure are not.
- **A push still opens with the list of every ref**, as the protocol has it: 11 MB for 150,000 refs, on every push. Hiding refs from that list is not done.
- **Listing refs reads each tag to find what it points at.** The peeled values are not stored next to the refs. Listing 50,000 annotated tags takes 50 ms.
- **A fetch can send what the client has by another way.** A change picked onto a release branch brings the same files as the original, and a client that has the original is sent them again unless bitmaps say it has them. With etcd's repository (183k objects, 23 branches, 993 tags), a client holding the default branch that fetches every other branch is sent 3% it already had once the pool has bitmaps, 25% before (git itself, serving the same repository without bitmaps, sends 15%). What is pushed after the last compaction is not covered by bitmaps until the next. Measured on that one repository only.
- **Pushes at a high rate are measured on one machine only**, with packs on local disk and the database next to the node. In object storage each push is three uploads and each compaction reads what it merges back, so both will be slower there, by how much is not known. Two nodes on the same laptop handled no more than one (840 against 1,110 a second): they share its processors, so this says nothing about adding nodes. Pushes to the same branch were not part of it.
- **SSH is narrower than HTTP.** Fetches need protocol version 2 (libgit2 and clients older than git 2.26 fetch over HTTP). Clients are known by plain public keys: no certificates, no passwords. Tested with OpenSSH's client and Ed25519 and RSA keys; not under load, and a large clone over SSH has not been timed against HTTP.
- **Search, blame and the history of a path are worked out each time**, from the objects: nothing is indexed. A search reads every file of the revision, a blame diffs the file through its history, and each gives up after ten seconds and says so (`truncated`). Renames are not followed. Blame agreed with `git blame` line for line on the histories it was tried on, which were small; on a long history neither its speed nor its agreement has been measured.
- **Large files are never collected.** One that no commit points at any more stays until its repository is deleted. There are no locks, no quota, and no transfer but the basic one. Tested with git-lfs 3.5 over HTTP and over SSH, with files of a few megabytes.
- **SHA-256 repositories.**
- **Pruned objects are gone at once.** There is no window in which a force-pushed-away commit can still be fetched by id after the next full compaction.
- **Indexing a push takes memory in proportion to its objects**: about 180 bytes for each (72 MiB for the 412k objects of git's repository, where the whole server peaked at 142 MiB over that push), for the table of entries, the ids they point at and the index being built. On top come the objects whose deltas are being resolved, up to 64 MiB on each of up to eight threads; beyond that they are dropped and read again when needed. A single object larger than that is still held whole, together with the version made from it. Not measured beyond 412k objects. The spool file needs local disk the size of the push, and is mapped into memory: it counts as the process's memory wherever mapped files do, though the kernel can drop its pages at any time.
- **Compaction computes deltas only for files changed since the last full merge**, against their previous version. It does not search for better bases; with packs that come from git, which already hold good deltas, nothing was lost by that in the measurement above.
- **The pre-receive hook sees the commits a push brings, but not their files.** Rules about file sizes or secrets need a way to read a push before it commits.
- **The event feed covers pushes, merges and imports only.** Repositories created, deleted or compacted are the application's own actions and are not echoed back. An import is told as one event without its refs: the application reads them afterwards.
- **TLS, rate limiting, and quotas.** `GET /api/repos/<id>` reports the bytes a repository holds; nothing enforces a limit.
