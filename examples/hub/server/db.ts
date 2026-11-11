// The hub's own data: who exists, what they own and what they may do.
// Git data is not here; repositories are known to Strata by `repos.id` only.
import { Database } from "bun:sqlite";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { config } from "./config";

if (config.database !== ":memory:") mkdirSync(dirname(config.database), { recursive: true });
export const db = new Database(config.database, { strict: true });
db.exec("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;");
db.exec(`
CREATE TABLE IF NOT EXISTS owners (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE COLLATE NOCASE,
  kind TEXT NOT NULL CHECK (kind IN ('user', 'org')),
  display_name TEXT NOT NULL DEFAULT '',
  password_hash TEXT,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS members (
  org_id INTEGER NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
  user_id INTEGER NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
  role TEXT NOT NULL CHECK (role IN ('owner', 'member')),
  PRIMARY KEY (org_id, user_id)
);
CREATE TABLE IF NOT EXISTS sessions (
  hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
  expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tokens (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  hash TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER
);
CREATE TABLE IF NOT EXISTS repos (
  id TEXT PRIMARY KEY,
  owner_id INTEGER NOT NULL REFERENCES owners(id),
  name TEXT NOT NULL COLLATE NOCASE,
  description TEXT NOT NULL DEFAULT '',
  private INTEGER NOT NULL DEFAULT 0,
  protect_default INTEGER NOT NULL DEFAULT 0,
  require_pulls INTEGER NOT NULL DEFAULT 0,
  require_signed INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  pushed_at INTEGER,
  UNIQUE (owner_id, name)
);
CREATE TABLE IF NOT EXISTS activity (
  id INTEGER PRIMARY KEY,
  repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  actor TEXT,
  time INTEGER NOT NULL,
  updates TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS activity_by_repo ON activity (repo_id, id);
CREATE TABLE IF NOT EXISTS state (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS pulls (
  id INTEGER PRIMARY KEY,
  repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  number INTEGER NOT NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL DEFAULT '',
  author_id INTEGER NOT NULL REFERENCES owners(id),
  base TEXT NOT NULL,
  head TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'closed', 'merged')),
  created_at INTEGER NOT NULL,
  closed_at INTEGER,
  closed_by INTEGER REFERENCES owners(id),
  -- Set when it stops being open: what it was comparing then, since the branches move on or go.
  base_oid TEXT,
  head_oid TEXT,
  merge_oid TEXT,
  UNIQUE (repo_id, number)
);
CREATE TABLE IF NOT EXISTS comments (
  id INTEGER PRIMARY KEY,
  pull_id INTEGER NOT NULL REFERENCES pulls(id) ON DELETE CASCADE,
  author_id INTEGER NOT NULL REFERENCES owners(id),
  body TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS comments_by_pull ON comments (pull_id, id);
CREATE TABLE IF NOT EXISTS signing_keys (
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('ssh', 'gpg')),
  title TEXT NOT NULL,
  -- ssh: the key blob in base64. gpg: the armored key.
  material TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  -- gpg: the ids of the key and its subkeys, which is what a signature names.
  key_ids TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  UNIQUE (kind, fingerprint)
);
`);
// Databases made before these settings existed.
for (const setting of ["require_pulls", "require_signed"]) {
  if (!db.query<{ name: string }, []>("PRAGMA table_info(repos)").all().some((column) => column.name === setting)) {
    db.exec(`ALTER TABLE repos ADD COLUMN ${setting} INTEGER NOT NULL DEFAULT 0`);
  }
}

export const now = () => Math.floor(Date.now() / 1000);

export type Owner = { id: number; name: string; kind: "user" | "org"; display_name: string; password_hash: string | null; created_at: number };
export type Repo = {
  id: string;
  owner_id: number;
  name: string;
  description: string;
  private: number;
  protect_default: number;
  require_pulls: number;
  require_signed: number;
  created_at: number;
  pushed_at: number | null;
  owner_name: string;
  owner_kind: "user" | "org";
};

export const REPO_COLUMNS = "repos.*, owners.name AS owner_name, owners.kind AS owner_kind";

export function ownerByName(name: string): Owner | null {
  return db.query<Owner, [string]>("SELECT * FROM owners WHERE name = ?").get(name);
}

export function repoByPath(owner: string, name: string): Repo | null {
  return db
    .query<Repo, [string, string]>(`SELECT ${REPO_COLUMNS} FROM repos JOIN owners ON owners.id = repos.owner_id WHERE owners.name = ? AND repos.name = ?`)
    .get(owner, name);
}

export function repoById(id: string): Repo | null {
  return db.query<Repo, [string]>(`SELECT ${REPO_COLUMNS} FROM repos JOIN owners ON owners.id = repos.owner_id WHERE repos.id = ?`).get(id);
}

export function roleIn(orgId: number, userId: number): "owner" | "member" | null {
  return db.query<{ role: "owner" | "member" }, [number, number]>("SELECT role FROM members WHERE org_id = ? AND user_id = ?").get(orgId, userId)?.role ?? null;
}

/** What a user (or nobody) may do with a repository. */
export type Permission = "none" | "read" | "write" | "admin";

export function permission(repo: Repo, userId: number | null): Permission {
  const open: Permission = repo.private ? "none" : "read";
  if (userId === null) return open;
  if (repo.owner_kind === "user") return repo.owner_id === userId ? "admin" : open;
  const role = roleIn(repo.owner_id, userId);
  return role === "owner" ? "admin" : role === "member" ? "write" : open;
}

export const canRead = (p: Permission) => p !== "none";
export const canWrite = (p: Permission) => p === "write" || p === "admin";
