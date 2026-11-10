//! Token authentication and per-repository authorization.
//!
//! Git clients send tokens as the password of HTTP Basic credentials; scripts
//! may use a Bearer header instead.

use axum::http::{header, HeaderMap, StatusCode};
use base64::Engine;
use sha2::{Digest, Sha256};
use strata_store::Access;

use crate::{App, HttpError};

/// What a request is about to do.
pub(crate) enum Need<'a> {
    Repo(&'a str, Access),
    /// Managing tokens: reserved to holders of admin access to everything.
    Tokens,
}

pub(crate) fn hash_secret(secret: &str) -> String {
    Sha256::digest(secret.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
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

/// Checks that the request may do what it is about to. Runs before anything
/// reveals whether a repository exists.
pub(crate) async fn authorize(app: &App, headers: &HeaderMap, need: Need<'_>) -> Result<(), HttpError> {
    let Some(admin_token) = &app.options.admin_token else {
        // No admin token configured: authentication is switched off.
        return Ok(());
    };
    if app.options.anonymous_read && matches!(need, Need::Repo(_, Access::Read)) {
        return Ok(());
    }
    let Some(secret) = presented_secret(headers) else {
        return Err(HttpError::unauthorized());
    };
    let hash = hash_secret(&secret);
    // Comparing hashes keeps the comparison independent of where the secrets differ.
    if hash == hash_secret(admin_token) {
        return Ok(());
    }
    let Some(token) = app.meta.find_token(&hash).await? else {
        return Err(HttpError::unauthorized());
    };
    let allowed = match need {
        Need::Repo(repo, access) => token.grants.iter().any(|grant| grant.covers(repo) && grant.access >= access),
        Need::Tokens => token.grants.iter().any(|grant| grant.pattern == "*" && grant.access == Access::Admin),
    };
    if allowed {
        Ok(())
    } else {
        Err(HttpError::new(StatusCode::FORBIDDEN, "this token does not grant that access"))
    }
}
