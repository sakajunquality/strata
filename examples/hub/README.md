# Strata Hub

A small code host built on Strata, to show where the line between the engine
and an application runs. The hub owns accounts, organizations, repository
names, permissions and the UI. Strata owns every git object and ref. They
talk over HTTP only, and the hub never touches a repository's storage.

Bun, Hono and SQLite on the server; React, Vite and Ant Design in the browser.

## Run

```sh
cargo build            # in the repository root: the hub starts target/debug/strata
cd examples/hub
bun install
bun run start          # builds the UI, then runs Strata (:8080) and the hub (:3000)
```

Open http://127.0.0.1:3000, create an account and a repository, and push:

```sh
git remote add origin http://127.0.0.1:8080/<you>/<repo>.git
git push -u origin main    # user name, and your password or an access token
```

`bun run dev` does the same with Vite's dev server on :5173 and reloads on
change. Data stays in `.data/`. `bun test` runs the end-to-end test, which
starts both servers and drives them with git.

To run the hub against a Strata started elsewhere, `bun run server` with:

| | |
|---|---|
| `STRATA_URL` | Where the hub reaches Strata. |
| `STRATA_PUBLIC_URL` | Where git clients reach it, for clone URLs. Defaults to `STRATA_URL`. |
| `STRATA_ADMIN_TOKEN` | Strata's `--admin-token`. |
| `STRATA_HOOK_SECRET` | Strata's `--hook-secret`. |
| `HUB_PORT`, `HUB_HOST`, `HUB_DB` | Where the hub listens (127.0.0.1:3000) and keeps its SQLite file. |
| `HUB_SECURE_COOKIES=1` | When the hub is served over HTTPS. |

and Strata with `--authz-url http://<hub>/hooks/authz --pre-receive-url http://<hub>/hooks/pre-receive`.

## What uses what

| The hub does | through Strata's |
|---|---|
| Decides who may fetch and push, and maps `owner/name` to a repository id (`server/hooks.ts`) | authorization hook |
| Protects the default branch: from force pushes and deletion, from pushes that are not a merged pull request, from commits not signed with a registered key (`server/hooks.ts`, `server/signing.ts`) | pre-receive hook |
| Records pushes as activity; picks a default branch on a first push; closes pull requests whose branch went and marks those merged by a push (`server/events.ts`, `server/pulls.ts`) | event feed |
| Creates and deletes repositories, sets the default branch (`server/api.ts`) | management API |
| Shows files, history, what a commit changed and whether its signature is from a key a user registered (`server/api.ts`, `server/signing.ts`, `web/`) | content API |
| Shows what a pull request holds and whether it merges, and merges it (`server/pulls.ts`) | compare and merge |

Strata knows a repository as `r-<uuid>`. Renaming one, or changing who may
see it, is a row updated in the hub's database and nothing else.

## What it leaves out

It is an example, not a product:

- No issues, forks, search, webhooks or email.
- Pull requests are between branches of one repository, with a conversation
  and a merge button (merge commit, squash or rebase). No reviews or
  approvals, no comments on lines, and conflicts are settled on your machine.
  Merged branches are not deleted for you.
- Permissions are per owner: a user's repositories are theirs alone, and an
  organization's members can push to all of its repositories.
- Signatures are checked against the SSH and OpenPGP keys users register;
  X.509 is not. A key's expiry or revocation is not looked at, and the signer
  is not required to be the committer. Commits the hub makes when it merges
  a pull request are not signed.
- No sign-in rate limiting, password reset or two-factor authentication.
- File history and blame are not there; diffs are
  computed in the browser and skip large and binary files.
- The directory listing shows the branch's latest commit, not each entry's.
