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
| `STRATA_SSH_URL` | Where git clients reach Strata over SSH, if it was started with `--ssh-listen`: `ssh://git@host:port`. Clone URLs then come in both kinds. |
| `STRATA_ADMIN_TOKEN` | Strata's `--admin-token`. |
| `STRATA_HOOK_SECRET` | Strata's `--hook-secret`. |
| `HUB_PORT`, `HUB_HOST`, `HUB_DB` | Where the hub listens (127.0.0.1:3000) and keeps its SQLite file. |
| `HUB_SECURE_COOKIES=1` | When the hub is served over HTTPS. |

and Strata with `--authz-url http://<hub>/hooks/authz --pre-receive-url http://<hub>/hooks/pre-receive`.

As a container: `docker build -t strata-hub examples/hub`. It listens on
0.0.0.0:8080 and keeps its SQLite file at `/data/hub.sqlite`. Where the
container's disk does not outlive it, set `HUB_DB_REPLICA` (`gs://bucket/path`
or `s3://bucket/path`): the hub then runs under [Litestream](https://litestream.io),
which fetches the file from there on start and copies every change back. One
instance only. [examples/cloud-run](../cloud-run) uses it.

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

## SSH

`bun run start` has Strata listen for SSH on :2222. Add the public half of a
key under "SSH and signing keys", and git is let in with it as you:

```sh
git clone ssh://git@127.0.0.1:2222/<you>/<repo>.git
```

The same keys verify signed commits. A key belongs to one account.

## Teams

An organization's owners can do anything with its repositories. Its other
members get what the organization gives every member (read and push, read,
or nothing: set on the organization's page), and what their teams add: a
team is given repositories, each to read, to push to, or to administer.

A team can follow a group at a sign-in provider. Whoever signs in as part of
that group joins the team, and its organization; whoever signs in without it
leaves the team, and the organization too if a group is what brought them
in. Where the groups come from:

| Provider | The group is named by | Needs |
|---|---|---|
| Entra ID | the group's object id | The app registration's token configuration to include the `groups` claim. Someone in more than 200 groups gets no list from Entra; their teams are then left as they are. |
| Other OpenID Connect | a value of the token's `groups` claim | The provider to send that claim. |
| GitHub | `organization/team` | The `read:org` scope, which the hub asks for. |
| Google | (not supported) | Google's ID token does not say which groups someone is in. |

Memberships change at sign-in, not in between: someone removed from a group
keeps what the team gave them until their session here ends and they sign in
again (sessions last 30 days), and their access tokens and SSH keys keep
working until then.

## Branch rules, reviews and checks

A repository's settings hold rules for branches, by name or by pattern
(`release/*`). A rule can refuse force pushes and deletion, refuse pushes
that are not a merged pull request, refuse commits not signed with a
registered key, and ask of a pull request, before it merges:

- **Approvals**: so many approving reviews of its latest commit, from people
  other than its author who can push to the repository. A new commit makes
  earlier approvals stale. A request for changes blocks until the same
  reviewer approves.
- **Checks**: named commit statuses that must have succeeded on its latest
  commit.

A status is reported by a program, with an access token of someone who can
push:

```sh
curl -X POST http://127.0.0.1:3000/api/repos/<owner>/<repo>/statuses/<commit id> \
  -H "authorization: Bearer hub_..." -H "content-type: application/json" \
  -d '{"state": "success", "context": "ci/build", "description": "Built", "targetUrl": "https://ci.example.com/1"}'
```

`state` is `pending`, `success`, `failure` or `error`. Every route of the
API takes an access token this way.

**Comments on lines.** In a pull request's changed files, a line takes a
comment, which starts a conversation others answer in and anyone in the pull
request can mark settled. A conversation belongs to a line of one version of
a file: when the branch brings another version, it is kept as outdated and
shown in the pull request's conversation, no longer on the line.

## Search, blame and history

The box at the top finds repositories by a word in their name or
description. Inside a repository, "Search code" looks through the files of
one branch for text or a regular expression; a file's page has Blame (who
last changed each line) and History (the commits that changed it). All
three are answered by Strata from the objects each time, without an index:
fine for a repository of some thousands of files, slow for a very large one.

## Large files

git-lfs works against the clone URL as it is, over HTTP and SSH: Strata
serves the LFS API and keeps the files. For SSH, Strata needs
`--public-url` (`bun run start` sets it). A file kept this way is shown
with its real size, and Raw and Download give its content.

## Audit log

Changes to access and settings are written down with who made them: sign-ins
(failed ones too), tokens and keys, members and teams (also when a sign-in
group moves someone), repositories made, deleted, renamed or hidden, branch
rules, webhooks, and merged pull requests with who had approved them. An
organization's owners read its log on its page, a repository's on its
settings page, and everyone their own under "Audit log". Entries are kept
when what they are about is deleted, and are never removed. Pushes are not
in it: they are each repository's activity.

## Webhooks

A repository's settings also hold webhooks: addresses that get a POST with a
JSON body for the events they asked for.

| `X-Hub-Event` | When | In the body |
|---|---|---|
| `push` | A ref moved, by push or merge. One per ref. | `ref`, `before`, `after`, `change`, `created`, `deleted`, `forced` |
| `pull_request` | `action`: `opened`, `synchronize` (its branch moved), `closed`, `reopened`, `merged` | `number`, `pull_request` |
| `pull_request_review` | A review was given | `number`, `review` |
| `status` | A commit status was reported | `sha`, `context`, `state`, `description`, `target_url` |
| `ping` | The webhook was made, or asked to | `hook` |

All carry `repository` and `sender`. With a secret, `X-Hub-Signature-256` is
`sha256=` and the body's HMAC-SHA256 under it. An answer other than 2xx, or
none within 10 seconds, is tried again after 10 s, 1 min, 5 min and 30 min;
the settings page lists the last deliveries and can send one again.
Deliveries are made one at a time in the order things happened, so one slow
receiver holds up the rest. Pushes are heard of from Strata's event feed, a
moment after they happen.

Webhooks to loopback and private addresses are refused unless
`HUB_WEBHOOKS_INSIDE=1` (`bun run start` sets it): whoever owns a repository
could otherwise have the hub call into its own network. The address is
looked up again at each delivery and redirects are not followed; a name that
changes where it points between the lookup and the request is not guarded
against.

## What uses what

| The hub does | through Strata's |
|---|---|
| Decides who may fetch and push, by password, access token or SSH key, and maps `owner/name` to a repository id (`server/hooks.ts`) | authorization hook |
| Holds branches to their rules: no force pushes or deletion, nothing but merged pull requests, only commits signed with a registered key (`server/hooks.ts`, `server/rules.ts`, `server/signing.ts`) | pre-receive hook |
| Records pushes and imports as activity; picks a default branch on a first push; closes pull requests whose branch went and marks those merged by a push; tells webhooks (`server/events.ts`, `server/pulls.ts`, `server/webhooks.ts`) | event feed |
| Creates and deletes repositories, sets the default branch (`server/api.ts`) | management API |
| Shows files, history, what a commit changed and whether its signature is from a key a user registered (`server/api.ts`, `server/signing.ts`, `web/`) | content API |
| Shows what a pull request holds and whether it merges, and merges it (`server/pulls.ts`) | compare and merge |
| Keeps mirrors of repositories on GitHub (`server/mirror.ts`) | import |

Strata knows a repository as `r-<uuid>`. Renaming one, or changing who may
see it, is a row updated in the hub's database and nothing else.

## What it leaves out

It is an example, not a product:

- No issues, forks, search or email.
- Pull requests are between branches of one repository, with a conversation,
  reviews and a merge button (merge commit, squash or rebase). No asking
  someone in particular for a review, no code owners,
  and conflicts are settled on your machine. Merged branches are not deleted
  for you.
- Branch rules bind everyone, owners included: there is no bypass. A rule
  that asks for signed commits does not hold for the commits the hub makes
  when it merges.
- A user's repositories are theirs alone: no collaborators from outside.
  Teams do not nest.
- Webhook secrets are kept as given in the hub's database, as mirror tokens
  are. There are no webhooks for a whole organization.
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
- Diffs are computed in the browser and skip large and binary files.
  Search is of one repository's one branch at a time, not across
  repositories, and not of pull requests.
- Comments on lines do not hold up a merge when left unsettled, are not
  sent to webhooks, and cannot be attached to a review or span several
  lines.
- The audit log has no export, no retention setting and no record of who
  read what. It lives in the hub's database, so whoever can write there can
  change it.
- The directory listing shows the branch's latest commit, not each entry's.
