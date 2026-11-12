// Follows Strata's event feed: records pushes and imports as activity, keeps pull
// requests in step with their branches and gives a repository a default
// branch when its first branch arrives.
import { db, repoById } from "./db";
import { followPush } from "./pulls";
import { strata, type StrataEvent } from "./strata";
import { emit, repoJson } from "./webhooks";

const CURSOR = "events_cursor";

async function record(event: StrataEvent) {
  const repo = repoById(event.repo);
  // Events of repositories deleted since, or of someone else's, are not ours.
  if (!repo || !event.event) return;
  if (event.event.type === "import") {
    // Too many refs to list, and none are: every open pull request is looked at again.
    db.run("INSERT INTO activity (repo_id, actor, time, updates, imported) VALUES (?, ?, ?, '[]', ?)", [repo.id, event.event.actor ?? null, event.time, event.event.refs ?? 0]);
    db.run("UPDATE repos SET pushed_at = ? WHERE id = ?", [event.time, repo.id]);
    await followPush(repo, null);
    return;
  }
  if (event.event.type !== "push") return;
  const updates = event.event.updates ?? [];
  db.run("INSERT INTO activity (repo_id, actor, time, updates) VALUES (?, ?, ?, ?)", [repo.id, event.event.actor ?? null, event.time, JSON.stringify(updates)]);
  db.run("UPDATE repos SET pushed_at = ? WHERE id = ?", [event.time, repo.id]);
  for (const update of updates) {
    emit(repo, "push", { ref: update.ref, before: update.old, after: update.new, change: update.change, created: !update.old, deleted: !update.new, forced: update.change === "forced", repository: repoJson(repo), sender: event.event.actor ?? null });
  }
  await followPush(repo, updates, event.event.actor ?? null);

  // HEAD names "main" until told otherwise. If that branch is not there but
  // another is (a first push of "master", say), make the other the default.
  const { head, refs } = await strata.refs(repo.id);
  if (refs.some((ref) => ref.name === head)) return;
  const pushed = updates.find((update) => update.new && update.ref.startsWith("refs/heads/"))?.ref;
  const branch = pushed ?? refs.find((ref) => ref.name.startsWith("refs/heads/"))?.name;
  if (branch) await strata.setHead(repo.id, branch);
}

/** Reads what is new in the feed. Returns how many events it handled. */
export async function pollOnce(): Promise<number> {
  const cursor = db.query<{ value: string }, [string]>("SELECT value FROM state WHERE key = ?").get(CURSOR)?.value ?? null;
  const { events, next } = await strata.events(cursor);
  for (const event of events) {
    try {
      await record(event);
    } catch (error) {
      console.error(`event ${event.cursor} of ${event.repo}:`, error);
    }
  }
  if (next && next !== cursor) db.run("INSERT INTO state (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value", [CURSOR, next]);
  return events.length;
}

export function followEvents(intervalMs = 2000) {
  let failing = false;
  const tick = async () => {
    try {
      // Catch up without waiting while there is a backlog.
      while ((await pollOnce()) > 0);
      failing = false;
    } catch (error) {
      if (!failing) console.error("cannot read Strata's event feed:", error instanceof Error ? error.message : error);
      failing = true;
    }
    setTimeout(tick, intervalMs);
  };
  void tick();
}
