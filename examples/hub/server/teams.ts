// Teams: groups of an organization's members that are given access to
// repositories together. A team can follow a group at a sign-in provider;
// its members are then whoever signs in as part of that group.
import type { Context, Hono } from "hono";
import { fail, signedIn, text, type Env } from "./common";
import { signInAt } from "./config";
import { db, ownerByName, repoByPath, roleIn, type Owner } from "./db";

type TeamRow = { id: number; org_id: number; name: string; description: string; sso_provider: string | null; sso_group: string | null };

const TEAM_NAME = /^[a-zA-Z0-9](?:[a-zA-Z0-9._-]{0,48}[a-zA-Z0-9])?$/;
const LEVELS = ["read", "write", "admin"];
const BASES = ["none", "read", "write"];

const teamsOfOrg = (orgId: number) => db.query<TeamRow & { members: number; repos: number }, [number]>(
  `SELECT teams.*, (SELECT COUNT(*) FROM team_members WHERE team_id = teams.id) AS members, (SELECT COUNT(*) FROM team_repos WHERE team_id = teams.id) AS repos
   FROM teams WHERE org_id = ? ORDER BY name`,
).all(orgId);

/** Takes someone out of an organization, and out of its teams with it. */
export function removeMember(orgId: number, userId: number) {
  db.transaction(() => {
    db.run("DELETE FROM team_members WHERE user_id = ? AND team_id IN (SELECT id FROM teams WHERE org_id = ?)", [userId, orgId]);
    db.run("DELETE FROM members WHERE org_id = ? AND user_id = ?", [orgId, userId]);
  })();
}

/**
 * Brings someone's teams in line with the groups they signed in with: into
 * every team that follows one of those groups, out of every team that
 * follows another group of the same provider. Joining a team makes them a
 * member of its organization; a membership that came this way goes with the
 * last such team. `groups` is null when the provider did not say, and then
 * nothing changes.
 */
export function syncTeams(userId: number, providerId: string, groups: string[] | null) {
  if (!groups) return;
  const has = new Set(groups.map((group) => group.toLowerCase()));
  db.transaction(() => {
    const following = db.query<TeamRow, [string]>("SELECT * FROM teams WHERE sso_provider = ? AND sso_group IS NOT NULL").all(providerId);
    const touched = new Set<number>();
    for (const team of following) {
      touched.add(team.org_id);
      if (has.has(team.sso_group!.toLowerCase())) {
        db.run("INSERT OR IGNORE INTO members (org_id, user_id, role, synced) VALUES (?, ?, 'member', 1)", [team.org_id, userId]);
        db.run("INSERT OR IGNORE INTO team_members (team_id, user_id) VALUES (?, ?)", [team.id, userId]);
      } else {
        db.run("DELETE FROM team_members WHERE team_id = ? AND user_id = ?", [team.id, userId]);
      }
    }
    for (const orgId of touched) {
      const left = db.query<{ n: number }, [number, number]>("SELECT COUNT(*) AS n FROM team_members JOIN teams ON teams.id = team_members.team_id WHERE teams.org_id = ? AND team_members.user_id = ?").get(orgId, userId)!.n;
      if (left === 0) db.run("DELETE FROM members WHERE org_id = ? AND user_id = ? AND synced = 1", [orgId, userId]);
    }
  })();
}

/** The teams that reach a repository, for its settings page. */
export function teamsWithAccess(repoId: string) {
  return db.query<{ name: string; level: string }, [string]>("SELECT teams.name, team_repos.level FROM team_repos JOIN teams ON teams.id = team_repos.team_id WHERE team_repos.repo_id = ? ORDER BY teams.name").all(repoId);
}

export function addTeamRoutes(api: Hono<Env>) {
  /** The organization in the path, for one of its members; with `owner`, for one of its owners. */
  const org = (c: Context<Env>, owner = false): Owner => {
    const user = signedIn(c);
    const found = ownerByName(c.req.param("name") ?? "");
    const role = found?.kind === "org" ? roleIn(found.id, user.id) : null;
    // Teams are the organization's own business: to others they are not there.
    if (!found || !role) throw fail(404, "no such organization");
    if (owner && role !== "owner") throw fail(403, "only owners of the organization can change its teams");
    return found;
  };
  const team = (c: Context<Env>, orgId: number) => {
    const found = db.query<TeamRow, [number, string]>("SELECT * FROM teams WHERE org_id = ? AND name = ?").get(orgId, c.req.param("team") ?? "");
    if (!found) throw fail(404, "no such team");
    return found;
  };
  /** The group a team follows, as the request names it: both parts or neither. */
  const followed = (body: { ssoProvider?: unknown; ssoGroup?: unknown }) => {
    const [provider, group] = [text(body.ssoProvider, 50), text(body.ssoGroup, 200)];
    if (!provider && !group) return [null, null];
    if (!provider || !group) throw fail(400, "to follow a group, name both the sign-in provider and the group");
    if (!signInAt.some((known) => known.id === provider)) throw fail(400, `there is no sign-in provider ${provider} here`);
    return [provider, group];
  };

  /** What every member of the organization may do with each of its repositories. */
  api.patch("/owners/:name", async (c) => {
    const found = org(c, true);
    const body = await c.req.json();
    const base = body.basePermission === undefined ? found.base_permission : String(body.basePermission);
    if (!BASES.includes(base)) throw fail(400, `basePermission is one of ${BASES.join(", ")}`);
    db.run("UPDATE owners SET base_permission = ?, display_name = ? WHERE id = ?", [base, body.displayName === undefined ? found.display_name : text(body.displayName, 100), found.id]);
    return c.json({});
  });

  api.get("/owners/:name/teams", (c) => {
    const found = org(c);
    return c.json({
      teams: teamsOfOrg(found.id).map((row) => ({ name: row.name, description: row.description, members: row.members, repos: row.repos, ssoProvider: row.sso_provider, ssoGroup: row.sso_group })),
      providers: signInAt.map(({ id, name }) => ({ id, name })),
    });
  });

  api.post("/owners/:name/teams", async (c) => {
    const found = org(c, true);
    const body = await c.req.json();
    const name = String(body.name ?? "").trim();
    if (!TEAM_NAME.test(name)) throw fail(400, "team names are 1 to 50 letters, digits, '-', '_' or '.'");
    if (db.query("SELECT 1 FROM teams WHERE org_id = ? AND name = ?").get(found.id, name)) throw fail(409, `there is a team ${name} already`);
    const [provider, group] = followed(body);
    db.run("INSERT INTO teams (org_id, name, description, sso_provider, sso_group) VALUES (?, ?, ?, ?, ?)", [found.id, name, text(body.description, 300), provider, group]);
    return c.json({ name }, 201);
  });

  api.get("/owners/:name/teams/:team", (c) => {
    const found = org(c);
    const row = team(c, found.id);
    const members = db.query<{ name: string }, [number]>("SELECT owners.name FROM team_members JOIN owners ON owners.id = team_members.user_id WHERE team_members.team_id = ? ORDER BY owners.name").all(row.id);
    const repos = db.query<{ name: string; level: string }, [number]>("SELECT repos.name, team_repos.level FROM team_repos JOIN repos ON repos.id = team_repos.repo_id WHERE team_repos.team_id = ? ORDER BY repos.name").all(row.id);
    const all = db.query<{ name: string }, [number]>("SELECT name FROM repos WHERE owner_id = ? ORDER BY name").all(found.id);
    return c.json({
      org: found.name,
      name: row.name,
      description: row.description,
      ssoProvider: row.sso_provider,
      ssoGroup: row.sso_group,
      members: members.map((member) => member.name),
      repos,
      orgRepos: all.map((repo) => repo.name),
      providers: signInAt.map(({ id, name }) => ({ id, name })),
      canChange: roleIn(found.id, signedIn(c).id) === "owner",
    });
  });

  api.patch("/owners/:name/teams/:team", async (c) => {
    const found = org(c, true);
    const row = team(c, found.id);
    const body = await c.req.json();
    const [provider, group] = body.ssoProvider === undefined && body.ssoGroup === undefined ? [row.sso_provider, row.sso_group] : followed(body);
    db.run("UPDATE teams SET description = ?, sso_provider = ?, sso_group = ? WHERE id = ?", [body.description === undefined ? row.description : text(body.description, 300), provider, group, row.id]);
    return c.json({});
  });

  api.delete("/owners/:name/teams/:team", (c) => {
    const found = org(c, true);
    db.run("DELETE FROM teams WHERE id = ?", [team(c, found.id).id]);
    return c.json({});
  });

  api.put("/owners/:name/teams/:team/members/:username", (c) => {
    const found = org(c, true);
    const row = team(c, found.id);
    const member = ownerByName(c.req.param("username"));
    if (!member || member.kind !== "user") throw fail(404, "no such user");
    // A team is made of the organization's members: joining one is joining the other.
    db.transaction(() => {
      db.run("INSERT OR IGNORE INTO members (org_id, user_id, role) VALUES (?, ?, 'member')", [found.id, member.id]);
      db.run("INSERT OR IGNORE INTO team_members (team_id, user_id) VALUES (?, ?)", [row.id, member.id]);
    })();
    return c.json({});
  });

  api.delete("/owners/:name/teams/:team/members/:username", (c) => {
    const found = org(c, true);
    const row = team(c, found.id);
    const member = ownerByName(c.req.param("username"));
    if (member) db.run("DELETE FROM team_members WHERE team_id = ? AND user_id = ?", [row.id, member.id]);
    return c.json({});
  });

  api.put("/owners/:name/teams/:team/repos/:repo", async (c) => {
    const found = org(c, true);
    const row = team(c, found.id);
    const repo = repoByPath(found.name, c.req.param("repo"));
    if (!repo) throw fail(404, "the organization has no such repository");
    const level = String((await c.req.json()).level ?? "");
    if (!LEVELS.includes(level)) throw fail(400, `level is one of ${LEVELS.join(", ")}`);
    db.run("INSERT INTO team_repos (team_id, repo_id, level) VALUES (?, ?, ?) ON CONFLICT (team_id, repo_id) DO UPDATE SET level = excluded.level", [row.id, repo.id, level]);
    return c.json({});
  });

  api.delete("/owners/:name/teams/:team/repos/:repo", (c) => {
    const found = org(c, true);
    const row = team(c, found.id);
    const repo = repoByPath(found.name, c.req.param("repo"));
    if (repo) db.run("DELETE FROM team_repos WHERE team_id = ? AND repo_id = ?", [row.id, repo.id]);
    return c.json({});
  });
}
