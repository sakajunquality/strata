// What the parts of the hub's API share.
import type { Context } from "hono";
import { HTTPException } from "hono/http-exception";
import type { Owner, Permission, Repo } from "./db";

export type Env = { Variables: { user: Owner | null; repo: Repo; level: Permission } };

/** Routes below this have `repo` and `level` set, and are only reached by those who may read it. */
export const REPO = "/repos/:owner/:name";

export const fail = (status: 400 | 401 | 403 | 404 | 409, message: string) => new HTTPException(status, { message });

export const text = (raw: unknown, max: number) => String(raw ?? "").trim().slice(0, max);

export function signedIn(c: Context<Env>): Owner {
  const user = c.get("user");
  if (!user) throw fail(401, "sign in to continue");
  return user;
}
