import { App, Button, Card, Form, Input, Popconfirm, Select, Space, Table, Tag, Typography } from "antd";
import { Link, useParams } from "react-router-dom";
import { api, useLoad, type OwnerPage as Data } from "../api";
import { RepoList } from "../RepoList";
import { useSession } from "../session";
import { Loaded } from "../util";
import { Teams } from "./Team";

export function OwnerPage() {
  const { owner = "" } = useParams();
  const { me } = useSession();
  const { message } = App.useApp();
  const page = useLoad(() => api<Data>(`/owners/${owner}`), `owner:${owner}:${me?.user?.name}`);
  const [form] = Form.useForm();

  const change = async (action: () => Promise<unknown>) => {
    try {
      await action();
      page.reload();
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  return (
    <Loaded from={page}>
      {(data) => {
        const isOwner = data.role === "owner";
        const mayCreate = data.kind === "user" ? me?.user?.name.toLowerCase() === data.name.toLowerCase() : !!data.role;
        return (
          <>
            <div className="owner-head">
              <Typography.Title level={2}>{data.displayName || data.name}</Typography.Title>
              <Space>
                <span className="muted">{data.name}</span>
                <Tag>{data.kind === "org" ? "Organization" : "User"}</Tag>
                {data.orgs?.map((org) => <Link key={org} to={`/${org}`}>{org}</Link>)}
              </Space>
            </div>
            <Card title="Repositories" extra={mayCreate && <Link to={`/new?owner=${data.name}`}><Button type="primary">New</Button></Link>}>
              <RepoList repos={data.repos} empty="No repositories to show" />
            </Card>
            {data.members && (
              <Card title="Members" className="stack">
                <Table
                  rowKey="name"
                  size="small"
                  pagination={false}
                  dataSource={data.members}
                  columns={[
                    { title: "User", dataIndex: "name", render: (name: string) => <Link to={`/${name}`}>{name}</Link> },
                    {
                      title: "Role",
                      dataIndex: "role",
                      render: (role: string, member) =>
                        isOwner ? (
                          <Select
                            size="small"
                            value={role}
                            aria-label={`Role of ${member.name}`}
                            options={[{ value: "owner", label: "Owner" }, { value: "member", label: "Member" }]}
                            onChange={(next) => change(() => api(`/owners/${data.name}/members/${member.name}`, "PUT", { role: next }))}
                          />
                        ) : role === "owner" ? "Owner" : "Member",
                    },
                    {
                      key: "remove",
                      align: "right",
                      render: (_, member) =>
                        isOwner && (
                          <Popconfirm title={`Remove ${member.name}?`} okText="Remove" onConfirm={() => change(() => api(`/owners/${data.name}/members/${member.name}`, "DELETE"))}>
                            <Button danger size="small">Remove</Button>
                          </Popconfirm>
                        ),
                    },
                  ]}
                />
                {isOwner && (
                  <Form
                    form={form}
                    layout="inline"
                    className="stack"
                    onFinish={({ username }: { username: string }) =>
                      change(async () => {
                        await api(`/owners/${data.name}/members/${username.trim()}`, "PUT", { role: "member" });
                        form.resetFields();
                      })
                    }
                  >
                    <Form.Item name="username" rules={[{ required: true, message: "Enter a user name" }]}>
                      <Input placeholder="User name" aria-label="User name to add" />
                    </Form.Item>
                    <Button htmlType="submit">Add member</Button>
                  </Form>
                )}
                <Space className="stack" wrap>
                  <span>Every member may</span>
                  <Select
                    size="small"
                    value={data.basePermission}
                    disabled={!isOwner}
                    popupMatchSelectWidth={false}
                    aria-label="What every member may do with the organization's repositories"
                    options={[{ value: "write", label: "read and push to every repository" }, { value: "read", label: "read every repository" }, { value: "none", label: "do nothing beyond what anyone may" }]}
                    onChange={(basePermission) => change(() => api(`/owners/${data.name}`, "PATCH", { basePermission }))}
                  />
                </Space>
                <Typography.Paragraph className="muted stack">Teams add to that for the repositories they are given. Owners can do anything, and change settings, members and teams.</Typography.Paragraph>
              </Card>
            )}
            {data.kind === "org" && data.role && <Teams org={data.name} canChange={isOwner} />}
          </>
        );
      }}
    </Loaded>
  );
}
