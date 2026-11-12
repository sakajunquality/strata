// Accounts, browser sessions and the credentials git clients present.
import type { Context } from "hono";
import { deleteCookie, getCookie, setCookie } from "hono/cookie";
import { config } from "./config";
import { db, now, ownerByName, type Owner } from "./db";

const COOKIE = "hub_session";
const SESSION_SECONDS = 30 * 24 * 3600;
const TOKEN_PREFIX = "hub_";

export const sha256 = (text: string) => new Bun.CryptoHasher("sha256").update(text).digest("hex");
const random = (bytes: number) => Buffer.from(crypto.getRandomValues(new Uint8Array(bytes))).toString("base64url");

export function startSession(c: Context, userId: number) {
  const secret = random(32);
  db.run("INSERT INTO sessions (hash, user_id, expires_at) VALUES (?, ?, ?)", [sha256(secret), userId, now() + SESSION_SECONDS]);
  setCookie(c, COOKIE, secret, { httpOnly: true, sameSite: "Lax", path: "/", maxAge: SESSION_SECONDS, secure: config.secureCookies });
}

export function endSession(c: Context) {
  const secret = getCookie(c, COOKIE);
  if (secret) db.run("DELETE FROM sessions WHERE hash = ?", [sha256(secret)]);
  deleteCookie(c, COOKIE, { path: "/" });
}

/** The signed-in user behind a browser request. */
export function sessionUser(c: Context): Owner | null {
  const secret = getCookie(c, COOKIE);
  if (!secret) return null;
  return db
    .query<Owner, [string, number]>("SELECT owners.* FROM sessions JOIN owners ON owners.id = sessions.user_id WHERE sessions.hash = ? AND sessions.expires_at > ?")
    .get(sha256(secret), now());
}

export function createToken(userId: number, name: string): string {
  const token = TOKEN_PREFIX + random(30);
  db.run("INSERT INTO tokens (user_id, name, hash, created_at) VALUES (?, ?, ?, ?)", [userId, name, sha256(token), now()]);
  return token;
}

// Spent on unknown user names too, so that the answer takes as long either way.
const dummyHash = await Bun.password.hash(random(16));

export async function checkPassword(username: string, password: string): Promise<Owner | null> {
  const user = ownerByName(username);
  if (!user || user.kind !== "user" || !user.password_hash) {
    await Bun.password.verify(password, dummyHash);
    return null;
  }
  return (await Bun.password.verify(password, user.password_hash)) ? user : null;
}

const tokenUser = (token: string): Owner | null => {
  const hash = sha256(token);
  const user = db.query<Owner, [string]>("SELECT owners.* FROM tokens JOIN owners ON owners.id = tokens.user_id WHERE tokens.hash = ?").get(hash);
  if (user) db.run("UPDATE tokens SET last_used_at = ? WHERE hash = ?", [now(), hash]);
  return user;
};

/** Who a program calling the API is: it sends an access token as `Authorization: Bearer <token>`. */
export function apiUser(authorization: string | null | undefined): Owner | null {
  const token = authorization?.startsWith("Bearer ") ? authorization.slice(7).trim() : "";
  return token.startsWith(TOKEN_PREFIX) ? tokenUser(token) : null;
}

/** Whose SSH key this is: the one a git client proved to hold, as Strata passes it on (`ssh-ed25519 AAAA…`). */
export function sshUser(key: string): Owner | null {
  const material = key.trim().split(/\s+/)[1] ?? "";
  if (!material) return null;
  return db.query<Owner, [string]>("SELECT owners.* FROM signing_keys JOIN owners ON owners.id = signing_keys.user_id WHERE signing_keys.kind = 'ssh' AND signing_keys.material = ?").get(material);
}

/**
 * Who a git client is, from its `Authorization` header: Basic credentials
 * with an access token (under any user name) or the account's password.
 */
export async function gitUser(authorization: string | null | undefined): Promise<Owner | null> {
  if (!authorization?.startsWith("Basic ")) return null;
  let decoded: string;
  try {
    decoded = atob(authorization.slice(6).trim());
  } catch {
    return null;
  }
  const colon = decoded.indexOf(":");
  const username = colon < 0 ? decoded : decoded.slice(0, colon);
  const password = colon < 0 ? "" : decoded.slice(colon + 1);
  // Some tools put the token in the user name and leave the password empty.
  const secret = password || username;
  if (secret.startsWith(TOKEN_PREFIX)) return tokenUser(secret);
  return password && config.passwords ? checkPassword(username, password) : null;
}
