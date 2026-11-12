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
  updates TEXT NOT NULL,
  -- Set for an import, which moves too many refs to list: how many.
  imported INTEGER
);
-- Who a user is elsewhere: what signing in at a provider comes back with.
CREATE TABLE IF NOT EXISTS identities (
  provider TEXT NOT NULL,
  subject TEXT NOT NULL,
  user_id INTEGER NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (provider, subject)
);
-- Repositories that follow one on GitHub.
CREATE TABLE IF NOT EXISTS mirrors (
  repo_id TEXT PRIMARY KEY REFERENCES repos(id) ON DELETE CASCADE,
  -- "owner/name" there.
  source TEXT NOT NULL,
  -- Whether the heads of its pull requests come along (refs/pull/*/head).
  pulls INTEGER NOT NULL DEFAULT 0,
  -- For a private source. Kept as given: it has to be presented again at every fetch.
  token TEXT,
  state TEXT NOT NULL DEFAULT 'waiting' CHECK (state IN ('waiting', 'syncing', 'synced', 'failed')),
  error TEXT,
  -- When the source was last fetched, and when that last worked.
  checked_at INTEGER,
  synced_at INTEGER
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
-- What a branch asks of the changes that reach it. The pattern is a branch's name, where * stands for anything.
CREATE TABLE IF NOT EXISTS branch_rules (
  id INTEGER PRIMARY KEY,
  repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  pattern TEXT NOT NULL,
  no_force INTEGER NOT NULL DEFAULT 1,
  no_delete INTEGER NOT NULL DEFAULT 1,
  require_pulls INTEGER NOT NULL DEFAULT 0,
  -- Approving reviews of its latest commit that a pull request needs before it merges.
  approvals INTEGER NOT NULL DEFAULT 0,
  require_signed INTEGER NOT NULL DEFAULT 0,
  -- The checks that must have passed on a pull request's latest commit: a JSON list of their names.
  checks TEXT NOT NULL DEFAULT '[]',
  UNIQUE (repo_id, pattern)
);
CREATE TABLE IF NOT EXISTS reviews (
  id INTEGER PRIMARY KEY,
  pull_id INTEGER NOT NULL REFERENCES pulls(id) ON DELETE CASCADE,
  author_id INTEGER NOT NULL REFERENCES owners(id),
  state TEXT NOT NULL CHECK (state IN ('approved', 'changes_requested', 'commented')),
  body TEXT NOT NULL DEFAULT '',
  -- The commit that was looked at: an approval is of that one, not of what came after.
  head_oid TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS reviews_by_pull ON reviews (pull_id, id);
-- Conversations on lines of a pull request's files.
CREATE TABLE IF NOT EXISTS threads (
  id INTEGER PRIMARY KEY,
  pull_id INTEGER NOT NULL REFERENCES pulls(id) ON DELETE CASCADE,
  author_id INTEGER NOT NULL REFERENCES owners(id),
  path TEXT NOT NULL,
  -- Which version of the file the line is counted in: the base's (old) or the branch's (new).
  side TEXT NOT NULL CHECK (side IN ('old', 'new')),
  line INTEGER NOT NULL,
  -- That version, by its id: the line means something only in it.
  blob_oid TEXT NOT NULL,
  resolved_at INTEGER,
  resolved_by_id INTEGER REFERENCES owners(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS threads_by_pull ON threads (pull_id, id);
CREATE TABLE IF NOT EXISTS thread_comments (
  id INTEGER PRIMARY KEY,
  thread_id INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
  author_id INTEGER NOT NULL REFERENCES owners(id),
  body TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS thread_comments_by_thread ON thread_comments (thread_id, id);
-- What something outside (a CI system, say) found out about a commit.
CREATE TABLE IF NOT EXISTS statuses (
  id INTEGER PRIMARY KEY,
  repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  oid TEXT NOT NULL,
  context TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending', 'success', 'failure', 'error')),
  description TEXT NOT NULL DEFAULT '',
  target_url TEXT NOT NULL DEFAULT '',
  creator_id INTEGER REFERENCES owners(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS statuses_by_commit ON statuses (repo_id, oid, id);
-- Addresses that are told what happens to a repository.
CREATE TABLE IF NOT EXISTS webhooks (
  id INTEGER PRIMARY KEY,
  repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  url TEXT NOT NULL,
  -- Signs each delivery, so that the receiver knows it comes from here. Kept as given: it is used at every delivery.
  secret TEXT NOT NULL DEFAULT '',
  -- A JSON list of event names.
  events TEXT NOT NULL,
  active INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS deliveries (
  id INTEGER PRIMARY KEY,
  webhook_id INTEGER NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
  event TEXT NOT NULL,
  payload TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'delivered', 'failed')),
  attempts INTEGER NOT NULL DEFAULT 0,
  -- The receiver's last answer, or why there was none.
  status INTEGER,
  error TEXT,
  created_at INTEGER NOT NULL,
  -- When it is next tried, while pending.
  next_at INTEGER NOT NULL,
  delivered_at INTEGER
);
CREATE INDEX IF NOT EXISTS deliveries_due ON deliveries (state, next_at);
CREATE INDEX IF NOT EXISTS deliveries_by_hook ON deliveries (webhook_id, id);
-- Groups of an organization's members, given access to repositories together.
CREATE TABLE IF NOT EXISTS teams (
  id INTEGER PRIMARY KEY,
  org_id INTEGER NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
  name TEXT NOT NULL COLLATE NOCASE,
  description TEXT NOT NULL DEFAULT '',
  -- Set for a team that follows a group at a sign-in provider: its members are whoever signs in as part of that group.
  sso_provider TEXT,
  sso_group TEXT,
  UNIQUE (org_id, name)
);
CREATE TABLE IF NOT EXISTS team_members (
  team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  user_id INTEGER NOT NULL REFERENCES owners(id) ON DELETE CASCADE,
  PRIMARY KEY (team_id, user_id)
);
CREATE TABLE IF NOT EXISTS team_repos (
  team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  level TEXT NOT NULL CHECK (level IN ('read', 'write', 'admin')),
  PRIMARY KEY (team_id, repo_id)
);
`);

const hasColumn = (table: string, column: string) => db.query<{ name: string }, []>(`PRAGMA table_info(${table})`).all().some((found) => found.name === column);
// The commit of its branch that webhooks were last told of: a push is only news if it brings another.
if (!hasColumn("pulls", "told_oid")) db.exec("ALTER TABLE pulls ADD COLUMN told_oid TEXT");
// What every member of an organization may do with each of its repositories, before teams add to it.
if (!hasColumn("owners", "base_permission")) db.exec("ALTER TABLE owners ADD COLUMN base_permission TEXT NOT NULL DEFAULT 'write'");
// Set for a membership that a sign-in provider's group brought, and that goes when the group does.
if (!hasColumn("members", "synced")) db.exec("ALTER TABLE members ADD COLUMN synced INTEGER NOT NULL DEFAULT 0");

// Databases made before these settings existed.
for (const setting of ["require_pulls", "require_signed"]) {
  if (!db.query<{ name: string }, []>("PRAGMA table_info(repos)").all().some((column) => column.name === setting)) {
    db.exec(`ALTER TABLE repos ADD COLUMN ${setting} INTEGER NOT NULL DEFAULT 0`);
  }
}

if (!db.query<{ name: string }, []>("PRAGMA table_info(activity)").all().some((column) => column.name === "imported")) {
  db.exec("ALTER TABLE activity ADD COLUMN imported INTEGER");
}

export const now = () => Math.floor(Date.now() / 1000);

export type Owner = { id: number; name: string; kind: "user" | "org"; display_name: string; password_hash: string | null; created_at: number; base_permission: Permission };
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
  /** For an organization's repository: what each member may do with it. */
  owner_base: Permission;
};

export const REPO_COLUMNS = "repos.*, owners.name AS owner_name, owners.kind AS owner_kind, owners.base_permission AS owner_base";

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

const LEVELS: Permission[] = ["none", "read", "write", "admin"];
const most = (levels: Permission[]) => LEVELS[Math.max(...levels.map((level) => LEVELS.indexOf(level)))]!;

export function permission(repo: Repo, userId: number | null): Permission {
  const open: Permission = repo.private ? "none" : "read";
  if (userId === null) return open;
  if (repo.owner_kind === "user") return repo.owner_id === userId ? "admin" : open;
  const role = roleIn(repo.owner_id, userId);
  if (role === "owner") return "admin";
  if (role === null) return open;
  // A member: what every member may do, and what the teams they are in add to that.
  const teams = db
    .query<{ level: Permission }, [string, number]>("SELECT team_repos.level FROM team_repos JOIN team_members ON team_members.team_id = team_repos.team_id WHERE team_repos.repo_id = ? AND team_members.user_id = ?")
    .all(repo.id, userId);
  return most([open, repo.owner_base, ...teams.map((team) => team.level)]);
}

export type Mirror = { repo_id: string; source: string; pulls: number; token: string | null; state: "waiting" | "syncing" | "synced" | "failed"; error: string | null; checked_at: number | null; synced_at: number | null };

export const mirrorOf = (repoId: string) => db.query<Mirror, [string]>("SELECT * FROM mirrors WHERE repo_id = ?").get(repoId);

export const canRead = (p: Permission) => p !== "none";
export const canWrite = (p: Permission) => p === "write" || p === "admin";
