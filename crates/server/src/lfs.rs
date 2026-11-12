//! Git LFS: files too large for packs, kept whole in object storage under
//! the SHA-256 of their content, beside the repository's packs.
//!
//! - `POST <repo>.git/info/lfs/objects/batch`: the LFS batch API. Says where
//!   each file is fetched from or sent to, which is the two routes below.
//! - `GET <repo>.git/info/lfs/objects/<oid>`
//! - `PUT <repo>.git/info/lfs/objects/<oid>`: stored once its content has
//!   been checked to hash to `<oid>`.
//! - `GET /api/repos/<id>/lfs/<oid>`, with the admin token, for the application.
//!
//! Who may do what is decided as for git itself: a download is a fetch, an
//! upload is a push. A client that came in over SSH is given a ticket for
//! these routes instead (`git-lfs-authenticate`), signed by the node.
//!
//! Only the basic transfer is spoken; there are no locks. A file stays until
//! its repository is deleted, whether or not any commit still points at it.

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use strata_store::RefScope;

use crate::auth::{self, Access, Granted, Presented};
use crate::{blocking, receive_pack, repo_name, App, HttpError};

const MEDIA_TYPE: &str = "application/vnd.git-lfs+json";
/// How long a ticket handed out over SSH is good for.
const TICKET_SECONDS: u64 = 3600;
/// Files asked about in one batch.
const MAX_BATCH: usize = 1000;

fn valid_oid(oid: &str) -> bool {
    oid.len() == 64 && oid.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// LFS clients read errors as JSON.
fn error(e: HttpError) -> Response {
    let mut response = (e.status, [(header::CONTENT_TYPE, MEDIA_TYPE)], json!({ "message": e.message }).to_string()).into_response();
    if e.status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert("lfs-authenticate", "Basic realm=\"strata\"".parse().expect("valid header"));
        response.headers_mut().insert(header::WWW_AUTHENTICATE, "Basic realm=\"strata\"".parse().expect("valid header"));
    }
    response
}

fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner = Sha256::new().chain_update(block.map(|b| b ^ 0x36)).chain_update(message).finalize();
    Sha256::new().chain_update(block.map(|b| b ^ 0x5c)).chain_update(inner).finalize().into()
}

/// What tickets are signed with: every node with the same secrets signs alike.
fn ticket_key(app: &App) -> Option<Vec<u8>> {
    let secret = app.options.admin_token.as_ref().or(app.options.hook_secret.as_ref())?;
    Some(hmac(secret.as_bytes(), b"strata lfs tickets").to_vec())
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A ticket that lets its holder use the LFS routes for `path`, as `granted` was allowed to.
fn ticket(key: &[u8], path: &str, granted: &Granted, access: Access) -> String {
    let claims = json!({ "path": path, "repo": granted.repo, "actor": granted.actor, "write": access == Access::Write, "exp": now() + TICKET_SECONDS });
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
    let signature = hmac(key, body.as_bytes());
    format!("{body}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature))
}

fn redeem(key: &[u8], ticket: &str, path: &str, access: Access) -> Option<Granted> {
    let (body, signature) = ticket.split_once('.')?;
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(signature).ok()?;
    // Compared as hashes, so that it takes as long wherever they differ.
    if Sha256::digest(hmac(key, body.as_bytes())) != Sha256::digest(&signature) {
        return None;
    }
    let claims: Value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body).ok()?).ok()?;
    let allowed = claims["path"] == path && claims["exp"].as_u64()? > now() && (access == Access::Read || claims["write"] == true);
    allowed.then(|| Granted { repo: claims["repo"].as_str().unwrap_or_default().to_string(), actor: claims["actor"].as_str().map(str::to_string) })
}

/// The answer to `git-lfs-authenticate <path> <operation>` over SSH: where
/// the LFS routes are, and the ticket to show there.
pub(crate) async fn authenticate(app: &App, presented: Presented<'_>, path: &str, operation: &str) -> Result<String, HttpError> {
    let access = match operation {
        "download" => Access::Read,
        "upload" => Access::Write,
        _ => return Err(HttpError::bad_request("the operation is download or upload")),
    };
    let name = repo_name(path)?;
    let granted = auth::authorize_git(app, presented, name, access).await?;
    let Some(base) = &app.options.public_url else {
        return Err(HttpError::new(StatusCode::NOT_IMPLEMENTED, "large files over SSH need the server to be told its HTTP address (--public-url)"));
    };
    let mut answer = json!({ "href": format!("{base}/{name}.git/info/lfs"), "expires_in": TICKET_SECONDS });
    if let Some(key) = ticket_key(app) {
        answer["header"] = json!({ "Authorization": format!("RemoteAuth {}", ticket(&key, name, &granted, access)) });
    }
    Ok(answer.to_string())
}

async fn authorize(app: &App, headers: &HeaderMap, name: &str, access: Access) -> Result<Granted, HttpError> {
    let presented = Presented::by_http(headers);
    if let Some(shown) = presented.authorization.and_then(|value| value.strip_prefix("RemoteAuth ")) {
        return ticket_key(app).and_then(|key| redeem(&key, shown.trim(), name, access)).ok_or_else(HttpError::unauthorized);
    }
    auth::authorize_git(app, presented, name, access).await
}

/// Where clients reach this node, for the addresses a batch answers with.
fn base_url(app: &App, headers: &HeaderMap) -> String {
    if let Some(base) = &app.options.public_url {
        return base.clone();
    }
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let scheme = header("x-forwarded-proto").filter(|scheme| *scheme == "https").unwrap_or("http");
    format!("{scheme}://{}", header("host").unwrap_or("localhost"))
}

#[derive(Deserialize)]
struct Batch {
    operation: String,
    #[serde(default)]
    transfers: Vec<String>,
    objects: Vec<Wanted>,
    hash_algo: Option<String>,
}

#[derive(Deserialize)]
struct Wanted {
    oid: String,
    size: u64,
}

/// Routes below `<repo>/info/lfs/`. `rest` is what follows that in the path.
pub(crate) async fn handle(app: &App, method: &Method, raw_name: &str, rest: &str, headers: &HeaderMap, body: Body) -> Response {
    match respond(app, method, raw_name, rest, headers, body).await {
        Ok(response) => response,
        Err(e) => error(e),
    }
}

async fn respond(app: &App, method: &Method, raw_name: &str, rest: &str, headers: &HeaderMap, body: Body) -> Result<Response, HttpError> {
    let name = repo_name(raw_name)?;
    if rest == "objects/batch" {
        if method != Method::POST {
            return Err(crate::method_not_allowed());
        }
        return batch(app, name, headers, body).await;
    }
    let oid = rest.strip_prefix("objects/").filter(|oid| valid_oid(oid)).ok_or_else(HttpError::not_found)?;
    match *method {
        Method::GET => {
            let granted = authorize(app, headers, name, Access::Read).await?;
            download(app, &granted.repo, oid).await
        }
        Method::PUT => {
            let granted = authorize(app, headers, name, Access::Write).await?;
            upload(app, &granted.repo, oid, body).await
        }
        _ => Err(crate::method_not_allowed()),
    }
}

async fn pool_of(app: &App, repo: &str) -> Result<String, HttpError> {
    Ok(app.meta.get_repo(repo, &RefScope::none()).await?.ok_or_else(HttpError::not_found)?.pool_id)
}

async fn batch(app: &App, name: &str, headers: &HeaderMap, body: Body) -> Result<Response, HttpError> {
    let body = axum::body::to_bytes(body, 4 << 20).await.map_err(|_| HttpError::new(StatusCode::PAYLOAD_TOO_LARGE, "batch request too large"))?;
    let Ok(batch) = serde_json::from_slice::<Batch>(&body) else {
        // Who is asking comes before what is wrong with the question.
        authorize(app, headers, name, Access::Read).await?;
        return Err(HttpError::bad_request("malformed batch request"));
    };
    let upload = match batch.operation.as_str() {
        "upload" => true,
        "download" => false,
        _ => return Err(HttpError::bad_request("the operation is download or upload")),
    };
    let granted = authorize(app, headers, name, if upload { Access::Write } else { Access::Read }).await?;
    if !batch.transfers.is_empty() && !batch.transfers.iter().any(|transfer| transfer == "basic") {
        return Err(HttpError::new(StatusCode::UNPROCESSABLE_ENTITY, "only the basic transfer is offered"));
    }
    if batch.hash_algo.as_deref().is_some_and(|algo| algo != "sha256") {
        return Err(HttpError::new(StatusCode::CONFLICT, "only sha256 is offered"));
    }
    if batch.objects.len() > MAX_BATCH {
        return Err(HttpError::new(StatusCode::PAYLOAD_TOO_LARGE, "too many files in one batch"));
    }
    let pool = pool_of(app, &granted.repo).await?;
    let base = base_url(app, headers);
    // The transfer is let in the way the batch was: with what the client showed here.
    let shown: Value = match Presented::by_http(headers).authorization {
        Some(authorization) => json!({ "Authorization": authorization }),
        None => json!({}),
    };
    let mut objects = Vec::new();
    for wanted in &batch.objects {
        let mut object = json!({ "oid": wanted.oid, "size": wanted.size, "authenticated": true });
        if !valid_oid(&wanted.oid) {
            object["error"] = json!({ "code": 422, "message": "not a SHA-256 id" });
            objects.push(object);
            continue;
        }
        let href = format!("{base}/{name}.git/info/lfs/objects/{}", wanted.oid);
        let action = json!({ "href": href, "header": shown, "expires_in": TICKET_SECONDS });
        match (app.packs.lfs_size(&pool, &wanted.oid).await?, upload) {
            // Here already: nothing for the client to send.
            (Some(_), true) => {}
            (None, true) => object["actions"] = json!({ "upload": action }),
            (Some(size), false) => {
                object["size"] = json!(size);
                object["actions"] = json!({ "download": action });
            }
            (None, false) => object["error"] = json!({ "code": 404, "message": "this file is not here" }),
        }
        objects.push(object);
    }
    let answer = json!({ "transfer": "basic", "objects": objects, "hash_algo": "sha256" });
    Ok(([(header::CONTENT_TYPE, MEDIA_TYPE)], answer.to_string()).into_response())
}

async fn download(app: &App, repo: &str, oid: &str) -> Result<Response, HttpError> {
    let pool = pool_of(app, repo).await?;
    let found = app.packs.lfs_get(&pool, oid).await?.ok_or_else(|| HttpError::new(StatusCode::NOT_FOUND, "this file is not here"))?;
    let size = found.meta.size as u64;
    app.metrics.lfs("download", size);
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size)
        .header(header::ETAG, format!("\"{oid}\""))
        .body(Body::from_stream(found.into_stream()))
        .expect("static headers are valid"))
}

async fn upload(app: &App, repo: &str, oid: &str, body: Body) -> Result<Response, HttpError> {
    let pool = pool_of(app, repo).await?;
    if app.packs.lfs_size(&pool, oid).await?.is_some() {
        return Ok(StatusCode::OK.into_response());
    }
    // Onto local disk first: a file is only stored under an id its content has.
    let spooled = receive_pack::spool(app, bytes::Bytes::new(), body.into_data_stream()).await?.map(std::sync::Arc::new);
    let size = spooled.as_ref().map_or(0, |file| strata_git::PackData::size(file.as_ref()));
    let hashed = spooled.clone();
    let digest = blocking(move || -> strata_git::Result<String> {
        let mut hasher = Sha256::new();
        let mut offset = 0;
        if let Some(file) = &hashed {
            while offset < size {
                let chunk = strata_git::PackData::chunk(file.as_ref(), offset)?;
                let piece = &chunk[..chunk.len().min(8 << 20)];
                hasher.update(piece);
                offset += piece.len() as u64;
            }
        }
        Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
    })
    .await??;
    if digest != oid {
        return Err(HttpError::new(StatusCode::UNPROCESSABLE_ENTITY, "what was sent does not hash to the id it was sent as"));
    }
    app.packs.lfs_put(&pool, oid, spooled.map(|file| file as std::sync::Arc<dyn strata_git::PackData>)).await?;
    app.metrics.lfs("upload", size);
    Ok(StatusCode::OK.into_response())
}

/// `GET /api/repos/<id>/lfs/<oid>`: a large file, for the application to show or hand on.
pub(crate) async fn read(app: &App, repo: &str, oid: &str) -> Result<Response, HttpError> {
    if !valid_oid(oid) {
        return Err(HttpError::bad_request("malformed object id"));
    }
    download(app, repo, oid).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_the_published_test_vector() {
        // RFC 4231, test case 2.
        let mac = hmac(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn a_ticket_is_good_for_its_repository_and_no_more_than_it_says() {
        let key = b"key";
        let granted = Granted { repo: "r-1".into(), actor: Some("alice".into()) };
        let read = ticket(key, "acme/app", &granted, Access::Read);
        assert_eq!(redeem(key, &read, "acme/app", Access::Read).unwrap().repo, "r-1");
        assert!(redeem(key, &read, "acme/app", Access::Write).is_none());
        assert!(redeem(key, &read, "acme/other", Access::Read).is_none());
        assert!(redeem(b"another", &read, "acme/app", Access::Read).is_none());
        let write = ticket(key, "acme/app", &granted, Access::Write);
        assert_eq!(redeem(key, &write, "acme/app", Access::Read).unwrap().actor.as_deref(), Some("alice"));
        let (body, signature) = write.split_once('.').unwrap();
        assert!(redeem(key, &format!("{body}x.{signature}"), "acme/app", Access::Write).is_none());
    }
}
