// An organization's teams: the list on the organization's page, and a
// team's own page with its members and the repositories it reaches.
import { App, Button, Card, Form, Input, Popconfirm, Select, Space, Table, Tag, Typography } from "antd";
import { Link, useNavigate, useParams } from "react-router-dom";
import { api, useLoad, type TeamDetail, type TeamSummary } from "../api";
import { useSession } from "../session";
import { Loaded } from "../util";

type Providers = { id: string; name: string }[];

/** The two fields that tie a team to a group at a sign-in provider. */
function Following({ providers }: { providers: Providers }) {
  if (providers.length === 0) return null;
  return (
    <>
      <Form.Item name="ssoProvider" label="Follow a group at">
        <Select allowClear placeholder="Nowhere: members are added by hand" options={providers.map(({ id, name }) => ({ value: id, label: name }))} />
      </Form.Item>
      <Form.Item
        name="ssoGroup"
        label="Group"
        extra="As the provider names it when someone signs in: an Entra ID group's object id, a name from the token's groups claim, or organization/team on GitHub. Whoever signs in as part of it joins the team; whoever signs in without it leaves."
      >
        <Input className="mono" />
      </Form.Item>
    </>
  );
}

const followed = (providers: Providers, team: { ssoProvider: string | null; ssoGroup: string | null }) =>
  team.ssoProvider && <Tag>follows {team.ssoGroup} at {providers.find((provider) => provider.id === team.ssoProvider)?.name ?? team.ssoProvider}</Tag>;

export function Teams({ org, canChange }: { org: string; canChange: boolean }) {
  const { message } = App.useApp();
  const teams = useLoad(() => api<{ teams: TeamSummary[]; providers: Providers }>(`/owners/${org}/teams`), `teams:${org}`);
  const [form] = Form.useForm();
  return (
    <Card title="Teams" className="stack">
      <Loaded from={teams}>
        {(data) => (
          <>
            <Table
              rowKey="name"
              size="small"
              pagination={false}
              dataSource={data.teams}
              locale={{ emptyText: "No teams" }}
              columns={[
                { title: "Team", dataIndex: "name", render: (name: string, team) => <><Link to={`/orgs/${org}/teams/${name}`}>{name}</Link> {followed(data.providers, team)}<div className="muted small">{team.description}</div></> },
                { title: "Members", dataIndex: "members", align: "right" },
                { title: "Repositories", dataIndex: "repos", align: "right" },
              ]}
            />
            {canChange && (
              <Form
                form={form}
                layout="vertical"
                className="stack"
                requiredMark={false}
                onFinish={(values) => api(`/owners/${org}/teams`, "POST", values).then(() => { form.resetFields(); teams.reload(); }, (e) => message.error((e as Error).message))}
              >
                <Form.Item name="name" label="New team" rules={[{ required: true, whitespace: true, message: "Enter a name" }]}>
                  <Input placeholder="backend" />
                </Form.Item>
                <Form.Item name="description" label="Description">
                  <Input />
                </Form.Item>
                <Following providers={data.providers} />
                <Button htmlType="submit">Create team</Button>
              </Form>
            )}
          </>
        )}
      </Loaded>
    </Card>
  );
}

const LEVELS = [{ value: "read", label: "Read" }, { value: "write", label: "Read and push" }, { value: "admin", label: "Read, push and change settings" }];

export function TeamPage() {
  const { org = "", team = "" } = useParams();
  const { me } = useSession();
  const { message } = App.useApp();
  const navigate = useNavigate();
  const path = `/owners/${org}/teams/${team}`;
  const page = useLoad(() => api<TeamDetail>(path), `team:${path}:${me?.user?.name}`);
  const [memberForm] = Form.useForm();
  const [repoForm] = Form.useForm();
  const change = (what: Promise<unknown>) => what.then(page.reload, (e) => message.error((e as Error).message));

  return (
    <Loaded from={page}>
      {(data) => (
        <div className="medium wide">
          <div className="owner-head">
            <Typography.Title level={2}>{data.name}</Typography.Title>
            <Space>
              <span className="muted">A team of <Link to={`/${data.org}`}>{data.org}</Link></span>
              {followed(data.providers, data)}
            </Space>
            {data.description && <Typography.Paragraph className="stack">{data.description}</Typography.Paragraph>}
          </div>

          <Card title="Members">
            <Table
              rowKey={(name) => name}
              size="small"
              pagination={false}
              dataSource={data.members}
              locale={{ emptyText: data.ssoGroup ? "Nobody of that group has signed in yet" : "No members" }}
              columns={[
                { title: "User", render: (_, name) => <Link to={`/${name}`}>{name}</Link> },
                {
                  key: "remove",
                  align: "right",
                  render: (_, name) => data.canChange && <Button danger size="small" onClick={() => change(api(`${path}/members/${name}`, "DELETE"))}>Remove</Button>,
                },
              ]}
            />
            {data.canChange && (
              <Form form={memberForm} layout="inline" className="stack" onFinish={({ username }: { username: string }) => change(api(`${path}/members/${username.trim()}`, "PUT").then(() => memberForm.resetFields()))}>
                <Form.Item name="username" rules={[{ required: true, message: "Enter a user name" }]}>
                  <Input placeholder="User name" aria-label="User name to add" />
                </Form.Item>
                <Button htmlType="submit">Add member</Button>
              </Form>
            )}
            {data.ssoGroup && <Typography.Paragraph className="muted stack">This team follows a group: someone added or removed by hand is put back in line the next time they sign in.</Typography.Paragraph>}
          </Card>

          <Card title="Repositories" className="stack">
            <Table
              rowKey="name"
              size="small"
              pagination={false}
              dataSource={data.repos}
              locale={{ emptyText: "This team has not been given any repository" }}
              columns={[
                { title: "Repository", dataIndex: "name", render: (name: string) => <Link to={`/${data.org}/${name}`}>{name}</Link> },
                {
                  title: "Members may",
                  dataIndex: "level",
                  render: (level: string, repo) =>
                    data.canChange ? (
                      <Select size="small" value={level} options={LEVELS} popupMatchSelectWidth={false} aria-label={`What the team may do with ${repo.name}`} onChange={(next) => change(api(`${path}/repos/${repo.name}`, "PUT", { level: next }))} />
                    ) : LEVELS.find((known) => known.value === level)?.label,
                },
                { key: "remove", align: "right", render: (_, repo) => data.canChange && <Button danger size="small" onClick={() => change(api(`${path}/repos/${repo.name}`, "DELETE"))}>Remove</Button> },
              ]}
            />
            {data.canChange && (
              <Form form={repoForm} layout="inline" className="stack" initialValues={{ level: "write" }} onFinish={({ repo, level }: { repo: string; level: string }) => change(api(`${path}/repos/${repo}`, "PUT", { level }).then(() => repoForm.resetFields()))}>
                <Form.Item name="repo" rules={[{ required: true, message: "Choose a repository" }]}>
                  <Select showSearch placeholder="Repository" aria-label="Repository to add" style={{ minWidth: 200 }} options={data.orgRepos.filter((name) => !data.repos.some((repo) => repo.name === name)).map((name) => ({ value: name, label: name }))} />
                </Form.Item>
                <Form.Item name="level">
                  <Select options={LEVELS} popupMatchSelectWidth={false} aria-label="What the team may do with it" />
                </Form.Item>
                <Button htmlType="submit">Add repository</Button>
              </Form>
            )}
          </Card>

          {data.canChange && (
            <Card title="Settings" className="stack">
              <Form layout="vertical" initialValues={data} onFinish={(values) => change(api(path, "PATCH", { description: values.description ?? "", ssoProvider: values.ssoProvider ?? "", ssoGroup: values.ssoGroup ?? "" }))}>
                <Form.Item name="description" label="Description">
                  <Input />
                </Form.Item>
                <Following providers={data.providers} />
                <Space>
                  <Button type="primary" htmlType="submit">Save</Button>
                  <Popconfirm title={`Delete the team ${data.name}? Its members lose what it gave them.`} okText="Delete" onConfirm={() => api(path, "DELETE").then(() => navigate(`/${data.org}`), (e) => message.error((e as Error).message))}>
                    <Button danger>Delete team</Button>
                  </Popconfirm>
                </Space>
              </Form>
            </Card>
          )}
        </div>
      )}
    </Loaded>
  );
}
