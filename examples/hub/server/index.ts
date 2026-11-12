import { Hono } from "hono";
import { serveStatic } from "hono/bun";
import { api } from "./api";
import { config } from "./config";
import { followEvents } from "./events";
import { hooks } from "./hooks";
import { followSources } from "./mirror";
import { followDeliveries } from "./webhooks";

const app = new Hono();
app.route("/api", api);
app.route("/hooks", hooks);
app.all("/api/*", (c) => c.json({ error: "not found" }, 404));
// The built UI, and its index for every path the UI routes itself.
app.use("*", serveStatic({ root: "./dist" }));
app.get("*", serveStatic({ path: "./dist/index.html" }));

if (!config.adminToken || !config.hookSecret) {
  console.warn("STRATA_ADMIN_TOKEN and STRATA_HOOK_SECRET should both be set, to the values Strata was started with");
}
followEvents();
followSources();
followDeliveries();
console.log(`hub listening on http://127.0.0.1:${config.port}, git backend at ${config.strataUrl}`);

export default { port: config.port, hostname: process.env.HUB_HOST || "127.0.0.1", fetch: app.fetch };
