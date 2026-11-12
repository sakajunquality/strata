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
  /** Where browsers reach the hub: what a sign-in elsewhere is told to come back to. */
  publicUrl: env("HUB_PUBLIC_URL", `http://127.0.0.1:${env("HUB_PORT", "3000")}`).replace(/\/$/, ""),
  /** Whether accounts can be made and used with a password. Off, signing in elsewhere is the only way in. */
  passwords: env("HUB_PASSWORD_LOGIN", "1") !== "0",
  /** GitHub, or a GitHub Enterprise Server: where mirrors are fetched from and where "Sign in with GitHub" goes. */
  githubUrl: env("HUB_GITHUB_URL", "https://github.com").replace(/\/$/, ""),
  githubApiUrl: env("HUB_GITHUB_API_URL", "https://api.github.com").replace(/\/$/, ""),
  /** Where the hub keeps its own copy of each mirrored repository, to fetch only what is new. */
  mirrorDir: env("HUB_MIRROR_DIR", ".data/mirrors"),
  /** Minutes between two fetches of a mirror. 0: only when asked. */
  mirrorMinutes: Number(env("HUB_MIRROR_MINUTES", "60")),
};

export type Provider = {
  id: string;
  /** What the button says: "Sign in with <name>". */
  name: string;
  kind: "oidc" | "github";
  clientId: string;
  clientSecret: string;
  /** OpenID Connect: where the provider describes itself, under /.well-known/openid-configuration. */
  issuer?: string;
  /** Google: the Workspace domains whose accounts are let in. Empty: any Google account. */
  domains?: string[];
};

/** The places one can sign in at, each switched on by giving its client id. */
function providers(): Provider[] {
  const found: Provider[] = [];
  const client = (prefix: string) => ({ clientId: env(`${prefix}_CLIENT_ID`, ""), clientSecret: env(`${prefix}_CLIENT_SECRET`, "") });
  const github = client("HUB_GITHUB");
  if (github.clientId) found.push({ id: "github", name: "GitHub", kind: "github", ...github });
  const google = client("HUB_GOOGLE");
  if (google.clientId) {
    const domains = env("HUB_GOOGLE_DOMAIN", "").split(",").map((domain) => domain.trim().toLowerCase()).filter(Boolean);
    found.push({ id: "google", name: "Google", kind: "oidc", issuer: "https://accounts.google.com", domains, ...google });
  }
  const entra = client("HUB_ENTRA");
  if (entra.clientId) {
    // One tenant, by its id: "common" would let in every Microsoft account there is.
    const tenant = env("HUB_ENTRA_TENANT", "");
    if (!tenant) throw new Error("HUB_ENTRA_TENANT must be set to the directory (tenant) id");
    found.push({ id: "entra", name: "Microsoft", kind: "oidc", issuer: `https://login.microsoftonline.com/${tenant}/v2.0`, ...entra });
  }
  // Any other OpenID Connect provider: Okta, Keycloak, Auth0 and the like.
  const oidc = client("HUB_OIDC");
  if (oidc.clientId) found.push({ id: "oidc", name: env("HUB_OIDC_NAME", "single sign-on"), kind: "oidc", issuer: env("HUB_OIDC_ISSUER", "").replace(/\/$/, ""), ...oidc });
  return found;
}

export const signInAt = providers();

