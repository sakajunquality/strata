import { FileOutlined, FolderFilled, LinkOutlined } from "@ant-design/icons";
import { Alert, Button, Card, Popover, Segmented, Space, Typography } from "antd";
import { useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Link, useParams } from "react-router-dom";
import { api, query, useLoad, type Commit, type Tree } from "../api";
import { blobUrl, CodeView, loadBlob } from "../blob";
import { useSession } from "../session";
import { bytes, Loaded, shortOid, splitRev, subject, urlPath, When } from "../util";
import { RevSelect, type RepoContext } from "./Repo";

const IMAGE = /\.(png|jpe?g|gif|webp|svg|ico)$/i;
const MARKDOWN = /\.(md|markdown)$/i;
const README = /^readme(\.(md|markdown|txt))?$/i;

function usePlace(repo: RepoContext["repo"]) {
  const rest = useParams()["*"] ?? "";
  return rest ? splitRev(rest, [...repo.branches, ...repo.tags]) : { rev: repo.defaultBranch, path: "" };
}

function Crumbs({ root, repo, rev, path }: { root: string; repo: string; rev: string; path: string }) {
  const segments = path ? path.split("/") : [];
  const tree = `${root}/tree/${urlPath(rev)}`;
  return (
    <span className="crumbs">
      <Link to={tree}><strong>{repo}</strong></Link>
      {segments.map((segment, i) => (
        <span key={i}>
          <span className="muted"> / </span>
          {i === segments.length - 1 ? segment : <Link to={`${tree}/${urlPath(segments.slice(0, i + 1).join("/"))}`}>{segment}</Link>}
        </span>
      ))}
    </span>
  );
}

function CloneButton({ url }: { url: string }) {
  return (
    <Popover
      trigger="click"
      placement="bottomRight"
      title="Clone over HTTP"
      content={
        <div className="clone">
          <Typography.Text code copyable>{url}</Typography.Text>
          <p className="muted small">Sign in with your user name and an access token or your password.</p>
        </div>
      }
    >
      <Button type="primary">Clone</Button>
    </Popover>
  );
}

function EmptyRepo({ repo }: RepoContext) {
  const { me } = useSession();
  return (
    <Card title="This repository is empty">
      <Typography.Paragraph>Push an existing repository to fill it:</Typography.Paragraph>
      <pre className="snippet">{`git remote add origin ${repo.cloneUrl}\ngit push -u origin ${repo.defaultBranch}`}</pre>
      <Typography.Paragraph className="muted">
        git asks for a user name and password: use {me?.user ? <strong>{me.user.name}</strong> : "your user name"} and an{" "}
        <Link to="/settings/tokens">access token</Link>, or your account password.
      </Typography.Paragraph>
    </Card>
  );
}

function LatestCommit({ base, root, rev }: { base: string; root: string; rev: string }) {
  const latest = useLoad(() => api<{ commits: Commit[] }>(`${base}/commits${query({ rev })}`), `latest:${base}:${rev}`);
  const commit = latest.data?.commits[0];
  if (!commit) return <div className="latest" />;
  return (
    <div className="latest">
      <span className="ellipsis">
        <strong>{commit.author?.name}</strong> <Link to={`${root}/commit/${commit.oid}`} className="plain">{subject(commit.message)}</Link>
      </span>
      <span className="muted nowrap">
        <Link to={`${root}/commit/${commit.oid}`} className="mono">{shortOid(commit.oid)}</Link> · <When time={commit.committer?.time} /> ·{" "}
        <Link to={`${root}/commits/${urlPath(rev)}`}>History</Link>
      </span>
    </div>
  );
}

function Readme({ base, oid, name }: { base: string; oid: string; name: string }) {
  const blob = useLoad(() => loadBlob(base, { oid }), `readme:${base}:${oid}`);
  if (blob.data?.text == null) return null;
  return (
    <Card title={name} size="small" className="stack">
      {MARKDOWN.test(name) ? <div className="markdown"><Markdown remarkPlugins={[remarkGfm]}>{blob.data.text}</Markdown></div> : <pre className="snippet">{blob.data.text}</pre>}
    </Card>
  );
}

export function CodePage(context: RepoContext) {
  const { repo, root, base } = context;
  const { rev, path } = usePlace(repo);
  const atRoot = !useParams()["*"];
  const tree = useLoad(
    () => (repo.empty && atRoot ? Promise.resolve(null) : api<Tree>(`${base}/tree${query({ rev, path })}`)),
    `tree:${base}:${rev}:${path}:${repo.empty}`,
  );
  if (repo.empty && atRoot) return <EmptyRepo {...context} />;

  const treeUrl = `${root}/tree/${urlPath(rev)}`;
  return (
    <>
      <div className="toolbar">
        <RevSelect repo={repo} rev={rev} to={(next) => `${root}/tree/${urlPath(next)}${path ? `/${urlPath(path)}` : ""}`} />
        <Crumbs root={root} repo={repo.name} rev={rev} path={path} />
        <span className="grow" />
        <CloneButton url={repo.cloneUrl} />
      </div>
      <Loaded from={tree}>
        {(data) => {
          if (!data) return null;
          const entries = [...data.entries].sort((a, b) => Number(b.type === "tree") - Number(a.type === "tree") || a.name.localeCompare(b.name));
          const readme = entries.find((entry) => entry.type === "blob" && README.test(entry.name));
          const prefix = path ? `${urlPath(path)}/` : "";
          return (
            <>
              <div className="files">
                <LatestCommit base={base} root={root} rev={rev} />
                {path && (
                  <div className="file">
                    <FolderFilled className="folder" />
                    <Link to={path.includes("/") ? `${treeUrl}/${urlPath(path.slice(0, path.lastIndexOf("/")))}` : treeUrl} aria-label="Parent directory">..</Link>
                  </div>
                )}
                {entries.map((entry) => (
                  <div className="file" key={entry.name}>
                    {entry.type === "tree" ? <FolderFilled className="folder" /> : entry.type === "commit" ? <LinkOutlined className="muted" /> : <FileOutlined className="muted" />}
                    {entry.type === "commit" ? (
                      <span>{entry.name} <span className="muted mono">@ {shortOid(entry.oid)}</span></span>
                    ) : (
                      <Link to={`${root}/${entry.type}/${urlPath(rev)}/${prefix}${encodeURIComponent(entry.name)}`}>{entry.name}</Link>
                    )}
                  </div>
                ))}
                {entries.length === 0 && <div className="file muted">This directory is empty</div>}
              </div>
              {readme && <Readme base={base} oid={readme.oid} name={readme.name} />}
            </>
          );
        }}
      </Loaded>
    </>
  );
}

export function BlobPage({ repo, root, base }: RepoContext) {
  const { rev, path } = usePlace(repo);
  const [raw, setRaw] = useState(false);
  const image = IMAGE.test(path);
  const blob = useLoad(() => (image ? Promise.resolve({ size: -1, text: null }) : loadBlob(base, { rev, path })), `blob:${base}:${rev}:${path}`);
  const markdown = MARKDOWN.test(path);
  return (
    <>
      <div className="toolbar">
        <RevSelect repo={repo} rev={rev} to={(next) => `${root}/blob/${urlPath(next)}/${urlPath(path)}`} />
        <Crumbs root={root} repo={repo.name} rev={rev} path={path} />
      </div>
      <Loaded from={blob}>
        {(data) => (
          <Card
            size="small"
            title={<span className="muted small">{data.size >= 0 ? bytes(data.size) : path.split("/").pop()}</span>}
            extra={
              <Space>
                {markdown && data.text !== null && (
                  <Segmented size="small" value={raw ? "Code" : "Preview"} options={["Preview", "Code"]} onChange={(value) => setRaw(value === "Code")} />
                )}
                <Button size="small" href={blobUrl(base, { rev, path })} target="_blank">Raw</Button>
                <Button size="small" href={blobUrl(base, { rev, path, download: 1 })}>Download</Button>
              </Space>
            }
            styles={{ body: { padding: 0 } }}
          >
            {image ? (
              <div className="center"><img src={blobUrl(base, { rev, path })} alt={path} className="preview" /></div>
            ) : data.text === null ? (
              <Alert type="info" banner title="This file is binary or too large to show. Download it to see it." />
            ) : markdown && !raw ? (
              <div className="markdown padded"><Markdown remarkPlugins={[remarkGfm]}>{data.text}</Markdown></div>
            ) : (
              <CodeView text={data.text} path={path} />
            )}
          </Card>
        )}
      </Loaded>
    </>
  );
}
