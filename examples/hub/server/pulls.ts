// Pull requests: a proposal to merge one branch of a repository into another.
// The hub keeps the conversation; what differs and whether it merges is asked
// of Strata each time, so a pull request follows its branches as they move.
import type { Hono } from "hono";
import { fail, REPO, signedIn, text, type Env } from "./common";
import { canWrite, db, now, type Owner, type Permission, type Repo } from "./db";
import { strata, StrataError, type Compare, type StrataEvent } from "./strata";

type PullRow = {
  id: number;
  repo_id: string;
  number: number;
  title: string;
  body: string;
  author_id: number;
  base: string;
  head: string;
  state: "open" | "closed" | "merged";
  created_at: number;
  closed_at: number | null;
  base_oid: string | null;
  head_oid: string | null;
  merge_oid: string | null;
  author_name: string;
  closed_by_name: string | null;
  comments: number;
};

const PULLS = `
  SELECT pulls.*, author.name AS author_name, closer.name AS closed_by_name,
    (SELECT COUNT(*) FROM comments WHERE comments.pull_id = pulls.id) AS comments
  FROM pulls JOIN owners author ON author.id = pulls.author_id LEFT JOIN owners closer ON closer.id = pulls.closed_by`;

const pullJson = (row: PullRow) => ({
  number: row.number,
  title: row.title,
  body: row.body,
  author: row.author_name,
  base: row.base,
  head: row.head,
  state: row.state,
  createdAt: row.created_at,
  closedAt: row.closed_at,
  closedBy: row.closed_by_name,
  mergeOid: row.merge_oid,
  comments: row.comments,
});

export const openPullCount = (repoId: string) => db.query<{ n: number }, [string]>("SELECT COUNT(*) AS n FROM pulls WHERE repo_id = ? AND state = 'open'").get(repoId)!.n;

const openPulls = (repoId: string) => db.query<PullRow, [string]>(`${PULLS} WHERE pulls.repo_id = ? AND pulls.state = 'open'`).all(repoId);

/** Strata's answer, or null where a branch or commit is no longer there. */
async function unlessGone<T>(asked: Promise<T>): Promise<T | null> {
  try {
    return await asked;
  } catch (error) {
    if (error instanceof StrataError && error.status === 404) return null;
    throw error;
  }
}

type Seen = Pick<Compare, "merge_base" | "base" | "head">;
const seen = (compared: Seen | null) => [compared ? (compared.merge_base ?? compared.base) : null, compared?.head ?? null];

/**
 * Notes what an open pull request compares right now. It is what is shown
 * once the pull request has ended, when the branches have moved on or gone.
 */
function remember(pull: PullRow, compared: Seen) {
  const [base, head] = seen(compared);
  if (base !== pull.base_oid || head !== pull.head_oid) db.run("UPDATE pulls SET base_oid = ?, head_oid = ? WHERE id = ?", [base, head, pull.id]);
}

/** Ends a pull request. Without a comparison, what was last noted stands. */
function finish(pull: PullRow, state: "closed" | "merged", by: Owner | null, compared: Seen | null, mergeOid: string | null = null) {
  db.run("UPDATE pulls SET state = ?, closed_at = ?, closed_by = ?, base_oid = COALESCE(?, base_oid), head_oid = COALESCE(?, head_oid), merge_oid = ? WHERE id = ?", [
    state,
    now(),
    by?.id ?? null,
    ...seen(compared),
    mergeOid,
    pull.id,
  ]);
}

/**
 * Keeps pull requests in step with a push: one that lost a branch is closed,
 * and one whose commits have all arrived in its base (merged by hand and
 * pushed, say) is merged. The event only says where to look; the feed can
 * lag, so what counts is how the branches stand now.
 */
export async function followPush(repo: Repo, updates: NonNullable<NonNullable<StrataEvent["event"]>["updates"]>) {
  const pushed = new Set(updates.filter((update) => update.ref.startsWith("refs/heads/")).map((update) => update.ref.slice("refs/heads/".length)));
  for (const pull of openPulls(repo.id)) {
    if (!pushed.has(pull.base) && !pushed.has(pull.head)) continue;
    const compared = await unlessGone(strata.compare(repo.id, pull.base, pull.head));
    if (!compared) finish(pull, "closed", null, null);
    else if (compared.ahead === 0) finish(pull, "merged", null, null, compared.base);
    else remember(pull, compared);
  }
}

function mayChange(pull: PullRow, user: Owner, level: Permission) {
  return pull.author_id === user.id || canWrite(level);
}

export function addPullRoutes(api: Hono<Env>) {
  const branchesOf = async (repo: Repo) => {
    const { refs } = await strata.refs(repo.id);
    return new Set(refs.filter((ref) => ref.name.startsWith("refs/heads/")).map((ref) => ref.name.slice("refs/heads/".length)));
  };

  const duplicate = (repo: Repo, base: string, head: string) =>
    db.query<{ number: number }, [string, string, string]>("SELECT number FROM pulls WHERE repo_id = ? AND base = ? AND head = ? AND state = 'open'").get(repo.id, base, head)?.number ?? null;

  /** Two branches of the repository, checked to be there and to differ. */
  async function pair(repo: Repo, rawBase: unknown, rawHead: unknown) {
    const [base, head] = [String(rawBase ?? ""), String(rawHead ?? "")];
    const branches = await branchesOf(repo);
    for (const branch of [base, head]) if (!branches.has(branch)) throw fail(404, `there is no branch ${branch || "(none)"}`);
    if (base === head) throw fail(400, "choose two different branches");
    return { base, head };
  }

  const find = (repo: Repo, raw: string | undefined) => {
    const pull = db.query<PullRow, [string, number]>(`${PULLS} WHERE pulls.repo_id = ? AND pulls.number = ?`).get(repo.id, Number(raw));
    if (!pull) throw fail(404, "no such pull request");
    return pull;
  };

  /** What a pull request between two branches would hold. */
  api.get(`${REPO}/compare`, async (c) => {
    const repo = c.get("repo");
    const { base, head } = await pair(repo, c.req.query("base"), c.req.query("head"));
    return c.json({ ...(await strata.compare(repo.id, base, head)), existing: duplicate(repo, base, head) });
  });

  api.get(`${REPO}/pulls`, (c) => {
    const repo = c.get("repo");
    const open = c.req.query("state") !== "closed";
    const rows = db.query<PullRow, [string]>(`${PULLS} WHERE pulls.repo_id = ? AND pulls.state ${open ? "=" : "<>"} 'open' ORDER BY pulls.number DESC LIMIT 200`).all(repo.id);
    const total = db.query<{ n: number }, [string]>("SELECT COUNT(*) AS n FROM pulls WHERE repo_id = ?").get(repo.id)!.n;
    const openCount = openPullCount(repo.id);
    return c.json({ pulls: rows.map(pullJson), open: openCount, closed: total - openCount });
  });

  api.post(`${REPO}/pulls`, async (c) => {
    const user = signedIn(c);
    const repo = c.get("repo");
    const body = await c.req.json();
    const title = text(body.title, 200);
    if (!title) throw fail(400, "give the pull request a title");
    const { base, head } = await pair(repo, body.base, body.head);
    const existing = duplicate(repo, base, head);
    if (existing) throw fail(409, `#${existing} already proposes merging ${head} into ${base}`);
    const compared = await strata.compare(repo.id, base, head);
    if (compared.ahead === 0) throw fail(409, `${head} has nothing that ${base} lacks`);
    const number = db.transaction(() => {
      const next = db.query<{ n: number }, [string]>("SELECT COALESCE(MAX(number), 0) + 1 AS n FROM pulls WHERE repo_id = ?").get(repo.id)!.n;
      db.run("INSERT INTO pulls (repo_id, number, title, body, author_id, base, head, created_at, base_oid, head_oid) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)", [
        repo.id, next, title, text(body.body, 20000), user.id, base, head, now(), ...seen(compared),
      ]);
      return next;
    })();
    return c.json({ number }, 201);
  });

  api.get(`${REPO}/pulls/:number`, async (c) => {
    const repo = c.get("repo");
    const user = c.get("user");
    const pull = find(repo, c.req.param("number"));
    const open = pull.state === "open";
    // Open: the branches as they are now. Otherwise: as they were when it ended.
    const [base, head] = open ? [pull.base, pull.head] : [pull.base_oid, pull.head_oid];
    const compared = base && head ? await unlessGone(strata.compare(repo.id, base, head)) : null;
    if (open && compared && compared.ahead > 0) remember(pull, compared);
    const check = open && compared && compared.ahead > 0 ? await unlessGone(strata.mergeable(repo.id, pull.base, pull.head)) : null;
    const comments = db
      .query("SELECT comments.id, comments.body, comments.created_at AS createdAt, owners.name AS author FROM comments JOIN owners ON owners.id = comments.author_id WHERE comments.pull_id = ? ORDER BY comments.id")
      .all(pull.id);
    return c.json({
      ...pullJson(pull),
      discussion: comments,
      compare: compared,
      mergeable: check?.mergeable ?? null,
      conflicts: check?.conflicts ?? [],
      can: { change: !!user && mayChange(pull, user, c.get("level")), merge: canWrite(c.get("level")) },
    });
  });

  api.patch(`${REPO}/pulls/:number`, async (c) => {
    const user = signedIn(c);
    const repo = c.get("repo");
    const pull = find(repo, c.req.param("number"));
    if (!mayChange(pull, user, c.get("level"))) throw fail(403, "only its author and those who can push here can change a pull request");
    const body = await c.req.json();
    if (body.title !== undefined || body.body !== undefined) {
      const title = body.title === undefined ? pull.title : text(body.title, 200);
      if (!title) throw fail(400, "give the pull request a title");
      db.run("UPDATE pulls SET title = ?, body = ? WHERE id = ?", [title, body.body === undefined ? pull.body : text(body.body, 20000), pull.id]);
    }
    if (body.state === "closed" && pull.state === "open") {
      const compared = await unlessGone(strata.compare(repo.id, pull.base, pull.head));
      finish(pull, "closed", user, compared && compared.ahead > 0 ? compared : null);
    } else if (body.state === "open" && pull.state !== "open") {
      if (pull.state === "merged") throw fail(409, "a merged pull request stays merged");
      await pair(repo, pull.base, pull.head);
      const existing = duplicate(repo, pull.base, pull.head);
      if (existing) throw fail(409, `#${existing} already proposes merging ${pull.head} into ${pull.base}`);
      const compared = await strata.compare(repo.id, pull.base, pull.head);
      if (compared.ahead === 0) throw fail(409, `${pull.head} has nothing that ${pull.base} lacks`);
      db.run("UPDATE pulls SET state = 'open', closed_at = NULL, closed_by = NULL, base_oid = ?, head_oid = ? WHERE id = ?", [...seen(compared), pull.id]);
    }
    return c.json({});
  });

  api.post(`${REPO}/pulls/:number/comments`, async (c) => {
    const user = signedIn(c);
    const pull = find(c.get("repo"), c.req.param("number"));
    const body = text((await c.req.json()).body, 20000);
    if (!body) throw fail(400, "write something first");
    db.run("INSERT INTO comments (pull_id, author_id, body, created_at) VALUES (?, ?, ?, ?)", [pull.id, user.id, body, now()]);
    return c.json({}, 201);
  });

  api.post(`${REPO}/pulls/:number/merge`, async (c) => {
    const user = signedIn(c);
    const repo = c.get("repo");
    const pull = find(repo, c.req.param("number"));
    if (!canWrite(c.get("level"))) throw fail(403, "only those who can push here can merge");
    if (pull.state !== "open") throw fail(409, `this pull request is ${pull.state}`);
    const body = await c.req.json();
    const method = body.method === "squash" || body.method === "rebase" ? (body.method as "squash" | "rebase") : "merge";
    const merged = await strata.merge(repo.id, {
      base: pull.base,
      head: pull.head,
      // What the page showed when the button was pressed, if it said.
      expect_head: body.head ? String(body.head) : undefined,
      method,
      // A rebase keeps each commit's own message.
      message: method === "squash" ? `${pull.title} (#${pull.number})\n\n${pull.body}` : `Merge pull request #${pull.number} from ${pull.head}\n\n${pull.title}`,
      author: { name: user.display_name || user.name, email: `${user.name}@users.noreply.hub` },
      actor: user.name,
    });
    finish(pull, "merged", user, merged, merged.oid);
    return c.json({ oid: merged.oid });
  });
}
