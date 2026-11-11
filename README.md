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

Compaction runs behind pushes (`--compact-at`). It merges the smallest packs
so that pack sizes stay in geometric progression, which keeps the pack count
logarithmic while rewriting the bulk of a repository only rarely. Every merged
pack gets reachability bitmaps: complete ones when the whole pool is merged,
otherwise a layer that builds on the bitmaps of the packs left alone. With
bitmaps a clone is a sequential copy of the packs. A merge of the whole pool
also drops objects no ref reaches (`--no-prune` keeps them) and stores changed
files as deltas against their previous versions. Replaced packs are deleted
after a grace period (`--gc-grace-secs`), as are packs left behind by pushes
that never committed (`--orphan-grace-secs`). A push or compaction checks that
its pack is still in storage right before committing it, so a pack the orphan
sweep removed is uploaded again rather than listed. The check and the commit
are not atomic: keep the orphan grace far above the time a commit can take.

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
those its old value lacks, and for a new one, those no ref has yet. Up to
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
| `GET /api/repos/<id>/refs` | HEAD and every ref with the id it points at. |
| `GET /api/repos/<id>/commits?rev=&skip=&limit=` | History of a revision, newest first, up to 100 at a time. |
| `GET /api/repos/<id>/commit?rev=` | One commit and the files it changed against its first parent. |
| `GET /api/repos/<id>/tree?rev=&path=` | A directory listing. |
| `GET /api/repos/<id>/blob?rev=&path=` | A file's bytes (or `?oid=` for a blob by id). |
| `GET /api/repos/<id>/compare?base=&head=` | What `head` has that `base` does not: the merge base, the commits, and the files changed since the two parted. |
| `POST /api/repos/<id>/merge` | Merge a revision into a branch: as a merge commit, squashed into one commit, or rebased. |
| `GET /api/events` | The feed above. |
| `GET /healthz` | Liveness, without credentials. |
| `GET /readyz` | Readiness, without credentials: 503 while the metadata store cannot be reached. |

A `rev` is a ref (in full, or a branch or tag name) or a commit id, and
defaults to HEAD. These read calls are what a code browser is built from;
they do no access control of their own, which is the application's to do
before it asks.

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

The server speaks plain HTTP: put TLS in front of it.

[`examples/k8s`](examples/k8s) runs several nodes on Kubernetes, as a plain
`Deployment` over one database and one bucket, with a script that tries it in
a local kind cluster: pods are deleted, replaced and added while pushes go on.

[`examples/hub`](examples/hub) is such an application, small but whole:
accounts, organizations, private repositories, a protected default branch,
signed commits, a code browser, pull requests and an activity feed, in Bun, Hono and React.

## Test

```sh
cargo test --workspace   # includes end-to-end tests driving the real git client
./scripts/smoke.sh       # push, restart the server, clone
./scripts/s3-smoke.sh    # the same through the S3 API, against LocalStack in Docker
./scripts/meta-test.sh   # PostgreSQL and the Spanner emulator in Docker: backend tests, then two nodes sharing each
cargo build --release && FILES=50000 COMMITS=100 ./scripts/bench.sh   # timings and server memory on a synthetic repository
./scripts/bench-repo.sh /path/to/bare.git master   # the same against a real repository
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

## Not done

- **The shared metadata stores are tested against local servers only.** PostgreSQL 17 in Docker and the Cloud Spanner emulator, each with two nodes on one database (`./scripts/meta-test.sh`). Not tested: Cloud Spanner itself (the emulator runs one transaction at a time, so real contention and Google credentials are unexercised), managed PostgreSQL services, and more than two nodes or any load.
- **Real cloud buckets are untested.** The S3 API path is exercised against LocalStack (`./scripts/s3-smoke.sh`); AWS S3 itself, GCS and Azure are not.
- **SSH.** Access is over HTTP(S) only.
- **SHA-256 repositories.**
- **Pruned objects are gone at once.** There is no window in which a force-pushed-away commit can still be fetched by id after the next full compaction.
- **Indexing a push takes memory in proportion to its objects**: for the table of entries, the ids they point at, and one delta family per thread (up to eight) being resolved. The spool file needs local disk the size of the push, and is mapped into memory (the kernel pages it out as needed).
- **Compaction computes deltas only for files changed since the last full merge**, against their previous version. It does not search for better bases; with packs that come from git, which already hold good deltas, nothing was lost by that in the measurement above.
- **The pre-receive hook sees the commits a push brings, but not their files.** Rules about file sizes or secrets need a way to read a push before it commits.
- **The event feed covers pushes only.** Repositories created, deleted or compacted are the application's own actions and are not echoed back.
- **TLS, rate limiting, and quotas.** `GET /api/repos/<id>` reports the bytes a repository holds; nothing enforces a limit.
