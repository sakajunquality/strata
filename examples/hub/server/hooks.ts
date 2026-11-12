// The two questions Strata asks the hub: may this git request go ahead, and
// may this push move these refs.
import { Hono } from "hono";
import { gitUser, sha256, sshUser } from "./auth";
import { config } from "./config";
import { canRead, canWrite, mirrorOf, permission, repoById, repoByPath } from "./db";
import { ruleFor, rulesOf } from "./rules";
import { verify } from "./signing";
import type { StrataCommit } from "./strata";

export const hooks = new Hono();

hooks.use("*", async (c, next) => {
  const presented = c.req.header("authorization") ?? "";
  if (!config.hookSecret || sha256(presented) !== sha256(`Bearer ${config.hookSecret}`)) return c.text("forbidden\n", 403);
  await next();
});

hooks.post("/authz", async (c) => {
  const { path, access, authorization, ssh_key: sshKey } = await c.req.json<{ path: string; access: "read" | "write" | "identify"; authorization?: string | null; ssh_key?: string | null }>();
  // Over SSH a client is known by its key, and is asked about before it is let in at all.
  const user = sshKey ? sshUser(sshKey) : await gitUser(authorization);
  if (access === "identify") return c.json(user ? { allow: true, actor: user.name } : { allow: false });
  if (sshKey && !user) return c.json({ allow: false, status: 401, message: "that SSH key is not registered here" });
  if (authorization && !user) return c.json({ allow: false, status: 401, message: "wrong user name, password or access token" });

  const [owner, name, ...rest] = path.split("/");
  const repo = owner && name && rest.length === 0 ? repoByPath(owner, name) : null;
  const level = repo ? permission(repo, user?.id ?? null) : "none";
  if (!repo || !canRead(level)) {
    // Do not tell a private repository from one that does not exist.
    return c.json(user ? { allow: false, status: 404, message: "repository not found" } : { allow: false, status: 401, message: "sign in to continue" });
  }
  if (access === "write" && !canWrite(level)) {
    return c.json(user ? { allow: false, status: 403, message: `${user.name} may not push to ${repo.owner_name}/${repo.name}` } : { allow: false, status: 401, message: "sign in to push" });
  }
  // The next fetch from its source would undo whatever was pushed.
  const mirror = access === "write" ? mirrorOf(repo.id) : null;
  if (mirror) return c.json({ allow: false, status: 403, message: `${repo.owner_name}/${repo.name} mirrors ${mirror.source} and does not take pushes` });
  return c.json({ allow: true, repo: repo.id, actor: user?.name });
});

hooks.post("/pre-receive", async (c) => {
  const { repo: id, updates } = await c.req.json<{ repo: string; actor?: string; updates: { ref: string; change: string; commits?: StrataCommit[]; commits_truncated?: boolean }[] }>();
  const repo = repoById(id);
  if (!repo) return c.json({ allow: false, message: "repository not found" });
  const rules = await rulesOf(repo);
  for (const update of rules.length ? updates : []) {
    if (!update.ref.startsWith("refs/heads/")) continue;
    const branch = update.ref.slice("refs/heads/".length);
    const rule = ruleFor(rules, branch);
    if (!rule) continue;
    if (rule.noDelete && update.change === "delete") return c.json({ allow: false, message: `${branch} is protected and cannot be deleted` });
    if (rule.noForce && update.change === "forced") return c.json({ allow: false, message: `${branch} is protected and does not accept force pushes` });
    if (rule.requireSigned && update.change !== "delete") {
      // Every commit the push brings to the branch, not just its tip.
      if (update.commits_truncated) return c.json({ allow: false, message: `too many commits to check the signatures of in one push to ${branch}` });
      for (const commit of update.commits ?? []) {
        const checked = await verify(commit.signature);
        if (checked?.state === "verified") continue;
        return c.json({ allow: false, message: `${branch} takes signed commits only, and ${commit.oid.slice(0, 7)} ${checked ? checked.reason : "is not signed"}` });
      }
    }
    // Merging a pull request is the hub's own doing and does not come through here.
    // A branch may be started by a push: there is nothing yet to propose changes to.
    if (rule.requirePulls && update.change !== "create" && update.change !== "delete") return c.json({ allow: false, message: `${branch} only takes changes through pull requests` });
  }
  return c.json({ allow: true });
});
