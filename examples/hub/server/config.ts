// Everything the hub needs to know about its surroundings, from the environment.
const env = (name: string, fallback: string) => process.env[name] || fallback;

export const config = {
  port: Number(env("HUB_PORT", "3000")),
  database: env("HUB_DB", ".data/hub.sqlite"),
  /** Where the hub reaches Strata. */
  strataUrl: env("STRATA_URL", "http://127.0.0.1:8080").replace(/\/$/, ""),
  /** Where git clients reach Strata: what clone URLs are built from. */
  strataPublicUrl: env("STRATA_PUBLIC_URL", env("STRATA_URL", "http://127.0.0.1:8080")).replace(/\/$/, ""),
  /** Strata's --admin-token. */
  adminToken: env("STRATA_ADMIN_TOKEN", ""),
  /** Strata's --hook-secret: proves that a hook call comes from Strata. */
  hookSecret: env("STRATA_HOOK_SECRET", ""),
  /** Set when the hub is served over HTTPS, so the session cookie is HTTPS-only. */
  secureCookies: env("HUB_SECURE_COOKIES", "") === "1",
};
