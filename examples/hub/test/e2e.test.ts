// Runs Strata and the hub as they run for real, and drives them with git and
// the hub's API. Needs the Strata binary: `cargo build` in the repository root.
import { afterAll, beforeAll, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { exportJWK, generateKeyPair, SignJWT } from "jose";

const root = resolve(import.meta.dir, "..");
const strataBin = process.env.STRATA_BIN ?? resolve(root, "../../target/debug/strata");
const strataPort = 18190 + Math.floor(Math.random() * 500);
const hubPort = strataPort + 1000;
const hub = `http://127.0.0.1:${hubPort}`;
const elsewherePort = strataPort + 2000;
const elsewhere = `http://127.0.0.1:${elsewherePort}`;
const sshPort = strataPort + 3000;
const tmp = mkdtempSync(join(tmpdir(), "strata-hub-"));
const children: Bun.Subprocess[] = [];
// gpg talks to its agent over a socket in here, and socket paths have to be short.
const gnupg = mkdtempSync("/tmp/hub-gpg-");
const gpgEnv = { PATH: process.env.PATH!, GNUPGHOME: gnupg };

async function up(url: string) {
  for (let i = 0; i < 100; i++) {
    if (await fetch(url).then((r) => r.status < 500, () => false)) return;
    await Bun.sleep(100);
  }
  throw new Error(`${url} did not come up`);
}

/**
 * Everything the hub reaches outside itself, on one port: an OpenID Connect
 * provider under /oidc, GitHub's sign-in and API, and GitHub's repositories,
 * which are the ones in Strata.
 */
const outside = {
  /** Who signs in next. */
  person: { sub: "", preferred_username: "", name: "", id: 0, login: "", groups: null as string[] | null, teams: [] as string[] },
  /** What webhooks delivered to /webhook. */
  delivered: [] as { event: string; signature: string; body: string }[],
  /** Sign-ins under way, by the code handed out: what the token has to answer. */
  codes: new Map<string, { nonce: string; challenge: string }>(),
};

async function startElsewhere() {
  const { publicKey, privateKey } = await generateKeyPair("RS256");
  const jwk = { ...(await exportJWK(publicKey)), kid: "k1", alg: "RS256", use: "sig" };
  const s256 = (text: string) => new Bun.CryptoHasher("sha256").update(text).digest("base64url");
  const authorize = (url: URL) => {
    const code = crypto.randomUUID();
    outside.codes.set(code, { nonce: url.searchParams.get("nonce") ?? "", challenge: url.searchParams.get("code_challenge") ?? "" });
    return Response.redirect(`${url.searchParams.get("redirect_uri")}?${new URLSearchParams({ code, state: url.searchParams.get("state") ?? "" })}`, 302);
  };
  /** The sign-in a token request is for, if it proves to come from who started it. */
  const redeem = async (request: Request, secret: string) => {
    const form = new URLSearchParams(await request.text());
    const asked = outside.codes.get(form.get("code") ?? "");
    outside.codes.delete(form.get("code") ?? "");
    return asked && form.get("client_secret") === secret && s256(form.get("code_verifier") ?? "") === asked.challenge ? asked : null;
  };
  Bun.serve({
    port: elsewherePort,
    hostname: "127.0.0.1",
    async fetch(request) {
      const url = new URL(request.url);
      switch (url.pathname) {
        case "/oidc/.well-known/openid-configuration":
          return Response.json({ issuer: `${elsewhere}/oidc`, authorization_endpoint: `${elsewhere}/oidc/authorize`, token_endpoint: `${elsewhere}/oidc/token`, jwks_uri: `${elsewhere}/oidc/jwks` });
        case "/oidc/jwks":
          return Response.json({ keys: [jwk] });
        case "/oidc/authorize":
        case "/login/oauth/authorize":
          return authorize(url);
        case "/oidc/token": {
          const asked = await redeem(request, "oidc-secret");
          if (!asked) return Response.json({ error: "invalid_grant" }, { status: 400 });
          const { sub, preferred_username, name } = outside.person;
          const token = new SignJWT({ nonce: asked.nonce, preferred_username, name, ...(outside.person.groups ? { groups: outside.person.groups } : {}) }).setProtectedHeader({ alg: "RS256", kid: "k1" }).setIssuer(`${elsewhere}/oidc`).setAudience("hub-at-oidc").setSubject(sub).setIssuedAt().setExpirationTime("5m");
          return Response.json({ id_token: await token.sign(privateKey), token_type: "Bearer", access_token: "unused" });
        }
        case "/login/oauth/access_token":
          return Response.json((await redeem(request, "github-secret")) ? { access_token: "gho_test", token_type: "bearer" } : { error: "bad_verification_code" });
        case "/api/user":
          if (request.headers.get("authorization") !== "Bearer gho_test") return new Response("", { status: 401 });
          return Response.json({ id: outside.person.id, login: outside.person.login, name: outside.person.name });
      }
      if (url.pathname === "/api/user/teams") {
        return Response.json(outside.person.teams.map((team) => ({ slug: team.split("/")[1], organization: { login: team.split("/")[0] } })));
      }
      if (url.pathname === "/webhook") {
        outside.delivered.push({ event: request.headers.get("x-hub-event") ?? "", signature: request.headers.get("x-hub-signature-256") ?? "", body: await request.text() });
        return new Response("ok");
      }
      // Anything else is a repository being fetched.
      const headers = new Headers(request.headers);
      headers.delete("host");
      return fetch(`http://127.0.0.1:${strataPort}${url.pathname}${url.search}`, { method: request.method, headers, body: request.method === "POST" ? await request.arrayBuffer() : undefined, decompress: false } as RequestInit);
    },
  });
}

beforeAll(async () => {
  if (!existsSync(strataBin)) throw new Error(`build Strata first: ${strataBin} is missing`);
  await startElsewhere();
  const env = {
    ...process.env,
    HUB_PUBLIC_URL: hub,
    HUB_OIDC_ISSUER: `${elsewhere}/oidc`,
    HUB_OIDC_NAME: "Example",
    HUB_OIDC_CLIENT_ID: "hub-at-oidc",
    HUB_OIDC_CLIENT_SECRET: "oidc-secret",
    HUB_GITHUB_URL: elsewhere,
    HUB_GITHUB_API_URL: `${elsewhere}/api`,
    HUB_GITHUB_CLIENT_ID: "hub-at-github",
    HUB_GITHUB_CLIENT_SECRET: "github-secret",
    HUB_MIRROR_DIR: join(tmp, "mirrors"),
    HUB_MIRROR_MINUTES: "0",
    // What stands in for a webhook's receiver is on this machine.
    HUB_WEBHOOKS_INSIDE: "1",
    STRATA_SSH_URL: `ssh://git@127.0.0.1:${sshPort}`,
    HUB_PORT: String(hubPort),
    HUB_DB: join(tmp, "hub.sqlite"),
    STRATA_URL: `http://127.0.0.1:${strataPort}`,
    STRATA_ADMIN_TOKEN: "test-admin-token",
    STRATA_HOOK_SECRET: "test-hook-secret",
  };
  const spawn = (cmd: string[]) => children.push(Bun.spawn(cmd, { cwd: root, env, stdout: "ignore", stderr: "inherit" }));
  spawn([
    strataBin,
    "--listen", `127.0.0.1:${strataPort}`,
    "--ssh-listen", `127.0.0.1:${sshPort}`,
    "--public-url", `http://127.0.0.1:${strataPort}`,
    "--data-dir", join(tmp, "strata"),
    "--authz-url", `${hub}/hooks/authz`,
    "--pre-receive-url", `${hub}/hooks/pre-receive`,
    "--authz-cache-secs", "0",
  ]);
  spawn(["bun", "run", "server/index.ts"]);
  await Promise.all([up(`http://127.0.0.1:${strataPort}/healthz`), up(`${hub}/api/me`)]);
});

afterAll(() => {
  for (const child of children) child.kill();
  Bun.spawnSync(["gpgconf", "--kill", "all"], { env: gpgEnv });
  rmSync(tmp, { recursive: true, force: true });
  rmSync(gnupg, { recursive: true, force: true });
});

/** A browser, as far as the API can tell: remembers its session cookie. */
function browser() {
  let cookie = "";
  return async (method: string, path: string, body?: unknown) => {
    const response = await fetch(`${hub}/api${path}`, {
      method,
      // Browsers name the page a request comes from; the hub refuses bodiless and form requests from anywhere else.
      headers: { origin: hub, ...(cookie ? { cookie } : {}), ...(body === undefined ? {} : { "content-type": "application/json" }) },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const set = response.headers.get("set-cookie");
    if (set) cookie = set.split(";")[0]!;
    const text = await response.text();
    let data: any = text;
    try {
      data = JSON.parse(text);
    } catch {}
    return { status: response.status, data, headers: response.headers };
  };
}

function git(dir: string, ...args: string[]) {
  return gitWith({}, dir, ...args);
}

/** git over SSH, as whoever holds the key in that file. */
function gitAs(key: string, dir: string, ...args: string[]) {
  const ssh = `ssh -F /dev/null -i ${key} -o IdentitiesOnly=yes -o IdentityAgent=none -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes`;
  return gitWith({ GIT_SSH_COMMAND: ssh }, dir, ...args);
}

function gitWith(more: Record<string, string>, dir: string, ...args: string[]) {
  const result = Bun.spawnSync(["git", ...args], {
    cwd: dir,
    env: {
      ...more,
      PATH: process.env.PATH!,
      HOME: tmp,
      GNUPGHOME: gnupg,
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_CONFIG_NOSYSTEM: "1",
      GIT_TERMINAL_PROMPT: "0",
      GIT_AUTHOR_NAME: "Alice",
      GIT_AUTHOR_EMAIL: "alice@example.com",
      GIT_COMMITTER_NAME: "Alice",
      GIT_COMMITTER_EMAIL: "alice@example.com",
    },
  });
  return { ok: result.exitCode === 0, out: result.stdout.toString().trim(), err: result.stderr.toString() };
}

const remote = (credentials: string, path: string) => `http://${credentials ? `${credentials}@` : ""}127.0.0.1:${strataPort}/${path}.git`;

async function eventually<T>(what: () => Promise<T | undefined | false>): Promise<T> {
  for (let i = 0; i < 100; i++) {
    const value = await what();
    if (value) return value;
    await Bun.sleep(100);
  }
  throw new Error("did not happen in time");
}

test("a small code host, end to end", async () => {
  const [alice, bob, anyone] = [browser(), browser(), browser()];
  expect((await alice("POST", "/signup", { username: "alice", password: "correct horse" })).status).toBe(201);
  expect((await bob("POST", "/signup", { username: "bob", password: "battery staple" })).status).toBe(201);
  expect((await anyone("POST", "/signup", { username: "Alice", password: "something long" })).status).toBe(409);
  expect((await anyone("POST", "/signup", { username: "settings", password: "something long" })).status).toBe(400);
  expect((await anyone("POST", "/login", { username: "alice", password: "wrong" })).status).toBe(401);
  expect((await alice("GET", "/me")).data.user.name).toBe("alice");

  expect((await alice("POST", "/orgs", { name: "acme" })).status).toBe(201);
  expect((await anyone("POST", "/repos", { owner: "acme", name: "widget" })).status).toBe(401);
  expect((await bob("POST", "/repos", { owner: "acme", name: "widget" })).status).toBe(403);
  expect((await alice("POST", "/repos", { owner: "acme", name: "widget", private: true, description: "A thing" })).status).toBe(201);
  expect((await alice("POST", "/repos", { owner: "acme", name: "Widget" })).status).toBe(409);
  expect((await alice("POST", "/repos", { owner: "acme", name: "bad name" })).status).toBe(400);

  let detail = (await alice("GET", "/repos/acme/widget")).data;
  expect(detail).toMatchObject({ empty: true, private: true, permission: "admin", defaultBranch: "main", branches: [] });
  expect(detail.cloneUrl).toBe(remote("", "acme/widget"));
  // Private: indistinguishable from missing, for everyone else.
  expect((await bob("GET", "/repos/acme/widget")).status).toBe(404);
  expect((await anyone("GET", "/repos/acme/widget/tree")).status).toBe(404);

  const token: string = (await alice("POST", "/tokens", { name: "laptop" })).data.token;
  expect(token).toStartWith("hub_");
  const bobToken: string = (await bob("POST", "/tokens", { name: "laptop" })).data.token;

  // A first push, of a branch that is not called "main".
  const work = join(tmp, "work");
  git(tmp, "init", "-q", "-b", "trunk", work);
  writeFileSync(join(work, "README.md"), "# Widget\n\nHello.\n");
  Bun.spawnSync(["mkdir", "-p", join(work, "src")]);
  writeFileSync(join(work, "src/main.rs"), "fn main() {}\n");
  git(work, "add", ".");
  git(work, "commit", "-q", "-m", "First commit");
  const first = git(work, "rev-parse", "HEAD").out;
  expect(git(work, "push", "-q", remote("", "acme/widget"), "trunk").ok).toBe(false);
  expect(git(work, "push", "-q", remote("alice:wrong-password", "acme/widget"), "trunk").ok).toBe(false);
  expect(git(work, "push", "-q", remote(`bob:${bobToken}`, "acme/widget"), "trunk").err).toContain("not found");
  const pushed = git(work, "push", "-q", remote(`alice:${token}`, "acme/widget"), "trunk");
  expect(pushed.err).toBe("");

  // The hub hears of it through the event feed, and makes that branch the default.
  const activity = await eventually(async () => {
    const { data } = await alice("GET", "/repos/acme/widget/activity");
    return data.activity.length > 0 && data.activity;
  });
  expect(activity[0]).toMatchObject({ actor: "alice", owner: "acme", repo: "widget" });
  expect(activity[0].updates).toEqual([{ ref: "refs/heads/trunk", old: null, new: first, change: "create" }]);
  detail = await eventually(async () => {
    const { data } = await alice("GET", "/repos/acme/widget");
    return !data.empty && data;
  });
  expect(detail).toMatchObject({ defaultBranch: "trunk", branches: [{ name: "trunk", oid: first }] });
  expect(detail.pushedAt).toBeGreaterThan(0);
  expect((await alice("GET", "/activity")).data.activity).toHaveLength(1);
  expect((await bob("GET", "/activity")).data.activity).toHaveLength(0);

  // Reading code.
  const tree = (await alice("GET", "/repos/acme/widget/tree")).data;
  expect(tree.entries.map((entry: any) => [entry.name, entry.type])).toEqual([["README.md", "blob"], ["src", "tree"]]);
  const blob = await alice("GET", "/repos/acme/widget/blob?path=src/main.rs");
  expect(blob.data).toBe("fn main() {}\n");
  expect(blob.headers.get("content-security-policy")).toContain("sandbox");
  expect((await alice("GET", "/repos/acme/widget/blob?path=nope")).status).toBe(404);
  const commits = (await alice("GET", "/repos/acme/widget/commits?rev=trunk")).data;
  expect(commits.commits).toHaveLength(1);
  expect(commits.commits[0]).toMatchObject({ oid: first, message: "First commit\n", author: { name: "Alice" } });
  const shown = (await alice("GET", `/repos/acme/widget/commit?rev=${first}`)).data;
  expect(shown.files.map((file: any) => [file.path, file.status])).toEqual([["README.md", "added"], ["src/main.rs", "added"]]);

  // Membership opens the door; the account password works for git as well.
  expect((await bob("PUT", "/owners/acme/members/bob", { role: "owner" })).status).toBe(403);
  expect((await alice("PUT", "/owners/acme/members/bob", { role: "member" })).status).toBe(200);
  expect((await alice("DELETE", "/owners/acme/members/alice")).status).toBe(409);
  expect((await bob("GET", "/repos/acme/widget")).data.permission).toBe("write");
  expect((await bob("PATCH", "/repos/acme/widget", { private: false })).status).toBe(403);
  writeFileSync(join(work, "src/main.rs"), 'fn main() { println!("hi"); }\n');
  git(work, "commit", "-q", "-am", "Say hi");
  expect(git(work, "push", "-q", remote("bob:battery%20staple", "acme/widget"), "trunk").err).toBe("");

  // Rules for the default branch are the hub's, enforced before the push commits.
  const guarded = await alice("POST", "/repos/acme/widget/rules", { pattern: "tr*" });
  expect(guarded.data.rules).toMatchObject([{ pattern: "tr*", noForce: true, noDelete: true, requirePulls: false }]);
  expect((await bob("POST", "/repos/acme/widget/rules", { pattern: "x" })).status).toBe(403);
  expect((await alice("GET", "/repos/acme/widget")).data.branches).toMatchObject([{ name: "trunk", protected: true }]);
  git(work, "commit", "-q", "--amend", "-m", "Say hi, rewritten");
  const forced = git(work, "push", "-q", "--force", remote(`bob:${bobToken}`, "acme/widget"), "trunk");
  expect(forced.ok).toBe(false);
  expect(forced.err).toContain("trunk is protected and does not accept force pushes");
  expect(git(work, "push", "-q", remote(`bob:${bobToken}`, "acme/widget"), ":trunk").err).toContain("cannot be deleted");
  expect(git(work, "push", "-q", "--force", remote(`bob:${bobToken}`, "acme/widget"), "trunk:topic/x").err).toBe("");
  expect((await alice("DELETE", `/repos/acme/widget/rules/${guarded.data.rules[0].id}`)).status).toBe(200);
  expect(git(work, "push", "-q", "--force", remote(`bob:${bobToken}`, "acme/widget"), "trunk").err).toBe("");
  const head = git(work, "rev-parse", "HEAD").out;

  // A rename is the hub's alone: the same repository answers at the new path.
  expect((await alice("PATCH", "/repos/acme/widget", { name: "gadget" })).data).toEqual({ owner: "acme", name: "gadget" });
  expect(git(tmp, "ls-remote", remote(`alice:${token}`, "acme/widget")).ok).toBe(false);
  expect(git(tmp, "clone", "-q", remote(`alice:${token}`, "acme/gadget"), join(tmp, "clone")).err).toBe("");
  expect(git(join(tmp, "clone"), "rev-parse", "HEAD").out).toBe(head);
  expect(git(join(tmp, "clone"), "symbolic-ref", "--short", "HEAD").out).toBe("trunk");
  expect((await alice("GET", "/repos/acme/gadget")).data.branches.map((b: any) => b.name)).toEqual(["topic/x", "trunk"]);
  expect((await alice("GET", "/repos/acme/gadget/commits?rev=topic/x")).data.commits).toHaveLength(2);

  // Public: anyone reads, only those with access write.
  expect((await alice("PATCH", "/repos/acme/gadget", { private: false })).status).toBe(200);
  expect((await anyone("GET", "/repos/acme/gadget")).data.permission).toBe("read");
  expect((await anyone("GET", "/repos")).data.repos.map((r: any) => `${r.owner}/${r.name}`)).toEqual(["acme/gadget"]);
  expect(git(tmp, "ls-remote", remote("", "acme/gadget")).out).toContain("refs/heads/trunk");
  expect(git(work, "push", "-q", remote("", "acme/gadget"), "trunk:anonymous").ok).toBe(false);
  expect((await alice("DELETE", "/owners/acme/members/bob")).status).toBe(200);
  expect(git(work, "push", "-q", remote(`bob:${bobToken}`, "acme/gadget"), "trunk:bobs").err).toContain("bob may not push to acme/gadget");
  expect((await anyone("GET", "/owners/acme")).data).toMatchObject({ kind: "org", members: [{ name: "alice", role: "owner" }], role: null });

  // Deleting.
  expect((await bob("DELETE", "/repos/acme/gadget")).status).toBe(403);
  expect((await alice("DELETE", "/repos/acme/gadget")).status).toBe(200);
  expect((await alice("GET", "/repos/acme/gadget")).status).toBe(404);
  expect(git(tmp, "ls-remote", remote(`alice:${token}`, "acme/gadget")).ok).toBe(false);

  // A revoked token and a closed session stop working.
  const tokens = (await alice("GET", "/tokens")).data.tokens;
  expect(tokens[0].lastUsedAt).toBeGreaterThan(0);
  // Private, or git would not need to present the token at all.
  await alice("POST", "/repos", { name: "notes", private: true });
  expect(git(tmp, "ls-remote", remote(`alice:${token}`, "alice/notes")).ok).toBe(true);
  await alice("DELETE", `/tokens/${tokens[0].id}`);
  expect(git(tmp, "ls-remote", remote(`alice:${token}`, "alice/notes")).ok).toBe(false);
  await alice("POST", "/logout");
  expect((await alice("GET", "/me")).data.user).toBeNull();
}, 60_000);

test("pull requests", async () => {
  const [carol, dave, anyone] = [browser(), browser(), browser()];
  await carol("POST", "/signup", { username: "carol", password: "carols password", displayName: "Carol C" });
  await dave("POST", "/signup", { username: "dave", password: "daves password" });
  expect((await carol("POST", "/repos", { name: "site" })).status).toBe(201);
  const origin = remote("carol:carols%20password", "carol/site");
  const lines = (changed: Record<number, string>) => Array.from({ length: 10 }, (_, i) => changed[i + 1] ?? String(i + 1)).join("\n") + "\n";
  const work = join(tmp, "site");
  git(tmp, "init", "-q", "-b", "main", work);
  const commit = (file: string, content: string, message: string) => {
    writeFileSync(join(work, file), content);
    git(work, "add", ".");
    git(work, "commit", "-q", "-m", message);
    return git(work, "rev-parse", "HEAD").out;
  };
  commit("a.txt", lines({}), "Start");
  git(work, "checkout", "-q", "-b", "feature");
  const feature = commit("a.txt", lines({ 2: "two" }), "Spell out two");
  expect(git(work, "push", "-q", origin, "main", "feature").err).toBe("");
  const pulls = "/repos/carol/site/pulls";
  const state = async (number: number) => (await carol("GET", `${pulls}/${number}`)).data.state;

  // Proposing: anyone signed in who can see the repository.
  const compared = (await anyone("GET", "/repos/carol/site/compare?base=main&head=feature")).data;
  expect(compared).toMatchObject({ ahead: 1, behind: 0, existing: null, head: feature });
  expect(compared.files.map((file: any) => file.path)).toEqual(["a.txt"]);
  expect((await anyone("POST", pulls, { title: "Feature", base: "main", head: "feature" })).status).toBe(401);
  expect((await dave("POST", pulls, { title: "", base: "main", head: "feature" })).status).toBe(400);
  expect((await dave("POST", pulls, { title: "Same", base: "main", head: "main" })).status).toBe(400);
  expect((await dave("POST", pulls, { title: "Missing", base: "main", head: "nope" })).status).toBe(404);
  expect((await dave("POST", pulls, { title: "Backwards", base: "feature", head: "main" })).status).toBe(409);
  expect((await dave("POST", pulls, { title: "Feature", body: "Please **merge**.", base: "main", head: "feature" })).data).toEqual({ number: 1 });
  expect((await carol("POST", pulls, { title: "Again", base: "main", head: "feature" })).status).toBe(409);
  expect((await anyone("GET", pulls)).data).toMatchObject({ open: 1, closed: 0, pulls: [{ number: 1, title: "Feature", author: "dave", state: "open" }] });
  expect((await anyone("GET", "/repos/carol/site")).data.openPulls).toBe(1);

  let pull = (await dave("GET", `${pulls}/1`)).data;
  expect(pull).toMatchObject({ mergeable: true, conflicts: [], can: { change: true, merge: false }, compare: { ahead: 1 } });
  expect((await anyone("GET", `${pulls}/1`)).data.can).toMatchObject({ change: false, merge: false });
  expect((await anyone("POST", `${pulls}/1/comments`, { body: "hello" })).status).toBe(401);
  expect((await dave("POST", `${pulls}/1/comments`, { body: "  " })).status).toBe(400);
  expect((await dave("POST", `${pulls}/1/comments`, { body: "Ready when you are." })).status).toBe(201);
  expect((await dave("PATCH", `${pulls}/1`, { title: "Spell out two" })).status).toBe(200);
  expect((await dave("POST", `${pulls}/1/merge`, {})).status).toBe(403);

  // The base moves under it, onto the same line.
  git(work, "checkout", "-q", "main");
  commit("a.txt", lines({ 2: "zwei" }), "Zwei");
  git(work, "push", "-q", origin, "main");
  pull = (await carol("GET", `${pulls}/1`)).data;
  expect(pull).toMatchObject({ title: "Spell out two", mergeable: false, conflicts: ["a.txt"], compare: { ahead: 1, behind: 1 } });
  expect(pull.discussion).toMatchObject([{ author: "dave", body: "Ready when you are." }]);
  const refused = await carol("POST", `${pulls}/1/merge`, {});
  expect(refused.status).toBe(409);
  expect(refused.data.error).toBe("conflicting changes in a.txt");
  // Settled the usual way: merge the base into the branch, and push.
  git(work, "checkout", "-q", "feature");
  git(work, "merge", "-q", "main");
  const settled = commit("a.txt", lines({ 2: "two (zwei)" }), "Settle it");
  git(work, "push", "-q", origin, "feature");
  expect((await carol("GET", `${pulls}/1`)).data).toMatchObject({ mergeable: true, compare: { head: settled, behind: 0 } });

  // With pull requests required, the default branch takes nothing else.
  const viaPulls = (await carol("POST", "/repos/carol/site/rules", { pattern: "main", requirePulls: true })).data.rules[0];
  git(work, "checkout", "-q", "main");
  commit("direct.txt", "direct\n", "Straight to main");
  expect(git(work, "push", "-q", origin, "main").err).toContain("main only takes changes through pull requests");
  git(work, "reset", "-q", "--hard", "HEAD~1");
  expect((await carol("POST", `${pulls}/1/merge`, { head: feature })).status).toBe(409);
  const merged = await carol("POST", `${pulls}/1/merge`, { head: settled });
  expect(merged.status).toBe(200);
  git(work, "pull", "-q", origin, "main");
  expect(git(work, "rev-parse", "HEAD").out).toBe(merged.data.oid);
  expect(git(work, "log", "-1", "--format=%an|%s|%p").out).toMatch(/^Carol C\|Merge pull request #1 from feature\|\w+ \w+$/);
  expect(await Bun.file(join(work, "a.txt")).text()).toBe(lines({ 2: "two (zwei)" }));
  pull = (await anyone("GET", `${pulls}/1`)).data;
  expect(pull).toMatchObject({ state: "merged", closedBy: "carol", mergeOid: merged.data.oid, mergeable: null });
  // Still shown as it was: against where it left the base, not the base as it is now.
  expect(pull.compare.files.map((file: any) => file.path)).toEqual(["a.txt"]);
  expect((await carol("POST", `${pulls}/1/merge`, {})).status).toBe(409);
  expect((await carol("PATCH", `${pulls}/1`, { state: "open" })).status).toBe(409);
  const activity = await eventually(async () => {
    const { data } = await carol("GET", "/repos/carol/site/activity");
    return data.activity[0]?.updates[0].new === merged.data.oid && data.activity;
  });
  expect(activity[0]).toMatchObject({ actor: "carol", updates: [{ ref: "refs/heads/main", change: "fast-forward" }] });

  // Closing, reopening, and squashing.
  git(work, "checkout", "-q", "-b", "second");
  commit("b.txt", "b\n", "Add b");
  commit("c.txt", "c\n", "Add c");
  git(work, "push", "-q", origin, "second");
  expect((await carol("POST", pulls, { title: "Second", body: "Two files.", base: "main", head: "second" })).data).toEqual({ number: 2 });
  expect((await dave("PATCH", `${pulls}/2`, { state: "closed" })).status).toBe(403);
  expect((await carol("PATCH", `${pulls}/2`, { state: "closed" })).status).toBe(200);
  expect((await carol("GET", `${pulls}/2`)).data).toMatchObject({ state: "closed", closedBy: "carol", compare: { ahead: 2 } });
  expect((await anyone("GET", `${pulls}?state=closed`)).data.pulls.map((p: any) => [p.number, p.state])).toEqual([[2, "closed"], [1, "merged"]]);
  expect((await carol("PATCH", `${pulls}/2`, { state: "open" })).status).toBe(200);
  expect((await carol("POST", `${pulls}/2/merge`, { method: "squash" })).status).toBe(200);
  git(work, "fetch", "-q", origin, "main");
  expect(git(work, "log", "-1", "--format=%s|%p|%b", "FETCH_HEAD").out).toMatch(/^Second \(#2\)\|\w+\|Two files\.$/);
  expect(git(work, "diff", "--stat", "FETCH_HEAD", "second").out).toBe("");

  // Merged by hand and pushed: the hub notices.
  expect((await carol("DELETE", `/repos/carol/site/rules/${viaPulls.id}`)).status).toBe(200);
  git(work, "checkout", "-q", "-B", "main", "FETCH_HEAD");
  git(work, "checkout", "-q", "-b", "third");
  const third = commit("d.txt", "d\n", "Add d");
  git(work, "checkout", "-q", "-b", "fourth", "main");
  commit("e.txt", "e\n", "Add e");
  git(work, "push", "-q", origin, "third", "fourth");
  expect((await dave("POST", pulls, { title: "Third", base: "main", head: "third" })).data).toEqual({ number: 3 });
  expect((await dave("POST", pulls, { title: "Fourth", base: "main", head: "fourth" })).data).toEqual({ number: 4 });
  git(work, "checkout", "-q", "main");
  git(work, "merge", "-q", "--ff-only", "third");
  expect(git(work, "push", "-q", origin, "main").err).toBe("");
  await eventually(async () => (await state(3)) === "merged");
  expect((await carol("GET", `${pulls}/3`)).data).toMatchObject({ closedBy: null, mergeOid: third, compare: { ahead: 1, head: third } });
  // A branch deleted takes its pull request with it.
  expect(git(work, "push", "-q", origin, ":fourth").err).toBe("");
  await eventually(async () => (await state(4)) === "closed");
  expect((await carol("GET", `${pulls}/4`)).data.compare).toMatchObject({ ahead: 1 });
  expect((await carol("PATCH", `${pulls}/4`, { state: "open" })).status).toBe(404);
  expect((await anyone("GET", pulls)).data).toMatchObject({ open: 0, closed: 4 });
  expect((await anyone("GET", `${pulls}/9`)).status).toBe(404);

  // Rebasing: the branch's commits go on top of the base, with no merge commit.
  git(work, "checkout", "-q", "-b", "fifth", "main~1");
  commit("f.txt", "f\n", "Add f");
  git(work, "push", "-q", origin, "fifth");
  expect((await carol("POST", pulls, { title: "Fifth", base: "main", head: "fifth" })).data).toEqual({ number: 5 });
  const rebased = await carol("POST", `${pulls}/5/merge`, { method: "rebase" });
  expect(rebased.status).toBe(200);
  git(work, "fetch", "-q", origin, "main");
  expect(git(work, "log", "-1", "--format=%s|%an|%cn|%p", "FETCH_HEAD").out).toBe(`Add f|Alice|Carol C|${third.slice(0, 7)}`);
  expect(await state(5)).toBe("merged");
}, 60_000);

test("signed commits", async () => {
  const [erin, anyone] = [browser(), browser()];
  await erin("POST", "/signup", { username: "erin", password: "erins password" });
  expect((await erin("POST", "/repos", { name: "sealed" })).status).toBe(201);
  const origin = remote("erin:erins%20password", "erin/sealed");
  const work = join(tmp, "sealed");
  git(tmp, "init", "-q", "-b", "main", work);
  let n = 0;
  /** Commits a change, signed as the extra git options say, and returns its id. */
  const commit = (message: string, ...signing: string[]) => {
    writeFileSync(join(work, "file.txt"), `${++n}\n`);
    git(work, "add", ".");
    const made = git(work, ...signing, "commit", "-q", "-m", message);
    expect(made.err).toBe("");
    return git(work, "rev-parse", "HEAD").out;
  };
  const sshKey = (name: string, ...kind: string[]) => {
    const path = join(tmp, name);
    expect(Bun.spawnSync(["ssh-keygen", "-q", "-N", "", "-C", name, "-f", path, ...kind]).exitCode).toBe(0);
    return { public: readFileSync(`${path}.pub`, "utf8"), sign: ["-c", "gpg.format=ssh", "-c", `user.signingkey=${path}.pub`, "-c", "commit.gpgsign=true"] };
  };
  const verification = async (oid: string) => (await anyone("GET", `/repos/erin/sealed/commit?rev=${oid}`)).data.verification;

  const plain = commit("Unsigned");
  const laptop = sshKey("laptop", "-t", "ed25519");
  const signed = commit("Signed", ...laptop.sign);
  expect(git(work, "push", "-q", origin, "main").err).toBe("");
  // A signature counts once someone here owns up to the key.
  expect(await verification(plain)).toBeNull();
  expect(await verification(signed)).toEqual({ state: "unverified", reason: "is signed with a key nobody has registered here" });
  expect((await anyone("POST", "/keys", { key: laptop.public })).status).toBe(401);
  expect((await erin("POST", "/keys", { key: "ssh-ed25519 not-a-key" })).status).toBe(400);
  expect((await erin("POST", "/keys", { title: "Laptop", key: laptop.public })).status).toBe(201);
  expect((await erin("POST", "/keys", { key: laptop.public })).status).toBe(400);
  const keys = (await erin("GET", "/keys")).data.keys;
  expect(keys).toMatchObject([{ kind: "ssh", title: "Laptop" }]);
  expect(keys[0].fingerprint).toStartWith("SHA256:");
  expect(await verification(signed)).toEqual({ state: "verified", signer: "erin", key: keys[0].fingerprint });
  const listed = (await anyone("GET", "/repos/erin/sealed/commits")).data.commits;
  expect(listed.map((c: any) => c.verification?.state ?? null)).toEqual(["verified", null]);
  expect(listed[0].signature).toBeUndefined();

  // Required: the default branch takes nothing else, anywhere in a push.
  expect((await erin("POST", "/repos/erin/sealed/rules", { pattern: "main", requireSigned: true })).status).toBe(200);
  expect((await anyone("GET", "/repos/erin/sealed/rules")).data.rules).toMatchObject([{ pattern: "main", requireSigned: true }]);
  const unsigned = commit("Unsigned again");
  let refused = git(work, "push", "-q", origin, "main");
  expect(refused.err).toContain(`main takes signed commits only, and ${unsigned.slice(0, 7)} is not signed`);
  commit("Signed on top of it", ...laptop.sign);
  refused = git(work, "push", "-q", origin, "main");
  expect(refused.err).toContain(`${unsigned.slice(0, 7)} is not signed`);
  // Other branches are not held to it.
  expect(git(work, "push", "-q", origin, "main:loose").err).toBe("");
  git(work, "reset", "-q", "--hard", signed);
  const stranger = sshKey("stranger", "-t", "ed25519");
  commit("Signed by who knows", ...stranger.sign);
  expect(git(work, "push", "-q", origin, "main").err).toContain("is signed with a key nobody has registered here");
  git(work, "reset", "-q", "--hard", signed);

  // The other kinds of SSH key, and OpenPGP.
  const rsa = sshKey("rsa", "-t", "rsa", "-b", "2048");
  const ecdsa = sshKey("ecdsa", "-t", "ecdsa", "-b", "256");
  expect(Bun.spawnSync(["gpg", "--batch", "--quiet", "--pinentry-mode", "loopback", "--passphrase", "", "--quick-generate-key", "Erin <erin@example.com>", "ed25519", "sign", "never"], { env: gpgEnv }).exitCode).toBe(0);
  const armored = Bun.spawnSync(["gpg", "--armor", "--export", "erin@example.com"], { env: gpgEnv }).stdout.toString();
  for (const key of [rsa.public, ecdsa.public, armored]) expect((await erin("POST", "/keys", { key })).status).toBe(201);
  expect((await erin("GET", "/keys")).data.keys.map((key: any) => key.kind)).toEqual(["gpg", "ssh", "ssh", "ssh"]);
  const made = [
    commit("Signed with RSA", ...rsa.sign),
    commit("Signed with ECDSA", ...ecdsa.sign),
    commit("Signed with OpenPGP", "-c", "gpg.format=openpgp", "-c", "user.signingkey=erin@example.com", "-c", "commit.gpgsign=true"),
  ];
  expect(git(work, "push", "-q", origin, "main").err).toBe("");
  for (const oid of made) expect(await verification(oid)).toMatchObject({ state: "verified", signer: "erin" });

  // A key taken back no longer vouches for anything.
  await erin("DELETE", `/keys/${keys[0].id}`);
  expect((await verification(signed)).state).toBe("unverified");
}, 60_000);

test("other sites cannot act for a signed-in user", async () => {
  const response = await fetch(`${hub}/api/logout`, { method: "POST", headers: { origin: "https://evil.example", "content-type": "text/plain" }, body: "x" });
  expect(response.status).toBe(403);
});

test("only Strata may ask the hooks", async () => {
  const ask = (secret?: string) =>
    fetch(`${hub}/hooks/authz`, {
      method: "POST",
      headers: { "content-type": "application/json", ...(secret ? { authorization: `Bearer ${secret}` } : {}) },
      body: JSON.stringify({ path: "nobody/nothing", access: "read", authorization: null }),
    });
  expect((await ask()).status).toBe(403);
  expect((await ask("guess")).status).toBe(403);
  const answer = await ask("test-hook-secret");
  expect(await answer.json()).toEqual({ allow: false, status: 401, message: "sign in to continue" });
});

/**
 * A browser going through a sign-in elsewhere: sent there, sent back, and
 * then whoever the hub says it is. `meddle` changes the address it comes back to.
 */
async function signInAt(provider: string, meddle = (back: URL) => back) {
  const cookies = new Map<string, string>();
  const visit = async (url: string) => {
    const response = await fetch(url, { redirect: "manual", headers: { cookie: [...cookies].map(([name, value]) => `${name}=${value}`).join("; ") } });
    for (const cookie of response.headers.getSetCookie()) {
      const [name, value] = cookie.split(";")[0]!.split("=") as [string, string];
      if (value) cookies.set(name, value);
      else cookies.delete(name);
    }
    return response;
  };
  const sent = await visit(`${hub}/api/auth/${provider}`);
  expect(sent.status).toBe(302);
  const there = await fetch(sent.headers.get("location")!, { redirect: "manual" });
  const back = await visit(meddle(new URL(there.headers.get("location")!)).toString());
  const me = await (await visit(`${hub}/api/me`)).json();
  return { landed: back.headers.get("location")!, user: me.user as { name: string; displayName: string } | null, cookie: `hub_session=${cookies.get("hub_session")}` };
}

test("signing in elsewhere", async () => {
  const me = (await browser()("GET", "/me")).data;
  expect(me.signIn).toEqual({ passwords: true, providers: [{ id: "github", name: "GitHub" }, { id: "oidc", name: "Example" }] });

  // OpenID Connect: a first arrival makes an account, named after who arrived.
  outside.person = { ...outside.person, sub: "subject-1", preferred_username: "Carol.Jones@example.com", name: "Carol Jones" };
  const carol = await signInAt("oidc");
  expect(carol).toMatchObject({ landed: `${hub}/`, user: { name: "Carol-Jones", displayName: "Carol Jones" } });
  // Coming back is the same account, whatever they are called by then.
  outside.person.preferred_username = "carol@example.com";
  expect((await signInAt("oidc")).user!.name).toBe("Carol-Jones");
  // Someone else who would get a name that is taken (alice signed up with a password) gets another.
  outside.person = { ...outside.person, sub: "subject-2", preferred_username: "alice@example.com", name: "Another Alice" };
  expect((await signInAt("oidc")).user!.name).toBe("alice-2");

  // GitHub: the same, by the account's number there.
  outside.person = { ...outside.person, id: 4242, login: "gina", name: "Gina" };
  const gina = await signInAt("github");
  expect(gina).toMatchObject({ landed: `${hub}/`, user: { name: "gina", displayName: "Gina" } });
  outside.person.login = "gina-renamed";
  expect((await signInAt("github")).user!.name).toBe("gina");
  // The same number at another provider is somebody else.
  outside.person = { ...outside.person, sub: "4242", preferred_username: "hana" };
  expect((await signInAt("oidc")).user!.name).toBe("hana");

  // Such an account has no password; git takes an access token.
  const made = await fetch(`${hub}/api/tokens`, { method: "POST", headers: { origin: hub, cookie: gina.cookie, "content-type": "application/json" }, body: JSON.stringify({ name: "laptop" }) });
  expect(made.status).toBe(201);
  expect((await anyoneLogin("gina", "")).status).toBe(401);

  // An answer that is not to this browser's question signs nobody in.
  const forged = await signInAt("oidc", (back) => (back.searchParams.set("state", "guessed"), back));
  expect(forged.user).toBeNull();
  expect(forged.landed).toStartWith(`${hub}/login?error=`);
  // Nor does a code the provider does not know, or a provider saying no.
  const stale = await signInAt("github", (back) => (back.searchParams.set("code", "made-up"), back));
  expect(stale).toMatchObject({ user: null, landed: `${hub}/login?error=GitHub+did+not+accept+the+sign-in` });
  const declined = await signInAt("oidc", (back) => (back.searchParams.set("error", "access_denied"), back));
  expect(declined.user).toBeNull();
  // Without the browser that started it, the answer is worth nothing.
  outside.codes.set("stolen", { nonce: "", challenge: "" });
  const alone = await fetch(`${hub}/api/auth/oidc/callback?code=stolen&state=anything`, { redirect: "manual" });
  expect(alone.headers.get("location")).toStartWith(`${hub}/login?error=`);
  expect(alone.headers.getSetCookie().some((cookie) => cookie.startsWith("hub_session="))).toBe(false);
  expect((await fetch(`${hub}/api/auth/nowhere`, { redirect: "manual" })).status).toBe(404);
});

const anyoneLogin = (username: string, password: string) => browser()("POST", "/login", { username, password });

test("mirrors of repositories on GitHub", async () => {
  const mia = browser();
  expect((await mia("POST", "/signup", { username: "mia", password: "correct horse" })).status).toBe(201);
  const token: string = (await mia("POST", "/tokens", { name: "laptop" })).data.token;

  // What stands in for GitHub here serves the hub's own repositories: the source is one of them.
  expect((await mia("POST", "/repos", { name: "source" })).status).toBe(201);
  const work = join(tmp, "mirror-work");
  git(tmp, "init", "-q", "-b", "trunk", work);
  writeFileSync(join(work, "README.md"), "# Source\n");
  git(work, "add", ".");
  git(work, "commit", "-q", "-m", "first");
  git(work, "tag", "-a", "-m", "one", "v1");
  git(work, "branch", "side");
  const source = remote(`mia:${token}`, "mia/source");
  expect(git(work, "push", "-q", source, "trunk", "side", "v1").ok).toBe(true);
  const first = git(work, "rev-parse", "HEAD").out;
  expect((await mia("PATCH", "/repos/mia/source", { defaultBranch: "trunk" })).status).toBe(200);

  expect((await mia("POST", "/repos", { name: "bad", mirror: "not a repository" })).status).toBe(400);
  expect((await mia("POST", "/repos", { name: "copy", mirror: `${elsewhere}/mia/source.git` })).status).toBe(201);
  const synced = async (path: string, since = 0) => {
    const detail = (await mia("GET", path)).data;
    return detail.mirror.state === "synced" && detail.mirror.syncedAt >= since && detail;
  };
  let copy = await eventually(() => synced("/repos/mia/copy"));
  expect(copy.mirror).toMatchObject({ source: "mia/source", url: `${elsewhere}/mia/source`, pulls: false, error: null });
  expect(copy).toMatchObject({ empty: false, defaultBranch: "trunk", branches: [{ name: "side", oid: first }, { name: "trunk", oid: first }] });
  expect(copy.tags.map((tag: { name: string }) => tag.name)).toEqual(["v1"]);
  const clone = join(tmp, "mirror-clone");
  expect(git(tmp, "clone", "-q", remote("", "mia/copy"), clone).ok).toBe(true);
  expect(git(clone, "rev-parse", "HEAD", "v1^{commit}").out).toBe(`${first}\n${first}`);
  expect(git(clone, "fsck", "--strict").ok).toBe(true);

  // A mirror changes with its source and not otherwise.
  const pushed = git(work, "push", remote(`mia:${token}`, "mia/copy"), "trunk:other");
  expect(pushed.ok).toBe(false);
  expect(pushed.err).toContain("mia/copy mirrors mia/source and does not take pushes");

  // The source moves on: a commit, a new branch, a branch gone, a tag moved to a commit already there.
  writeFileSync(join(work, "README.md"), "# Source\n\nMore.\n");
  git(work, "commit", "-q", "-am", "second");
  const second = git(work, "rev-parse", "HEAD").out;
  git(work, "branch", "next", first);
  git(work, "tag", "-f", "light", first);
  expect(git(work, "push", "-q", source, "trunk", "next", "light", ":side").ok).toBe(true);
  expect((await browser()("POST", "/repos/mia/copy/mirror/sync")).status).toBe(401);
  const asked = Math.floor(Date.now() / 1000);
  await Bun.sleep(1100);
  expect((await mia("POST", "/repos/mia/copy/mirror/sync")).status).toBe(202);
  copy = await eventually(() => synced("/repos/mia/copy", asked + 1));
  expect(copy.branches).toMatchObject([{ name: "next", oid: first }, { name: "trunk", oid: second }]);
  expect(copy.tags.map((tag: { name: string }) => tag.name)).toEqual(["light", "v1"]);
  expect(git(clone, "pull", "-q").ok).toBe(true);
  expect(git(clone, "rev-parse", "HEAD").out).toBe(second);
  // Each fetch that brought something shows as one entry, however many refs it moved.
  const activity = await eventually(async () => {
    const { activity } = (await mia("GET", "/repos/mia/copy/activity")).data;
    return activity.length === 2 && activity;
  });
  expect(activity.map((entry: { actor: string; imported: number }) => [entry.actor, entry.imported])).toEqual([["mirror", 4], ["mirror", 3]]);

  // A private source needs a token that may read it; without one it looks as if it were not there.
  expect((await mia("PATCH", "/repos/mia/source", { private: true })).status).toBe(200);
  expect((await mia("POST", "/repos", { name: "dark", mirror: "mia/source" })).status).toBe(201);
  const failed = await eventually(async () => {
    const { mirror } = (await mia("GET", "/repos/mia/dark")).data;
    return mirror.state === "failed" && mirror;
  });
  expect(failed.error).toBe("mia/source is not there, or is private: a private repository needs an access token");
  expect((await mia("POST", "/repos", { name: "lit", mirror: "mia/source", mirrorToken: token, mirrorPulls: true })).status).toBe(201);
  const lit = await eventually(() => synced("/repos/mia/lit"));
  expect(lit.branches).toMatchObject(copy.branches);
  expect(JSON.stringify(lit)).not.toContain(token);

  // Let go of its source, it is a repository like any other.
  expect((await mia("DELETE", "/repos/mia/copy/mirror")).status).toBe(200);
  expect((await mia("GET", "/repos/mia/copy")).data.mirror).toBeNull();
  expect(git(work, "push", "-q", remote(`mia:${token}`, "mia/copy"), "trunk:other").ok).toBe(true);
});

test("teams, SSH, branch rules, reviews, checks and webhooks", async () => {
  const [olga, pat, quin] = [browser(), browser(), browser()];
  for (const [who, name] of [[olga, "olga"], [pat, "pat"], [quin, "quin"]] as const) expect((await who("POST", "/signup", { username: name, password: "correct horse" })).status).toBe(201);
  expect((await olga("POST", "/orgs", { name: "orbit" })).status).toBe(201);
  expect((await olga("POST", "/repos", { owner: "orbit", name: "core", private: true })).status).toBe(201);
  expect((await olga("POST", "/repos", { owner: "orbit", name: "docs", private: true })).status).toBe(201);
  const core = "/repos/orbit/core";

  // ---- Teams: members get what the organization gives everyone, and what their teams add.
  expect((await olga("PUT", "/owners/orbit/members/pat", { role: "member" })).status).toBe(200);
  expect((await pat("GET", core)).data.permission).toBe("write");
  expect((await pat("PATCH", "/owners/orbit", { basePermission: "none" })).status).toBe(403);
  expect((await olga("PATCH", "/owners/orbit", { basePermission: "none" })).status).toBe(200);
  expect((await pat("GET", core)).status).toBe(404);
  expect((await pat("POST", "/owners/orbit/teams", { name: "engine" })).status).toBe(403);
  expect((await olga("POST", "/owners/orbit/teams", { name: "engine", description: "Those who build it" })).status).toBe(201);
  expect((await olga("POST", "/owners/orbit/teams", { name: "Engine" })).status).toBe(409);
  expect((await olga("PUT", "/owners/orbit/teams/engine/repos/core", { level: "write" })).status).toBe(200);
  expect((await olga("PUT", "/owners/orbit/teams/engine/repos/nope", { level: "write" })).status).toBe(404);
  expect((await pat("GET", core)).status).toBe(404);
  expect((await olga("PUT", "/owners/orbit/teams/engine/members/pat")).status).toBe(200);
  expect((await pat("GET", core)).data.permission).toBe("write");
  expect((await pat("GET", "/repos/orbit/docs")).status).toBe(404);
  expect((await pat("GET", "/owners/orbit/teams")).data.teams).toMatchObject([{ name: "engine", members: 1, repos: 1 }]);
  expect((await olga("GET", core)).data.access).toEqual({ base: "none", teams: [{ name: "engine", level: "write" }] });
  // Not the business of those outside; joining a team is joining the organization.
  expect((await quin("GET", "/owners/orbit/teams")).status).toBe(404);
  expect((await olga("PUT", "/owners/orbit/teams/engine/members/quin")).status).toBe(200);
  expect((await quin("GET", "/owners/orbit/teams/engine")).data).toMatchObject({ members: ["pat", "quin"], repos: [{ name: "core", level: "write" }], canChange: false });
  // Out of the organization is out of its teams.
  expect((await olga("DELETE", "/owners/orbit/members/quin")).status).toBe(200);
  expect((await quin("GET", core)).status).toBe(404);
  expect((await olga("GET", "/owners/orbit/teams/engine")).data.members).toEqual(["pat"]);

  // A team that follows a group at a sign-in provider is filled by signing in.
  expect((await olga("POST", "/owners/orbit/teams", { name: "readers", ssoProvider: "oidc" })).status).toBe(400);
  expect((await olga("POST", "/owners/orbit/teams", { name: "readers", ssoProvider: "oidc", ssoGroup: "Docs-Readers" })).status).toBe(201);
  expect((await olga("PUT", "/owners/orbit/teams/readers/repos/docs", { level: "read" })).status).toBe(200);
  expect((await olga("POST", "/owners/orbit/teams", { name: "octo", ssoProvider: "github", ssoGroup: "octo/devs" })).status).toBe(201);
  const as = (cookie: string) => async (path: string) => {
    const response = await fetch(`${hub}/api${path}`, { headers: { cookie } });
    return { status: response.status, data: (await response.json()) as any };
  };
  outside.person = { ...outside.person, sub: "subject-rita", preferred_username: "rita", name: "Rita", groups: ["everyone", "docs-readers"] };
  let rita = await signInAt("oidc");
  expect((await as(rita.cookie)("/repos/orbit/docs")).data.permission).toBe("read");
  expect((await as(rita.cookie)(core)).status).toBe(404);
  expect((await olga("GET", "/owners/orbit")).data.members.map((member: any) => member.name)).toEqual(["olga", "pat", "rita"]);
  // The provider not saying is not the same as saying "none".
  outside.person.groups = null;
  rita = await signInAt("oidc");
  expect((await as(rita.cookie)("/repos/orbit/docs")).status).toBe(200);
  outside.person.groups = ["everyone"];
  rita = await signInAt("oidc");
  expect((await as(rita.cookie)("/repos/orbit/docs")).status).toBe(404);
  expect((await olga("GET", "/owners/orbit")).data.members.map((member: any) => member.name)).toEqual(["olga", "pat"]);
  // GitHub says which teams someone is in there.
  outside.person = { ...outside.person, id: 777, login: "sam", name: "Sam", teams: ["octo/devs", "octo/other"] };
  await signInAt("github");
  expect((await olga("GET", "/owners/orbit/teams/octo")).data.members).toEqual(["sam"]);
  outside.person.teams = [];
  await signInAt("github");
  expect((await olga("GET", "/owners/orbit/teams/octo")).data.members).toEqual([]);

  // ---- SSH: git is let in with a key registered here, as whoever registered it.
  const key = (name: string) => {
    const path = join(tmp, `ssh-${name}`);
    expect(Bun.spawnSync(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", name, "-f", path]).exitCode).toBe(0);
    return { path, public: readFileSync(`${path}.pub`, "utf8") };
  };
  const [olgaKey, patKey] = [key("olga"), key("pat")];
  expect((await olga("POST", "/keys", { title: "Laptop", key: olgaKey.public })).status).toBe(201);
  const detail = (await olga("GET", core)).data;
  expect(detail.sshUrl).toBe(`ssh://git@127.0.0.1:${sshPort}/orbit/core.git`);
  const work = join(tmp, "orbit");
  git(tmp, "init", "-q", "-b", "main", work);
  const commit = (file: string, text: string, message: string) => {
    writeFileSync(join(work, file), text);
    git(work, "add", ".");
    git(work, "commit", "-q", "-m", message);
    return git(work, "rev-parse", "HEAD").out;
  };
  const first = commit("a.txt", "a\n", "First");
  expect(gitAs(patKey.path, work, "push", "-q", detail.sshUrl, "main").err).toContain("Permission denied");
  expect(gitAs(olgaKey.path, work, "push", "-q", detail.sshUrl, "main").err).toBe("");
  expect((await pat("POST", "/keys", { key: patKey.public })).status).toBe(201);
  expect(gitAs(patKey.path, tmp, "ls-remote", detail.sshUrl, "refs/heads/main").out).toStartWith(first);
  expect(gitAs(patKey.path, tmp, "ls-remote", `ssh://git@127.0.0.1:${sshPort}/orbit/docs.git`).err).toContain("repository not found");
  await eventually(async () => (await olga("GET", `${core}/activity`)).data.activity[0]?.actor === "olga");

  // ---- Webhooks: told of what happens from here on, each delivery signed.
  const everything = ["push", "pull_request", "pull_request_review", "status"];
  expect((await pat("GET", `${core}/hooks`)).status).toBe(403);
  expect((await olga("POST", `${core}/hooks`, { url: "ftp://example.com/x", events: everything })).status).toBe(400);
  expect((await olga("POST", `${core}/hooks`, { url: `${elsewhere}/webhook`, events: ["issues"] })).status).toBe(400);
  outside.delivered.length = 0;
  expect((await olga("POST", `${core}/hooks`, { url: `${elsewhere}/webhook`, secret: "s3cret", events: everything })).status).toBe(201);
  const received = (event: string) => outside.delivered.filter((delivery) => delivery.event === event).map((delivery) => JSON.parse(delivery.body));
  await eventually(async () => received("ping").length === 1);
  const ping = outside.delivered[0]!;
  expect(ping.signature).toBe(`sha256=${new Bun.CryptoHasher("sha256", "s3cret").update(ping.body).digest("hex")}`);
  const hooks = (await olga("GET", `${core}/hooks`)).data.hooks;
  expect(hooks).toMatchObject([{ url: `${elsewhere}/webhook`, hasSecret: true, active: true, last: { state: "delivered", status: 200 } }]);
  expect(JSON.stringify(hooks)).not.toContain("s3cret");

  // ---- A rule: main changes through pull requests that were approved and passed their check.
  const ruled = (await olga("POST", `${core}/rules`, { pattern: "main", approvals: 1, checks: ["ci/build"] })).data.rules;
  expect(ruled).toMatchObject([{ pattern: "main", requirePulls: true, approvals: 1, checks: ["ci/build"], noForce: true, noDelete: true }]);
  commit("direct.txt", "direct\n", "Straight to main");
  expect(gitAs(olgaKey.path, work, "push", "-q", detail.sshUrl, "main").err).toContain("main only takes changes through pull requests");
  git(work, "reset", "-q", "--hard", first);
  git(work, "checkout", "-q", "-b", "feature");
  const second = commit("b.txt", "b\n", "Add b");
  expect(gitAs(patKey.path, work, "push", "-q", detail.sshUrl, "feature").err).toBe("");
  expect((await pat("POST", `${core}/pulls`, { title: "Add b", base: "main", head: "feature" })).data).toEqual({ number: 1 });
  const pull = async () => (await olga("GET", `${core}/pulls/1`)).data;
  expect((await pull()).blocked).toEqual(["needs 1 approving review of its latest commit, and has 0", "the check ci/build has not reported"]);
  expect((await pull()).rule).toEqual({ pattern: "main", approvals: 1, checks: ["ci/build"] });
  expect((await olga("POST", `${core}/pulls/1/merge`, {})).data.error).toContain("cannot merge yet");

  // Reviews: not one's own, and not of a commit one has not seen.
  expect((await pat("POST", `${core}/pulls/1/reviews`, { state: "approved" })).status).toBe(403);
  expect((await olga("POST", `${core}/pulls/1/reviews`, { state: "approved", head: first })).status).toBe(409);
  expect((await olga("POST", `${core}/pulls/1/reviews`, { state: "changes_requested" })).status).toBe(400);
  expect((await olga("POST", `${core}/pulls/1/reviews`, { state: "approved", body: "Looks right", head: second })).status).toBe(201);
  expect((await pull()).blocked).toEqual(["the check ci/build has not reported"]);

  // Checks: reported by a program, which comes with an access token.
  const token: string = (await pat("POST", "/tokens", { name: "ci" })).data.token;
  const report = async (oid: string, body: unknown, bearer = token) => {
    const response = await fetch(`${hub}/api${core}/statuses/${oid}`, { method: "POST", headers: { authorization: `Bearer ${bearer}`, "content-type": "application/json" }, body: JSON.stringify(body) });
    return { status: response.status, data: (await response.json()) as any };
  };
  expect((await report(second, { state: "success", context: "ci/build" }, "hub_made-up")).status).toBe(404);
  expect((await report(second, { state: "green", context: "ci/build" })).status).toBe(400);
  expect((await report("f".repeat(40), { state: "success", context: "ci/build" })).status).toBe(404);
  expect((await report(second, { state: "pending", context: "ci/build", description: "Building" })).data).toEqual({ state: "pending" });
  expect((await pull()).blocked).toEqual(["the check ci/build has not finished"]);
  expect((await report(second, { state: "success", context: "ci/build", description: "Built", targetUrl: "https://ci.example.com/1" })).data).toEqual({ state: "success" });
  expect((await pull()).blocked).toEqual([]);
  expect((await pull()).checks).toMatchObject([{ context: "ci/build", state: "success", description: "Built", creator: "pat" }]);

  // A new commit is one nobody has approved or checked.
  const third = commit("c.txt", "c\n", "Add c");
  expect(gitAs(patKey.path, work, "push", "-q", detail.sshUrl, "feature").err).toBe("");
  expect((await pull()).blocked).toEqual(["needs 1 approving review of its latest commit, and has 0", "the check ci/build has not reported"]);
  expect((await pull()).reviewers).toEqual([{ author: "olga", state: "approved", stale: true, counts: true }]);
  // The hub hears of the push from Strata's feed, a moment later.
  await eventually(async () => received("pull_request").some((event) => event.action === "synchronize" && event.pull_request.head_oid === third));
  expect((await olga("POST", `${core}/pulls/1/reviews`, { state: "changes_requested", body: "Why c?" })).status).toBe(201);
  expect((await report(third, { state: "success", context: "ci/build" })).status).toBe(201);
  expect((await pull()).blocked).toEqual(["olga asked for changes", "needs 1 approving review of its latest commit, and has 0"]);
  expect((await olga("POST", `${core}/pulls/1/reviews`, { state: "approved" })).status).toBe(201);
  expect((await pull()).blocked).toEqual([]);
  const merged = await pat("POST", `${core}/pulls/1/merge`, { head: third });
  expect(merged.status).toBe(200);
  expect((await pull()).state).toBe("merged");

  // What the webhook heard, in the order it happened.
  await eventually(async () => received("push").some((push) => push.after === merged.data.oid));
  expect(received("push").map((push) => [push.ref, push.after, push.sender])).toEqual([
    ["refs/heads/feature", second, "pat"],
    ["refs/heads/feature", third, "pat"],
    ["refs/heads/main", merged.data.oid, "pat"],
  ]);
  expect(received("push")[0]).toMatchObject({ created: true, repository: { full_name: "orbit/core", private: true } });
  expect(received("pull_request").map((event) => event.action)).toEqual(["opened", "synchronize", "merged"]);
  expect(received("pull_request_review").map((event) => event.review.state)).toEqual(["approved", "changes_requested", "approved"]);
  expect(received("status").map((event) => [event.sha, event.state])).toEqual([[second, "pending"], [second, "success"], [third, "success"]]);
  for (const delivery of outside.delivered) expect(delivery.signature).toBe(`sha256=${new Bun.CryptoHasher("sha256", "s3cret").update(delivery.body).digest("hex")}`);
  const hook = (await olga("GET", `${core}/hooks`)).data.hooks[0];
  const log = (await olga("GET", `${core}/hooks/${hook.id}/deliveries`)).data.deliveries;
  expect(log.length).toBe(outside.delivered.length);
  expect(log.every((delivery: any) => delivery.state === "delivered" && delivery.status === 200)).toBe(true);
  // Switched off, it hears no more.
  expect((await olga("PATCH", `${core}/hooks/${hook.id}`, { active: false })).status).toBe(200);
  const before = outside.delivered.length;
  expect((await report(third, { state: "success", context: "lint" })).status).toBe(201);
  await Bun.sleep(300);
  expect(outside.delivered.length).toBe(before);
}, 60_000);

test("comments on lines, the audit log, search, blame and large files", async () => {
  const [uma, vic, anyone] = [browser(), browser(), browser()];
  for (const [who, name] of [[uma, "uma"], [vic, "vic"]] as const) expect((await who("POST", "/signup", { username: name, password: "correct horse" })).status).toBe(201);
  expect((await uma("POST", "/orgs", { name: "quay" })).status).toBe(201);
  expect((await uma("POST", "/repos", { owner: "quay", name: "dock", description: "Where ships tie up" })).status).toBe(201);
  expect((await uma("PUT", "/owners/quay/members/vic", { role: "member" })).status).toBe(200);
  const dock = "/repos/quay/dock";
  const token: string = (await uma("POST", "/tokens", { name: "laptop" })).data.token;
  const origin = remote(`uma:${token}`, "quay/dock");

  const work = join(tmp, "dock");
  git(tmp, "init", "-q", "-b", "main", work);
  const commit = (file: string, text: string, message: string) => {
    writeFileSync(join(work, file), text);
    git(work, "add", ".");
    git(work, "commit", "-q", "-m", message);
    return git(work, "rev-parse", "HEAD").out;
  };
  const first = commit("ships.txt", "ketch\nsloop\nyawl\n", "Three ships");
  commit("README.md", "# Dock\n\nA Harbour for small ships.\n", "Say what it is");
  const third = commit("ships.txt", "ketch\nsloop\nyawl\nbarque\n", "A fourth");
  expect(git(work, "push", "-q", origin, "main").err).toBe("");

  // ---- Search: repositories by a word, code by its text.
  expect((await anyone("GET", "/search?q=ships")).data.repos).toMatchObject([{ owner: "quay", name: "dock" }]);
  expect((await anyone("GET", "/search?q=quay")).data.repos).toHaveLength(1);
  expect((await anyone("GET", "/search?q=100%25")).data.repos).toEqual([]);
  const found = (await anyone("GET", `${dock}/search?q=harbour`)).data;
  expect(found).toMatchObject({ matches: 1, truncated: false, files: [{ path: "README.md", matches: [{ line: 3, text: "A Harbour for small ships." }] }] });
  expect((await anyone("GET", `${dock}/search?q=harbour&case=1`)).data.matches).toBe(0);
  expect((await anyone("GET", `${dock}/search?q=${encodeURIComponent("^(ketch|yawl)$")}&regex=1`)).data.matches).toBe(2);
  expect((await anyone("GET", `${dock}/search?q=ships.txt`)).data.paths).toEqual(["ships.txt"]);
  expect((await anyone("GET", `${dock}/search?q=${encodeURIComponent("(")}&regex=1`)).status).toBe(400);

  // ---- Blame and the history of one file.
  const blame = (await anyone("GET", `${dock}/blame?path=ships.txt`)).data;
  expect(blame.hunks).toEqual([{ start: 1, lines: 3, oid: first }, { start: 4, lines: 1, oid: third }]);
  expect(blame.commits[first]).toMatchObject({ message: "Three ships\n", author: { name: "Alice" } });
  expect((await anyone("GET", `${dock}/blame?path=nope.txt`)).status).toBe(404);
  const history = (await anyone("GET", `${dock}/commits?path=ships.txt`)).data;
  expect(history.commits.map((c: any) => c.oid)).toEqual([third, first]);

  // ---- Comments on lines of a pull request.
  git(work, "checkout", "-q", "-b", "more");
  commit("ships.txt", "ketch\nschooner\nyawl\nbarque\nbrig\n", "Change one, add one");
  expect(git(work, "push", "-q", origin, "more").err).toBe("");
  expect((await vic("POST", `${dock}/pulls`, { title: "More ships", base: "main", head: "more" })).data).toEqual({ number: 1 });
  const threads = `${dock}/pulls/1/threads`;
  expect((await anyone("POST", threads, { path: "ships.txt", side: "new", line: 2, body: "?" })).status).toBe(401);
  expect((await uma("POST", threads, { path: "README.md", side: "new", line: 1, body: "?" })).status).toBe(404);
  expect((await uma("POST", threads, { path: "ships.txt", side: "new", line: 0, body: "?" })).status).toBe(400);
  expect((await uma("POST", threads, { path: "ships.txt", side: "new", line: 2, body: "Why a schooner?" })).data).toEqual({ id: 1 });
  expect((await uma("POST", threads, { path: "ships.txt", side: "old", line: 2, body: "The sloop was fine." })).status).toBe(201);
  expect((await vic("POST", `${threads}/1/comments`, { body: "It has two masts." })).status).toBe(201);
  expect((await vic("POST", `${threads}/99/comments`, { body: "?" })).status).toBe(404);
  let pull = (await anyone("GET", `${dock}/pulls/1`)).data;
  expect(pull.threads).toMatchObject([
    { id: 1, path: "ships.txt", side: "new", line: 2, outdated: false, resolved: false, comments: [{ author: "uma", body: "Why a schooner?" }, { author: "vic", body: "It has two masts." }] },
    { id: 2, side: "old", line: 2, outdated: false, comments: [{ author: "uma" }] },
  ]);
  expect((await anyone("GET", `${dock}/pulls`)).data.pulls[0].comments).toBe(3);
  // Settled by those in it; a stranger may not.
  const wes = browser();
  await wes("POST", "/signup", { username: "wes", password: "correct horse" });
  expect((await wes("PATCH", `${threads}/1`, { resolved: true })).status).toBe(403);
  expect((await vic("PATCH", `${threads}/1`, { resolved: true })).status).toBe(200);
  pull = (await anyone("GET", `${dock}/pulls/1`)).data;
  expect(pull.threads[0]).toMatchObject({ resolved: true, resolvedBy: "vic" });
  // The branch moves on: a comment on a version of the file that is no longer there is outdated; one on the base's is not.
  commit("ships.txt", "ketch\nschooner\nyawl\nbarque\nbrig\ncutter\n", "One more");
  expect(git(work, "push", "-q", origin, "more").err).toBe("");
  pull = (await anyone("GET", `${dock}/pulls/1`)).data;
  expect(pull.threads.map((thread: any) => [thread.id, thread.outdated])).toEqual([[1, true], [2, false]]);
  expect((await uma("POST", `${dock}/pulls/1/merge`, {})).status).toBe(200);
  expect((await uma("POST", threads, { path: "ships.txt", side: "new", line: 1, body: "late" })).status).toBe(409);

  // ---- The audit log: what was changed about access and settings, for those who own it.
  expect((await uma("POST", `${dock}/rules`, { pattern: "main", approvals: 1 })).status).toBe(200);
  expect((await uma("PATCH", dock, { private: true })).status).toBe(200);
  expect((await anyone("POST", "/login", { username: "uma", password: "wrong" })).status).toBe(401);
  expect((await vic("GET", `${dock}/audit`)).status).toBe(403);
  const repoLog = (await uma("GET", `${dock}/audit`)).data.entries;
  expect(repoLog.map((entry: any) => [entry.actor, entry.action])).toEqual([["uma", "repo.visibility"], ["uma", "rule.set"], ["uma", "pull.merge"], ["uma", "repo.create"]]);
  expect(repoLog[1]).toMatchObject({ subject: "quay/dock", detail: { pattern: "main", approvals: 1, requirePulls: true } });
  expect(repoLog[2].detail).toMatchObject({ number: 1, base: "main", head: "more", approvedBy: [] });
  const orgLog = (await uma("GET", "/owners/quay/audit")).data.entries;
  expect(orgLog.map((entry: any) => entry.action)).toEqual(["repo.visibility", "rule.set", "pull.merge", "member.set", "repo.create", "org.create"]);
  expect(orgLog[3]).toMatchObject({ subject: "vic", detail: { role: "member" } });
  expect((await vic("GET", "/owners/quay/audit")).status).toBe(404);
  // One's own account: sign-ins, failed ones too, and tokens. Nobody else's.
  const own = (await uma("GET", "/owners/uma/audit")).data.entries;
  expect(own.map((entry: any) => [entry.actor, entry.action])).toEqual([[null, "account.sign-in-failed"], ["uma", "token.create"], ["uma", "account.create"]]);
  expect((await vic("GET", "/owners/uma/audit")).status).toBe(404);
  expect((await anyone("GET", "/owners/uma/audit")).status).toBe(401);
  // A deleted repository stays in its owner's log.
  expect((await uma("POST", "/repos", { owner: "quay", name: "gone" })).status).toBe(201);
  expect((await uma("DELETE", "/repos/quay/gone")).status).toBe(200);
  expect((await uma("GET", "/owners/quay/audit")).data.entries.slice(0, 2)).toMatchObject([{ action: "repo.delete", subject: "quay/gone" }, { action: "repo.create", subject: "quay/gone" }]);

  // ---- Large files, with the real client where there is one.
  if (!Bun.which("git-lfs")) {
    console.warn("git-lfs is not installed: large files are not tested with it here");
    return;
  }
  expect((await uma("PATCH", dock, { private: true })).status).toBe(200);
  expect((await uma("DELETE", `${dock}/rules/${(await uma("GET", `${dock}/rules`)).data.rules[0].id}`)).status).toBe(200);
  git(work, "checkout", "-q", "main");
  git(work, "pull", "-q", origin, "main");
  expect(git(work, "lfs", "install", "--local").ok).toBe(true);
  git(work, "lfs", "track", "*.bin");
  const large = crypto.getRandomValues(new Uint8Array(65536));
  for (let i = 0; i < 4; i++) writeFileSync(join(work, "chart.bin"), large, { flag: i ? "a" : "w" });
  const sum = new Bun.CryptoHasher("sha256").update(readFileSync(join(work, "chart.bin"))).digest("hex");
  git(work, "add", ".");
  git(work, "commit", "-q", "-m", "A chart");
  const pushed = git(work, "push", "-q", origin, "main");
  expect(pushed.err).toBe("");
  // What git holds is the note; the hub shows it as one, and hands out the content when asked.
  const noted = await uma("GET", `${dock}/blob?path=chart.bin`);
  expect(noted.data).toContain(`oid sha256:${sum}`);
  const cookie = (await signInAs("uma", "correct horse")).cookie;
  const whole = await fetch(`${hub}/api${dock}/blob?path=chart.bin&resolve=1`, { headers: { cookie } });
  expect(new Bun.CryptoHasher("sha256").update(await whole.arrayBuffer()).digest("hex")).toBe(sum);
  expect((await anyone("GET", `${dock}/blob?path=chart.bin&resolve=1`)).status).toBe(404);

  // A clone over SSH is told where to fetch the content, and is let in there with what it is given.
  const path = join(tmp, "ssh-uma-lfs");
  expect(Bun.spawnSync(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "uma", "-f", path]).exitCode).toBe(0);
  expect((await uma("POST", "/keys", { key: readFileSync(`${path}.pub`, "utf8") })).status).toBe(201);
  const smudge = ["-c", "filter.lfs.smudge=git-lfs smudge -- %f", "-c", "filter.lfs.process=git-lfs filter-process", "-c", "filter.lfs.required=true"];
  const cloned = gitAs(path, tmp, ...smudge, "clone", "-q", `ssh://git@127.0.0.1:${sshPort}/quay/dock.git`, "dock-clone");
  expect(cloned.err).toBe("");
  expect(new Bun.CryptoHasher("sha256").update(readFileSync(join(tmp, "dock-clone/chart.bin"))).digest("hex")).toBe(sum);
  // Someone who may not read the repository gets neither the note nor the content.
  const stranger = gitWith({}, tmp, ...smudge, "clone", "-q", remote("wes:correct%20horse", "quay/dock"), "dock-stranger");
  expect(stranger.ok).toBe(false);
}, 60_000);

/** A session as someone with a password, for requests the browser helper cannot make. */
async function signInAs(username: string, password: string) {
  const response = await fetch(`${hub}/api/login`, { method: "POST", headers: { origin: hub, "content-type": "application/json" }, body: JSON.stringify({ username, password }) });
  return { cookie: response.headers.get("set-cookie")!.split(";")[0]! };
}
