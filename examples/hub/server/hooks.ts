// The two questions Strata asks the hub: may this git request go ahead, and
// may this push move these refs.
import { Hono } from "hono";
import { gitUser, sha256 } from "./auth";
import { config } from "./config";
import { canRead, canWrite, permission, repoById, repoByPath } from "./db";
import { verify } from "./signing";
import { strata, type StrataCommit } from "./strata";

export const hooks = new Hono();

hooks.use("*", async (c, next) => {
  const presented = c.req.header("authorization") ?? "";
  if (!config.hookSecret || sha256(presented) !== sha256(`Bearer ${config.hookSecret}`)) return c.text("forbidden\n", 403);
  await next();
});

hooks.post("/authz", async (c) => {
  const { path, access, authorization } = await c.req.json<{ path: string; access: "read" | "write"; authorization?: string | null }>();
  const user = await gitUser(authorization);
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
  return c.json({ allow: true, repo: repo.id, actor: user?.name });
});

hooks.post("/pre-receive", async (c) => {
  const { repo: id, updates } = await c.req.json<{ repo: string; actor?: string; updates: { ref: string; change: string; commits?: StrataCommit[]; commits_truncated?: boolean }[] }>();
  const repo = repoById(id);
  if (!repo) return c.json({ allow: false, message: "repository not found" });
  if (repo.protect_default || repo.require_pulls || repo.require_signed) {
    const { head } = await strata.refs(id);
    const branch = head.replace("refs/heads/", "");
    if (repo.require_signed) {
      // Every commit the push brings to the branch, not just its tip.
      for (const update of updates.filter((update) => update.ref === head)) {
        if (update.commits_truncated) return c.json({ allow: false, message: `too many commits to check the signatures of in one push to ${branch}` });
        for (const commit of update.commits ?? []) {
          const checked = await verify(commit.signature);
          if (checked?.state === "verified") continue;
          return c.json({ allow: false, message: `${branch} takes signed commits only, and ${commit.oid.slice(0, 7)} ${checked ? checked.reason : "is not signed"}` });
        }
      }
    }
    // Merging a pull request is the hub's own doing and does not come through here.
    if (repo.require_pulls && updates.some((update) => update.ref === head && update.change !== "create")) {
      return c.json({ allow: false, message: `${branch} only takes changes through pull requests` });
    }
    const broken = repo.protect_default && updates.find((update) => update.ref === head && (update.change === "forced" || update.change === "delete"));
    if (broken) {
      const what = broken.change === "delete" ? "cannot be deleted" : "does not accept force pushes";
      return c.json({ allow: false, message: `${branch} is protected and ${what}` });
    }
  }
  return c.json({ allow: true });
});
