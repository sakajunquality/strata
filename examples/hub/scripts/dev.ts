// Starts everything on one machine: Strata, the hub and (unless --built) the
// Vite dev server. Secrets are made up for the run; data stays in .data/.
import { existsSync } from "node:fs";
import { resolve } from "node:path";

const root = resolve(import.meta.dir, "..");
const built = process.argv.includes("--built");
const strataBin = process.env.STRATA_BIN ?? resolve(root, "../../target/debug/strata");
if (!existsSync(strataBin)) {
  console.error(`Strata is not built (${strataBin}). Run "cargo build" in the repository root, or set STRATA_BIN.`);
  process.exit(1);
}

const strataPort = process.env.STRATA_PORT ?? "8080";
const hubPort = process.env.HUB_PORT ?? "3000";
const sshPort = process.env.STRATA_SSH_PORT ?? "2222";
const secret = () => crypto.randomUUID().replaceAll("-", "");
const env = {
  ...process.env,
  HUB_PORT: hubPort,
  // Where a sign-in elsewhere comes back to: the address the UI is open at.
  HUB_PUBLIC_URL: process.env.HUB_PUBLIC_URL ?? `http://127.0.0.1:${built ? hubPort : "5173"}`,
  STRATA_URL: `http://127.0.0.1:${strataPort}`,
  STRATA_SSH_URL: `ssh://git@127.0.0.1:${sshPort}`,
  // Webhooks to this machine are what one tries first.
  HUB_WEBHOOKS_INSIDE: process.env.HUB_WEBHOOKS_INSIDE ?? "1",
  STRATA_ADMIN_TOKEN: process.env.STRATA_ADMIN_TOKEN ?? secret(),
  STRATA_HOOK_SECRET: process.env.STRATA_HOOK_SECRET ?? secret(),
};

const run = (cmd: string[]) => Bun.spawn(cmd, { cwd: root, env, stdout: "inherit", stderr: "inherit" });
const children = [
  run([
    strataBin,
    "--listen", `127.0.0.1:${strataPort}`,
    "--ssh-listen", `127.0.0.1:${sshPort}`,
    // Where large files (Git LFS) are fetched from, for clients that came in over SSH.
    "--public-url", `http://127.0.0.1:${strataPort}`,
    "--data-dir", resolve(root, ".data/strata"),
    "--authz-url", `http://127.0.0.1:${hubPort}/hooks/authz`,
    "--pre-receive-url", `http://127.0.0.1:${hubPort}/hooks/pre-receive`,
    // A changed permission shows within this many seconds.
    "--authz-cache-secs", "2",
  ]),
  run(built ? ["bun", "run", "server/index.ts"] : ["bun", "--watch", "run", "server/index.ts"]),
  ...(built ? [] : [run(["bunx", "--bun", "vite"])]),
];

console.log(`\n  UI:  http://127.0.0.1:${built ? hubPort : "5173"}\n  git: http://127.0.0.1:${strataPort}/<owner>/<repo>.git\n       ssh://git@127.0.0.1:${sshPort}/<owner>/<repo>.git\n`);

let stopping = false;
const stop = () => {
  if (stopping) return;
  stopping = true;
  for (const child of children) child.kill();
};
process.on("SIGINT", stop);
process.on("SIGTERM", stop);
// One of them going down takes the rest with it.
await Promise.race(children.map((child) => child.exited));
stop();
await Promise.all(children.map((child) => child.exited));
