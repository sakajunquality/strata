// The hub's side of Strata's management and content API.
import { config } from "./config";

export class StrataError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

async function call(method: string, path: string, body?: string | Blob): Promise<Response> {
  let response: Response;
  try {
    response = await fetch(`${config.strataUrl}${path}`, {
      method,
      body,
      headers: config.adminToken ? { authorization: `Bearer ${config.adminToken}` } : {},
    });
  } catch {
    throw new StrataError(502, "the git backend cannot be reached");
  }
  if (!response.ok) {
    const message = (await response.text()).trim();
    // Only "not there" and "bad request" are the caller's doing.
    throw new StrataError([400, 404, 409, 422].includes(response.status) ? response.status : 502, message || "the git backend failed");
  }
  return response;
}

const query = (params: Record<string, string | number | undefined>) => {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) if (value !== undefined && value !== "") search.set(key, String(value));
  const text = search.toString();
  return text ? `?${text}` : "";
};

export type Ref = { name: string; oid: string };
export type Refs = { head: string; refs: Ref[] };
export type StrataEvent = {
  cursor: string;
  repo: string;
  time: number;
  /** A push lists its updates; an import only says how many refs it set. */
  event: { type?: string; actor?: string | null; refs?: number; updates?: { ref: string; old: string | null; new: string | null; change: string }[] } | null;
};

export type Compare = { base: string; head: string; merge_base: string | null; ahead: number; behind: number; commits: StrataCommit[]; files: unknown[]; truncated: boolean };
/** A commit as Strata describes it. Only what the hub itself looks at is spelled out. */
export type StrataCommit = { oid: string; signature?: { format: string; signature: string; payload: string } | null; [more: string]: unknown };
export type MergeRequest = {
  base: string;
  head: string;
  expect_head?: string;
  method?: "merge" | "squash" | "rebase";
  message?: string;
  author?: { name: string; email: string };
  actor?: string;
};
type Merged = { base: string; head: string; merge_base: string | null; ahead: number };

const repo = (id: string) => `/api/repos/${id}`;

export const strata = {
  create: (id: string) => call("PUT", repo(id)),
  remove: (id: string) => call("DELETE", repo(id)),
  setHead: (id: string, ref: string) => call("PUT", `${repo(id)}/head`, ref),
  info: async (id: string) => (await call("GET", repo(id))).json() as Promise<{ packs: number; objects: number; bytes: number }>,
  /** All refs, or those whose names start with `prefix`. */
  refs: async (id: string, prefix?: string) => (await call("GET", `${repo(id)}/refs${query({ prefix })}`)).json() as Promise<Refs>,
  /** History, newest first. With a path: only the commits that changed what is there. */
  commits: async (id: string, rev: string, skip: number, limit: number, path?: string) =>
    (await call("GET", `${repo(id)}/commits${query({ rev, skip, limit, path })}`)).json() as Promise<{ commits: StrataCommit[]; next: number | null; truncated: boolean }>,
  blame: async (id: string, rev: string, path: string) => (await call("GET", `${repo(id)}/blame${query({ rev, path })}`)).json(),
  search: async (id: string, params: Record<string, string | undefined>) => (await call("GET", `${repo(id)}/search${query(params)}`)).json(),
  /** A large file (Git LFS), by the SHA-256 of its content. */
  lfs: (id: string, oid: string) => call("GET", `${repo(id)}/lfs/${oid}`),
  commit: async (id: string, rev: string) => (await call("GET", `${repo(id)}/commit${query({ rev })}`)).json() as Promise<StrataCommit>,
  tree: async (id: string, rev: string, path: string) => (await call("GET", `${repo(id)}/tree${query({ rev, path })}`)).json(),
  blob: (id: string, params: { rev?: string; path?: string; oid?: string }) => call("GET", `${repo(id)}/blob${query(params)}`),
  compare: async (id: string, base: string, head: string) => (await call("GET", `${repo(id)}/compare${query({ base, head })}`)).json() as Promise<Compare>,
  /** Whether a merge would work, without doing it. */
  mergeable: async (id: string, base: string, head: string) =>
    (await call("POST", `${repo(id)}/merge`, JSON.stringify({ base, head, dry_run: true }))).json() as Promise<Merged & { mergeable: boolean; conflicts: string[] }>,
  merge: async (id: string, request: MergeRequest) => (await call("POST", `${repo(id)}/merge`, JSON.stringify(request))).json() as Promise<Merged & { oid: string }>,
  /** Takes in a bundle: sets every ref in it, and deletes those under `prune` that it does not name. */
  import: async (id: string, bundle: Blob, actor: string, prune: string[]) =>
    (await call("POST", `${repo(id)}/import?${new URLSearchParams([["actor", actor], ...prune.map((prefix) => ["prune", prefix])])}`, bundle)).json() as Promise<{ refs: number; updated: number; objects: number }>,
  events: async (after: string | null) =>
    (await call("GET", `/api/events${query({ after: after ?? undefined, limit: 200 })}`)).json() as Promise<{ events: StrataEvent[]; next: string | null }>,
};
