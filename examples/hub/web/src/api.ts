// The hub's HTTP API, as the UI sees it.
import { useCallback, useEffect, useState } from "react";

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

export async function api<T = unknown>(path: string, method = "GET", body?: unknown): Promise<T> {
  const response = await fetch(`/api${path}`, {
    method,
    headers: body === undefined ? {} : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new ApiError(response.status, data.error ?? `request failed (${response.status})`);
  return data as T;
}

export const query = (params: Record<string, string | number | undefined>) => {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) if (value !== undefined && value !== "") search.set(key, String(value));
  const text = search.toString();
  return text ? `?${text}` : "";
};

/** Loads something when `key` changes. `data` stays undefined until it arrives. */
export function useLoad<T>(load: () => Promise<T>, key: string) {
  const [state, setState] = useState<{ key: string; data?: T; error?: ApiError }>({ key });
  const [version, setVersion] = useState(0);
  useEffect(() => {
    let current = true;
    load().then(
      (data) => current && setState({ key, data }),
      (error) => current && setState({ key, error: error instanceof ApiError ? error : new ApiError(0, String(error)) }),
    );
    return () => {
      current = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, version]);
  const reload = useCallback(() => setVersion((n) => n + 1), []);
  // What was loaded for another key is not this key's data.
  const fresh = state.key === key ? state : { key };
  return { data: fresh.data as T | undefined, error: fresh.error as ApiError | undefined, reload };
}

export type User = { name: string; displayName: string };
export type Me = { user: User | null; orgs: { name: string; role: string }[] };
export type RepoSummary = { owner: string; name: string; description: string; private: boolean; createdAt: number; pushedAt: number | null };
export type Named = { name: string; oid: string };
export type RepoDetail = RepoSummary & {
  ownerKind: "user" | "org";
  protectDefault: boolean;
  permission: "read" | "write" | "admin";
  cloneUrl: string;
  defaultBranch: string;
  empty: boolean;
  branches: Named[];
  tags: Named[];
  size: number;
};
export type Person = { name: string; email: string; time: number; zone: string };
export type Commit = { oid: string; parents: string[]; tree: string; message: string; author?: Person; committer?: Person };
export type Side = { oid: string; mode: string } | null;
export type FileChange = { path: string; status: "added" | "deleted" | "modified"; old: Side; new: Side };
export type CommitDetail = Commit & { files: FileChange[]; truncated: boolean };
export type TreeEntry = { name: string; mode: string; type: "tree" | "blob" | "commit"; oid: string };
export type Tree = { commit: string; path: string; entries: TreeEntry[] };
export type Update = { ref: string; old: string | null; new: string | null; change: string };
export type Activity = { id: number; owner: string; repo: string; actor: string | null; time: number; updates: Update[] };
export type OwnerPage = {
  name: string;
  kind: "user" | "org";
  displayName: string;
  createdAt: number;
  repos: RepoSummary[];
  orgs?: string[];
  members?: { name: string; role: "owner" | "member" }[];
  role?: "owner" | "member" | null;
};
