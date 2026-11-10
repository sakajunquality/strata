import { Alert, Button, Card, Form, Input } from "antd";
import { useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { api } from "../api";
import { useSession } from "../session";

export function Login({ mode }: { mode: "login" | "signup" }) {
  const { refresh } = useSession();
  const navigate = useNavigate();
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const signup = mode === "signup";

  const submit = async (values: { username: string; password: string; displayName?: string }) => {
    setBusy(true);
    setError("");
    try {
      await api(signup ? "/signup" : "/login", "POST", values);
      await refresh();
      navigate("/");
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card title={signup ? "Create an account" : "Sign in"} className="narrow">
      {error && <Alert type="error" showIcon title={error} className="form-alert" />}
      <Form layout="vertical" onFinish={submit} requiredMark={false}>
        <Form.Item name="username" label="User name" rules={[{ required: true, message: "Enter a user name" }]}>
          <Input autoComplete="username" autoFocus />
        </Form.Item>
        {signup && (
          <Form.Item name="displayName" label="Name (optional)">
            <Input autoComplete="name" />
          </Form.Item>
        )}
        <Form.Item name="password" label="Password" rules={[{ required: true, min: signup ? 8 : 1, message: signup ? "At least 8 characters" : "Enter your password" }]}>
          <Input.Password autoComplete={signup ? "new-password" : "current-password"} />
        </Form.Item>
        <Button type="primary" htmlType="submit" loading={busy} block>
          {signup ? "Create account" : "Sign in"}
        </Button>
      </Form>
      <p className="muted form-foot">
        {signup ? <>Already have an account? <Link to="/login">Sign in</Link></> : <>New here? <Link to="/signup">Create an account</Link></>}
      </p>
    </Card>
  );
}
