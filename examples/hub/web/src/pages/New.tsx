import { Alert, Button, Card, Form, Input, Radio, Select } from "antd";
import { useState } from "react";
import { Navigate, useNavigate, useSearchParams } from "react-router-dom";
import { api } from "../api";
import { useSession } from "../session";

export function NewRepo() {
  const { me } = useSession();
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const [error, setError] = useState("");
  if (!me) return null;
  if (!me.user) return <Navigate to="/login" />;

  const owners = [me.user.name, ...me.orgs.map((org) => org.name)];
  const submit = async (values: { owner: string; name: string; description?: string; private: boolean }) => {
    setError("");
    try {
      const made = await api<{ owner: string; name: string }>("/repos", "POST", values);
      navigate(`/${made.owner}/${made.name}`);
    } catch (e) {
      setError((e as Error).message);
    }
  };
  return (
    <Card title="New repository" className="medium">
      {error && <Alert type="error" showIcon title={error} className="form-alert" />}
      <Form layout="vertical" onFinish={submit} requiredMark={false} initialValues={{ owner: params.get("owner") ?? me.user.name, private: false }}>
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
