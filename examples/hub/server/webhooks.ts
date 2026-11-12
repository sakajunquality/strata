// Webhooks: addresses that are told what happens to a repository. Each event
// is written down first and delivered after, so that a receiver that is away
// for a while still hears of it.
import { createHmac } from "node:crypto";
import { lookup } from "node:dns/promises";
import { isIP } from "node:net";
import type { Hono } from "hono";
import { fail, REPO, text, type Env } from "./common";
import { config } from "./config";
import { db, now, type Repo } from "./db";

export const EVENTS = ["push", "pull_request", "pull_request_review", "status"] as const;
type EventName = (typeof EVENTS)[number] | "ping";

type HookRow = { id: number; repo_id: string; url: string; secret: string; events: string; active: number; created_at: number };
type DeliveryRow = { id: number; webhook_id: number; event: string; payload: string; state: "pending" | "delivered" | "failed"; attempts: number; status: number | null; error: string | null; created_at: number; next_at: number; delivered_at: number | null };

/** Seconds to wait after each failed attempt; after the last one, the delivery has failed. */
const RETRY_AFTER = [10, 60, 300, 1800];
const TIMEOUT_MS = 10_000;

export const repoJson = (repo: Repo) => ({
  id: repo.id,
  owner: repo.owner_name,
  name: repo.name,
  full_name: `${repo.owner_name}/${repo.name}`,
  private: !!repo.private,
  url: `${config.publicUrl}/${repo.owner_name}/${repo.name}`,
  clone_url: `${config.strataPublicUrl}/${repo.owner_name}/${repo.name}.git`,
});

/** Notes an event for every webhook of the repository that asked for its kind. */
export function emit(repo: Repo, event: EventName, payload: Record<string, unknown>, only?: number) {
  const hooks = db.query<HookRow, [string]>("SELECT * FROM webhooks WHERE repo_id = ? AND active = 1").all(repo.id);
  const body = JSON.stringify(payload);
  for (const hook of hooks) {
    if (only !== undefined ? hook.id !== only : !(JSON.parse(hook.events) as string[]).includes(event)) continue;
    db.run("INSERT INTO deliveries (webhook_id, event, payload, created_at, next_at) VALUES (?, ?, ?, ?, ?)", [hook.id, event, body, now(), now()]);
  }
  if (hooks.length) void deliverDue();
}

const INSIDE = [/^127\./, /^10\./, /^192\.168\./, /^172\.(1[6-9]|2\d|3[01])\./, /^169\.254\./, /^0\./, /^100\.(6[4-9]|[7-9]\d|1[01]\d|12[0-7])\./, /^::1$/, /^::$/, /^f[cd]/i, /^fe80:/i, /^::ffff:/i];

/**
 * Why a webhook may not go to this address, if it may not: anyone who owns
 * a repository can set one, and the hub's own network is not theirs to
 * reach. Looked up at every delivery, since a name can change where it leads.
 */
export async function refusal(url: string): Promise<string | null> {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return "that is not an address";
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return "webhooks go to http or https addresses";
  if (config.webhooksInside) return null;
  const host = parsed.hostname.replace(/^\[|\]$/g, "");
  const addresses = isIP(host) ? [host] : await lookup(host, { all: true }).then((found) => found.map((entry) => entry.address), () => [] as string[]);
  if (addresses.length === 0) return `${host} cannot be found`;
  return addresses.some((address) => INSIDE.some((range) => range.test(address))) ? "webhooks cannot go to addresses inside the hub's own network" : null;
}

async function attempt(delivery: DeliveryRow, hook: HookRow): Promise<{ status: number | null; error: string | null }> {
  const refused = await refusal(hook.url);
  if (refused) return { status: null, error: refused };
  const headers: Record<string, string> = { "content-type": "application/json", "user-agent": "strata-hub", "x-hub-event": delivery.event, "x-hub-delivery": String(delivery.id) };
  if (hook.secret) headers["x-hub-signature-256"] = `sha256=${createHmac("sha256", hook.secret).update(delivery.payload).digest("hex")}`;
  try {
    // A redirect is not followed: it could lead where the address itself may not.
    const response = await fetch(hook.url, { method: "POST", headers, body: delivery.payload, redirect: "manual", signal: AbortSignal.timeout(TIMEOUT_MS) });
    await response.body?.cancel();
    return { status: response.status, error: response.status >= 200 && response.status < 300 ? null : `answered ${response.status}` };
  } catch (error) {
    return { status: null, error: error instanceof Error && error.name === "TimeoutError" ? "did not answer in time" : "could not be reached" };
  }
}

let delivering = false;

/** Sends what is due. One at a time, oldest first: a webhook hears of events in the order they happened. */
export async function deliverDue() {
  if (delivering) return;
  delivering = true;
  try {
    for (;;) {
      const delivery = db.query<DeliveryRow, [number]>("SELECT * FROM deliveries WHERE state = 'pending' AND next_at <= ? ORDER BY id LIMIT 1").get(now());
      if (!delivery) break;
      const hook = db.query<HookRow, [number]>("SELECT * FROM webhooks WHERE id = ?").get(delivery.webhook_id)!;
      const { status, error } = await attempt(delivery, hook);
      const attempts = delivery.attempts + 1;
      if (!error) db.run("UPDATE deliveries SET state = 'delivered', attempts = ?, status = ?, error = NULL, delivered_at = ? WHERE id = ?", [attempts, status, now(), delivery.id]);
      else if (attempts > RETRY_AFTER.length) db.run("UPDATE deliveries SET state = 'failed', attempts = ?, status = ?, error = ? WHERE id = ?", [attempts, status, error, delivery.id]);
      else db.run("UPDATE deliveries SET attempts = ?, status = ?, error = ?, next_at = ? WHERE id = ?", [attempts, status, error, now() + RETRY_AFTER[attempts - 1]!, delivery.id]);
    }
  } finally {
    delivering = false;
  }
}

/** Keeps trying what was not delivered, and forgets what is a month old. */
export function followDeliveries() {
  setInterval(() => void deliverDue().catch((error) => console.error("webhook deliveries:", error)), 5000);
  setInterval(() => db.run("DELETE FROM deliveries WHERE created_at < ?", [now() - 30 * 24 * 3600]), 3600_000);
}

export function addWebhookRoutes(api: Hono<Env>) {
  // The secret stays here: it is used, never shown.
  const hookJson = (hook: HookRow) => {
    const last = db.query<DeliveryRow, [number]>("SELECT * FROM deliveries WHERE webhook_id = ? ORDER BY id DESC LIMIT 1").get(hook.id);
    return { id: hook.id, url: hook.url, events: JSON.parse(hook.events), active: !!hook.active, hasSecret: !!hook.secret, createdAt: hook.created_at, last: last && { state: last.state, status: last.status, error: last.error, time: last.created_at } };
  };
  const admin = (level: string) => {
    if (level !== "admin") throw fail(403, "only the repository's owners can see and change its webhooks");
  };
  const find = (repo: Repo, raw: string) => {
    const hook = db.query<HookRow, [number, string]>("SELECT * FROM webhooks WHERE id = ? AND repo_id = ?").get(Number(raw), repo.id);
    if (!hook) throw fail(404, "no such webhook");
    return hook;
  };
  const events = (raw: unknown) => {
    const chosen = (Array.isArray(raw) ? raw : []).filter((event): event is string => EVENTS.includes(event));
    if (chosen.length === 0) throw fail(400, `choose at least one of ${EVENTS.join(", ")}`);
    return JSON.stringify(chosen);
  };

  api.get(`${REPO}/hooks`, (c) => {
    admin(c.get("level"));
    return c.json({ hooks: db.query<HookRow, [string]>("SELECT * FROM webhooks WHERE repo_id = ? ORDER BY id").all(c.get("repo").id).map(hookJson), events: EVENTS });
  });

  api.post(`${REPO}/hooks`, async (c) => {
    admin(c.get("level"));
    const repo = c.get("repo");
    const body = await c.req.json();
    const url = text(body.url, 500);
    const refused = await refusal(url);
    if (refused) throw fail(400, refused);
    if (db.query<{ n: number }, [string]>("SELECT COUNT(*) AS n FROM webhooks WHERE repo_id = ?").get(repo.id)!.n >= 20) throw fail(409, "a repository has at most 20 webhooks");
    const { lastInsertRowid } = db.run("INSERT INTO webhooks (repo_id, url, secret, events, created_at) VALUES (?, ?, ?, ?, ?)", [repo.id, url, text(body.secret, 200), events(body.events ?? ["push"]), now()]);
    emit(repo, "ping", { hook: Number(lastInsertRowid), repository: repoJson(repo) }, Number(lastInsertRowid));
    return c.json({ id: Number(lastInsertRowid) }, 201);
  });

  api.patch(`${REPO}/hooks/:id`, async (c) => {
    admin(c.get("level"));
    const hook = find(c.get("repo"), c.req.param("id"));
    const body = await c.req.json();
    const url = body.url === undefined ? hook.url : text(body.url, 500);
    const refused = body.url === undefined ? null : await refusal(url);
    if (refused) throw fail(400, refused);
    db.run("UPDATE webhooks SET url = ?, secret = ?, events = ?, active = ? WHERE id = ?", [
      url,
      body.secret === undefined ? hook.secret : text(body.secret, 200),
      body.events === undefined ? hook.events : events(body.events),
      body.active === undefined ? hook.active : body.active ? 1 : 0,
      hook.id,
    ]);
    return c.json({});
  });

  api.delete(`${REPO}/hooks/:id`, (c) => {
    admin(c.get("level"));
    db.run("DELETE FROM webhooks WHERE id = ?", [find(c.get("repo"), c.req.param("id")).id]);
    return c.json({});
  });

  api.post(`${REPO}/hooks/:id/ping`, (c) => {
    admin(c.get("level"));
    const repo = c.get("repo");
    const hook = find(repo, c.req.param("id"));
    emit(repo, "ping", { hook: hook.id, repository: repoJson(repo) }, hook.id);
    return c.json({}, 202);
  });

  api.get(`${REPO}/hooks/:id/deliveries`, (c) => {
    admin(c.get("level"));
    const hook = find(c.get("repo"), c.req.param("id"));
    const rows = db.query<DeliveryRow, [number]>("SELECT * FROM deliveries WHERE webhook_id = ? ORDER BY id DESC LIMIT 50").all(hook.id);
    return c.json({ deliveries: rows.map((row) => ({ id: row.id, event: row.event, state: row.state, attempts: row.attempts, status: row.status, error: row.error, createdAt: row.created_at, deliveredAt: row.delivered_at })) });
  });

  /** Sends a delivery again, as a new one with the same content. */
  api.post(`${REPO}/hooks/:id/deliveries/:delivery`, (c) => {
    admin(c.get("level"));
    const hook = find(c.get("repo"), c.req.param("id"));
    const delivery = db.query<DeliveryRow, [number, number]>("SELECT * FROM deliveries WHERE id = ? AND webhook_id = ?").get(Number(c.req.param("delivery")), hook.id);
    if (!delivery) throw fail(404, "no such delivery");
    db.run("INSERT INTO deliveries (webhook_id, event, payload, created_at, next_at) VALUES (?, ?, ?, ?, ?)", [hook.id, delivery.event, delivery.payload, now(), now()]);
    void deliverDue();
    return c.json({}, 202);
  });
}
