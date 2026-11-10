import { Alert, App, Button, Card, Form, Input, Popconfirm, Table, Typography } from "antd";
import { useState } from "react";
import { Navigate } from "react-router-dom";
import { api, useLoad } from "../api";
import { useSession } from "../session";
import { Loaded, When } from "../util";

type Token = { id: number; name: string; createdAt: number; lastUsedAt: number | null };

export function Tokens() {
  const { me } = useSession();
  const { message } = App.useApp();
  const tokens = useLoad(() => api<{ tokens: Token[] }>("/tokens"), "tokens");
  const [made, setMade] = useState("");
  const [form] = Form.useForm();
  if (!me) return null;
  if (!me.user) return <Navigate to="/login" />;

  const create = async ({ name }: { name: string }) => {
    try {
      setMade((await api<{ token: string }>("/tokens", "POST", { name })).token);
      form.resetFields();
      tokens.reload();
    } catch (e) {
      message.error((e as Error).message);
    }
  };
  const revoke = async (id: number) => {
    await api(`/tokens/${id}`, "DELETE");
    tokens.reload();
  };

  return (
    <Card title="Access tokens" className="medium">
      <Typography.Paragraph className="muted">
        Use a token instead of your password when git asks for one. A token can do whatever you can.
      </Typography.Paragraph>
      {made && (
        <Alert
          type="success"
          className="form-alert"
          title="Copy the token now. It is not shown again."
          description={<Typography.Text code copyable>{made}</Typography.Text>}
        />
      )}
      <Form form={form} layout="inline" onFinish={create} className="form-alert">
        <Form.Item name="name" rules={[{ required: true, message: "Name the token" }]}>
          <Input placeholder="What is it for?" aria-label="Token name" />
        </Form.Item>
        <Button type="primary" htmlType="submit">Create token</Button>
      </Form>
      <Loaded from={tokens}>
        {(data) => (
          <Table
            rowKey="id"
            size="small"
            pagination={false}
            dataSource={data.tokens}
            locale={{ emptyText: "No tokens" }}
            columns={[
              { title: "Name", dataIndex: "name" },
              { title: "Created", dataIndex: "createdAt", render: (time: number) => <When time={time} /> },
              { title: "Last used", dataIndex: "lastUsedAt", render: (time: number | null) => (time ? <When time={time} /> : "never") },
              {
                key: "revoke",
                align: "right",
                render: (_, token) => (
                  <Popconfirm title="Revoke this token?" okText="Revoke" onConfirm={() => revoke(token.id)}>
                    <Button danger size="small">Revoke</Button>
                  </Popconfirm>
                ),
              },
            ]}
          />
        )}
      </Loaded>
    </Card>
  );
}
