# Security policy

Strata is experimental software. Security fixes go to `main`; there are no
releases to backport to.

## Report a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/sakajunquality/strata/security/advisories/new).
Say which commit you tried, how to reproduce it, and what you expected to be
kept apart. Leave out real tokens, private keys and repository contents.

If that form is not available, open an issue asking for a private channel,
without the details. Do not describe the vulnerability in a public issue or
pull request. Answers are best effort.

## What Strata takes on trust

- **The application.** Whoever holds the admin token (`STRATA_ADMIN_TOKEN`)
  can do everything through the management API. The authorization and
  pre-receive hooks decide every git request; Strata does what they answer.
- **A server started without an admin token** allows everything. That is how
  it is meant to behave, for a trusted network only; so is `--anonymous-read`
  letting anyone fetch.
- **The metadata store and the object store.** Anyone who can write to them
  can change what repositories contain.

Worth reporting: a git client or an HTTP caller reading or writing a
repository the application did not allow, getting past the admin token or the
hook secret, a pack or request that makes the server read or write outside
its stores, and one that takes the server down for everyone.

## The hub

`examples/hub` is an example of an application, not a product. What it leaves
out is listed in [its README](examples/hub/README.md): among other things it
has no sign-in rate limiting and no two-factor authentication. Reports about
it are welcome, but do not run it as it is where that matters.

## Checks

Every week and on every change to a lockfile, the `audit` workflow runs
`cargo audit` and `bun audit`. Dependabot proposes updates weekly for Cargo,
Bun, GitHub Actions and the base images.
