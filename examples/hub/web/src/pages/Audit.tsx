// The audit log of an account, an organization or a repository: who changed
// what about access and settings.
import { Button, Card, Table, Typography } from "antd";
import { useState } from "react";
import { Navigate } from "react-router-dom";
import { api, useLoad, type AuditEntry } from "../api";
import { useSession } from "../session";
import { Loaded, When } from "../util";

/** What an entry says happened, in words. Actions not listed are shown as they are named. */
const ACTIONS: Record<string, string> = {
  "account.create": "Account created",
  "account.sign-in": "Signed in",
  "account.sign-in-failed": "Sign-in failed",
  "token.create": "Access token created",
  "token.delete": "Access token deleted",
  "key.add": "Key added",
  "key.remove": "Key removed",
  "org.create": "Organization created",
  "org.base-permission": "What every member may do changed",
  "member.set": "Member added or role changed",
  "member.remove": "Member removed",
  "team.create": "Team created",
  "team.delete": "Team deleted",
  "team.follow": "Team's sign-in group changed",
  "team.member-add": "Added to team",
  "team.member-remove": "Removed from team",
  "team.repo-set": "Team given a repository",
  "team.repo-remove": "Repository taken from team",
  "repo.create": "Repository created",
  "repo.delete": "Repository deleted",
  "repo.rename": "Repository renamed",
  "repo.visibility": "Visibility changed",
  "repo.default-branch": "Default branch changed",
  "repo.mirror-stop": "Mirroring stopped",
  "rule.set": "Branch rule set",
  "rule.remove": "Branch rule removed",
  "webhook.create": "Webhook added",
  "webhook.change": "Webhook changed",
  "webhook.remove": "Webhook removed",
  "pull.merge": "Pull request merged",
};

const words = (value: unknown): string => (Array.isArray(value) ? value.join(", ") || "none" : typeof value === "boolean" ? (value ? "yes" : "no") : String(value));

function Entries({ path }: { path: string }) {
  const [before, setBefore] = useState<number[]>([]);
  const at = before[before.length - 1];
  const log = useLoad(() => api<{ entries: AuditEntry[]; next: number | null }>(`${path}${at ? `?before=${at}` : ""}`), `audit:${path}:${at}`);
  return (
    <Loaded from={log}>
      {(data) => (
        <>
          <Table
            rowKey="id"
            size="small"
            pagination={false}
            dataSource={data.entries}
            locale={{ emptyText: "Nothing recorded yet" }}
            columns={[
              { title: "When", dataIndex: "time", width: 130, render: (time: number) => <When time={time} /> },
              { title: "Who", dataIndex: "actor", width: 120, render: (actor: string | null) => actor ?? <span className="muted">nobody signed in</span> },
              {
                title: "What",
                key: "what",
                render: (_, entry) => (
                  <>
                    {ACTIONS[entry.action] ?? entry.action}
                    {entry.subject && <>: <strong>{entry.subject}</strong></>}
                    {Object.keys(entry.detail).length > 0 && (
                      <div className="muted small">{Object.entries(entry.detail).map(([key, value]) => `${key}: ${words(value)}`).join(" · ")}</div>
                    )}
                  </>
                ),
              },
            ]}
          />
          {(before.length > 0 || data.next !== null) && (
            <div className="stack toolbar">
              {before.length > 0 && <Button onClick={() => setBefore(before.slice(0, -1))}>Newer</Button>}
              {data.next !== null && <Button onClick={() => setBefore([...before, data.next!])}>Older</Button>}
            </div>
          )}
        </>
      )}
    </Loaded>
  );
}

export function AuditCard({ path, about }: { path: string; about: string }) {
  return (
    <Card title="Audit log" className="stack">
      <Typography.Paragraph className="muted">Who changed what about {about}, newest first. Pushes are in the activity of each repository.</Typography.Paragraph>
      <Entries path={path} />
    </Card>
  );
}

/** One's own account: sign-ins, tokens and keys. */
export function AuditPage() {
  const { me } = useSession();
  if (!me) return null;
  if (!me.user) return <Navigate to="/login" />;
  return (
    <div className="medium wide">
      <AuditCard path={`/owners/${me.user.name}/audit`} about="your account and your own repositories" />
    </div>
  );
}
