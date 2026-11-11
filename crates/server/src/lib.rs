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
use strata_store::{MetaError, MetaStore, PackStore, RefScope, Txn};

mod auth;
mod compaction;
mod content;
mod hooks;
mod import;
mod merge;
mod receive_pack;
mod upload_pack;

use auth::Access;
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
    /// Number of packs at which a push waits for compaction before it goes
    /// on. Every request costs as much as the pool has packs; pushes that
    /// come faster than compaction merges their packs would pile them up
    /// without end, each making the next one slower. Zero: never wait.
    pub wait_at: usize,
    /// A push bringing this many objects into a pool where nothing has
    /// bitmaps asks for the whole pool to be compacted, to get them. Without
    /// bitmaps a clone walks all of history, and a fetch cannot tell what
    /// the client has beyond the commits it names: it then sends again what
    /// the client got by another way, such as changes picked onto a release
    /// branch. Below this size, walking is quick enough. Zero: never.
    pub bitmaps_from: usize,
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
    /// Let anyone fetch without credentials. Ignored with an authorization hook.
    pub anonymous_read: bool,
    /// Where the application decides who may fetch and push. See [`auth`].
    pub authz_url: Option<String>,
    /// How long an answer from the authorization hook is reused. Zero asks every time.
    pub authz_cache: Duration,
    /// Where the application may veto a push before it is committed.
    pub pre_receive_url: Option<String>,
    /// Sent to the hooks as a Bearer token, so that they know Strata is asking.
    pub hook_secret: Option<String>,
    /// How long events stay in the feed.
    pub event_retention: Duration,
}

impl Options {
    /// Whether a pool of this many objects, none of them covered by bitmaps, should get them now.
    pub(crate) fn wants_bitmaps(&self, objects: usize) -> bool {
        self.bitmaps_from > 0 && objects >= self.bitmaps_from
    }
}

impl Default for Options {
    fn default() -> Self {
        Options {
            auto_create: false,
            scratch_dir: std::env::temp_dir(),
            compact_at: 8,
            wait_at: 64,
            bitmaps_from: 20_000,
            prune: true,
            gc_grace: Duration::from_secs(3600),
            orphan_grace: Duration::from_secs(24 * 3600),
            admin_token: None,
            anonymous_read: false,
            authz_url: None,
            authz_cache: Duration::from_secs(10),
            pre_receive_url: None,
            hook_secret: None,
            event_retention: Duration::from_secs(7 * 24 * 3600),
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
    hooks: hooks::Hooks,
    /// Recent answers of the authorization hook.
    decisions: Option<moka::sync::Cache<String, auth::Decision>>,
}

impl App {
    pub fn new(meta: Arc<dyn MetaStore>, packs: PackStore, options: Options) -> App {
        // Two TLS backends are linked in (by different dependencies); the HTTP
        // and gRPC clients need to be told which one to use.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let decisions = (!options.authz_cache.is_zero())
            .then(|| moka::sync::Cache::builder().max_capacity(100_000).time_to_live(options.authz_cache).build());
        App {
            meta,
            packs,
            hooks: hooks::Hooks::new(options.hook_secret.clone()),
            decisions,
            options,
            node_id: PackStore::new_pack_id(),
            compactions: compaction::Queue::default(),
        }
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
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> HttpError {
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

    if path == "healthz" {
        return Ok("ok\n".into_response());
    }
    if path == "readyz" {
        // Alive is not enough to be sent requests: every one of them needs the metadata store.
        return match app.meta.list_events(None, 1).await {
            Ok(_) => Ok("ready\n".into_response()),
            Err(e) => {
                tracing::warn!("not ready: the metadata store cannot be reached: {e}");
                Err(HttpError::new(StatusCode::SERVICE_UNAVAILABLE, "the metadata store cannot be reached"))
            }
        };
    }
    if path == "api/events" {
        auth::authorize_admin(app, headers)?;
        if method != Method::GET {
            return Err(method_not_allowed());
        }
        return events(app, uri.query().unwrap_or("")).await;
    }
    if let Some(rest) = path.strip_prefix("api/repos/") {
        auth::authorize_admin(app, headers)?;
        return repos(app, &method, rest, uri.query().unwrap_or(""), body).await;
    }

    if let Some(name) = path.strip_suffix("/info/refs") {
        if method != Method::GET {
            return Err(method_not_allowed());
        }
        let name = repo_name(name)?;
        let service = uri.query().unwrap_or("").split('&').find_map(|kv| kv.strip_prefix("service="));
        return match service {
            Some("git-upload-pack") => {
                let granted = auth::authorize_git(app, headers, name, Access::Read).await?;
                // Version 2 lists refs only when asked, and then only those asked about.
                let scope = if wants_v2(headers) { RefScope::none() } else { RefScope::All };
                let repo = app.meta.get_repo(&granted.repo, &scope).await?.ok_or_else(HttpError::not_found)?;
                let advertisement =
                    if wants_v2(headers) { upload_pack::advertise() } else { upload_pack::advertise_v0(app, &repo).await? };
                Ok(git_response("upload-pack-advertisement", advertisement))
            }
            Some("git-receive-pack") => {
                let granted = auth::authorize_git(app, headers, name, Access::Write).await?;
                // A push opens with every ref: the protocol has no way to ask for some.
                let repo = app.meta.get_repo(&granted.repo, &RefScope::All).await?;
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
        let granted = auth::authorize_git(app, headers, repo_name(name)?, Access::Read).await?;
        let body = axum::body::to_bytes(body, MAX_FETCH_REQUEST)
            .await
            .map_err(|_| HttpError::new(StatusCode::PAYLOAD_TOO_LARGE, "fetch request too large"))?;
        let body = decode_body(headers, body)?;
        let response = if wants_v2(headers) {
            upload_pack::handle(app, &granted.repo, body).await?
        } else {
            let repo = app.meta.get_repo(&granted.repo, &RefScope::none()).await?.ok_or_else(HttpError::not_found)?;
            upload_pack::handle_v0(app, repo, body).await?
        };
        return Ok(git_response("upload-pack-result", response));
    }

    if let Some(name) = path.strip_suffix("/git-receive-pack") {
        if method != Method::POST {
            return Err(method_not_allowed());
        }
        let granted = auth::authorize_git(app, headers, repo_name(name)?, Access::Write).await?;
        if headers.contains_key(header::CONTENT_ENCODING) {
            return Err(HttpError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "compressed push bodies are not supported"));
        }
        let result = receive_pack::handle(app, &granted, body).await?;
        return Ok(git_response("receive-pack-result", result));
    }

    Err(HttpError::not_found())
}

fn json(status: StatusCode, value: serde_json::Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], value.to_string() + "\n").into_response()
}

/// The management API for one repository:
///
/// - `PUT /api/repos/<id>` creates it, `DELETE` removes it, `GET` describes it
/// - `PUT /api/repos/<id>/head` points HEAD at the ref named in the body
/// - `POST /api/repos/<id>/compact` rewrites its packs into one
/// - `GET /api/repos/<id>/{refs,commits,commit,compare,tree,blob}` read its content, see [`content`]
/// - `POST /api/repos/<id>/merge` merges a revision into a branch, see [`merge`]
/// - `POST /api/repos/<id>/import` takes in a bundle: its objects and all its refs, see [`import`]
async fn repos(app: &App, method: &Method, rest: &str, query: &str, body: Body) -> Result<Response, HttpError> {
    if let Some((name, view)) = rest.rsplit_once('/').filter(|(_, view)| content::VIEWS.contains(view)) {
        let name = repo_name(name)?;
        if method != Method::GET {
            return Err(method_not_allowed());
        }
        return content::handle(app, name, view, query).await;
    }
    if let Some(name) = rest.strip_suffix("/merge") {
        let name = repo_name(name)?;
        if method != Method::POST {
            return Err(method_not_allowed());
        }
        return merge::merge(app, name, body).await;
    }
    if let Some(name) = rest.strip_suffix("/import") {
        let name = repo_name(name)?;
        if !matches!(*method, Method::POST | Method::PUT) {
            return Err(method_not_allowed());
        }
        return import::import(app, name, query, body).await;
    }
    if let Some(name) = rest.strip_suffix("/compact") {
        let name = repo_name(name)?;
        if method != Method::POST {
            return Err(method_not_allowed());
        }
        app.meta.get_repo(name, &RefScope::none()).await?.ok_or_else(HttpError::not_found)?;
        return Ok(match compact(app, name, true).await? {
            Some(report) => format!("merged {} packs into one holding {} objects\n", report.merged, report.objects).into_response(),
            None => "nothing to do\n".into_response(),
        });
    }
    if let Some(name) = rest.strip_suffix("/head") {
        let name = repo_name(name)?;
        if method != Method::PUT {
            return Err(method_not_allowed());
        }
        let body = axum::body::to_bytes(body, 4096).await.map_err(|_| HttpError::bad_request("ref name too long"))?;
        let head = std::str::from_utf8(&body).map(str::trim).map_err(|_| HttpError::bad_request("invalid ref name"))?;
        if !receive_pack::valid_ref_name(head) {
            return Err(HttpError::bad_request("HEAD must name a ref, like refs/heads/main"));
        }
        return match app.meta.commit(name, Txn { set_head: Some(head.to_string()), ..Txn::default() }).await {
            Ok(()) => Ok("updated\n".into_response()),
            Err(MetaError::NotFound) => Err(HttpError::not_found()),
            Err(e) => Err(e.into()),
        };
    }

    let name = repo_name(rest)?;
    match *method {
        Method::PUT => match app.meta.create_repo(name).await {
            Ok(_) => Ok((StatusCode::CREATED, "created\n").into_response()),
            Err(MetaError::AlreadyExists) => Err(HttpError::new(StatusCode::CONFLICT, "repository already exists")),
            Err(e) => Err(e.into()),
        },
        Method::GET => {
            let repo = app.meta.get_repo(name, &RefScope::All).await?.ok_or_else(HttpError::not_found)?;
            let packs = app.packs.open_each(&repo.pool_id, &repo.packs).await?;
            Ok(json(
                StatusCode::OK,
                serde_json::json!({
                    "repo": repo.name,
                    "head": repo.head,
                    "refs": repo.refs.len(),
                    "packs": packs.len(),
                    // An object stored in two packs counts twice, until compaction merges them.
                    "objects": packs.iter().map(|pack| pack.index().len()).sum::<usize>(),
                    "bytes": packs.iter().map(|pack| pack.size()).sum::<u64>(),
                }),
            ))
        }
        // Gone for every request from here on; the packs follow after the grace period.
        Method::DELETE => match app.meta.delete_repo(name).await? {
            true => Ok("deleted\n".into_response()),
            false => Err(HttpError::not_found()),
        },
        _ => Err(method_not_allowed()),
    }
}

/// `GET /api/events?after=<cursor>&limit=<n>`: what happened to repositories,
/// oldest first. Poll with the `next` of the previous answer.
async fn events(app: &App, query: &str) -> Result<Response, HttpError> {
    let param = |name: &str| query.split('&').filter_map(|kv| kv.split_once('=')).find(|(key, _)| *key == name).map(|(_, v)| v);
    let limit = match param("limit") {
        None => 100,
        Some(limit) => limit.parse::<usize>().ok().filter(|n| (1..=1000).contains(n)).ok_or_else(|| HttpError::bad_request("limit must be 1 to 1000"))?,
    };
    let after = param("after").filter(|cursor| !cursor.is_empty());
    let events = match app.meta.list_events(after, limit).await {
        Ok(events) => events,
        Err(MetaError::BadCursor) => return Err(HttpError::bad_request("invalid cursor")),
        Err(e) => return Err(e.into()),
    };
    // Nothing new: keep polling from the same place.
    let next = events.last().map(|event| event.cursor.clone()).or_else(|| after.map(str::to_string));
    let events: Vec<serde_json::Value> = events
        .into_iter()
        .map(|event| {
            let detail = serde_json::from_str(&event.payload).unwrap_or(serde_json::Value::Null);
            serde_json::json!({ "cursor": event.cursor, "repo": event.repo, "time": event.created_at, "event": detail })
        })
        .collect();
    Ok(json(StatusCode::OK, serde_json::json!({ "events": events, "next": next })))
}

/// Normalises `owner/repo.git` to `owner/repo` and rejects anything that is
/// not a plain slash-separated name.
pub(crate) fn repo_name(raw: &str) -> Result<&str, HttpError> {
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
