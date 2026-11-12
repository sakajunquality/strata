//! Who may do what.
//!
//! Git requests are decided by the application's authorization hook when one
//! is configured: it sees the requested path and the client's credentials,
//! and answers with the repository to use. Without a hook, the admin token
//! is the only credential there is. The management API always takes the
//! admin token.

use axum::http::{header, HeaderMap, StatusCode};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{repo_name, App, HttpError};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Access {
    Read,
    Write,
}

/// A git request that may go ahead.
#[derive(Clone, Debug)]
pub(crate) struct Granted {
    /// The repository to act on. With a hook, this is whatever it mapped the
    /// requested path to: typically an id that survives renames.
    pub repo: String,
    /// Who is acting, as the hook named them. Recorded with pushes.
    pub actor: Option<String>,
}

/// A remembered answer of the authorization hook.
#[derive(Clone)]
pub(crate) enum Decision {
    Yes(Granted),
    No(StatusCode, String),
}

#[derive(Serialize)]
struct Question<'a> {
    path: &'a str,
    access: &'a str,
    /// The client's `Authorization` header, untouched.
    authorization: Option<&'a str>,
}

#[derive(Deserialize)]
struct Answer {
    allow: bool,
    repo: Option<String>,
    actor: Option<String>,
    status: Option<u16>,
    message: Option<String>,
}

fn sha256(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn presented_secret(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(token) = value.strip_prefix("Bearer ") {
        return Some(token.trim().to_string());
    }
    let encoded = value.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, password) = decoded.split_once(':').unwrap_or((&decoded, ""));
    // Some tools put the token in the user name and leave the password empty.
    Some(if password.is_empty() { user } else { password }.to_string())
}

fn is_admin(app: &App, headers: &HeaderMap) -> Result<bool, HttpError> {
    let Some(admin_token) = &app.options.admin_token else {
        // No admin token configured: authentication is switched off.
        return Ok(true);
    };
    let Some(secret) = presented_secret(headers) else {
        return Err(HttpError::unauthorized());
    };
    // Comparing hashes keeps the comparison independent of where the secrets differ.
    Ok(sha256(&[&secret]) == sha256(&[admin_token]))
}

/// For the management API.
pub(crate) fn authorize_admin(app: &App, headers: &HeaderMap) -> Result<(), HttpError> {
    match is_admin(app, headers)? {
        true => Ok(()),
        false => Err(HttpError::unauthorized()),
    }
}

/// For git requests. Runs before anything reveals whether a repository exists.
pub(crate) async fn authorize_git(app: &App, headers: &HeaderMap, path: &str, access: Access) -> Result<Granted, HttpError> {
    let Some(url) = &app.options.authz_url else {
        let open = app.options.anonymous_read && access == Access::Read;
        if open || is_admin(app, headers)? {
            return Ok(Granted { repo: path.to_string(), actor: None });
        }
        return Err(HttpError::unauthorized());
    };

    let authorization = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    let access_name = if access == Access::Read { "read" } else { "write" };
    let key = sha256(&[path, access_name, authorization.unwrap_or("")]);
    let remembered = app.decisions.as_ref().and_then(|cache| cache.get(&key));
    app.metrics.authz_cache(remembered.is_some());
    match remembered {
        Some(Decision::Yes(granted)) => return Ok(granted),
        Some(Decision::No(status, message)) => return Err(HttpError::new(status, message)),
        None => {}
    }
    let question = Question { path, access: access_name, authorization };
    let answer: Answer = app.hooks.ask("authz", url, &question).await.map_err(|e| {
        tracing::error!("authorization hook failed: {e:#}");
        HttpError::new(StatusCode::SERVICE_UNAVAILABLE, "authorization is unavailable, try again")
    })?;
    if !answer.allow {
        // Without credentials, "who are you?" makes git ask for them.
        let fallback = if authorization.is_some() { StatusCode::FORBIDDEN } else { StatusCode::UNAUTHORIZED };
        let status = match answer.status {
            Some(401) => StatusCode::UNAUTHORIZED,
            Some(403) => StatusCode::FORBIDDEN,
            Some(404) => StatusCode::NOT_FOUND,
            _ => fallback,
        };
        let message = answer.message.unwrap_or_else(|| "access denied".into());
        // git asks without credentials first, every time. That one "no" is
        // worth remembering; a "no" to someone in particular is not, or they
        // would stay locked out for a while after being let in.
        if let (Some(cache), None) = (&app.decisions, authorization) {
            cache.insert(key, Decision::No(status, message.clone()));
        }
        return Err(HttpError::new(status, message));
    }
    let repo = match answer.repo {
        None => path.to_string(),
        Some(repo) => repo_name(&repo)
            .map_err(|_| {
                tracing::error!(repo, "authorization hook named an invalid repository");
                HttpError::new(StatusCode::BAD_GATEWAY, "authorization gave an invalid answer")
            })?
            .to_string(),
    };
    let granted = Granted { repo, actor: answer.actor };
    // Remembered only briefly: a revoked permission lingers that long.
    if let Some(cache) = &app.decisions {
        cache.insert(key, Decision::Yes(granted.clone()));
    }
    Ok(granted)
}
