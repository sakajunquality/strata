//! Stateless git smart-HTTP frontend. All durable state lives behind
//! [`MetaStore`] and [`PackStore`].

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use bytes::Bytes;
use strata_store::{Access, Grant, MetaError, MetaStore, PackStore, TokenRecord};

mod auth;
mod compaction;
mod receive_pack;
mod upload_pack;

use auth::Need;
pub use compaction::{compact, spawn_background, sweep, sweep_orphans, CompactionReport};

/// Fetch requests are negotiation only; anything larger is not a git client.
const MAX_FETCH_REQUEST: usize = 16 << 20;

pub struct Options {
    /// Create repositories on first push instead of requiring `PUT /api/repos/<name>`.
    pub auto_create: bool,
    /// Where incoming packs are spooled while they are validated. Needs room
    /// for the largest push in flight; nothing here outlives its request.
    pub scratch_dir: PathBuf,
    /// Number of packs at which a push asks for the pool to be compacted.
    pub compact_at: usize,
    /// Whether a compaction that rewrites a whole pool leaves out objects no
    /// ref reaches any more.
    pub prune: bool,
    /// How long a replaced pack is kept, so that requests reading it can finish.
    pub gc_grace: Duration,
    /// How old a stored pack that no manifest mentions must be before it is
    /// deleted. Must comfortably exceed the longest push or compaction, whose
    /// packs look just like this until they commit.
    pub orphan_grace: Duration,
    /// The secret that may do anything. Without one, authentication is off
    /// and every request is allowed.
    pub admin_token: Option<String>,
    /// Let anyone fetch without credentials.
    pub anonymous_read: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            auto_create: false,
            scratch_dir: std::env::temp_dir(),
            compact_at: 8,
            prune: true,
            gc_grace: Duration::from_secs(3600),
            orphan_grace: Duration::from_secs(24 * 3600),
            admin_token: None,
            anonymous_read: false,
        }
    }
}

pub struct App {
    pub meta: Arc<dyn MetaStore>,
    pub packs: PackStore,
    pub options: Options,
    /// Identifies this process when claiming maintenance leases.
    node_id: String,
    compactions: compaction::Queue,
}

impl App {
    pub fn new(meta: Arc<dyn MetaStore>, packs: PackStore, options: Options) -> App {
        App { meta, packs, options, node_id: PackStore::new_pack_id(), compactions: compaction::Queue::default() }
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}

pub(crate) struct HttpError {
    status: StatusCode,
    message: String,
}

impl HttpError {
    fn new(status: StatusCode, message: impl Into<String>) -> HttpError {
        HttpError { status, message: message.into() }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> HttpError {
        HttpError::new(StatusCode::BAD_REQUEST, message)
    }

    fn not_found() -> HttpError {
        HttpError::new(StatusCode::NOT_FOUND, "not found")
    }

    pub(crate) fn unauthorized() -> HttpError {
        HttpError::new(StatusCode::UNAUTHORIZED, "authentication required")
    }
}

/// Anything not explicitly mapped to a client error is a server fault.
impl<E: Into<anyhow::Error>> From<E> for HttpError {
    fn from(e: E) -> Self {
        let e: anyhow::Error = e.into();
        tracing::error!("internal error: {e:#}");
        HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let mut response = (self.status, self.message + "\n").into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            // Makes git ask its credential helper and retry.
            response.headers_mut().insert(header::WWW_AUTHENTICATE, "Basic realm=\"strata\"".parse().expect("valid header"));
        }
        response
    }
}

/// Runs CPU-bound or blocking git work off the async executor.
pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, HttpError> {
    Ok(tokio::task::spawn_blocking(f).await?)
}

async fn handle(State(app): State<Arc<App>>, method: Method, uri: Uri, headers: HeaderMap, body: Body) -> Response {
    match route(&app, method, &uri, &headers, body).await {
        Ok(response) => response,
        Err(e) => e.into_response(),
    }
}

fn method_not_allowed() -> HttpError {
    HttpError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

async fn route(app: &App, method: Method, uri: &Uri, headers: &HeaderMap, body: Body) -> Result<Response, HttpError> {
    let path = uri.path().trim_start_matches('/');

    if path == "api/tokens" || path.starts_with("api/tokens/") {
        auth::authorize(app, headers, Need::Tokens).await?;
        return tokens(app, &method, path.strip_prefix("api/tokens").unwrap_or_default(), uri.query().unwrap_or("")).await;
    }
    if let Some(rest) = path.strip_prefix("api/repos/") {
        return repos(app, headers, &method, rest).await;
    }

    if let Some(name) = path.strip_suffix("/info/refs") {
        if method != Method::GET {
            return Err(method_not_allowed());
        }
        let name = repo_name(name)?;
        let service = uri.query().unwrap_or("").split('&').find_map(|kv| kv.strip_prefix("service="));
        return match service {
            Some("git-upload-pack") => {
                auth::authorize(app, headers, Need::Repo(name, Access::Read)).await?;
                let repo = app.meta.get_repo(name).await?.ok_or_else(HttpError::not_found)?;
                let advertisement =
                    if wants_v2(headers) { upload_pack::advertise() } else { upload_pack::advertise_v0(app, &repo).await? };
                Ok(git_response("upload-pack-advertisement", advertisement))
            }
            Some("git-receive-pack") => {
                auth::authorize(app, headers, Need::Repo(name, Access::Write)).await?;
                let repo = app.meta.get_repo(name).await?;
                if repo.is_none() && !app.options.auto_create {
                    return Err(HttpError::not_found());
                }
                Ok(git_response("receive-pack-advertisement", receive_pack::advertise(repo.as_ref())))
            }
            _ => Err(HttpError::bad_request("dumb HTTP protocol is not supported")),
        };
    }

    if let Some(name) = path.strip_suffix("/git-upload-pack") {
        if method != Method::POST {
            return Err(method_not_allowed());
        }
        let name = repo_name(name)?;
        auth::authorize(app, headers, Need::Repo(name, Access::Read)).await?;
        let repo = app.meta.get_repo(name).await?.ok_or_else(HttpError::not_found)?;
        let body = axum::body::to_bytes(body, MAX_FETCH_REQUEST)
            .await
            .map_err(|_| HttpError::new(StatusCode::PAYLOAD_TOO_LARGE, "fetch request too large"))?;
        let body = decode_body(headers, body)?;
        let response = if wants_v2(headers) {
            upload_pack::handle(app, repo, body).await?
        } else {
            upload_pack::handle_v0(app, repo, body).await?
        };
        return Ok(git_response("upload-pack-result", response));
    }

    if let Some(name) = path.strip_suffix("/git-receive-pack") {
        if method != Method::POST {
            return Err(method_not_allowed());
        }
        let name = repo_name(name)?;
        auth::authorize(app, headers, Need::Repo(name, Access::Write)).await?;
        let repo = app.meta.get_repo(name).await?;
        if repo.is_none() && !app.options.auto_create {
            return Err(HttpError::not_found());
        }
        if headers.contains_key(header::CONTENT_ENCODING) {
            return Err(HttpError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "compressed push bodies are not supported"));
        }
        let result = receive_pack::handle(app, name, repo, body).await?;
        return Ok(git_response("receive-pack-result", result));
    }

    Err(HttpError::not_found())
}

/// `PUT /api/repos/<name>` creates a repository; `POST /api/repos/<name>/compact` rewrites its pool.
async fn repos(app: &App, headers: &HeaderMap, method: &Method, rest: &str) -> Result<Response, HttpError> {
    if let Some(name) = rest.strip_suffix("/compact") {
        let name = repo_name(name)?;
        auth::authorize(app, headers, Need::Repo(name, Access::Admin)).await?;
        if method != Method::POST {
            return Err(method_not_allowed());
        }
        app.meta.get_repo(name).await?.ok_or_else(HttpError::not_found)?;
        return Ok(match compact(app, name, true).await? {
            Some(report) => format!("merged {} packs into one holding {} objects\n", report.merged, report.objects).into_response(),
            None => "nothing to do\n".into_response(),
        });
    }
    let name = repo_name(rest)?;
    auth::authorize(app, headers, Need::Repo(name, Access::Admin)).await?;
    if method != Method::PUT {
        return Err(method_not_allowed());
    }
    match app.meta.create_repo(name).await {
        Ok(_) => Ok((StatusCode::CREATED, "created\n").into_response()),
        Err(MetaError::AlreadyExists) => Err(HttpError::new(StatusCode::CONFLICT, "repository already exists")),
        Err(e) => Err(e.into()),
    }
}

/// `POST /api/tokens?name=<name>&grant=<pattern>:<access>...` issues a token
/// and returns its secret, once. `DELETE /api/tokens/<name>` revokes one.
async fn tokens(app: &App, method: &Method, rest: &str, query: &str) -> Result<Response, HttpError> {
    if let Some(name) = rest.strip_prefix('/').filter(|name| !name.is_empty()) {
        if method != Method::DELETE {
            return Err(method_not_allowed());
        }
        return match app.meta.delete_token(name).await? {
            true => Ok("revoked\n".into_response()),
            false => Err(HttpError::not_found()),
        };
    }
    if method != Method::POST {
        return Err(method_not_allowed());
    }
    let mut name = None;
    let mut grants = Vec::new();
    for (key, value) in query.split('&').filter_map(|kv| kv.split_once('=')) {
        match key {
            "name" => name = Some(value),
            "grant" => {
                let grant = value.rsplit_once(':').and_then(|(pattern, access)| {
                    let valid = pattern == "*" || repo_name(pattern.strip_suffix("/*").unwrap_or(pattern)).is_ok();
                    Some(Grant { pattern: pattern.to_string(), access: Access::parse(access).filter(|_| valid)? })
                });
                grants.push(grant.ok_or_else(|| HttpError::bad_request("grants look like acme/*:read, acme/app:write or *:admin"))?);
            }
            _ => {}
        }
    }
    let name = name
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
        .ok_or_else(|| HttpError::bad_request("a token needs a name of letters, digits, '-', '_' or '.'"))?;
    if grants.is_empty() {
        return Err(HttpError::bad_request("a token needs at least one grant"));
    }
    let secret = format!("strata_{}{}", PackStore::new_pack_id(), PackStore::new_pack_id());
    app.meta.put_token(TokenRecord { name: name.to_string(), hash: auth::hash_secret(&secret), grants }).await?;
    Ok((StatusCode::CREATED, secret + "\n").into_response())
}

/// Normalises `owner/repo.git` to `owner/repo` and rejects anything that is
/// not a plain slash-separated name.
fn repo_name(raw: &str) -> Result<&str, HttpError> {
    let name = raw.strip_suffix(".git").unwrap_or(raw);
    let segment_ok = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if name.len() > 255 || !name.split('/').all(segment_ok) {
        return Err(HttpError::bad_request("invalid repository name"));
    }
    Ok(name)
}

fn wants_v2(headers: &HeaderMap) -> bool {
    headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(':').any(|kv| kv == "version=2"))
}

/// git gzips large fetch requests.
fn decode_body(headers: &HeaderMap, body: Bytes) -> Result<Bytes, HttpError> {
    match headers.get(header::CONTENT_ENCODING).map(|v| v.as_bytes()) {
        None | Some(b"identity") => Ok(body),
        Some(b"gzip") | Some(b"x-gzip") => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(&body[..])
                .take(MAX_FETCH_REQUEST as u64 + 1)
                .read_to_end(&mut out)
                .map_err(|_| HttpError::bad_request("invalid gzip request body"))?;
            if out.len() > MAX_FETCH_REQUEST {
                return Err(HttpError::new(StatusCode::PAYLOAD_TOO_LARGE, "fetch request too large"));
            }
            Ok(out.into())
        }
        Some(_) => Err(HttpError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported content encoding")),
    }
}

fn git_response(kind: &str, body: impl Into<Body>) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, format!("application/x-git-{kind}"))
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body.into())
        .expect("static headers are valid")
}
