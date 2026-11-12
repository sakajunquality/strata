import { CheckCircleFilled, ClockCircleFilled, CloseCircleFilled, CommentOutlined, PullRequestOutlined } from "@ant-design/icons";
import { Alert, App, Button, Card, Empty, Form, Input, Segmented, Select, Space, Tabs, Tag, Typography } from "antd";
import { useState, type ReactNode } from "react";
import Markdown from "react-markdown";
import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom";
import remarkGfm from "remark-gfm";
import { api, query, useLoad, type Compare, type Pull, type PullDetail } from "../api";
import { useSession } from "../session";
import { Loaded, shortOid, subject, When } from "../util";
import { CommitRows, FileCard, ThreadBox, type Review } from "./History";
import type { RepoContext } from "./Repo";

const STATES = { open: ["green", "Open"], merged: ["purple", "Merged"], closed: ["red", "Closed"] } as const;

function StateTag({ state }: { state: Pull["state"] }) {
  const [color, label] = STATES[state];
  return <Tag color={color} icon={<PullRequestOutlined />}>{label}</Tag>;
}

const Branch = ({ name }: { name: string }) => <code className="branch">{name}</code>;

const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;

export function PullsPage({ repo, root, base }: RepoContext) {
  const [params, setParams] = useSearchParams();
  const state = params.get("state") === "closed" ? "closed" : "open";
  const list = useLoad(() => api<{ pulls: Pull[]; open: number; closed: number }>(`${base}/pulls?state=${state}`), `pulls:${base}:${state}`);
  return (
    <Loaded from={list}>
      {(data) => (
        <>
          <div className="toolbar spread">
            <Segmented
              value={state}
              onChange={(next) => setParams(next === "open" ? {} : { state: next })}
              options={[{ value: "open", label: `${data.open} Open` }, { value: "closed", label: `${data.closed} Closed` }]}
            />
            {repo.branches.length > 1 && <Link to={`${root}/pulls/new`}><Button type="primary">New pull request</Button></Link>}
          </div>
          {data.pulls.length === 0 ? (
            <Empty
              image={Empty.PRESENTED_IMAGE_SIMPLE}
              description={state === "open" ? "No open pull requests. Push a branch and propose merging it." : "No closed pull requests"}
            />
          ) : (
            <ul className="rows boxed">
              {data.pulls.map((pull) => (
                <li key={pull.number} className="commit-row">
                  <div className="ellipsis">
                    <StateTag state={pull.state} />
                    <Link to={`${root}/pulls/${pull.number}`} className="plain"><strong>{pull.title}</strong></Link>
                    <div className="muted small">
                      #{pull.number} opened <When time={pull.createdAt} /> by {pull.author} · <Branch name={pull.head} /> into <Branch name={pull.base} />
                    </div>
                  </div>
                  {pull.comments > 0 && <span className="muted"><CommentOutlined /> {pull.comments}</span>}
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </Loaded>
  );
}

/** The commits and files of a comparison. */
function Changes({ compare, root, base, only, review }: { compare: Compare; root: string; base: string; only?: "commits" | "files"; review?: Review }) {
  return (
    <>
      {only !== "files" && (
        <>
          <CommitRows root={root} commits={compare.commits} />
          {compare.ahead > compare.commits.length && <p className="muted small">Showing the newest {compare.commits.length} of {compare.ahead} commits.</p>}
        </>
      )}
      {only !== "commits" && (
        <>
          {!only && <Typography.Paragraph className="stack muted">{plural(compare.files.length, "changed file")}</Typography.Paragraph>}
          {compare.truncated && <Alert type="warning" showIcon title="Too many files changed to list them all." />}
          {compare.files.map((file, i) => <FileCard key={file.path} base={base} root={root} commit={compare.head} file={file} open={i < 20} review={review} />)}
        </>
      )}
    </>
  );
}

export function NewPullPage({ repo, root, base, reload }: RepoContext) {
  const { me } = useSession();
  const { message } = App.useApp();
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const names = repo.branches.map((branch) => branch.name);
  const into = params.get("base") ?? repo.defaultBranch;
  const from = params.get("head") ?? "";
  const ready = names.includes(into) && names.includes(from) && into !== from;
  const compared = useLoad(
    () => (ready ? api<Compare & { existing: number | null }>(`${base}/compare${query({ base: into, head: from })}`) : Promise.resolve(null)),
    `compare:${base}:${into}:${from}:${ready}`,
  );
  const choose = (next: { base?: string; head?: string }) => setParams({ base: into, head: from, ...next });
  const options = names.map((name) => ({ value: name, label: name }));

  const create = async (values: { title: string; body?: string }) => {
    try {
      const made = await api<{ number: number }>(`${base}/pulls`, "POST", { ...values, base: into, head: from });
      reload();
      navigate(`${root}/pulls/${made.number}`);
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  return (
    <>
      <Typography.Title level={4}>New pull request</Typography.Title>
      <div className="toolbar">
        <span>Merge</span>
        <Select value={from || undefined} placeholder="Choose a branch" aria-label="Branch to merge" showSearch popupMatchSelectWidth={false} className="rev-select" options={options} onChange={(head) => choose({ head })} />
        <span>into</span>
        <Select value={into} aria-label="Branch to merge into" showSearch popupMatchSelectWidth={false} className="rev-select" options={options} onChange={(next) => choose({ base: next })} />
      </div>
      {!ready && <Alert type="info" showIcon title={into === from ? "Choose two different branches." : "Choose the branch that holds your changes."} />}
      <Loaded from={compared}>
        {(data) => {
          if (!data) return null;
          if (data.existing) {
            return <Alert type="info" showIcon title={<>There is an open pull request for these branches already: <Link to={`${root}/pulls/${data.existing}`}>#{data.existing}</Link></>} />;
          }
          if (data.ahead === 0) return <Alert type="info" showIcon title={`${from} has nothing that ${into} lacks.`} />;
          const only = data.commits.length === 1 ? data.commits[0] : undefined;
          return (
            <>
              <Card className="medium" key={`${into}:${from}`}>
                {me?.user ? (
                  <Form
                    layout="vertical"
                    requiredMark={false}
                    onFinish={create}
                    initialValues={{ title: only ? subject(only.message) : from, body: only ? only.message.split("\n").slice(1).join("\n").trim() : "" }}
                  >
                    <Form.Item name="title" label="Title" rules={[{ required: true, whitespace: true, message: "Enter a title" }]}>
                      <Input maxLength={200} />
                    </Form.Item>
                    <Form.Item name="body" label="Description" extra="Markdown is shown formatted.">
                      <Input.TextArea rows={5} />
                    </Form.Item>
                    <Button type="primary" htmlType="submit">Create pull request</Button>
                  </Form>
                ) : (
                  <span><Link to="/login">Sign in</Link> to open a pull request.</span>
                )}
              </Card>
              <Typography.Paragraph className="stack muted">
                {plural(data.ahead, "commit")}
                {data.behind > 0 && <>; {into} has {plural(data.behind, "commit")} that {from} lacks</>}
              </Typography.Paragraph>
              <Changes compare={data} root={root} base={base} />
            </>
          );
        }}
      </Loaded>
    </>
  );
}

function Note({ author, time, body, did = "commented", tag }: { author: string; time: number; body: string; did?: string; tag?: ReactNode }) {
  return (
    <Card size="small" className="stack" title={<span className="note-head"><strong>{author}</strong> <span className="muted">{did} <When time={time} /></span> {tag}</span>}>
      {body ? <div className="markdown"><Markdown remarkPlugins={[remarkGfm]}>{body}</Markdown></div> : <span className="muted">{did === "opened this" ? "No description." : "Nothing more to say."}</span>}
    </Card>
  );
}

const CHECK_ICONS = {
  success: <CheckCircleFilled className="check-success" aria-label="passed" />,
  pending: <ClockCircleFilled className="check-pending" aria-label="running" />,
  failure: <CloseCircleFilled className="check-failure" aria-label="failed" />,
  error: <CloseCircleFilled className="check-failure" aria-label="could not run" />,
};

/** What reviewers and checks say about an open pull request, and what its base branch asks. */
function Standing({ pull }: { pull: PullDetail }) {
  // Checks the rule asks for that nobody has reported on yet are part of the picture.
  const awaited = (pull.rule?.checks ?? []).filter((name) => !pull.checks.some((check) => check.context === name));
  if (pull.reviewers.length === 0 && pull.checks.length === 0 && !pull.rule) return null;
  return (
    <Card size="small" className="stack" title="Reviews and checks">
      {pull.rule && (pull.rule.approvals > 0 || pull.rule.checks.length > 0) && (
        <p className="muted">
          <Branch name={pull.base} /> asks for{" "}
          {[pull.rule.approvals > 0 && plural(pull.rule.approvals, "approval"), pull.rule.checks.length > 0 && `the ${pull.rule.checks.length === 1 ? "check" : "checks"} ${pull.rule.checks.join(", ")}`].filter(Boolean).join(" and ")}.
        </p>
      )}
      <ul className="standing">
        {pull.reviewers.map((review) => (
          <li key={review.author}>
            {review.state === "approved" ? (review.stale ? CHECK_ICONS.pending : CHECK_ICONS.success) : CHECK_ICONS.failure}
            <strong>{review.author}</strong> {review.state === "approved" ? (review.stale ? "approved an earlier commit" : "approved") : "asked for changes"}
            {!review.counts && <span className="muted"> · cannot push here, so this does not count towards the rule</span>}
          </li>
        ))}
        {pull.checks.map((check) => (
          <li key={check.context}>
            {CHECK_ICONS[check.state]}
            <strong>{check.context}</strong> {check.description && <span className="muted">{check.description}</span>} {check.targetUrl && <a href={check.targetUrl} rel="noreferrer">Details</a>}
          </li>
        ))}
        {awaited.map((name) => (
          <li key={name}>{CHECK_ICONS.pending}<strong>{name}</strong> <span className="muted">has not reported</span></li>
        ))}
      </ul>
    </Card>
  );
}

const VERDICTS = { approved: ["approved", <Tag color="green" key="tag">Approved</Tag>], changes_requested: ["asked for changes", <Tag color="red" key="tag">Changes requested</Tag>], commented: ["reviewed", null] } as const;

/** Where an open pull request stands, and the button that merges it. */
function MergeBox({ pull, base, act }: { pull: PullDetail; base: string; act: (what: () => Promise<unknown>) => Promise<void> }) {
  const [method, setMethod] = useState<"merge" | "squash" | "rebase">("merge");
  const [busy, setBusy] = useState(false);
  if (!pull.compare) return <Alert className="stack" type="warning" showIcon title="One of its branches is gone, so there is nothing to merge." />;
  if (pull.compare.ahead === 0) return <Alert className="stack" type="info" showIcon title={`${pull.head} has nothing that ${pull.base} lacks.`} />;
  if (!pull.mergeable) {
    return (
      <Alert
        className="stack"
        type="warning"
        showIcon
        title="This branch has conflicts with the base"
        description={
          <>
            <ul className="conflicts">{pull.conflicts.map((path) => <li key={path} className="mono">{path}</li>)}</ul>
            Merge <Branch name={pull.base} /> into <Branch name={pull.head} /> on your machine, settle them there and push.
          </>
        }
      />
    );
  }
  if (pull.blocked.length > 0) {
    return (
      <Alert
        className="stack"
        type="warning"
        showIcon
        title="Not ready to merge"
        description={<ul className="conflicts">{pull.blocked.map((reason) => <li key={reason}>{reason}</li>)}</ul>}
      />
    );
  }
  const merge = async () => {
    setBusy(true);
    await act(() => api(`${base}/pulls/${pull.number}/merge`, "POST", { method, head: pull.compare!.head }));
    setBusy(false);
  };
  return (
    <Alert
      className="stack"
      type="success"
      showIcon
      title="No conflicts with the base"
      description={
        pull.can.merge ? (
          <Space wrap>
            <Select
              value={method}
              onChange={setMethod}
              aria-label="How to merge"
              popupMatchSelectWidth={false}
              options={[{ value: "merge", label: "Create a merge commit" }, { value: "squash", label: "Squash into one commit" }, { value: "rebase", label: "Rebase onto the base" }]}
            />
            <Button type="primary" loading={busy} onClick={merge}>Merge pull request</Button>
          </Space>
        ) : (
          "Those who can push to this repository can merge it."
        )
      }
    />
  );
}

export function PullPage({ root, base, reload }: RepoContext) {
  const { number = "" } = useParams();
  const { me } = useSession();
  const { message } = App.useApp();
  const [form] = Form.useForm<{ body: string }>();
  const detail = useLoad(() => api<PullDetail>(`${base}/pulls/${number}`), `pull:${base}:${number}:${me?.user?.name}`);

  // Does something to this pull request, then shows how things stand.
  const act = async (what: () => Promise<unknown>) => {
    try {
      await what();
    } catch (e) {
      message.error((e as Error).message);
    }
    detail.reload();
    reload();
  };
  const change = (body: unknown) => act(() => api(`${base}/pulls/${number}`, "PATCH", body));

  return (
    <Loaded from={detail}>
      {(pull) => {
        const open = pull.state === "open";
        // Comments and reviews, in the order they were made.
        const said = [
          ...pull.discussion.map((comment) => ({ key: `c${comment.id}`, time: comment.createdAt, note: <Note key={`c${comment.id}`} author={comment.author} time={comment.createdAt} body={comment.body} /> })),
          ...pull.reviews.map((review) => ({
            key: `r${review.id}`,
            time: review.createdAt,
            note: <Note key={`r${review.id}`} author={review.author} time={review.createdAt} body={review.body} did={VERDICTS[review.state][0]} tag={<>{VERDICTS[review.state][1]}{review.stale && review.state !== "commented" && <Tag>earlier commit</Tag>}</>} />,
          })),
        ].sort((a, b) => a.time - b.time || a.key.localeCompare(b.key));
        const judge = (state: string) => act(async () => {
          await api(`${base}/pulls/${number}/reviews`, "POST", { state, body: form.getFieldValue("body") ?? "", head: pull.compare?.head });
          form.resetFields();
        });
        // Conversations on lines: started and answered by anyone signed in, while the pull request is open.
        const threads = `${base}/pulls/${number}/threads`;
        const review: Review = {
          threads: pull.threads,
          start: pull.can.review ? (at, body) => act(() => api(threads, "POST", { ...at, body })) : null,
          reply: me?.user ? (thread, body) => act(() => api(`${threads}/${thread}/comments`, "POST", { body })) : null,
          settle: me?.user ? (thread, resolved) => act(() => api(`${threads}/${thread}`, "PATCH", { resolved })) : null,
        };
        const unsettled = pull.threads.filter((thread) => !thread.resolved).length;
        const conversation = (
          <div className="medium wide">
            <Note author={pull.author} time={pull.createdAt} body={pull.body} did="opened this" />
            {said.map((entry) => entry.note)}
            {pull.threads.length > 0 && (
              <Card size="small" className="stack" title={`Comments on lines (${unsettled} open, ${pull.threads.length - unsettled} settled)`}>
                {pull.threads.map((thread) => (
                  <ThreadBox key={thread.id} thread={thread} review={review} where={<span className="mono">{thread.path}:{thread.line}</span>} />
                ))}
              </Card>
            )}
            {open && <Standing pull={pull} />}
            {open && <MergeBox pull={pull} base={base} act={act} />}
            {pull.state === "merged" && (
              <Alert
                className="stack"
                type="success"
                showIcon
                title={<>Merged <When time={pull.closedAt} />{pull.closedBy ? <> by {pull.closedBy}</> : " by a push"}{pull.mergeOid && <> as <Link to={`${root}/commit/${pull.mergeOid}`} className="mono">{shortOid(pull.mergeOid)}</Link></>}</>}
              />
            )}
            {pull.state === "closed" && <Alert className="stack" type="error" showIcon title={<>Closed <When time={pull.closedAt} />{pull.closedBy ? <> by {pull.closedBy}</> : ": a branch was deleted"}</>} />}
            {me?.user ? (
              <Form form={form} className="stack" onFinish={({ body }) => act(() => api(`${base}/pulls/${number}/comments`, "POST", { body })).then(() => form.resetFields())}>
                <Form.Item name="body" rules={[{ required: true, whitespace: true, message: "Write something first" }]}>
                  <Input.TextArea rows={4} placeholder="Leave a comment" aria-label="Comment" />
                </Form.Item>
                <Space>
                  <Button type="primary" htmlType="submit">Comment</Button>
                  {pull.can.judge && <Button onClick={() => judge("approved")}>Approve</Button>}
                  {pull.can.judge && <Button onClick={() => judge("changes_requested")}>Request changes</Button>}
                  {pull.can.change && open && <Button onClick={() => change({ state: "closed" })}>Close pull request</Button>}
                  {pull.can.change && pull.state === "closed" && <Button onClick={() => change({ state: "open" })}>Reopen</Button>}
                </Space>
              </Form>
            ) : (
              <p className="stack muted"><Link to="/login">Sign in</Link> to comment.</p>
            )}
          </div>
        );
        const gone = <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="Its commits are no longer there" />;
        return (
          <>
            <Typography.Title level={4} editable={pull.can.change ? { onChange: (title) => title.trim() && title !== pull.title && change({ title }), triggerType: ["icon"] } : false}>
              {pull.title}
            </Typography.Title>
            <div className="pull-summary">
              <StateTag state={pull.state} />
              <span className="muted">
                <strong>{pull.author}</strong> {open ? "wants to merge" : pull.state === "merged" ? "merged" : "wanted to merge"}{" "}
                {pull.compare ? plural(pull.compare.ahead, "commit") : "commits"} into <Branch name={pull.base} /> from <Branch name={pull.head} /> · #{pull.number}
              </span>
            </div>
            <Tabs
              items={[
                { key: "conversation", label: "Conversation", children: conversation },
                { key: "commits", label: `Commits${pull.compare ? ` (${pull.compare.ahead})` : ""}`, children: pull.compare ? <Changes compare={pull.compare} root={root} base={base} only="commits" /> : gone },
                { key: "files", label: `Files changed${pull.compare ? ` (${pull.compare.files.length})` : ""}`, children: pull.compare ? <Changes compare={pull.compare} root={root} base={base} only="files" review={review} /> : gone },
              ]}
            />
          </>
        );
      }}
    </Loaded>
  );
}
