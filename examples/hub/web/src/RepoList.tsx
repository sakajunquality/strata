import { LockOutlined } from "@ant-design/icons";
import { Empty, Tag } from "antd";
import { Link } from "react-router-dom";
import type { Activity, RepoSummary } from "./api";
import { shortOid, When } from "./util";

export function RepoList({ repos, empty }: { repos: RepoSummary[]; empty: string }) {
  if (repos.length === 0) return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={empty} />;
  return (
    <ul className="rows">
      {repos.map((repo) => (
        <li key={`${repo.owner}/${repo.name}`}>
          <div>
            <Link to={`/${repo.owner}/${repo.name}`} className="repo-link">
              <span className="muted">{repo.owner} / </span>
              {repo.name}
            </Link>
            {repo.private && <Tag icon={<LockOutlined />} className="tag-gap">Private</Tag>}
          </div>
          {repo.description && <div className="muted">{repo.description}</div>}
          <div className="muted small">{repo.pushedAt ? <>Pushed <When time={repo.pushedAt} /></> : <>Created <When time={repo.createdAt} /></>}</div>
        </li>
      ))}
    </ul>
  );
}

const VERBS: Record<string, string> = { create: "created", delete: "deleted", "fast-forward": "pushed to", forced: "force-pushed" };
const refLabel = (ref: string) => (ref.startsWith("refs/tags/") ? `tag ${ref.slice(10)}` : ref.replace("refs/heads/", ""));

export function ActivityList({ activity, showRepo }: { activity: Activity[]; showRepo?: boolean }) {
  if (activity.length === 0) return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No pushes yet" />;
  return (
    <ul className="rows">
      {activity.map((push) => (
        <li key={push.id}>
          {push.updates.map((update) => {
            const base = `/${push.owner}/${push.repo}`;
            return (
              <div key={update.ref}>
                <strong>{push.actor ?? "someone"}</strong> {VERBS[update.change] ?? "updated"} <code>{refLabel(update.ref)}</code>
                {showRepo && <> in <Link to={base}>{push.owner}/{push.repo}</Link></>}
                {update.new && <> at <Link to={`${base}/commit/${update.new}`} className="mono">{shortOid(update.new)}</Link></>}
              </div>
            );
          })}
          <div className="muted small"><When time={push.time} /></div>
        </li>
      ))}
    </ul>
  );
}
