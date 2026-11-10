import { Button, Card, Col, Row, Typography } from "antd";
import { Link } from "react-router-dom";
import { api, useLoad, type Activity, type RepoSummary } from "../api";
import { ActivityList, RepoList } from "../RepoList";
import { useSession } from "../session";
import { Loaded } from "../util";

export function Home() {
  const { me } = useSession();
  const user = me?.user;
  const repos = useLoad(() => api<{ repos: RepoSummary[] }>("/repos"), `repos:${user?.name}`);
  const activity = useLoad(() => (user ? api<{ activity: Activity[] }>("/activity") : Promise.resolve({ activity: [] })), `activity:${user?.name}`);
  if (!me) return null;

  if (!user) {
    return (
      <>
        <div className="hero">
          <Typography.Title>A small code host on Strata</Typography.Title>
          <Typography.Paragraph className="muted">
            Accounts, organizations and permissions live here. Every git object lives in Strata, which this application reaches over HTTP only.
          </Typography.Paragraph>
          <Link to="/signup"><Button type="primary" size="large">Create an account</Button></Link>
        </div>
        <Card title="Public repositories">
          <Loaded from={repos}>{(data) => <RepoList repos={data.repos} empty="Nothing public yet" />}</Loaded>
        </Card>
      </>
    );
  }

  const mine = new Set([user.name, ...me.orgs.map((org) => org.name)].map((name) => name.toLowerCase()));
  return (
    <Row gutter={[24, 24]}>
      <Col xs={24} md={14}>
        <Card title="Your repositories" extra={<Link to="/new"><Button type="primary">New</Button></Link>}>
          <Loaded from={repos}>
            {(data) => <RepoList repos={data.repos.filter((repo) => mine.has(repo.owner.toLowerCase()))} empty="You have no repositories yet" />}
          </Loaded>
        </Card>
        <Card title="Explore" className="stack">
          <Loaded from={repos}>
            {(data) => <RepoList repos={data.repos.filter((repo) => !mine.has(repo.owner.toLowerCase()))} empty="No other repositories you can see" />}
          </Loaded>
        </Card>
      </Col>
      <Col xs={24} md={10}>
        <Card title="Recent pushes">
          <Loaded from={activity}>{(data) => <ActivityList activity={data.activity} showRepo />}</Loaded>
        </Card>
      </Col>
    </Row>
  );
}
