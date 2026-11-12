import { DownOutlined, PlusOutlined } from "@ant-design/icons";
import { Button, Dropdown, Space } from "antd";
import { Link, Outlet, useNavigate } from "react-router-dom";
import { api } from "./api";
import { useSession } from "./session";

export function Shell() {
  const { me, refresh } = useSession();
  const navigate = useNavigate();
  const user = me?.user;
  const signOut = async () => {
    await api("/logout", "POST");
    await refresh();
    navigate("/");
  };
  return (
    <>
      <header className="topbar">
        <Link to="/" className="brand">
          <span className="brand-mark" aria-hidden />
          Strata Hub
        </Link>
        <span className="grow" />
        {user && (
          <Space>
            <Dropdown
              menu={{
                items: [
                  { key: "repo", label: <Link to="/new">New repository</Link> },
                  { key: "org", label: <Link to="/orgs/new">New organization</Link> },
                ],
              }}
            >
              <Button icon={<PlusOutlined />} aria-label="Create" />
            </Dropdown>
            <Dropdown
              menu={{
                items: [
                  { key: "profile", label: <Link to={`/${user.name}`}>Your repositories</Link> },
                  ...(me?.orgs ?? []).map((org) => ({ key: `org-${org.name}`, label: <Link to={`/${org.name}`}>{org.name}</Link> })),
                  { type: "divider" as const },
                  { key: "tokens", label: <Link to="/settings/tokens">Access tokens</Link> },
                  { key: "keys", label: <Link to="/settings/keys">Signing keys</Link> },
                  { key: "out", label: "Sign out", onClick: signOut },
                ],
              }}
            >
              <Button type="text">
                {user.name} <DownOutlined />
              </Button>
            </Dropdown>
          </Space>
        )}
        {me && !user && (
          <Space>
            <Link to="/login"><Button type="text">Sign in</Button></Link>
            {me.signIn.passwords && <Link to="/signup"><Button type="primary">Sign up</Button></Link>}
          </Space>
        )}
      </header>
      <main className="page">
        <Outlet />
      </main>
    </>
  );
}
