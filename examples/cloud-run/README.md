# Strata and the hub on Cloud Run

An experiment: the server and the [hub](../hub) as two Cloud Run services,
with Cloud Spanner and Cloud Storage behind them. It is the first time the
Spanner backend meets Spanner itself rather than the emulator, and the first
time several nodes run where they cannot see each other.

```
 git ──────────► strata (Cloud Run, 0..3 instances) ──► Spanner   refs, pack lists
                   ▲   │                             └► GCS       packs, LFS files
     admin token   │   │  hooks (hook secret)
                   │   ▼
 browser ──────► strata-hub (Cloud Run, 0..1 instance) ─► GCS     Litestream's copy of hub.sqlite
```

Nothing here creates infrastructure: the services, the database, the
buckets, the secrets and the accounts are made elsewhere, by Terraform.
This repository gives the experiment three things:

- **Settings from the environment.** Cloud Run sets variables, not
  arguments, so the options a deployment has to choose can all be given
  that way: `STRATA_META`, `STRATA_OBJECT_STORE`, `STRATA_DATA_DIR`,
  `STRATA_SCRATCH_DIR`, `STRATA_LISTEN`, `STRATA_PUBLIC_URL`, the three
  `STRATA_*_CACHE_MB`, and the hooks and secrets as before.
- **An image of the hub** (`examples/hub/Dockerfile`). With `HUB_DB_REPLICA`
  set it runs under [Litestream](https://litestream.io), which fetches the
  SQLite file from the bucket when an instance starts and copies every
  change back.
- **A workflow** (`.github/workflows/deploy.yml`) that builds both images
  and rolls both services. It is run by hand, and reads where to deploy
  from the repository's variables, which the workflow lists.

## Bringing it up

1. Apply the Terraform. It makes both services (running Cloud Run's
   placeholder image), a Spanner database, two buckets and the two secrets
   the services share, and tells each service the other's address.
2. To be able to sign in with Google: add
   `<the hub's address>/api/auth/google/callback` to the redirect addresses
   of the OAuth client the hub is given. A hub on a public address should
   have accounts with passwords off (`HUB_PASSWORD_LOGIN=0`) and let in one
   domain only (`HUB_GOOGLE_DOMAIN`).
3. Set the repository's variables and run the `deploy` workflow. On its
   first start strata creates its tables in Spanner, which takes about a
   minute.
4. Open the hub, sign in, make a repository, create an access token under
   Settings, and push to `<strata's address>/<you>/<repo>.git` with it.

## What to look at

- **Several instances.** Push from many clients at once
  (`scripts/bench-push.py`), and watch the instance count rise in the Cloud
  Run console. Every push must land, and a clone afterwards must have all
  of them. `strata_meta_requests_total{result="conflict"}` at `/metrics` (with
  the admin token) says how often two instances met on one repository.
- **Spanner for real.** The emulator runs one transaction at a time; this is
  the first place aborts and retries happen. Push latency against the
  numbers in the main README, which are from PostgreSQL on one machine.
- **A cold start.** Left idle, the service has no instances. The first
  fetch then pays for a start and for empty caches.
- **A large push.** See the first limit below.

## What Cloud Run changes

- **No SSH.** Cloud Run takes HTTP only. Git LFS over HTTP works.
- **HTTP/2 to the container.** Over HTTP/1 Cloud Run refuses bodies above
  32 MiB, less than a first push of most repositories, so the service asks
  for HTTP/2 end to end. Strata answers HTTP/2 without TLS (a ref listing was tried that
  way on one machine), but no push or clone has gone through Cloud Run's
  own front end yet. If it misbehaves, the service can go back to HTTP/1
  and the cap.
- **The disk is memory.** An incoming pack is spooled to the scratch
  directory while it is checked, and on Cloud Run that counts against the
  instance's 2 GiB together with the caches (640 MiB as set). A push larger
  than what is left kills the instance. The same goes for the hub's mirrors
  of GitHub repositories, in 1 GiB.
- **Work between requests.** Compaction runs after the push that asked for
  it has been answered, so both services keep their CPU when idle
  (instance-based billing). An instance that is shut down mid-compaction
  leaves a lease to expire and a pack to be collected, as any crash would.
- **One hub.** Its SQLite file allows one writer. Litestream copies changes
  every five seconds and once more on a clean stop, so a crash can lose the
  last five seconds. During a rollout the old instance and the new overlap
  briefly, and what the old one writes then may be lost too. While the hub
  sleeps (no instances), mirrors do not refresh and webhooks do not retry;
  pushes still work and are caught up on when it wakes.
- **Requests end after an hour**, which bounds the largest clone.
