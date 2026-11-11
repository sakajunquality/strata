import { Alert, Button, Card, Space, Tag, Typography } from "antd";
import { structuredPatch } from "diff";
import { useState } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import { api, query, useLoad, type Commit, type CommitDetail, type FileChange } from "../api";
import { loadBlob } from "../blob";
import { Loaded, shortOid, splitRev, subject, urlPath, When } from "../util";
import { RevSelect, type RepoContext } from "./Repo";

export function CommitRows({ root, commits }: { root: string; commits: Commit[] }) {
  return (
    <ul className="rows boxed">
      {commits.map((commit) => (
        <li key={commit.oid} className="commit-row">
          <div className="ellipsis">
            <Link to={`${root}/commit/${commit.oid}`} className="plain"><strong>{subject(commit.message)}</strong></Link>
            <div className="muted small">{commit.author?.name} committed <When time={commit.committer?.time} /></div>
          </div>
          <Space>
            <Link to={`${root}/commit/${commit.oid}`} className="mono">{shortOid(commit.oid)}</Link>
            <Link to={`${root}/tree/${commit.oid}`}>Browse</Link>
          </Space>
        </li>
      ))}
      {commits.length === 0 && <li className="muted">No commits</li>}
    </ul>
  );
}

export function CommitsPage({ repo, root, base }: RepoContext) {
  const rest = useParams()["*"] ?? "";
  const [params] = useSearchParams();
  const rev = rest ? splitRev(rest, [...repo.branches, ...repo.tags]).rev : repo.defaultBranch;
  const skip = Number(params.get("skip")) || 0;
  const page = useLoad(
    () => (repo.empty && !rest ? Promise.resolve({ commits: [], next: null }) : api<{ commits: Commit[]; next: number | null }>(`${base}/commits${query({ rev, skip })}`)),
    `commits:${base}:${rev}:${skip}`,
  );
  const here = `${root}/commits/${urlPath(rev)}`;
  return (
    <>
      <div className="toolbar">
        <RevSelect repo={repo} rev={rev} to={(next) => `${root}/commits/${urlPath(next)}`} />
      </div>
      <Loaded from={page}>
        {(data) => (
          <>
            <CommitRows root={root} commits={data.commits} />
            <Space className="stack">
              {skip > 0 && <Link to={`${here}${query({ skip: Math.max(0, skip - 30) || undefined })}`}><Button>Newer</Button></Link>}
              {data.next !== null && <Link to={`${here}?skip=${data.next}`}><Button>Older</Button></Link>}
            </Space>
          </>
        )}
      </Loaded>
    </>
  );
}

const MAX_DIFFED = 300_000;

/** One side of a change as text; null if it cannot be diffed. */
async function sideText(base: string, side: FileChange["old"]): Promise<string | null> {
  if (!side) return "";
  const blob = await loadBlob(base, { oid: side.oid });
  return blob.text !== null && blob.size <= MAX_DIFFED ? blob.text : null;
}

function Diff({ base, file }: { base: string; file: FileChange }) {
  const patch = useLoad(async () => {
    const [before, after] = await Promise.all([sideText(base, file.old), sideText(base, file.new)]);
    if (before === null || after === null) return null;
    return structuredPatch(file.path, file.path, before, after, "", "", { context: 3 });
  }, `diff:${base}:${file.old?.oid}:${file.new?.oid}`);
  return (
    <Loaded from={patch}>
      {(data) => {
        if (!data) return <div className="diff-note muted">Binary or large file: not shown</div>;
        if (data.hunks.length === 0) return <div className="diff-note muted">No change in content{file.old?.mode !== file.new?.mode && ` (mode ${file.old?.mode} → ${file.new?.mode})`}</div>;
        return (
          <table className="diff">
            <tbody>
              {data.hunks.map((hunk, h) => {
                let [before, after] = [hunk.oldStart, hunk.newStart];
                return [
                  <tr key={`h${h}`} className="hunk">
                    <td colSpan={3}>@@ -{hunk.oldStart},{hunk.oldLines} +{hunk.newStart},{hunk.newLines} @@</td>
                  </tr>,
                  ...hunk.lines.map((line, i) => {
                    const mark = line[0];
                    // "\ No newline at end of file" belongs to the line above it.
                    if (mark === "\\") return <tr key={`${h}-${i}`}><td /><td /><td className="muted">{line}</td></tr>;
                    const left = mark === "+" ? "" : before++;
                    const right = mark === "-" ? "" : after++;
                    return (
                      <tr key={`${h}-${i}`} className={mark === "+" ? "add" : mark === "-" ? "del" : undefined}>
                        <td className="n">{left}</td>
                        <td className="n">{right}</td>
                        <td>{mark}{line.slice(1)}</td>
                      </tr>
                    );
                  }),
                ];
              })}
            </tbody>
          </table>
        );
      }}
    </Loaded>
  );
}

const COLORS = { added: "green", deleted: "red", modified: "gold" } as const;

export function FileCard({ base, root, commit, file, open }: { base: string; root: string; commit: string; file: FileChange; open: boolean }) {
  const [shown, setShown] = useState(open);
  const submodule = (file.new ?? file.old)!.mode === "160000";
  return (
    <Card
      size="small"
      className="stack"
      styles={{ body: { padding: 0 } }}
      title={
        <Space>
          <Tag color={COLORS[file.status]}>{file.status}</Tag>
          {file.new && !submodule ? <Link to={`${root}/blob/${commit}/${urlPath(file.path)}`} className="mono">{file.path}</Link> : <span className="mono">{file.path}</span>}
        </Space>
      }
    >
      {submodule ? (
        <div className="diff-note muted">Submodule {file.old ? shortOid(file.old.oid) : "(none)"} → {file.new ? shortOid(file.new.oid) : "(removed)"}</div>
      ) : shown ? (
        <Diff base={base} file={file} />
      ) : (
        <div className="diff-note"><Button size="small" onClick={() => setShown(true)}>Show changes</Button></div>
      )}
    </Card>
  );
}

export function CommitPage({ root, base }: RepoContext) {
  const { oid = "" } = useParams();
  const commit = useLoad(() => api<CommitDetail>(`${base}/commit${query({ rev: oid })}`), `commit:${base}:${oid}`);
  return (
    <Loaded from={commit}>
      {(data) => {
        const body = data.message.split("\n").slice(1).join("\n").trim();
        return (
          <>
            <Card size="small" extra={<Link to={`${root}/tree/${data.oid}`}><Button size="small">Browse files</Button></Link>} title={subject(data.message)}>
              {body && <pre className="message">{body}</pre>}
              <div className="muted">
                <strong>{data.author?.name}</strong> authored <When time={data.author?.time} />
                {data.committer && data.committer.name !== data.author?.name && <>, <strong>{data.committer.name}</strong> committed <When time={data.committer.time} /></>}
              </div>
              <div className="muted small">
                Commit <span className="mono">{data.oid}</span>
                {data.parents.map((parent) => (
                  <span key={parent}> · parent <Link to={`${root}/commit/${parent}`} className="mono">{shortOid(parent)}</Link></span>
                ))}
              </div>
            </Card>
            <Typography.Paragraph className="stack muted">
              {data.files.length} changed {data.files.length === 1 ? "file" : "files"}
              {data.parents.length > 1 && ", compared with the first parent"}
            </Typography.Paragraph>
            {data.truncated && <Alert type="warning" showIcon title="This commit changes too many files to list them all." />}
            {data.files.map((file, i) => <FileCard key={file.path} base={base} root={root} commit={data.oid} file={file} open={i < 20} />)}
          </>
        );
      }}
    </Loaded>
  );
}
