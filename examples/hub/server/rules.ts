// Branch rules: what a branch asks of the changes that reach it, by push or
// by merging a pull request. And commit statuses, which rules can ask for.
import type { Hono } from "hono";
import { auditRepo } from "./audit";
import { fail, REPO, signedIn, text, type Env } from "./common";
import { canWrite, db, now, type Repo } from "./db";
import { strata, StrataError } from "./strata";
import { emit, repoJson } from "./webhooks";

type RuleRow = { id: number; repo_id: string; pattern: string; no_force: number; no_delete: number; require_pulls: number; approvals: number; require_signed: number; checks: string };
export type Rule = { id: number; pattern: string; noForce: boolean; noDelete: boolean; requirePulls: boolean; approvals: number; requireSigned: boolean; checks: string[] };

const rule = (row: RuleRow): Rule => ({
  id: row.id,
  pattern: row.pattern,
  noForce: !!row.no_force,
  noDelete: !!row.no_delete,
  requirePulls: !!row.require_pulls,
  approvals: row.approvals,
  requireSigned: !!row.require_signed,
  checks: JSON.parse(row.checks),
});

/**
 * A repository's rules. Those of a database from before there were rules
 * (three switches for the default branch) become a rule for that branch the
 * first time they are asked for.
 */
export async function rulesOf(repo: Repo): Promise<Rule[]> {
  if (repo.protect_default || repo.require_pulls || repo.require_signed) {
    const { head } = await strata.refs(repo.id, "refs/heads/");
    db.transaction(() => {
      db.run("INSERT OR IGNORE INTO branch_rules (repo_id, pattern, no_force, no_delete, require_pulls, require_signed) VALUES (?, ?, ?, ?, ?, ?)", [
        repo.id, head.replace("refs/heads/", ""), repo.protect_default, repo.protect_default, repo.require_pulls, repo.require_signed,
      ]);
      db.run("UPDATE repos SET protect_default = 0, require_pulls = 0, require_signed = 0 WHERE id = ?", [repo.id]);
    })();
    repo.protect_default = repo.require_pulls = repo.require_signed = 0;
  }
  return db.query<RuleRow, [string]>("SELECT * FROM branch_rules WHERE repo_id = ? ORDER BY id").all(repo.id).map(rule);
}

const matches = (pattern: string, branch: string) => new RegExp(`^${pattern.split("*").map((part) => part.replace(/[.+?^${}()|[\]\\]/g, "\\$&")).join(".*")}$`).test(branch);

/** The rule a branch falls under: the one that names it, or else the oldest whose pattern fits. */
export function ruleFor(rules: Rule[], branch: string): Rule | null {
  return rules.find((rule) => rule.pattern === branch) ?? rules.find((rule) => matches(rule.pattern, branch)) ?? null;
}

export type Status = { context: string; state: "pending" | "success" | "failure" | "error"; description: string; targetUrl: string; createdAt: number; creator: string | null };

/** What is known about a commit: for each check, what was reported last. */
export function statusesOf(repoId: string, oid: string): Status[] {
  return db
    .query<Status, [string, string]>(
      `SELECT statuses.context, statuses.state, statuses.description, statuses.target_url AS targetUrl, statuses.created_at AS createdAt, owners.name AS creator
       FROM statuses LEFT JOIN owners ON owners.id = statuses.creator_id
       WHERE statuses.id IN (SELECT MAX(id) FROM statuses WHERE repo_id = ? AND oid = ? GROUP BY context) ORDER BY statuses.context`,
    )
    .all(repoId, oid);
}

/** All of a commit's checks as one word. Null if nothing was reported. */
export function combined(statuses: Status[]): Status["state"] | null {
  if (statuses.length === 0) return null;
  if (statuses.some((status) => status.state === "failure" || status.state === "error")) return "failure";
  return statuses.some((status) => status.state === "pending") ? "pending" : "success";
}

const PATTERN = /^[^\s~^:?[\\]{1,200}$/;
const STATES = ["pending", "success", "failure", "error"];

export function addRuleRoutes(api: Hono<Env>) {
  const admin = (level: string) => {
    if (level !== "admin") throw fail(403, "only the repository's owners can change its settings");
  };

  api.get(`${REPO}/rules`, async (c) => c.json({ rules: await rulesOf(c.get("repo")) }));

  /** Sets the rule for a pattern: a new one, or the one there is. */
  api.post(`${REPO}/rules`, async (c) => {
    admin(c.get("level"));
    const repo = c.get("repo");
    await rulesOf(repo);
    const body = await c.req.json();
    const pattern = String(body.pattern ?? "").trim();
    if (!PATTERN.test(pattern) || pattern.includes("..")) throw fail(400, "name a branch, with * for any part of the name: main, release/*");
    const checks = [...new Set((Array.isArray(body.checks) ? body.checks : []).map((check: unknown) => text(check, 100)).filter(Boolean))].slice(0, 20);
    const approvals = Math.max(0, Math.min(10, Math.floor(Number(body.approvals) || 0)));
    // Reviews and checks are of pull requests: asking for them asks for pull requests.
    const requirePulls = !!body.requirePulls || approvals > 0 || checks.length > 0;
    db.run(
      `INSERT INTO branch_rules (repo_id, pattern, no_force, no_delete, require_pulls, approvals, require_signed, checks) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT (repo_id, pattern) DO UPDATE SET no_force = excluded.no_force, no_delete = excluded.no_delete, require_pulls = excluded.require_pulls,
         approvals = excluded.approvals, require_signed = excluded.require_signed, checks = excluded.checks`,
      [repo.id, pattern, body.noForce === false ? 0 : 1, body.noDelete === false ? 0 : 1, requirePulls ? 1 : 0, approvals, body.requireSigned ? 1 : 0, JSON.stringify(checks)],
    );
    auditRepo(c, "rule.set", repo, { pattern, noForce: body.noForce !== false, noDelete: body.noDelete !== false, requirePulls, approvals, checks, requireSigned: !!body.requireSigned });
    return c.json({ rules: await rulesOf(repo) });
  });

  api.delete(`${REPO}/rules/:id`, (c) => {
    admin(c.get("level"));
    const repo = c.get("repo");
    const gone = db.query<{ pattern: string }, [number, string]>("SELECT pattern FROM branch_rules WHERE id = ? AND repo_id = ?").get(Number(c.req.param("id")), repo.id);
    db.run("DELETE FROM branch_rules WHERE id = ? AND repo_id = ?", [Number(c.req.param("id")), repo.id]);
    if (gone) auditRepo(c, "rule.remove", repo, { pattern: gone.pattern });
    return c.json({});
  });

  api.get(`${REPO}/statuses/:oid`, (c) => {
    const statuses = statusesOf(c.get("repo").id, c.req.param("oid").toLowerCase());
    return c.json({ state: combined(statuses), statuses });
  });

  /** Reports what a check found out about a commit. For programs: they come with an access token. */
  api.post(`${REPO}/statuses/:oid`, async (c) => {
    const user = signedIn(c);
    const repo = c.get("repo");
    if (!canWrite(c.get("level"))) throw fail(403, "only those who can push here can report on commits");
    const oid = c.req.param("oid").toLowerCase();
    if (!/^[0-9a-f]{40}$/.test(oid)) throw fail(400, "name the commit by its full id");
    const body = await c.req.json();
    const state = String(body.state ?? "");
    if (!STATES.includes(state)) throw fail(400, `state is one of ${STATES.join(", ")}`);
    const context = text(body.context, 100) || "default";
    const targetUrl = text(body.targetUrl ?? body.target_url, 500);
    if (targetUrl && !/^https?:\/\//i.test(targetUrl)) throw fail(400, "targetUrl is an http or https address");
    await strata.commit(repo.id, oid).catch((error) => {
      throw error instanceof StrataError && error.status === 404 ? fail(404, "there is no such commit here") : error;
    });
    const description = text(body.description, 200);
    db.run("INSERT INTO statuses (repo_id, oid, context, state, description, target_url, creator_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)", [repo.id, oid, context, state, description, targetUrl, user.id, now()]);
    emit(repo, "status", { sha: oid, context, state, description, target_url: targetUrl, repository: repoJson(repo), sender: user.name });
    return c.json({ state: combined(statusesOf(repo.id, oid)) }, 201);
  });
}
