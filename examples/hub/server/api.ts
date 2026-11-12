// What the web UI talks to. Sessions are cookies; every answer is JSON,
// except file contents.
import { Hono, type Context } from "hono";
import { csrf } from "hono/csrf";
import { HTTPException } from "hono/http-exception";
import { apiUser, checkPassword, createToken, endSession, sessionUser, startSession } from "./auth";
import { fail, REPO, signedIn, text, type Env } from "./common";
import { config, signInAt } from "./config";
import { canRead, canWrite, db, mirrorOf, now, ownerByName, permission, REPO_COLUMNS, repoByPath, roleIn, type Mirror, type Owner, type Repo } from "./db";
import { forget, requestSync, sourceName, sourceUrl } from "./mirror";
import { addPullRoutes, openPullCount } from "./pulls";
import { addRuleRoutes, ruleFor, rulesOf } from "./rules";
import { addKey, keysOf, removeKey, withVerification } from "./signing";
import { addSignInRoutes } from "./sso";
import { strata, StrataError } from "./strata";
import { addTeamRoutes, removeMember, teamsWithAccess } from "./teams";
import { addWebhookRoutes } from "./webhooks";

export const api = new Hono<Env>();

// Names that would collide with the UI's own routes.
const RESERVED = new Set(["api", "hooks", "assets", "login", "signup", "new", "settings", "explore", "orgs"]);
const OWNER_NAME = /^[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,37}[a-zA-Z0-9])?$/;
const REPO_NAME = /^[a-zA-Z0-9_-][a-zA-Z0-9._-]{0,99}$/;

function ownerName(raw: unknown): string {
  const name = String(raw ?? "").trim();
  if (!OWNER_NAME.test(name) || RESERVED.has(name.toLowerCase())) throw fail(400, "names are 1 to 39 letters, digits or hyphens, and some are reserved");
  if (ownerByName(name)) throw fail(409, `the name ${name} is taken`);
  return name;
}

function repoName(raw: unknown): string {
  const name = String(raw ?? "").trim();
  if (!REPO_NAME.test(name) || name.toLowerCase().endsWith(".git")) throw fail(400, "repository names are letters, digits, '-', '_' and '.', and do not end in .git");
  return name;
}

api.use("*", csrf());
api.use("*", async (c, next) => {
  // A browser comes with its session; a program with an access token.
  c.set("user", sessionUser(c) ?? apiUser(c.req.header("authorization")));
  await next();
});

api.onError((error, c) => {
  if (error instanceof HTTPException) return c.json({ error: error.message }, error.status);
  if (error instanceof StrataError) return c.json({ error: error.message }, error.status as 400 | 404 | 409 | 502);
  console.error(error);
  return c.json({ error: "something went wrong" }, 500);
});

const orgsOf = (userId: number) =>
  db
    .query<{ name: string; role: string }, [number]>("SELECT owners.name, members.role FROM members JOIN owners ON owners.id = members.org_id WHERE members.user_id = ? ORDER BY owners.name")
    .all(userId);

const publicUser = (user: Owner) => ({ name: user.name, displayName: user.display_name });

// ---- accounts ----

api.get("/me", (c) => {
  const user = c.get("user");
  const signIn = { passwords: config.passwords, providers: signInAt.map(({ id, name }) => ({ id, name })) };
  return c.json({ user: user && publicUser(user), orgs: user ? orgsOf(user.id) : [], signIn });
});

addSignInRoutes(api, RESERVED);

api.post("/signup", async (c) => {
  if (!config.passwords) throw fail(403, "accounts are made by signing in, not here");
  const body = await c.req.json();
  const name = ownerName(body.username);
  const password = String(body.password ?? "");
  if (password.length < 8) throw fail(400, "passwords are at least 8 characters");
  const hash = await Bun.password.hash(password);
  const { lastInsertRowid } = db.run("INSERT INTO owners (name, kind, display_name, password_hash, created_at) VALUES (?, 'user', ?, ?, ?)", [name, text(body.displayName, 100), hash, now()]);
  startSession(c, Number(lastInsertRowid));
  return c.json({ user: { name, displayName: text(body.displayName, 100) } }, 201);
});

api.post("/login", async (c) => {
  if (!config.passwords) throw fail(403, "passwords are not used here");
  const body = await c.req.json();
  const user = await checkPassword(String(body.username ?? ""), String(body.password ?? ""));
  if (!user) throw fail(401, "wrong user name or password");
  startSession(c, user.id);
  return c.json({ user: publicUser(user) });
});

api.post("/logout", (c) => {
  endSession(c);
  return c.json({});
});

api.get("/tokens", (c) => {
  const user = signedIn(c);
  const tokens = db.query("SELECT id, name, created_at AS createdAt, last_used_at AS lastUsedAt FROM tokens WHERE user_id = ? ORDER BY id DESC").all(user.id);
  return c.json({ tokens });
});

api.post("/tokens", async (c) => {
  const user = signedIn(c);
  const name = text((await c.req.json()).name, 100);
  if (!name) throw fail(400, "give the token a name");
  // Shown once; only its hash is kept.
  return c.json({ token: createToken(user.id, name) }, 201);
});

api.delete("/tokens/:id", (c) => {
  const user = signedIn(c);
  db.run("DELETE FROM tokens WHERE id = ? AND user_id = ?", [Number(c.req.param("id")), user.id]);
  return c.json({});
});

api.get("/keys", (c) => c.json({ keys: keysOf(signedIn(c).id) }));

api.post("/keys", async (c) => {
  const user = signedIn(c);
  const body = await c.req.json();
  try {
    await addKey(user.id, text(body.title, 100) || "Key", String(body.key ?? ""));
  } catch (error) {
    throw fail(400, (error as Error).message);
  }
  return c.json({}, 201);
});

api.delete("/keys/:id", (c) => {
  removeKey(signedIn(c).id, Number(c.req.param("id")));
  return c.json({});
});

// ---- owners: users and organizations ----

const repoSummary = (repo: Repo) => ({
  owner: repo.owner_name,
  name: repo.name,
  description: repo.description,
  private: !!repo.private,
  createdAt: repo.created_at,
  pushedAt: repo.pushed_at,
});

function visibleRepos(userId: number | null, where = "1 = 1", args: (string | number)[] = []): Repo[] {
  return db
    .query<Repo, (string | number)[]>(`SELECT ${REPO_COLUMNS} FROM repos JOIN owners ON owners.id = repos.owner_id WHERE ${where} ORDER BY COALESCE(repos.pushed_at, repos.created_at) DESC LIMIT 200`)
    .all(...args)
    .filter((repo) => canRead(permission(repo, userId)));
}

api.post("/orgs", async (c) => {
  const user = signedIn(c);
  const body = await c.req.json();
  const name = ownerName(body.name);
  db.transaction(() => {
    const { lastInsertRowid } = db.run("INSERT INTO owners (name, kind, display_name, created_at) VALUES (?, 'org', ?, ?)", [name, text(body.displayName, 100), now()]);
    db.run("INSERT INTO members (org_id, user_id, role) VALUES (?, ?, 'owner')", [Number(lastInsertRowid), user.id]);
  })();
  return c.json({ name }, 201);
});

api.get("/owners/:name", (c) => {
  const owner = ownerByName(c.req.param("name"));
  if (!owner) throw fail(404, "no such user or organization");
  const user = c.get("user");
  const repos = visibleRepos(user?.id ?? null, "repos.owner_id = ?", [owner.id]).map(repoSummary);
  const base = { name: owner.name, kind: owner.kind, displayName: owner.display_name, createdAt: owner.created_at, repos };
  if (owner.kind === "user") return c.json({ ...base, orgs: orgsOf(owner.id).map((org) => org.name) });
  const members = db
    .query("SELECT owners.name, members.role FROM members JOIN owners ON owners.id = members.user_id WHERE members.org_id = ? ORDER BY owners.name")
    .all(owner.id);
  return c.json({ ...base, members, role: user ? roleIn(owner.id, user.id) : null, basePermission: owner.base_permission });
});

function ownedOrg(c: Context<Env>): Owner {
  const user = signedIn(c);
  const org = ownerByName(c.req.param("name") ?? "");
  if (!org || org.kind !== "org") throw fail(404, "no such organization");
  if (roleIn(org.id, user.id) !== "owner") throw fail(403, "only owners of the organization can change its members");
  return org;
}

const ownerCount = (orgId: number) => db.query<{ n: number }, [number]>("SELECT COUNT(*) AS n FROM members WHERE org_id = ? AND role = 'owner'").get(orgId)!.n;

api.put("/owners/:name/members/:username", async (c) => {
  const org = ownedOrg(c);
  const member = ownerByName(c.req.param("username"));
  if (!member || member.kind !== "user") throw fail(404, "no such user");
  const role = (await c.req.json()).role === "owner" ? "owner" : "member";
  if (role === "member" && roleIn(org.id, member.id) === "owner" && ownerCount(org.id) === 1) throw fail(409, "an organization needs at least one owner");
  // Set by hand, it is no longer a sign-in group's to take away.
  db.run("INSERT INTO members (org_id, user_id, role) VALUES (?, ?, ?) ON CONFLICT (org_id, user_id) DO UPDATE SET role = excluded.role, synced = 0", [org.id, member.id, role]);
  return c.json({});
});

api.delete("/owners/:name/members/:username", (c) => {
  const org = ownedOrg(c);
  const member = ownerByName(c.req.param("username"));
  if (!member) throw fail(404, "no such user");
  if (roleIn(org.id, member.id) === "owner" && ownerCount(org.id) === 1) throw fail(409, "an organization needs at least one owner");
  removeMember(org.id, member.id);
  return c.json({});
});

addTeamRoutes(api);

// ---- repositories ----

type ActivityRow = { id: number; actor: string | null; time: number; updates: string; imported: number | null; owner_name: string; name: string; repo_id: string };
const activityJson = (row: ActivityRow) => ({ id: row.id, owner: row.owner_name, repo: row.name, actor: row.actor, time: row.time, updates: JSON.parse(row.updates), imported: row.imported });
const ACTIVITY = "SELECT activity.*, repos.name, owners.name AS owner_name FROM activity JOIN repos ON repos.id = activity.repo_id JOIN owners ON owners.id = repos.owner_id";

api.get("/repos", (c) => c.json({ repos: visibleRepos(c.get("user")?.id ?? null).map(repoSummary) }));

/** Pushes to the repositories the user owns or belongs to, newest first. */
api.get("/activity", (c) => {
  const user = signedIn(c);
  const rows = db
    .query<ActivityRow, [number, number]>(`${ACTIVITY} WHERE repos.owner_id = ? OR repos.owner_id IN (SELECT org_id FROM members WHERE user_id = ?) ORDER BY activity.id DESC LIMIT 30`)
    .all(user.id, user.id);
  return c.json({ activity: rows.map(activityJson) });
});

api.post("/repos", async (c) => {
  const user = signedIn(c);
  const body = await c.req.json();
  const owner = ownerByName(String(body.owner ?? user.name));
  if (!owner) throw fail(404, "no such owner");
  const mayCreate = owner.kind === "user" ? owner.id === user.id : roleIn(owner.id, user.id) !== null;
  if (!mayCreate) throw fail(403, `you cannot create repositories for ${owner.name}`);
  const name = repoName(body.name);
  if (repoByPath(owner.name, name)) throw fail(409, `${owner.name}/${name} already exists`);

  // A mirror: the repository follows one on GitHub from the start.
  const source = body.mirror ? sourceName(String(body.mirror)) : null;
  if (body.mirror && !source) throw fail(400, `name a repository on ${new URL(config.githubUrl).host}, as owner/name or by its address`);

  // Strata knows the repository by this id only, so its name is free to change.
  const id = `r-${crypto.randomUUID()}`;
  await strata.create(id);
  try {
    db.transaction(() => {
      db.run("INSERT INTO repos (id, owner_id, name, description, private, created_at) VALUES (?, ?, ?, ?, ?, ?)", [id, owner.id, name, text(body.description, 300), body.private ? 1 : 0, now()]);
      if (source) db.run("INSERT INTO mirrors (repo_id, source, pulls, token) VALUES (?, ?, ?, ?)", [id, source, body.mirrorPulls ? 1 : 0, text(body.mirrorToken, 500) || null]);
    })();
  } catch (error) {
    await strata.remove(id).catch(() => {});
    throw error;
  }
  if (source) requestSync(id);
  return c.json({ owner: owner.name, name }, 201);
});

api.use(`${REPO}/*`, async (c, next) => {
  const found = repoByPath(c.req.param("owner") ?? "", c.req.param("name") ?? "");
  const level = found ? permission(found, c.get("user")?.id ?? null) : "none";
  // A private repository looks just like one that does not exist.
  if (!found || !canRead(level)) throw fail(404, "repository not found");
  c.set("repo", found);
  c.set("level", level);
  await next();
});

const short = (ref: string) => ref.replace(/^refs\/(heads|tags)\//, "");

// The access token stays here: it is used, never shown.
const mirrorJson = (mirror: Mirror) => ({ source: mirror.source, url: sourceUrl(mirror.source), pulls: !!mirror.pulls, state: mirror.state, error: mirror.error, syncedAt: mirror.synced_at });

api.get(REPO, async (c) => {
  const found = c.get("repo");
  // Branches and tags only: a mirror may hold a ref for every pull request there ever was.
  const listed = async () => {
    const [head, tags] = await Promise.all([strata.refs(found.id, "refs/heads/"), strata.refs(found.id, "refs/tags/")]);
    return { head: head.head, refs: [...head.refs, ...tags.refs] };
  };
  const [{ head, refs }, info] = await Promise.all([listed(), strata.info(found.id)]);
  const mirror = mirrorOf(found.id);
  const named = (prefix: string) => refs.filter((ref) => ref.name.startsWith(prefix)).map((ref) => ({ name: ref.name.slice(prefix.length), oid: ref.oid }));
  const rules = await rulesOf(found);
  const branches = named("refs/heads/").map((branch) => ({ ...branch, protected: !!ruleFor(rules, branch.name) }));
  return c.json({
    ...repoSummary(found),
    ownerKind: found.owner_kind,
    openPulls: openPullCount(found.id),
    permission: c.get("level"),
    cloneUrl: `${config.strataPublicUrl}/${found.owner_name}/${found.name}.git`,
    sshUrl: config.sshUrl ? `${config.sshUrl}/${found.owner_name}/${found.name}.git` : null,
    // Who reaches it besides its owners, for those who can change that.
    access: found.owner_kind === "org" && c.get("level") === "admin" ? { base: found.owner_base, teams: teamsWithAccess(found.id) } : null,
    defaultBranch: short(head),
    // Nothing to show until the default branch exists.
    empty: !branches.some((branch) => branch.name === short(head)),
    branches,
    tags: named("refs/tags/"),
    size: info.bytes,
    mirror: mirror && mirrorJson(mirror),
  });
});

/** Fetches a mirror's source now, rather than when it is next due. */
api.post(`${REPO}/mirror/sync`, (c) => {
  signedIn(c);
  const found = c.get("repo");
  if (!canWrite(c.get("level"))) throw fail(403, "only those who can push here can ask for that");
  if (!mirrorOf(found.id)) throw fail(404, "this repository is not a mirror");
  requestSync(found.id);
  return c.json({}, 202);
});

/** Lets go of the source: from here on the repository is an ordinary one, and takes pushes. */
api.delete(`${REPO}/mirror`, (c) => {
  const found = c.get("repo");
  if (c.get("level") !== "admin") throw fail(403, "only the repository's owners can change its settings");
  db.run("DELETE FROM mirrors WHERE repo_id = ?", [found.id]);
  forget(found.id);
  return c.json({});
});

api.patch(REPO, async (c) => {
  const found = c.get("repo");
  if (c.get("level") !== "admin") throw fail(403, "only the repository's owners can change its settings");
  const body = await c.req.json();
  if (body.defaultBranch !== undefined) await strata.setHead(found.id, `refs/heads/${String(body.defaultBranch)}`);
  let name = found.name;
  if (body.name !== undefined && String(body.name).trim() !== found.name) {
    name = repoName(body.name);
    const other = repoByPath(found.owner_name, name);
    if (other && other.id !== found.id) throw fail(409, `${found.owner_name}/${name} already exists`);
  }
  // A rename happens here and nowhere else: Strata keeps serving the same id.
  db.run("UPDATE repos SET name = ?, description = ?, private = ? WHERE id = ?", [
    name,
    body.description === undefined ? found.description : text(body.description, 300),
    body.private === undefined ? found.private : body.private ? 1 : 0,
    found.id,
  ]);
  return c.json({ owner: found.owner_name, name });
});

api.delete(REPO, async (c) => {
  const found = c.get("repo");
  if (c.get("level") !== "admin") throw fail(403, "only the repository's owners can delete it");
  // Gone from the hub first: from then on nobody is let through to it.
  db.run("DELETE FROM repos WHERE id = ?", [found.id]);
  forget(found.id);
  await strata.remove(found.id).catch((error) => console.error(`repository ${found.id} is left behind in Strata:`, error));
  return c.json({});
});

api.get(`${REPO}/commits`, async (c) => {
  const skip = Math.max(0, Number(c.req.query("skip")) || 0);
  const page = await strata.commits(c.get("repo").id, c.req.query("rev") ?? "", skip, 30);
  return c.json({ ...page, commits: await withVerification(page.commits) });
});

api.get(`${REPO}/commit`, async (c) => c.json((await withVerification([await strata.commit(c.get("repo").id, c.req.query("rev") ?? "")]))[0]));

api.get(`${REPO}/tree`, async (c) => c.json(await strata.tree(c.get("repo").id, c.req.query("rev") ?? "", c.req.query("path") ?? "")));

const IMAGES: Record<string, string> = { png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp", svg: "image/svg+xml", ico: "image/x-icon" };

/** A file's bytes, by path at a revision or by object id. */
api.get(`${REPO}/blob`, async (c) => {
  const { rev, path, oid, download } = c.req.query();
  const response = await strata.blob(c.get("repo").id, oid ? { oid } : { rev, path });
  const name = (path ?? "").split("/").pop() || "file";
  const headers = new Headers({
    "content-type": IMAGES[name.split(".").pop()?.toLowerCase() ?? ""] ?? "application/octet-stream",
    // Pushed content is untrusted: it must never run with the hub's origin.
    "content-security-policy": "sandbox; default-src 'none'; style-src 'unsafe-inline'",
    "x-content-type-options": "nosniff",
    "cache-control": "private, no-cache",
  });
  if (download) headers.set("content-disposition", `attachment; filename*=UTF-8''${encodeURIComponent(name)}`);
  return new Response(response.body, { headers });
});

api.get(`${REPO}/activity`, (c) => {
  const rows = db.query<ActivityRow, [string]>(`${ACTIVITY} WHERE activity.repo_id = ? ORDER BY activity.id DESC LIMIT 50`).all(c.get("repo").id);
  return c.json({ activity: rows.map(activityJson) });
});

addPullRoutes(api);
addRuleRoutes(api);
addWebhookRoutes(api);
