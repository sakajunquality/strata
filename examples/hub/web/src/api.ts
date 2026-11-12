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
export type Me = { user: User | null; orgs: { name: string; role: string }[]; signIn: { passwords: boolean; providers: { id: string; name: string }[] } };
export type RepoSummary = { owner: string; name: string; description: string; private: boolean; createdAt: number; pushedAt: number | null };
export type Named = { name: string; oid: string; /** For a branch: whether a rule protects it. */ protected?: boolean };
export type Rule = { id: number; pattern: string; noForce: boolean; noDelete: boolean; requirePulls: boolean; approvals: number; requireSigned: boolean; checks: string[] };
export type Webhook = { id: number; url: string; events: string[]; active: boolean; hasSecret: boolean; createdAt: number; last: { state: "pending" | "delivered" | "failed"; status: number | null; error: string | null; time: number } | null };
export type Status = { context: string; state: "pending" | "success" | "failure" | "error"; description: string; targetUrl: string; createdAt: number; creator: string | null };
export type TeamSummary = { name: string; description: string; members: number; repos: number; ssoProvider: string | null; ssoGroup: string | null };
export type TeamDetail = {
  org: string;
  name: string;
  description: string;
  ssoProvider: string | null;
  ssoGroup: string | null;
  members: string[];
  repos: { name: string; level: string }[];
  orgRepos: string[];
  providers: { id: string; name: string }[];
  canChange: boolean;
};
export type RepoDetail = RepoSummary & {
  ownerKind: "user" | "org";
  openPulls: number;
  permission: "read" | "write" | "admin";
  cloneUrl: string;
  /** Set if git is served over SSH too. */
  sshUrl: string | null;
  /** An organization's repository, for those who can change its settings: who reaches it. */
  access: { base: "none" | "read" | "write"; teams: { name: string; level: string }[] } | null;
  defaultBranch: string;
  empty: boolean;
  branches: Named[];
  tags: Named[];
  size: number;
  /** Set if the repository follows one on GitHub. */
  mirror: { source: string; url: string; pulls: boolean; state: "waiting" | "syncing" | "synced" | "failed"; error: string | null; syncedAt: number | null } | null;
};
export type Person = { name: string; email: string; time: number; zone: string };
/** What a commit's signature is worth: null if it has none. */
export type Verification = { state: "verified"; signer: string; key: string } | { state: "unverified"; reason: string } | null;
export type Commit = { oid: string; parents: string[]; tree: string; message: string; author?: Person; committer?: Person; verification?: Verification };
export type Side = { oid: string; mode: string } | null;
export type FileChange = { path: string; status: "added" | "deleted" | "modified"; old: Side; new: Side };
export type CommitDetail = Commit & { files: FileChange[]; truncated: boolean };
export type Compare = { base: string; head: string; merge_base: string | null; ahead: number; behind: number; commits: Commit[]; files: FileChange[]; truncated: boolean };
export type Pull = {
  number: number;
  title: string;
  body: string;
  author: string;
  base: string;
  head: string;
  state: "open" | "closed" | "merged";
  createdAt: number;
  closedAt: number | null;
  closedBy: string | null;
  mergeOid: string | null;
  comments: number;
};
export type PullDetail = Pull & {
  discussion: { id: number; body: string; createdAt: number; author: string }[];
  /** What it holds; null if its branches or commits are gone. */
  compare: Compare | null;
  /** Null when there is nothing to ask: it is not open, or has nothing to merge. */
  mergeable: boolean | null;
  conflicts: string[];
  reviews: { id: number; author: string; state: "approved" | "changes_requested" | "commented"; body: string; createdAt: number; stale: boolean }[];
  /** Each reviewer's last verdict. */
  reviewers: { author: string; state: "approved" | "changes_requested"; stale: boolean; counts: boolean }[];
  /** What was reported about its latest commit. */
  checks: Status[];
  /** What the base branch asks of it. */
  rule: { pattern: string; approvals: number; checks: string[] } | null;
  /** Why it may not merge yet. */
  blocked: string[];
  can: { change: boolean; merge: boolean; review: boolean; judge: boolean };
};
export type TreeEntry = { name: string; mode: string; type: "tree" | "blob" | "commit"; oid: string };
export type Tree = { commit: string; path: string; entries: TreeEntry[] };
export type Update = { ref: string; old: string | null; new: string | null; change: string };
export type Activity = { id: number; owner: string; repo: string; actor: string | null; time: number; updates: Update[]; imported: number | null };
export type OwnerPage = {
  name: string;
  kind: "user" | "org";
  displayName: string;
  createdAt: number;
  repos: RepoSummary[];
  orgs?: string[];
  members?: { name: string; role: "owner" | "member" }[];
  role?: "owner" | "member" | null;
  basePermission?: "none" | "read" | "write";
};
