// Looking for things: repositories by name, text in a repository's files,
// and who last changed each line of a file.
import { Alert, Card, Checkbox, Empty, Input, Space, Tooltip } from "antd";
import { useState } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import { api, query, useLoad, type Blame, type CodeSearch, type RepoSummary } from "../api";
import { loadBlob } from "../blob";
import { RepoList } from "../RepoList";
import { Loaded, shortOid, splitRev, subject, urlPath, When } from "../util";
import { RevSelect, type RepoContext } from "./Repo";

/** Every repository one can see, by a word in its name or description. */
export function SearchPage() {
  const [params] = useSearchParams();
  const q = params.get("q") ?? "";
  const found = useLoad(() => api<{ repos: RepoSummary[] }>(`/search${query({ q })}`), `search:${q}`);
  return (
    <Card title={q ? `Repositories matching “${q}”` : "Search"} className="medium wide">
      <Loaded from={found}>{(data) => <RepoList repos={data.repos} empty={q ? "No repository matches. To search code, open a repository and search there." : "Type a word in the box at the top."} />}</Loaded>
    </Card>
  );
}

/** Text in the files of one revision of a repository. */
export function CodeSearchPage({ repo, root, base }: RepoContext) {
  const [params, setParams] = useSearchParams();
  const q = params.get("q") ?? "";
  const rev = params.get("rev") ?? repo.defaultBranch;
  const [regex, exact] = [params.get("regex") === "1", params.get("case") === "1"];
  const set = (next: Record<string, string | undefined>) => {
    const merged = { q, rev, regex: regex ? "1" : undefined, case: exact ? "1" : undefined, ...next };
    setParams(Object.fromEntries(Object.entries(merged).filter((entry): entry is [string, string] => !!entry[1])));
  };
  const found = useLoad(() => (q ? api<CodeSearch>(`${base}/search${query({ q, rev, regex: regex ? 1 : undefined, case: exact ? 1 : undefined })}`) : Promise.resolve(null)), `code-search:${base}:${rev}:${q}:${regex}:${exact}`);
  return (
    <>
      <div className="toolbar">
        <RevSelect repo={repo} rev={rev} to={(next) => `${root}/search${query({ q, rev: next, regex: regex ? 1 : undefined, case: exact ? 1 : undefined })}`} />
        <Input.Search key={q} defaultValue={q} placeholder="Search the code of this branch" aria-label="Search code" allowClear className="search-box" onSearch={(next) => set({ q: next.trim() || undefined })} />
        <Checkbox checked={regex} onChange={(e) => set({ regex: e.target.checked ? "1" : undefined })}>Regular expression</Checkbox>
        <Checkbox checked={exact} onChange={(e) => set({ case: e.target.checked ? "1" : undefined })}>Match case</Checkbox>
      </div>
      <Loaded from={found}>
        {(data) => {
          if (!data) return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="Type what to look for. Every file of the branch is read for each search, so a large repository takes a while." />;
          const blob = (path: string) => `${root}/blob/${urlPath(rev)}/${urlPath(path)}`;
          return (
            <>
              <p className="muted">
                {data.matches} {data.matches === 1 ? "line" : "lines"} in {data.files.length} {data.files.length === 1 ? "file" : "files"}
                {data.truncated && " and more: the search stopped at its limit"}
              </p>
              {data.paths.length > 0 && (
                <Card size="small" title="Files by name" className="stack">
                  {data.paths.map((path) => <div key={path}><Link to={blob(path)} className="mono">{path}</Link></div>)}
                </Card>
              )}
              {data.files.map((file) => (
                <Card key={file.path} size="small" className="stack" styles={{ body: { padding: 0 } }} title={<Link to={blob(file.path)} className="mono">{file.path}</Link>}>
                  <table className="diff">
                    <tbody>
                      {file.matches.map((match) => (
                        <tr key={match.line}>
                          <td className="n"><Link to={`${blob(file.path)}#L${match.line}`}>{match.line}</Link></td>
                          <td>{match.text}{match.cut && <span className="muted"> …</span>}</td>
                        </tr>
                      ))}
                      {file.more && <tr><td /><td className="muted">more matches in this file</td></tr>}
                    </tbody>
                  </table>
                </Card>
              ))}
              {data.matches === 0 && data.paths.length === 0 && <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="Nothing found" />}
            </>
          );
        }}
      </Loaded>
    </>
  );
}

/** A file with, beside each run of lines, the commit that last changed them. */
export function BlamePage({ repo, root, base }: RepoContext) {
  const rest = useParams()["*"] ?? "";
  const { rev, path } = splitRev(rest, [...repo.branches, ...repo.tags]);
  const [hovered, setHovered] = useState("");
  const blamed = useLoad(async () => {
    const [blame, blob] = await Promise.all([api<Blame>(`${base}/blame${query({ rev, path })}`), loadBlob(base, { rev, path })]);
    return { blame, lines: (blob.text ?? "").replace(/\n$/, "").split("\n") };
  }, `blame:${base}:${rev}:${path}`);
  return (
    <>
      <div className="toolbar">
        <RevSelect repo={repo} rev={rev} to={(next) => `${root}/blame/${urlPath(next)}/${urlPath(path)}`} />
        <span className="crumbs"><strong>{path}</strong></span>
        <span className="grow" />
        <Space>
          <Link to={`${root}/blob/${urlPath(rev)}/${urlPath(path)}`}>File</Link>
          <Link to={`${root}/commits/${urlPath(rev)}${query({ path })}`}>History</Link>
        </Space>
      </div>
      <Loaded from={blamed}>
        {({ blame, lines }) => (
          <>
            {blame.truncated && <Alert type="warning" showIcon className="stack" title="This file's history is long: the oldest lines are put down to the oldest commit that was reached in time." />}
            <Card size="small" styles={{ body: { padding: 0 } }}>
              <table className="diff blame">
                <tbody>
                  {blame.hunks.flatMap((hunk) => {
                    const commit = blame.commits[hunk.oid]!;
                    return Array.from({ length: hunk.lines }, (_, i) => {
                      const number = hunk.start + i;
                      return (
                        <tr key={number} className={`${i === 0 ? "first" : ""} ${hovered === hunk.oid ? "same" : ""}`} onMouseEnter={() => setHovered(hunk.oid)}>
                          <td className="who">
                            {i === 0 && (
                              <Tooltip title={<>{commit.author?.name} · {shortOid(hunk.oid)}</>}>
                                <Link to={`${root}/commit/${hunk.oid}`} className="plain">{subject(commit.message)}</Link>
                              </Tooltip>
                            )}
                          </td>
                          <td className="when muted">{i === 0 && <When time={commit.author?.time} />}</td>
                          <td className="n" id={`L${number}`}>{number}</td>
                          <td>{lines[number - 1]}</td>
                        </tr>
                      );
                    });
                  })}
                </tbody>
              </table>
            </Card>
          </>
        )}
      </Loaded>
    </>
  );
}
