// Runs Strata and the hub as they run for real, and drives them with git and
// the hub's API. Needs the Strata binary: `cargo build` in the repository root.
import { afterAll, beforeAll, expect, test } from "bun:test";
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dir, "..");
const strataBin = process.env.STRATA_BIN ?? resolve(root, "../../target/debug/strata");
const strataPort = 18190 + Math.floor(Math.random() * 500);
const hubPort = strataPort + 1000;
const hub = `http://127.0.0.1:${hubPort}`;
const tmp = mkdtempSync(join(tmpdir(), "strata-hub-"));
const children: Bun.Subprocess[] = [];

async function up(url: string) {
  for (let i = 0; i < 100; i++) {
    if (await fetch(url).then((r) => r.status < 500, () => false)) return;
    await Bun.sleep(100);
  }
  throw new Error(`${url} did not come up`);
}

beforeAll(async () => {
  if (!existsSync(strataBin)) throw new Error(`build Strata first: ${strataBin} is missing`);
  const env = {
    ...process.env,
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
  rmSync(tmp, { recursive: true, force: true });
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
  const result = Bun.spawnSync(["git", ...args], {
    cwd: dir,
    env: {
      PATH: process.env.PATH!,
      HOME: tmp,
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
  expect((await alice("PATCH", "/repos/acme/widget", { protectDefault: true })).status).toBe(200);
  git(work, "commit", "-q", "--amend", "-m", "Say hi, rewritten");
  const forced = git(work, "push", "-q", "--force", remote(`bob:${bobToken}`, "acme/widget"), "trunk");
  expect(forced.ok).toBe(false);
  expect(forced.err).toContain("trunk is protected and does not accept force pushes");
  expect(git(work, "push", "-q", remote(`bob:${bobToken}`, "acme/widget"), ":trunk").err).toContain("cannot be deleted");
  expect(git(work, "push", "-q", "--force", remote(`bob:${bobToken}`, "acme/widget"), "trunk:topic/x").err).toBe("");
  expect((await alice("PATCH", "/repos/acme/widget", { protectDefault: false })).status).toBe(200);
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
