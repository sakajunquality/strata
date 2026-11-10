# strata

A git server with storage and compute separated. Server processes are
stateless; refs live in a transactional metadata store and objects live as
immutable packfiles in object storage.

## Layout

| Crate | Role |
|---|---|
| `crates/git` | Git formats, independent of storage: pkt-line, objects, delta apply/encode, pack read/write/index, `.idx` v2, commit-graph, bitmaps, graph walks, compaction |
| `crates/store` | `MetaStore` (refs, pack manifest, garbage list, tokens, leases; compare-and-swap commit) and `PackStore` (packs in object storage, bounded caches) |
| `crates/server` | Smart HTTP: fetch (protocol v2, and v0/v1 for older clients), push, tokens, background compaction and sweeping |

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

### Access control

Started without an admin token, the server allows everything: use that only on
a trusted network. With one, every request needs credentials:

```sh
STRATA_ADMIN_TOKEN=... cargo run

# Issue tokens (the secret is shown once). A grant is <pattern>:<read|write|admin>,
# where the pattern is a repository, a prefix ending in /*, or * for all.
curl -X POST -H "Authorization: Bearer $STRATA_ADMIN_TOKEN" \
  'http://127.0.0.1:8080/api/tokens?name=ci&grant=acme/*:read'
curl -X DELETE -H "Authorization: Bearer $STRATA_ADMIN_TOKEN" http://127.0.0.1:8080/api/tokens/ci

git clone http://git:<token>@127.0.0.1:8080/acme/app.git   # or via a credential helper
```

`--anonymous-read` lets anyone fetch. The server speaks plain HTTP: put TLS in
front of it, since tokens travel as Basic credentials.

## Test

```sh
cargo test --workspace   # includes end-to-end tests driving the real git client
./scripts/smoke.sh       # push, restart the server, clone
./scripts/s3-smoke.sh    # the same through the S3 API, against LocalStack in Docker
cargo build --release && FILES=50000 COMMITS=100 ./scripts/bench.sh   # timings and server memory on a synthetic repository
```

## Not done

- **Several nodes need a shared metadata store.** Only SQLite and an in-memory store exist. Everything else is ready for it: nodes share no state, and maintenance is coordinated through leases in the metadata store.
- **Real cloud buckets are untested.** The S3 API path is exercised against LocalStack (`./scripts/s3-smoke.sh`); AWS S3 itself, GCS and Azure are not.
- **SSH.** Access is over HTTP(S) only.
- **SHA-256 repositories.**
- **`sparse:oid` and combined partial clone filters.** `blob:none`, `blob:limit=<n>` and `tree:<n>` work.
- **Pruned objects are gone at once.** There is no window in which a force-pushed-away commit can still be fetched by id after the next full compaction.
- **Indexing a push keeps about 50 bytes per object in memory**, plus one delta family at a time; the spool file needs local disk the size of the push.
- **Compaction computes deltas only for files changed since the last full merge**, against their previous version. It does not search for better bases.
- **TLS, rate limiting, hooks, and per-branch protections.**
