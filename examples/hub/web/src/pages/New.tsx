import { Alert, Button, Card, Checkbox, Form, Input, Radio, Select } from "antd";
import { useState } from "react";
import { Navigate, useNavigate, useSearchParams } from "react-router-dom";
import { api } from "../api";
import { useSession } from "../session";

export function NewRepo() {
  const { me } = useSession();
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const [error, setError] = useState("");
  const [form] = Form.useForm();
  const mirroring = Form.useWatch("from", form) === "mirror";
  if (!me) return null;
  if (!me.user) return <Navigate to="/login" />;

  const owners = [me.user.name, ...me.orgs.map((org) => org.name)];
  const submit = async ({ from, ...values }: { owner: string; name: string; description?: string; private: boolean; from: string; mirror?: string; mirrorPulls?: boolean; mirrorToken?: string }) => {
    setError("");
    try {
      const made = await api<{ owner: string; name: string }>("/repos", "POST", from === "mirror" ? values : { ...values, mirror: undefined });
      navigate(`/${made.owner}/${made.name}`);
    } catch (e) {
      setError((e as Error).message);
    }
  };
  return (
    <Card title="New repository" className="medium">
      {error && <Alert type="error" showIcon title={error} className="form-alert" />}
      <Form form={form} layout="vertical" onFinish={submit} requiredMark={false} initialValues={{ owner: params.get("owner") ?? me.user.name, private: false, from: "empty" }}>
        <Form.Item name="owner" label="Owner">
          <Select options={owners.map((name) => ({ value: name, label: name }))} />
        </Form.Item>
        <Form.Item name="name" label="Repository name" rules={[{ required: true, message: "Enter a name" }]}>
          <Input autoFocus />
        </Form.Item>
        <Form.Item name="description" label="Description (optional)">
          <Input />
        </Form.Item>
        <Form.Item name="private" label="Visibility">
          <Radio.Group
            options={[
              { value: false, label: "Public: anyone can read it" },
              { value: true, label: "Private: only you, or members of the organization" },
            ]}
          />
        </Form.Item>
        <Form.Item name="from" label="Start">
          <Radio.Group
            options={[
              { value: "empty", label: "Empty: you push to it" },
              { value: "mirror", label: "As a mirror of a repository on GitHub" },
            ]}
          />
        </Form.Item>
        {mirroring && (
          <>
            <Form.Item
              name="mirror"
              label="Repository on GitHub"
              extra="Its branches and tags are copied now and fetched again from time to time. A mirror does not take pushes."
              rules={[{ required: true, message: "Name the repository to mirror" }]}
            >
              <Input placeholder="owner/name" />
            </Form.Item>
            <Form.Item name="mirrorPulls" valuePropName="checked" extra="One ref for every pull request there ever was: refs/pull/<number>/head.">
              <Checkbox>Also copy the heads of its pull requests</Checkbox>
            </Form.Item>
            <Form.Item name="mirrorToken" label="Access token (optional)" extra="Only for a private repository: a token that may read it. It is kept here to fetch with, and not shown again.">
              <Input.Password autoComplete="off" />
            </Form.Item>
          </>
        )}
        <Button type="primary" htmlType="submit">Create repository</Button>
      </Form>
    </Card>
  );
}

export function NewOrg() {
  const { me, refresh } = useSession();
  const navigate = useNavigate();
  const [error, setError] = useState("");
  if (!me) return null;
  if (!me.user) return <Navigate to="/login" />;

  const submit = async (values: { name: string; displayName?: string }) => {
    setError("");
    try {
      const made = await api<{ name: string }>("/orgs", "POST", values);
      await refresh();
      navigate(`/${made.name}`);
    } catch (e) {
      setError((e as Error).message);
    }
  };
  return (
    <Card title="New organization" className="medium">
      {error && <Alert type="error" showIcon title={error} className="form-alert" />}
      <Form layout="vertical" onFinish={submit} requiredMark={false}>
        <Form.Item name="name" label="Organization name" extra="Shares its namespace with user names." rules={[{ required: true, message: "Enter a name" }]}>
          <Input autoFocus />
        </Form.Item>
        <Form.Item name="displayName" label="Display name (optional)">
          <Input />
        </Form.Item>
        <Button type="primary" htmlType="submit">Create organization</Button>
      </Form>
    </Card>
  );
}
