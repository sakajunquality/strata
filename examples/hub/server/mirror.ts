// Mirrors: repositories that follow one on GitHub.
//
// Strata does not fetch from other servers, so the hub does it with git: it
// keeps a bare copy of each source, fetches what is new into it, and hands
// Strata a bundle of the difference. One mirror is brought up to date at a
// time.
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { config } from "./config";
import { db, mirrorOf, now, type Mirror } from "./db";
import { strata } from "./strata";

const NAME = /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})\/[A-Za-z0-9._-]{1,100}$/;

/** "owner/name" from that, or from the repository's address on GitHub. Null if it is neither. */
export function sourceName(raw: string): string | null {
  let name = raw.trim();
  if (name.toLowerCase().startsWith(`${config.githubUrl.toLowerCase()}/`)) name = name.slice(config.githubUrl.length + 1);
  name = name.replace(/\/+$/, "").replace(/\.git$/, "");
  return NAME.test(name) && !name.split("/").some((part) => part === "." || part === "..") ? name : null;
}

export const sourceUrl = (source: string) => `${config.githubUrl}/${source}`;

const dirOf = (repoId: string) => resolve(config.mirrorDir, `${repoId}.git`);
/** What Strata was last given, as `<oid> <ref>` lines: the next bundle holds what has changed since. */
const givenFile = (repoId: string) => join(dirOf(repoId), "strata-given");

class GitError extends Error {}

async function git(dir: string, args: string[], options: { token?: string | null; stdin?: string; stdout?: Bun.BunFile } = {}): Promise<string> {
  const env: Record<string, string> = {
    PATH: process.env.PATH ?? "",
    // Nothing of whoever runs the hub: no credential helpers, no rewritten URLs, no prompts.
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_TERMINAL_PROMPT: "0",
  };
  if (options.token) {
    // In the environment, not on the command line, where other users of the machine could read it.
    env.GIT_CONFIG_COUNT = "1";
    env.GIT_CONFIG_KEY_0 = "http.extraHeader";
    env.GIT_CONFIG_VALUE_0 = `Authorization: Basic ${btoa(`x-access-token:${options.token}`)}`;
  }
  const child = Bun.spawn(["git", ...args], { cwd: dir, env, stdin: options.stdin === undefined ? "ignore" : new Blob([options.stdin]), stdout: options.stdout ?? "pipe", stderr: "pipe" });
  const [out, err, code] = await Promise.all([options.stdout ? "" : new Response(child.stdout as ReadableStream).text(), new Response(child.stderr).text(), child.exited]);
  if (code !== 0) throw new GitError(err.trim() || `git ${args[0]} failed`);
  return out;
}

/** Why a fetch failed, for someone who only knows the name they typed. */
function explain(error: unknown, mirror: Mirror): string {
  const message = error instanceof Error ? error.message : String(error);
  if (!(error instanceof GitError)) return message;
  if (/could not read Username|Authentication failed|terminal prompts disabled|not found|403/i.test(message)) {
    return mirror.token ? `${mirror.source} is not there, or the access token may not read it` : `${mirror.source} is not there, or is private: a private repository needs an access token`;
  }
  // The last line is the one that says what went wrong; never the header that carries the token.
  return (message.split("\n").filter((line) => !line.includes("Authorization")).pop() ?? "the fetch failed").slice(0, 300);
}

const lines = (text: string) => text.split("\n").filter(Boolean);

/** Fetches what is new at the source and gives it to Strata. Returns how many refs changed. */
async function sync(mirror: Mirror): Promise<number> {
  const dir = dirOf(mirror.repo_id);
  const url = `${sourceUrl(mirror.source)}.git`;
  if (!existsSync(dir)) {
    mkdirSync(dir, { recursive: true });
    await git(dir, ["init", "-q", "--bare", "."]);
  }
  const prefixes = ["refs/heads/", "refs/tags/", ...(mirror.pulls ? ["refs/pull/"] : [])];
  const refspecs = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*", ...(mirror.pulls ? ["+refs/pull/*/head:refs/pull/*/head"] : [])];
  // Objects that refs have left stay until Strata has been told, so that the difference can still be worked out.
  await git(dir, ["-c", "gc.auto=0", "-c", "protocol.version=2", "fetch", "-q", "--prune", "--no-tags", url, ...refspecs], { token: mirror.token });

  const refs = await git(dir, ["for-each-ref", "--format=%(objectname) %(refname)"]);
  const given = existsSync(givenFile(mirror.repo_id)) ? readFileSync(givenFile(mirror.repo_id), "utf8") : null;
  if (refs === given) return 0;
  if (!refs.trim()) throw new GitError(`${mirror.source} has no branches yet`);

  // The bundle: the refs as they are now, then a pack of what they reach
  // that the refs given last time did not.
  const had = [...new Set(lines(given ?? "").map((line) => line.split(" ")[0]!))];
  // Some may be gone from the copy here; Strata still has them, and the pack is checked there.
  const present = had.length ? lines(await git(dir, ["cat-file", "--batch-check=%(objectname) %(objecttype)"], { stdin: had.join("\n") + "\n" })).filter((line) => !line.endsWith(" missing")).map((line) => line.split(" ")[0]!) : [];
  const tips = [...new Set(lines(refs).map((line) => line.split(" ")[0]!))];
  const bundle = join(dir, "strata.bundle");
  const pack = join(dir, "strata.pack");
  try {
    await git(dir, ["pack-objects", "--revs", "--stdout", "-q", "--delta-base-offset"], { stdin: [...tips, ...present.map((oid) => `^${oid}`)].join("\n") + "\n", stdout: Bun.file(pack) });
    const out = Bun.file(bundle).writer();
    out.write(`# v2 git bundle\n${refs}\n`);
    for await (const chunk of Bun.file(pack).stream()) out.write(chunk);
    await out.end();
    const { updated } = await strata.import(mirror.repo_id, Bun.file(bundle), "mirror", prefixes);
    if (given === null) {
      // A first time, Strata is also told which branch is the default one there: a bundle cannot say.
      const symref = lines(await git(dir, ["ls-remote", "--symref", url, "HEAD"], { token: mirror.token }));
      const branch = symref.find((line) => line.startsWith("ref: "))?.split(/\s+/)[1];
      if (branch && lines(refs).some((line) => line.endsWith(` ${branch}`))) await strata.setHead(mirror.repo_id, branch);
    }
    writeFileSync(givenFile(mirror.repo_id), refs);
    // Only now may the copy here drop what its refs have left.
    await git(dir, ["gc", "--auto", "-q"]).catch(() => {});
    return updated;
  } finally {
    rmSync(bundle, { force: true });
    rmSync(pack, { force: true });
  }
}

const waiting: string[] = [];
let busy = false;

async function work() {
  if (busy) return;
  busy = true;
  for (let id = waiting.shift(); id; id = waiting.shift()) {
    const mirror = mirrorOf(id);
    if (!mirror) continue;
    db.run("UPDATE mirrors SET state = 'syncing' WHERE repo_id = ?", [id]);
    try {
      await sync(mirror);
      db.run("UPDATE mirrors SET state = 'synced', error = NULL, checked_at = ?, synced_at = ? WHERE repo_id = ?", [now(), now(), id]);
    } catch (error) {
      const reason = explain(error, mirror);
      console.error(`mirror of ${mirror.source} (${id}): ${reason}`);
      db.run("UPDATE mirrors SET state = 'failed', error = ?, checked_at = ? WHERE repo_id = ?", [reason, now(), id]);
    }
  }
  busy = false;
}

/** Asks for a mirror to be brought up to date, soon. */
export function requestSync(repoId: string) {
  if (!waiting.includes(repoId)) waiting.push(repoId);
  db.run("UPDATE mirrors SET state = 'waiting' WHERE repo_id = ? AND state <> 'syncing'", [repoId]);
  void work();
}

/** The hub's copy of a source, once the mirror is gone. */
export function forget(repoId: string) {
  rmSync(dirOf(repoId), { recursive: true, force: true });
}

/** Keeps mirrors up to date: each is fetched again when its last fetch is `HUB_MIRROR_MINUTES` old. */
export function followSources() {
  // Whatever was in flight when the hub last stopped starts over.
  for (const { repo_id } of db.query<{ repo_id: string }, []>("SELECT repo_id FROM mirrors WHERE state IN ('waiting', 'syncing')").all()) requestSync(repo_id);
  if (config.mirrorMinutes <= 0) return;
  setInterval(() => {
    const due = db.query<{ repo_id: string }, [number]>("SELECT repo_id FROM mirrors WHERE state IN ('synced', 'failed') AND COALESCE(checked_at, 0) < ? ORDER BY checked_at").all(now() - config.mirrorMinutes * 60);
    for (const { repo_id } of due) requestSync(repo_id);
  }, 30_000);
}
