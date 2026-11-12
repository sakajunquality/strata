// Signing in elsewhere: at an OpenID Connect provider (Microsoft Entra ID,
// Google Workspace, or any other) or at GitHub. The hub sends the browser
// there and learns who came back; it never sees a password.
import type { Context, Hono } from "hono";
import { deleteCookie, getCookie, setCookie } from "hono/cookie";
import { createRemoteJWKSet, jwtVerify, type JWTPayload } from "jose";
import { startSession } from "./auth";
import type { Env } from "./common";
import { config, signInAt, type Provider } from "./config";
import { db, now, ownerByName } from "./db";

const COOKIE = "hub_signin";
const random = () => Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString("base64url");
const s256 = (text: string) => new Bun.CryptoHasher("sha256").update(text).digest("base64url");

/** Someone as a provider knows them. */
type Arrival = { subject: string; username: string; displayName: string };

class Refused extends Error {}

type Discovered = { issuer: string; authorization_endpoint: string; token_endpoint: string; jwks_uri: string; keys: ReturnType<typeof createRemoteJWKSet> };
const discovered = new Map<string, Promise<Discovered>>();

/** What an OpenID Connect provider says about itself. Asked once, and again after a failure. */
function discover(provider: Provider): Promise<Discovered> {
  let known = discovered.get(provider.id);
  if (!known) {
    known = (async () => {
      const response = await fetch(`${provider.issuer}/.well-known/openid-configuration`);
      if (!response.ok) throw new Error(`${provider.issuer} does not describe itself (${response.status})`);
      const document = (await response.json()) as Omit<Discovered, "keys">;
      return { ...document, keys: createRemoteJWKSet(new URL(document.jwks_uri)) };
    })();
    discovered.set(provider.id, known);
    known.catch(() => discovered.delete(provider.id));
  }
  return known;
}

const redirectUri = (provider: Provider) => `${config.publicUrl}/api/auth/${provider.id}/callback`;

async function authorizeUrl(provider: Provider, state: string, nonce: string, verifier: string): Promise<string> {
  const params = new URLSearchParams({ client_id: provider.clientId, redirect_uri: redirectUri(provider), state, code_challenge: s256(verifier), code_challenge_method: "S256" });
  if (provider.kind === "github") {
    params.set("scope", "read:user");
    return `${config.githubUrl}/login/oauth/authorize?${params}`;
  }
  params.set("response_type", "code");
  params.set("scope", "openid profile email");
  params.set("nonce", nonce);
  // Google then offers only accounts of that domain. A hint: the answer is checked all the same.
  if (provider.domains?.length === 1) params.set("hd", provider.domains[0]!);
  return `${(await discover(provider)).authorization_endpoint}?${params}`;
}

async function exchange(url: string, provider: Provider, code: string, verifier: string): Promise<Record<string, unknown>> {
  const response = await fetch(url, {
    method: "POST",
    headers: { accept: "application/json", "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({ grant_type: "authorization_code", code, redirect_uri: redirectUri(provider), client_id: provider.clientId, client_secret: provider.clientSecret, code_verifier: verifier }),
  });
  const answer = (await response.json().catch(() => ({}))) as Record<string, unknown>;
  // GitHub reports a refused code with status 200.
  if (!response.ok || answer.error) throw new Refused(`${provider.name} did not accept the sign-in`);
  return answer;
}

async function arrive(provider: Provider, code: string, nonce: string, verifier: string): Promise<Arrival> {
  if (provider.kind === "github") {
    const { access_token } = await exchange(`${config.githubUrl}/login/oauth/access_token`, provider, code, verifier);
    const response = await fetch(`${config.githubApiUrl}/user`, { headers: { authorization: `Bearer ${access_token}`, accept: "application/vnd.github+json", "user-agent": "strata-hub" } });
    if (!response.ok) throw new Refused("GitHub did not say who signed in");
    const user = (await response.json()) as { id: number; login: string; name?: string | null };
    return { subject: String(user.id), username: user.login, displayName: user.name ?? "" };
  }

  const known = await discover(provider);
  const { id_token } = await exchange(known.token_endpoint, provider, code, verifier);
  let claims: JWTPayload;
  try {
    // Signed by the provider, for this application, and not expired.
    ({ payload: claims } = await jwtVerify(String(id_token ?? ""), known.keys, { issuer: known.issuer, audience: provider.clientId }));
  } catch {
    throw new Refused(`${provider.name} sent a token that does not check out`);
  }
  // And in answer to the request this browser made.
  if (claims.nonce !== nonce || !claims.sub) throw new Refused(`${provider.name} sent a token that does not check out`);
  const email = typeof claims.email === "string" ? claims.email : "";
  if (provider.domains?.length) {
    // The Workspace domain an account belongs to; personal accounts have none.
    if (typeof claims.hd !== "string" || !provider.domains.includes(claims.hd.toLowerCase())) throw new Refused(`only accounts of ${provider.domains.join(", ")} can sign in`);
  }
  const preferred = typeof claims.preferred_username === "string" ? claims.preferred_username : "";
  return { subject: claims.sub, username: (preferred || email).split("@")[0] || String(claims.name ?? ""), displayName: typeof claims.name === "string" ? claims.name : "" };
}

/** A free account name as close to `wanted` as the rules for names allow. */
function freeName(wanted: string, taken: (name: string) => boolean): string {
  const base = wanted.normalize("NFKD").replace(/[^a-zA-Z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 30).replace(/-+$/, "") || "user";
  for (let n = 1; ; n++) {
    const name = n === 1 ? base : `${base}-${n}`;
    if (!taken(name)) return name;
  }
}

/** The account that belongs to an arrival: the one it was given before, or a new one. */
function accountFor(provider: Provider, arrival: Arrival, reserved: Set<string>): number {
  const known = db.query<{ user_id: number }, [string, string]>("SELECT user_id FROM identities WHERE provider = ? AND subject = ?").get(provider.id, arrival.subject);
  if (known) return known.user_id;
  // Never matched to an existing account by name or email address: that
  // would hand the account to whoever can take that name at the provider.
  return db.transaction(() => {
    const name = freeName(arrival.username, (name) => reserved.has(name.toLowerCase()) || !!ownerByName(name));
    const { lastInsertRowid } = db.run("INSERT INTO owners (name, kind, display_name, created_at) VALUES (?, 'user', ?, ?)", [name, arrival.displayName.slice(0, 100), now()]);
    db.run("INSERT INTO identities (provider, subject, user_id, created_at) VALUES (?, ?, ?, ?)", [provider.id, arrival.subject, Number(lastInsertRowid), now()]);
    return Number(lastInsertRowid);
  })();
}

export function addSignInRoutes(api: Hono<Env>, reserved: Set<string>) {
  const cookiePath = "/api/auth";
  const back = (c: Context, error: string) => c.redirect(`${config.publicUrl}/login?${new URLSearchParams({ error })}`);

  api.get("/auth/:provider", async (c) => {
    const provider = signInAt.find((provider) => provider.id === c.req.param("provider"));
    if (!provider) return c.json({ error: "there is no such way to sign in" }, 404);
    const [state, nonce, verifier] = [random(), random(), random()];
    let url: string;
    try {
      url = await authorizeUrl(provider, state, nonce, verifier);
    } catch (error) {
      console.error(`sign-in with ${provider.name}:`, error);
      return back(c, `${provider.name} cannot be reached`);
    }
    // What the answer has to match, kept by the browser that asked.
    setCookie(c, COOKIE, JSON.stringify({ provider: provider.id, state, nonce, verifier }), { httpOnly: true, sameSite: "Lax", path: cookiePath, maxAge: 600, secure: config.secureCookies });
    return c.redirect(url);
  });

  api.get("/auth/:provider/callback", async (c) => {
    const provider = signInAt.find((provider) => provider.id === c.req.param("provider"));
    if (!provider) return c.json({ error: "there is no such way to sign in" }, 404);
    let asked: { provider?: string; state?: string; nonce?: string; verifier?: string } = {};
    try {
      asked = JSON.parse(getCookie(c, COOKIE) ?? "{}");
    } catch {}
    deleteCookie(c, COOKIE, { path: cookiePath });
    const { code, state, error } = c.req.query();
    if (error) return back(c, `${provider.name} did not sign you in`);
    // An answer to a question this browser did not ask is someone else's sign-in.
    if (!code || !state || !asked.state || asked.provider !== provider.id || state !== asked.state) return back(c, "the sign-in was not started here, or took too long: try again");
    try {
      const arrival = await arrive(provider, code, asked.nonce ?? "", asked.verifier ?? "");
      startSession(c, accountFor(provider, arrival, reserved));
    } catch (error) {
      if (error instanceof Refused) return back(c, error.message);
      console.error(`sign-in with ${provider.name}:`, error);
      return back(c, `signing in with ${provider.name} failed`);
    }
    return c.redirect(`${config.publicUrl}/`);
  });
}
