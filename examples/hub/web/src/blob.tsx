// Reading file contents, and showing them as code.
import hljs from "highlight.js/lib/common";
import { useMemo } from "react";
import { query } from "./api";

export const MAX_SHOWN = 1024 * 1024;

export type Blob = { size: number; text: string | null };

export const blobUrl = (base: string, params: { rev?: string; path?: string; oid?: string; download?: number; resolve?: number }) => `/api${base}/blob${query(params)}`;

/** A file's content. `text` is null for what cannot be shown: binary or too big. */
export async function loadBlob(base: string, params: { rev?: string; path?: string; oid?: string }): Promise<Blob> {
  const response = await fetch(blobUrl(base, params));
  if (!response.ok) throw new Error(`could not load the file (${response.status})`);
  const data = new Uint8Array(await response.arrayBuffer());
  const binary = data.subarray(0, 8000).includes(0);
  return { size: data.length, text: binary || data.length > MAX_SHOWN ? null : new TextDecoder().decode(data) };
}

const BY_NAME: Record<string, string> = { dockerfile: "dockerfile", makefile: "makefile", "cargo.lock": "toml" };

function language(path: string): string | undefined {
  const name = path.split("/").pop()!.toLowerCase();
  const guess = BY_NAME[name] ?? name.split(".").pop() ?? "";
  return hljs.getLanguage(guess) ? guess : undefined;
}

const escape = (text: string) => text.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]!);

/** A file as code. `marked` is a line to point out. */
export function CodeView({ text, path, marked }: { text: string; path: string; marked?: number }) {
  const html = useMemo(() => {
    const lang = language(path);
    // Highlighting a very large file would stall the page.
    return lang && text.length < 300_000 ? hljs.highlight(text, { language: lang }).value : escape(text);
  }, [text, path]);
  const lines = text.endsWith("\n") ? text.split("\n").length - 1 : text.split("\n").length;
  return (
    <div className="code">
      {marked !== undefined && marked >= 1 && marked <= lines && <div id="marked-line" className="marked-line" style={{ top: `calc(12px + ${marked - 1} * var(--code-line))` }} />}
      <pre className="gutter" aria-hidden>{Array.from({ length: lines }, (_, i) => i + 1).join("\n")}</pre>
      <pre className="source"><code className="hljs" dangerouslySetInnerHTML={{ __html: html }} /></pre>
    </div>
  );
}
