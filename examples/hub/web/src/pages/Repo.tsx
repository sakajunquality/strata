import { LockOutlined, SyncOutlined } from "@ant-design/icons";
import { Alert, App, Badge, Button, Menu, Select, Table, Tag, Typography } from "antd";
import { useEffect } from "react";
import { Link, Route, Routes, useLocation, useNavigate, useParams } from "react-router-dom";
import { api, useLoad, type Activity, type RepoDetail } from "../api";
import { ActivityList } from "../RepoList";
import { useSession } from "../session";
import { Loaded, shortOid, urlPath, When } from "../util";
import { BlobPage, CodePage } from "./Code";
import { CommitPage, CommitsPage } from "./History";
import { NewPullPage, PullPage, PullsPage } from "./Pulls";
import { RepoSettings } from "./RepoSettings";
import { BlamePage, CodeSearchPage } from "./Search";

/** What the pages of one repository share. */
export type RepoContext = { repo: RepoDetail; root: string; base: string; reload: () => void };

/** Picks a branch or tag; `to` says where each choice leads. */
export function RevSelect({ repo, rev, to }: { repo: RepoDetail; rev: string; to: (rev: string) => string }) {
  const navigate = useNavigate();
  const known = [...repo.branches, ...repo.tags].some((named) => named.name === rev);
  const group = (label: string, names: { name: string }[]) => ({ label, options: names.map(({ name }) => ({ value: name, label: name })) });
  return (
    <Select
      value={rev}
      aria-label="Branch or tag"
      popupMatchSelectWidth={false}
      showSearch
      className="rev-select"
      onChange={(next) => navigate(to(next))}
      options={[
        ...(known ? [] : [{ value: rev, label: shortOid(rev) }]),
        ...(repo.branches.length ? [group("Branches", repo.branches)] : []),
        ...(repo.tags.length ? [group("Tags", repo.tags)] : []),
      ]}
    />
  );
}

function Branches({ repo, root }: RepoContext) {
  const rows = (names: RepoDetail["branches"]) => (
    <Table
      rowKey="name"
      size="small"
      pagination={false}
      showHeader={false}
      dataSource={names}
      locale={{ emptyText: "None" }}
      columns={[
        {
          dataIndex: "name",
          render: (name: string) => (
            <>
              <Link to={`${root}/tree/${urlPath(name)}`}>{name}</Link>
              {name === repo.defaultBranch && names === repo.branches && <Tag className="tag-gap">default</Tag>}
              {names === repo.branches && repo.branches.find((branch) => branch.name === name)?.protected && <Tag icon={<LockOutlined />}>protected</Tag>}
            </>
          ),
        },
        { dataIndex: "oid", align: "right", render: (oid: string) => <Link to={`${root}/commit/${oid}`} className="mono">{shortOid(oid)}</Link> },
        {
          key: "pull",
          align: "right",
          render: (_, named) => names === repo.branches && named.name !== repo.defaultBranch && !repo.empty && <Link to={`${root}/pulls/new?head=${encodeURIComponent(named.name)}`}>New pull request</Link>,
        },
        { key: "history", align: "right", width: 100, render: (_, named) => <Link to={`${root}/commits/${urlPath(named.name)}`}>History</Link> },
      ]}
    />
  );
  return (
    <>
      <Typography.Title level={5}>Branches</Typography.Title>
      {rows(repo.branches)}
      <Typography.Title level={5} className="stack">Tags</Typography.Title>
      {rows(repo.tags)}
    </>
  );
}

function RepoActivity({ base }: RepoContext) {
  const activity = useLoad(() => api<{ activity: Activity[] }>(`${base}/activity`), `activity:${base}`);
  return <Loaded from={activity}>{(data) => <ActivityList activity={data.activity} />}</Loaded>;
}

/** Where a mirror comes from and how fresh it is. */
function MirrorNote({ repo, base, reload }: RepoContext) {
  const { message } = App.useApp();
  const mirror = repo.mirror!;
  const working = mirror.state === "waiting" || mirror.state === "syncing";
  // Looks again while a fetch is under way, so that its result shows by itself.
  useEffect(() => {
    if (!working) return;
    const timer = setTimeout(reload, 2000);
    return () => clearTimeout(timer);
  });
  const sync = async () => {
    try {
      await api(`${base}/mirror/sync`, "POST");
      reload();
    } catch (e) {
      message.error((e as Error).message);
    }
  };
  return (
    <Alert
      className="mirror-note"
      type={mirror.state === "failed" ? "error" : "info"}
      showIcon
      icon={<SyncOutlined spin={working} />}
      title={
        <>
          Mirror of <a href={mirror.url}>{mirror.source}</a>
          {working ? <>: fetching…</> : mirror.syncedAt ? <>, fetched <When time={mirror.syncedAt} /></> : null}
        </>
      }
      description={mirror.state === "failed" ? `The last fetch failed: ${mirror.error}` : undefined}
      action={repo.permission !== "read" && <Button size="small" onClick={sync} disabled={working}>Fetch now</Button>}
    />
  );
}

export function Repo() {
  const { owner = "", repo: name = "" } = useParams();
  const { me } = useSession();
  const location = useLocation();
  const base = `/repos/${owner}/${name}`;
  // Reloaded on navigation, so that a push shows without a manual refresh.
  const detail = useLoad(() => api<RepoDetail>(base), `repo:${base}:${me?.user?.name}`);
  const root = `/${owner}/${name}`;
  const section = location.pathname.slice(root.length + 1).split("/")[0];
  const tab = { "": "code", tree: "code", blob: "code", blame: "code", search: "code", commits: "commits", commit: "commits", branches: "branches", pulls: "pulls", activity: "activity", settings: "settings" }[section] ?? "code";

  return (
    <Loaded from={detail}>
      {(repo) => {
        const context: RepoContext = { repo, root, base, reload: detail.reload };
        return (
          <>
            <div className="repo-head">
              <Typography.Title level={3}>
                <Link to={`/${repo.owner}`}>{repo.owner}</Link> <span className="muted">/</span> <Link to={root}>{repo.name}</Link>
              </Typography.Title>
              {repo.private && <Tag icon={<LockOutlined />}>Private</Tag>}
            </div>
            {repo.description && <p className="muted repo-description">{repo.description}</p>}
            {repo.mirror && <MirrorNote {...context} />}
            <Menu
              mode="horizontal"
              selectedKeys={[tab]}
              className="repo-tabs"
              items={[
                { key: "code", label: <Link to={root}>Code</Link> },
                { key: "commits", label: <Link to={`${root}/commits`}>Commits</Link> },
                { key: "pulls", label: <Link to={`${root}/pulls`}>Pull requests{repo.openPulls > 0 && <Badge count={repo.openPulls} color="#8c8c8c" className="tab-count" />}</Link> },
                { key: "branches", label: <Link to={`${root}/branches`}>Branches</Link> },
                { key: "activity", label: <Link to={`${root}/activity`}>Activity</Link> },
                ...(repo.permission === "admin" ? [{ key: "settings", label: <Link to={`${root}/settings`}>Settings</Link> }] : []),
              ]}
            />
            <Routes>
              <Route index element={<CodePage {...context} />} />
              <Route path="tree/*" element={<CodePage {...context} />} />
              <Route path="blob/*" element={<BlobPage {...context} />} />
              <Route path="blame/*" element={<BlamePage {...context} />} />
              <Route path="search" element={<CodeSearchPage {...context} />} />
              <Route path="commits/*" element={<CommitsPage {...context} />} />
              <Route path="commit/:oid" element={<CommitPage {...context} />} />
              <Route path="pulls" element={<PullsPage {...context} />} />
              <Route path="pulls/new" element={<NewPullPage {...context} />} />
              <Route path="pulls/:number" element={<PullPage {...context} />} />
              <Route path="branches" element={<Branches {...context} />} />
              <Route path="activity" element={<RepoActivity {...context} />} />
              <Route path="settings" element={<RepoSettings {...context} />} />
            </Routes>
          </>
        );
      }}
    </Loaded>
  );
}
