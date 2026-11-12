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

## Signing in elsewhere

Each of these is switched on by giving its client id, and then shows as a
button on the sign-in page. Register the hub there as a web application with
the redirect address `<HUB_PUBLIC_URL>/api/auth/<id>/callback`.

| `<id>` | Set | |
|---|---|---|
| `entra` | `HUB_ENTRA_TENANT`, `HUB_ENTRA_CLIENT_ID`, `HUB_ENTRA_CLIENT_SECRET` | Microsoft Entra ID. The tenant is the directory's id: accounts of that directory only. |
| `google` | `HUB_GOOGLE_CLIENT_ID`, `HUB_GOOGLE_CLIENT_SECRET`, `HUB_GOOGLE_DOMAIN` | Google. With the domain (or several, separated by commas) only accounts of that Workspace are let in; without it, any Google account. |
| `github` | `HUB_GITHUB_CLIENT_ID`, `HUB_GITHUB_CLIENT_SECRET` | A GitHub OAuth app. Any GitHub account. |
| `oidc` | `HUB_OIDC_ISSUER`, `HUB_OIDC_CLIENT_ID`, `HUB_OIDC_CLIENT_SECRET`, `HUB_OIDC_NAME` | Any other OpenID Connect provider (Okta, Keycloak, …); the name is what the button says. |

| | |
|---|---|
| `HUB_PUBLIC_URL` | Where browsers reach the hub (default `http://127.0.0.1:3000`). |
| `HUB_PASSWORD_LOGIN=0` | No accounts with passwords: signing in elsewhere is the only way in. |
| `HUB_GITHUB_URL`, `HUB_GITHUB_API_URL` | For a GitHub Enterprise Server: `https://ghe.example` and `https://ghe.example/api/v3`. Mirrors are fetched from there too. |

Whoever arrives for the first time gets an account, named after their
account there (`alice@corp.example` becomes `alice`, or `alice-2` if that is
taken) and known from then on by the provider's id for them, not by name or
address. Such an account has no password: git takes an
[access token](http://127.0.0.1:3000/settings/tokens).

## Mirrors

A new repository can start as a mirror of one on GitHub (`owner/name`). The
hub fetches its branches and tags with git, into a copy of its own under
`HUB_MIRROR_DIR` (default `.data/mirrors`), and hands Strata a bundle of what
changed: the import sets the refs that moved and deletes those that are gone.
It fetches again every `HUB_MIRROR_MINUTES` (default 60; 0 for only when
"Fetch now" is pressed). Optionally the heads of the source's pull requests
come along as `refs/pull/<n>/head`, which is a quick way to a repository with
very many refs. A private source takes an access token, which is stored in
the hub's database as it is. A mirror refuses pushes and merges until it is
told to let go of its source in its settings.

## What uses what

| The hub does | through Strata's |
|---|---|
| Decides who may fetch and push, and maps `owner/name` to a repository id (`server/hooks.ts`) | authorization hook |
| Protects the default branch: from force pushes and deletion, from pushes that are not a merged pull request, from commits not signed with a registered key (`server/hooks.ts`, `server/signing.ts`) | pre-receive hook |
| Records pushes and imports as activity; picks a default branch on a first push; closes pull requests whose branch went and marks those merged by a push (`server/events.ts`, `server/pulls.ts`) | event feed |
| Creates and deletes repositories, sets the default branch (`server/api.ts`) | management API |
| Shows files, history, what a commit changed and whether its signature is from a key a user registered (`server/api.ts`, `server/signing.ts`, `web/`) | content API |
| Shows what a pull request holds and whether it merges, and merges it (`server/pulls.ts`) | compare and merge |
| Keeps mirrors of repositories on GitHub (`server/mirror.ts`) | import |

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
  An account made by signing in elsewhere cannot be joined to one with a
  password, or to one from another provider; nobody is signed out here when
  their account there is closed, and their access tokens keep working.
- Mirrors go one way, from github.com or one GitHub Enterprise Server, one
  at a time. The hub keeps a full copy of each source on its own disk.
- File history and blame are not there; diffs are
  computed in the browser and skip large and binary files.
- The directory listing shows the branch's latest commit, not each entry's.
