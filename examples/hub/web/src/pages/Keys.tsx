import { App, Button, Card, Form, Input, Popconfirm, Table, Tag, Typography } from "antd";
import { Navigate } from "react-router-dom";
import { api, useLoad } from "../api";
import { useSession } from "../session";
import { Loaded, When } from "../util";

type Key = { id: number; kind: "ssh" | "gpg"; title: string; fingerprint: string; createdAt: number };

export function Keys() {
  const { me } = useSession();
  const { message } = App.useApp();
  const keys = useLoad(() => api<{ keys: Key[] }>("/keys"), "keys");
  const [form] = Form.useForm();
  if (!me) return null;
  if (!me.user) return <Navigate to="/login" />;

  const add = async (values: { title?: string; key: string }) => {
    try {
      await api("/keys", "POST", values);
      form.resetFields();
      keys.reload();
    } catch (e) {
      message.error((e as Error).message);
    }
  };
  const remove = async (id: number) => {
    await api(`/keys/${id}`, "DELETE");
    keys.reload();
  };

  return (
    <Card title="Signing keys" className="medium wide">
      <Typography.Paragraph className="muted">
        Commits signed with a key listed here are shown as verified, and are let into branches that take signed commits only.
        Add the public half of the key git signs with: an SSH key (<code>git config gpg.format ssh</code>) or an OpenPGP key (<code>gpg --armor --export</code>).
      </Typography.Paragraph>
      <Form form={form} layout="vertical" onFinish={add} requiredMark={false} className="form-alert">
        <Form.Item name="title" label="Name">
          <Input placeholder="Laptop" maxLength={100} />
        </Form.Item>
        <Form.Item name="key" label="Public key" rules={[{ required: true, whitespace: true, message: "Paste a public key" }]}>
          <Input.TextArea rows={4} className="mono" placeholder="ssh-ed25519 AAAA…  or  -----BEGIN PGP PUBLIC KEY BLOCK-----" />
        </Form.Item>
        <Button type="primary" htmlType="submit">Add key</Button>
      </Form>
      <Loaded from={keys}>
        {(data) => (
          <Table
            rowKey="id"
            size="small"
            pagination={false}
            dataSource={data.keys}
            locale={{ emptyText: "No keys" }}
            columns={[
              { title: "Name", dataIndex: "title", render: (title: string, key) => <>{title} <Tag className="tag-gap">{key.kind === "ssh" ? "SSH" : "OpenPGP"}</Tag></> },
              { title: "Fingerprint", dataIndex: "fingerprint", render: (fingerprint: string) => <span className="mono small">{fingerprint}</span> },
              { title: "Added", dataIndex: "createdAt", render: (time: number) => <When time={time} /> },
              {
                key: "remove",
                align: "right",
                render: (_, key) => (
                  <Popconfirm title="Remove this key? Commits signed with it stop being verified." okText="Remove" onConfirm={() => remove(key.id)}>
                    <Button danger size="small">Remove</Button>
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
