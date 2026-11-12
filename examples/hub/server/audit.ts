// The audit log: who changed what about accounts, organizations and
// repositories, and when. What happens to code (pushes, merges by push) is
// in each repository's activity; this is about access and settings.
//
// An entry belongs to an owner (a user's own account, or an organization)
// and maybe to one repository. It is kept when what it is about is deleted:
// the names it needs are written into it.
import type { Context, Hono } from "hono";
import { fail, REPO, signedIn, type Env } from "./common";
import { db, now, ownerByName, roleIn, type Owner, type Repo } from "./db";

db.exec(`
CREATE TABLE IF NOT EXISTS audit (
  id INTEGER PRIMARY KEY,
  time INTEGER NOT NULL,
  -- Who did it, by name: the account may be gone by the time this is read. Null for what nobody signed in did.
  actor TEXT,
  action TEXT NOT NULL,
  owner_id INTEGER NOT NULL,
  repo_id TEXT,
  -- What it was done to, in words: "orbit/core", "team engine", "bob".
  subject TEXT NOT NULL DEFAULT '',
  detail TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS audit_by_owner ON audit (owner_id, id);
CREATE INDEX IF NOT EXISTS audit_by_repo ON audit (repo_id, id);
`);

type Where = { owner: number; repo?: Repo | null; subject?: string };

/** Writes an entry. `actor` is whoever is signed in on `c`, a name, or nobody. */
export function audit(c: Context<Env> | string | null, action: string, where: Where, detail: Record<string, unknown> = {}) {
  const actor = typeof c === "string" || c === null ? c : (c.get("user")?.name ?? null);
  const subject = where.subject ?? (where.repo ? `${where.repo.owner_name}/${where.repo.name}` : "");
  db.run("INSERT INTO audit (time, actor, action, owner_id, repo_id, subject, detail) VALUES (?, ?, ?, ?, ?, ?, ?)", [now(), actor, action, where.owner, where.repo?.id ?? null, subject, JSON.stringify(detail)]);
}

/** An entry about a repository: it belongs to the repository's owner as well. */
export const auditRepo = (c: Context<Env> | string | null, action: string, repo: Repo, detail: Record<string, unknown> = {}) => audit(c, action, { owner: repo.owner_id, repo }, detail);

type Row = { id: number; time: number; actor: string | null; action: string; subject: string; detail: string };

function page(c: Context<Env>, where: string, args: (string | number)[]) {
  const before = Number(c.req.query("before")) || Number.MAX_SAFE_INTEGER;
  const rows = db.query<Row, (string | number)[]>(`SELECT id, time, actor, action, subject, detail FROM audit WHERE ${where} AND id < ? ORDER BY id DESC LIMIT 101`).all(...args, before);
  const entries = rows.slice(0, 100).map((row) => ({ ...row, detail: JSON.parse(row.detail) }));
  return c.json({ entries, next: rows.length > 100 ? entries[99]!.id : null });
}

export function addAuditRoutes(api: Hono<Env>) {
  /** What happened to an account (its own to read) or to an organization and its repositories (its owners'). */
  api.get("/owners/:name/audit", (c) => {
    const user = signedIn(c);
    const owner: Owner | null = ownerByName(c.req.param("name"));
    const allowed = owner && (owner.kind === "user" ? owner.id === user.id : roleIn(owner.id, user.id) === "owner");
    if (!owner || !allowed) throw fail(404, "no such audit log");
    return page(c, "owner_id = ?", [owner.id]);
  });

  api.get(`${REPO}/audit`, (c) => {
    if (c.get("level") !== "admin") throw fail(403, "only the repository's owners can read its audit log");
    return page(c, "repo_id = ?", [c.get("repo").id]);
  });
}
