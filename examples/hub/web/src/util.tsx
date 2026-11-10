import { Alert, Result, Spin } from "antd";
import type { ReactNode } from "react";
import type { ApiError, Named } from "./api";

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [["year", 31536000], ["month", 2592000], ["day", 86400], ["hour", 3600], ["minute", 60]];
const relative = new Intl.RelativeTimeFormat("en", { numeric: "auto" });

export function ago(seconds: number | null | undefined): string {
  if (!seconds) return "";
  const diff = seconds - Date.now() / 1000;
  for (const [unit, size] of UNITS) if (Math.abs(diff) >= size) return relative.format(Math.round(diff / size), unit);
  return "just now";
}

export function When({ time }: { time: number | null | undefined }) {
  if (!time) return null;
  return <time dateTime={new Date(time * 1000).toISOString()} title={new Date(time * 1000).toLocaleString()}>{ago(time)}</time>;
}

export const shortOid = (oid: string) => oid.slice(0, 7);
export const subject = (message: string) => message.split("\n", 1)[0] || "(no message)";

export function bytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/**
 * Splits "release/1.x/src/main.rs" into a revision and a path. Branch names
 * may hold slashes, so the longest known name wins; anything else is taken
 * to be a commit id up to the first slash.
 */
export function splitRev(rest: string, names: Named[]): { rev: string; path: string } {
  const known = names
    .map((named) => named.name)
    .filter((name) => rest === name || rest.startsWith(`${name}/`))
    .sort((a, b) => b.length - a.length)[0];
  if (known) return { rev: known, path: rest.slice(known.length + 1) };
  const slash = rest.indexOf("/");
  return slash < 0 ? { rev: rest, path: "" } : { rev: rest.slice(0, slash), path: rest.slice(slash + 1) };
}

/** A path as it goes into a URL: each segment escaped, slashes kept. */
export const urlPath = (path: string) => path.split("/").map(encodeURIComponent).join("/");

/** Shows `children` once loaded; a spinner or the error until then. */
export function Loaded<T>({ from, children }: { from: { data?: T; error?: ApiError }; children: (data: T) => ReactNode }) {
  if (from.error) {
    if (from.error.status === 404) return <Result status="404" title="Not found" subTitle={from.error.message} />;
    return <Alert type="error" showIcon title={from.error.message} />;
  }
  if (from.data === undefined) return <div className="center"><Spin /></div>;
  return <>{children(from.data)}</>;
}
