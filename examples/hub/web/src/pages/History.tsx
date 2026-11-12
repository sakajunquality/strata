import { PlusOutlined } from "@ant-design/icons";
import { Alert, Button, Card, Input, Space, Tag, Tooltip, Typography } from "antd";
import { structuredPatch } from "diff";
import { useState, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Link, useParams, useSearchParams } from "react-router-dom";
import { api, query, useLoad, type Commit, type CommitDetail, type FileChange, type Thread, type Verification } from "../api";
import { loadBlob } from "../blob";
import { Loaded, shortOid, splitRev, subject, urlPath, When } from "../util";
import { RevSelect, type RepoContext } from "./Repo";

/** Says whether a commit's signature was made with a key registered here. Nothing for an unsigned commit. */
export function Signed({ verification }: { verification?: Verification }) {
  if (!verification) return null;
  if (verification.state === "verified") {
    return <Tooltip title={`Signed by ${verification.signer} with ${verification.key}`}><Tag color="green">Verified</Tag></Tooltip>;
  }
  return <Tooltip title={`This commit ${verification.reason}`}><Tag>Unverified</Tag></Tooltip>;
}

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
            <Signed verification={commit.verification} />
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
  // With a path: the history of that file or directory alone.
  const path = params.get("path") ?? undefined;
  const page = useLoad(
    () => (repo.empty && !rest ? Promise.resolve({ commits: [] as Commit[], next: null as number | null, truncated: false }) : api<{ commits: Commit[]; next: number | null; truncated?: boolean }>(`${base}/commits${query({ rev, skip, path })}`)),
    `commits:${base}:${rev}:${skip}:${path}`,
  );
  const here = `${root}/commits/${urlPath(rev)}`;
  return (
    <>
      <div className="toolbar">
        <RevSelect repo={repo} rev={rev} to={(next) => `${root}/commits/${urlPath(next)}${query({ path })}`} />
        {path && <span>History of <Link to={`${root}/blob/${urlPath(rev)}/${urlPath(path)}`} className="mono">{path}</Link> · <Link to={here}>all commits</Link></span>}
      </div>
      <Loaded from={page}>
        {(data) => (
          <>
            <CommitRows root={root} commits={data.commits} />
            {data.truncated && <Alert type="warning" showIcon className="stack" title="The history is long: the search for older commits that changed this path was given up." />}
            <Space className="stack">
              {skip > 0 && <Link to={`${here}${query({ skip: Math.max(0, skip - 30) || undefined, path })}`}><Button>Newer</Button></Link>}
              {data.next !== null && <Link to={`${here}${query({ skip: data.next, path })}`}><Button>Older</Button></Link>}
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

/** What lets the lines of a diff be talked about: the conversations there are, and how to add to them. */
export type Review = {
  threads: Thread[];
  /** Null for someone who may not comment. */
  start: ((at: { path: string; side: "old" | "new"; line: number }, body: string) => Promise<void>) | null;
  reply: ((thread: number, body: string) => Promise<void>) | null;
  settle: ((thread: number, resolved: boolean) => Promise<void>) | null;
};

function Composer({ placeholder, send, cancel }: { placeholder: string; send: (body: string) => Promise<void>; cancel?: () => void }) {
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <div className="composer">
      <Input.TextArea value={body} onChange={(e) => setBody(e.target.value)} autoSize={{ minRows: 2, maxRows: 10 }} placeholder={placeholder} aria-label={placeholder} />
      <Space>
        <Button
          size="small"
          type="primary"
          loading={busy}
          disabled={!body.trim()}
          onClick={async () => {
            setBusy(true);
            await send(body);
            setBusy(false);
            setBody("");
          }}
        >
          Comment
        </Button>
        {cancel && <Button size="small" onClick={cancel}>Cancel</Button>}
      </Space>
    </div>
  );
}

/** One conversation: what was said, a box to answer in, and the button that settles it. */
export function ThreadBox({ thread, review, where }: { thread: Thread; review: Review; where?: ReactNode }) {
  const [open, setOpen] = useState(!thread.resolved);
  return (
    <div className="thread">
      <div className="thread-head">
        <span className="muted small">
          {where}
          {thread.resolved && <Tag className="tag-gap">Settled{thread.resolvedBy ? ` by ${thread.resolvedBy}` : ""}</Tag>}
          {thread.outdated && <Tag className="tag-gap">On an earlier version of the file</Tag>}
        </span>
        <Space size={4}>
          {thread.resolved && <Button size="small" type="text" onClick={() => setOpen(!open)}>{open ? "Hide" : "Show"}</Button>}
          {review.settle && <Button size="small" type="text" onClick={() => review.settle!(thread.id, !thread.resolved)}>{thread.resolved ? "Reopen" : "Settle"}</Button>}
        </Space>
      </div>
      {open && (
        <>
          {thread.comments.map((comment) => (
            <div key={comment.id} className="thread-comment">
              <div className="small"><strong>{comment.author}</strong> <span className="muted"><When time={comment.createdAt} /></span></div>
              <div className="markdown"><Markdown remarkPlugins={[remarkGfm]}>{comment.body}</Markdown></div>
            </div>
          ))}
          {review.reply && <Composer placeholder="Reply" send={(body) => review.reply!(thread.id, body)} />}
        </>
      )}
    </div>
  );
}

function Diff({ base, file, review }: { base: string; file: FileChange; review?: Review }) {
  // The line a new conversation is being started on, as "side:line".
  const [writing, setWriting] = useState("");
  const here = (review?.threads ?? []).filter((thread) => thread.path === file.path && !thread.outdated);
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
                    // A removed line is one of the old file; any other is one of the new.
                    const at = mark === "-" ? { side: "old" as const, line: Number(left) } : { side: "new" as const, line: Number(right) };
                    const key = `${at.side}:${at.line}`;
                    const said = here.filter((thread) => thread.side === at.side && thread.line === at.line);
                    return [
                      <tr key={`${h}-${i}`} className={mark === "+" ? "add" : mark === "-" ? "del" : undefined}>
                        <td className="n">{left}</td>
                        <td className="n">{right}</td>
                        <td>
                          {review?.start && <button type="button" className="add-comment" aria-label={`Comment on line ${at.line}`} onClick={() => setWriting(key)}><PlusOutlined /></button>}
                          {mark}{line.slice(1)}
                        </td>
                      </tr>,
                      (said.length > 0 || writing === key) && (
                        <tr key={`${h}-${i}-said`} className="said">
                          <td colSpan={3}>
                            {said.map((thread) => <ThreadBox key={thread.id} thread={thread} review={review!} />)}
                            {writing === key && <Composer placeholder="Comment on this line" cancel={() => setWriting("")} send={async (body) => { await review!.start!({ path: file.path, ...at }, body); setWriting(""); }} />}
                          </td>
                        </tr>
                      ),
                    ];
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

export function FileCard({ base, root, commit, file, open, review }: { base: string; root: string; commit: string; file: FileChange; open: boolean; review?: Review }) {
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
        <Diff base={base} file={file} review={review} />
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
                Commit <span className="mono">{data.oid}</span> <Signed verification={data.verification} />
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
