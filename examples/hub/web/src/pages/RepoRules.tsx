// The parts of a repository's settings that are lists of their own: branch
// rules, webhooks, and who reaches the repository through teams.
import { App, Button, Card, Checkbox, Form, Input, InputNumber, Modal, Popconfirm, Select, Space, Switch, Table, Tag, Typography } from "antd";
import { useState } from "react";
import { Link } from "react-router-dom";
import { api, useLoad, type Rule, type Webhook } from "../api";
import { Loaded, When } from "../util";
import type { RepoContext } from "./Repo";

const asks = (rule: Rule) =>
  [
    rule.noForce && "no force pushes",
    rule.noDelete && "no deletion",
    rule.requirePulls && "pull requests only",
    rule.approvals > 0 && `${rule.approvals} approval${rule.approvals === 1 ? "" : "s"}`,
    ...rule.checks.map((check) => `check: ${check}`),
    rule.requireSigned && "signed commits",
  ].filter(Boolean) as string[];

export function BranchRules({ repo, base, reload }: RepoContext) {
  const { message } = App.useApp();
  const rules = useLoad(() => api<{ rules: Rule[] }>(`${base}/rules`), `rules:${base}`);
  const [form] = Form.useForm();
  const [editing, setEditing] = useState(false);

  const done = () => {
    rules.reload();
    reload();
  };
  const edit = (rule?: Rule) => {
    form.resetFields();
    form.setFieldsValue(rule ?? { pattern: repo.empty ? "" : repo.defaultBranch, noForce: true, noDelete: true, requirePulls: false, approvals: 0, checks: [], requireSigned: false });
    setEditing(true);
  };
  const save = async () => {
    try {
      await api(`${base}/rules`, "POST", await form.validateFields());
      setEditing(false);
      done();
    } catch (e) {
      if (e instanceof Error) message.error(e.message);
    }
  };

  return (
    <Card title="Branch rules" className="stack" extra={<Button onClick={() => edit()}>Add rule</Button>}>
      <Typography.Paragraph className="muted">
        What a branch asks of the changes that reach it. A rule is for one branch, or for all whose names fit a pattern such as <code>release/*</code>; a branch named by a rule goes by that one.
      </Typography.Paragraph>
      <Loaded from={rules}>
        {(data) => (
          <Table
            rowKey="id"
            size="small"
            pagination={false}
            dataSource={data.rules}
            locale={{ emptyText: "No rules: every branch takes any push from those who can push" }}
            columns={[
              { title: "Branches", dataIndex: "pattern", render: (pattern: string) => <code className="branch">{pattern}</code> },
              { title: "Asks for", key: "asks", render: (_, rule) => <Space size={[0, 4]} wrap>{asks(rule).map((what) => <Tag key={what}>{what}</Tag>)}</Space> },
              {
                key: "actions",
                align: "right",
                render: (_, rule) => (
                  <Space>
                    <Button size="small" onClick={() => edit(rule)}>Edit</Button>
                    <Popconfirm title={`Remove the rule for ${rule.pattern}?`} okText="Remove" onConfirm={() => api(`${base}/rules/${rule.id}`, "DELETE").then(done, (e) => message.error((e as Error).message))}>
                      <Button danger size="small">Remove</Button>
                    </Popconfirm>
                  </Space>
                ),
              },
            ]}
          />
        )}
      </Loaded>
      <Modal open={editing} title="Branch rule" okText="Save" onOk={save} onCancel={() => setEditing(false)} forceRender>
        <Form form={form} layout="vertical" requiredMark={false}>
          <Form.Item name="pattern" label="Branches" extra="A branch's name, with * for any part of it. Saving a pattern that has a rule changes that rule." rules={[{ required: true, whitespace: true, message: "Name a branch" }]}>
            <Input className="mono" placeholder="main" />
          </Form.Item>
          <Form.Item name="noForce" label="Refuse force pushes" valuePropName="checked">
            <Switch />
          </Form.Item>
          <Form.Item name="noDelete" label="Refuse deletion" valuePropName="checked">
            <Switch />
          </Form.Item>
          <Form.Item name="requirePulls" label="Require pull requests" valuePropName="checked" extra="Refuses pushes straight to the branch. Asking for approvals or checks below asks for this too.">
            <Switch />
          </Form.Item>
          <Form.Item name="approvals" label="Approvals needed to merge" extra="Approving reviews of the pull request's latest commit, by people other than its author who can push here.">
            <InputNumber min={0} max={10} />
          </Form.Item>
          <Form.Item name="checks" label="Checks that must pass" extra="Names under which commit statuses are reported, such as ci/build. Each must have succeeded on the pull request's latest commit.">
            <Select mode="tags" open={false} suffixIcon={null} placeholder="Type a name and press Enter" tokenSeparators={[",", " "]} />
          </Form.Item>
          <Form.Item name="requireSigned" label="Require signed commits" valuePropName="checked" extra="Refuses pushes that bring a commit not signed with a key registered here. Commits made by merging a pull request here are not signed, and are let in.">
            <Switch />
          </Form.Item>
        </Form>
      </Modal>
    </Card>
  );
}

const EVENT_LABELS: Record<string, string> = { push: "Pushes", pull_request: "Pull requests", pull_request_review: "Reviews", status: "Commit statuses" };
type Delivery = { id: number; event: string; state: "pending" | "delivered" | "failed"; attempts: number; status: number | null; error: string | null; createdAt: number };
const DELIVERY = { delivered: "green", failed: "red", pending: "gold" } as const;

function Deliveries({ base, hook, onClose }: { base: string; hook: Webhook; onClose: () => void }) {
  const { message } = App.useApp();
  const list = useLoad(() => api<{ deliveries: Delivery[] }>(`${base}/hooks/${hook.id}/deliveries`), `deliveries:${base}:${hook.id}`);
  const again = (path: string) => api(path, "POST").then(() => setTimeout(list.reload, 800), (e) => message.error((e as Error).message));
  return (
    <Modal open title={`Deliveries to ${hook.url}`} footer={<Space><Button onClick={() => again(`${base}/hooks/${hook.id}/ping`)}>Send a ping</Button><Button onClick={list.reload}>Refresh</Button><Button type="primary" onClick={onClose}>Close</Button></Space>} onCancel={onClose} width={720}>
      <Loaded from={list}>
        {(data) => (
          <Table
            rowKey="id"
            size="small"
            pagination={false}
            dataSource={data.deliveries}
            locale={{ emptyText: "Nothing sent yet" }}
            columns={[
              { title: "Event", dataIndex: "event" },
              { title: "Result", key: "state", render: (_, row) => <><Tag color={DELIVERY[row.state]}>{row.state}</Tag>{row.error ?? (row.status ? `answered ${row.status}` : "")}{row.attempts > 1 && <span className="muted"> · {row.attempts} tries</span>}</> },
              { title: "When", dataIndex: "createdAt", render: (time: number) => <When time={time} /> },
              { key: "again", align: "right", render: (_, row) => <Button size="small" onClick={() => again(`${base}/hooks/${hook.id}/deliveries/${row.id}`)}>Send again</Button> },
            ]}
          />
        )}
      </Loaded>
    </Modal>
  );
}

export function Webhooks({ base }: RepoContext) {
  const { message } = App.useApp();
  const hooks = useLoad(() => api<{ hooks: Webhook[]; events: string[] }>(`${base}/hooks`), `hooks:${base}`);
  const [form] = Form.useForm();
  const [shown, setShown] = useState<Webhook | null>(null);
  const change = (what: Promise<unknown>) => what.then(hooks.reload, (e) => message.error((e as Error).message));

  return (
    <Card title="Webhooks" className="stack">
      <Typography.Paragraph className="muted">
        Addresses that are sent a POST with a JSON body when something happens here. With a secret, each carries <code>X-Hub-Signature-256</code>: the body's HMAC-SHA256 under that secret. A delivery that fails is tried again, five times over half an hour.
      </Typography.Paragraph>
      <Loaded from={hooks}>
        {(data) => (
          <>
            <Table
              rowKey="id"
              size="small"
              pagination={false}
              dataSource={data.hooks}
              locale={{ emptyText: "No webhooks" }}
              columns={[
                { title: "Address", dataIndex: "url", render: (url: string, hook) => <><span className="mono small">{url}</span>{hook.hasSecret && <Tag className="tag-gap">signed</Tag>}</> },
                { title: "Told of", dataIndex: "events", render: (events: string[]) => events.map((event) => EVENT_LABELS[event] ?? event).join(", ") },
                { title: "Last", key: "last", render: (_, hook) => (hook.last ? <Tag color={DELIVERY[hook.last.state]}>{hook.last.state}</Tag> : <span className="muted">never</span>) },
                { title: "On", dataIndex: "active", render: (active: boolean, hook) => <Switch size="small" checked={active} aria-label={`Deliver to ${hook.url}`} onChange={(next) => change(api(`${base}/hooks/${hook.id}`, "PATCH", { active: next }))} /> },
                {
                  key: "actions",
                  align: "right",
                  render: (_, hook) => (
                    <Space>
                      <Button size="small" onClick={() => setShown(hook)}>Deliveries</Button>
                      <Popconfirm title="Remove this webhook?" okText="Remove" onConfirm={() => change(api(`${base}/hooks/${hook.id}`, "DELETE"))}>
                        <Button danger size="small">Remove</Button>
                      </Popconfirm>
                    </Space>
                  ),
                },
              ]}
            />
            <Form form={form} layout="vertical" className="stack" requiredMark={false} initialValues={{ events: ["push"] }} onFinish={(values) => change(api(`${base}/hooks`, "POST", values).then(() => form.resetFields()))}>
              <Form.Item name="url" label="Address" rules={[{ required: true, whitespace: true, message: "Enter an address" }]}>
                <Input placeholder="https://ci.example.com/hooks/strata" />
              </Form.Item>
              <Form.Item name="secret" label="Secret" extra="Optional. It is not shown again.">
                <Input.Password autoComplete="new-password" />
              </Form.Item>
              <Form.Item name="events" label="Tell it of">
                <Checkbox.Group options={data.events.map((event) => ({ value: event, label: EVENT_LABELS[event] ?? event }))} />
              </Form.Item>
              <Button htmlType="submit">Add webhook</Button>
            </Form>
          </>
        )}
      </Loaded>
      {shown && <Deliveries base={base} hook={shown} onClose={() => { setShown(null); hooks.reload(); }} />}
    </Card>
  );
}

const BASE_WORDS = { none: "nothing beyond what anyone may", read: "read it", write: "read it and push to it" } as const;

/** Who reaches an organization's repository besides its owners. Changed on the organization's and its teams' pages. */
export function Access({ repo }: RepoContext) {
  if (!repo.access) return null;
  return (
    <Card title="Access" className="stack">
      <Typography.Paragraph>
        Every member of <Link to={`/${repo.owner}`}>{repo.owner}</Link> may {BASE_WORDS[repo.access.base]}; its owners may do anything. Teams add to that:
      </Typography.Paragraph>
      <Table
        rowKey="name"
        size="small"
        pagination={false}
        dataSource={repo.access.teams}
        locale={{ emptyText: "No team has been given this repository" }}
        columns={[
          { title: "Team", dataIndex: "name", render: (name: string) => <Link to={`/orgs/${repo.owner}/teams/${name}`}>{name}</Link> },
          { title: "May", dataIndex: "level", render: (level: string) => ({ read: "Read", write: "Read and push", admin: "Read, push and change settings" })[level] },
        ]}
      />
    </Card>
  );
}
