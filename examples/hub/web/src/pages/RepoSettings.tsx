import { App, Button, Card, Form, Input, Modal, Radio, Select, Switch, Typography } from "antd";
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "../api";
import type { RepoContext } from "./Repo";

export function RepoSettings({ repo, base, reload }: RepoContext) {
  const { message } = App.useApp();
  const navigate = useNavigate();
  const [deleting, setDeleting] = useState(false);
  const [typed, setTyped] = useState("");
  if (repo.permission !== "admin") return <Typography.Paragraph>Only the repository's owners can change its settings.</Typography.Paragraph>;

  const save = async (values: Record<string, unknown>) => {
    try {
      const saved = await api<{ owner: string; name: string }>(base, "PATCH", values);
      message.success("Saved");
      if (saved.name !== repo.name) navigate(`/${saved.owner}/${saved.name}/settings`);
      else reload();
    } catch (e) {
      message.error((e as Error).message);
    }
  };
  const remove = async () => {
    try {
      await api(base, "DELETE");
      navigate(`/${repo.owner}`);
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  return (
    <div className="medium">
      <Card title="General">
        <Form layout="vertical" onFinish={save} initialValues={{ name: repo.name, description: repo.description, private: repo.private }} requiredMark={false}>
          <Form.Item name="name" label="Name" extra="Renaming changes the clone URL at once. The old one stops working." rules={[{ required: true, message: "Enter a name" }]}>
            <Input />
          </Form.Item>
          <Form.Item name="description" label="Description">
            <Input />
          </Form.Item>
          <Form.Item name="private" label="Visibility">
            <Radio.Group options={[{ value: false, label: "Public" }, { value: true, label: "Private" }]} />
          </Form.Item>
          <Button type="primary" htmlType="submit">Save</Button>
        </Form>
      </Card>

      <Card title="Default branch" className="stack">
        <Form layout="vertical" onFinish={save} initialValues={{ defaultBranch: repo.empty ? undefined : repo.defaultBranch, protectDefault: repo.protectDefault }}>
          <Form.Item name="defaultBranch" label="Branch" extra="What a clone checks out, and what the Code tab shows first.">
            <Select disabled={repo.branches.length === 0} placeholder="No branches yet" options={repo.branches.map(({ name }) => ({ value: name, label: name }))} />
          </Form.Item>
          <Form.Item name="protectDefault" label="Protect it" valuePropName="checked" extra="Refuses force pushes to the default branch, and its deletion.">
            <Switch />
          </Form.Item>
          <Button type="primary" htmlType="submit">Save</Button>
        </Form>
      </Card>

      <Card title="Delete this repository" className="stack danger">
        <Typography.Paragraph>Its history and files are gone for good.</Typography.Paragraph>
        <Button danger onClick={() => setDeleting(true)}>Delete repository</Button>
        <Modal
          open={deleting}
          title={`Delete ${repo.owner}/${repo.name}?`}
          okText="Delete"
          okButtonProps={{ danger: true, disabled: typed !== repo.name }}
          onOk={remove}
          onCancel={() => setDeleting(false)}
        >
          <p>This cannot be undone. Type <strong>{repo.name}</strong> to confirm.</p>
          <Input value={typed} onChange={(e) => setTyped(e.target.value)} aria-label="Repository name" />
        </Modal>
      </Card>
    </div>
  );
}
