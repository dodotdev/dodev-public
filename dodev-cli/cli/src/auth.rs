use std::env;

use crate::config;

/// Errors that can occur when resolving the auth token.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Not logged in. Run `dodev login` to authenticate.")]
    NotFound,
}

/// Resolve an auth token (API key or session token) for relay authentication.
///
/// Priority:
/// 1. `DODEV_API_KEY` environment variable (backwards compat)
/// 2. `DODEV_SESSION_TOKEN` environment variable
/// 3. Config file session_token at `~/.dodev/config.toml`
/// 4. Config file api_key at `~/.dodev/config.toml` (legacy)
pub fn get_auth_token() -> Result<String, AuthError> {
    // 1. Check DODEV_API_KEY environment variable (backwards compat)
    if let Ok(key) = env::var("DODEV_API_KEY") {
        if !key.is_empty() {
            tracing::debug!("Using API key from DODEV_API_KEY env var");
            return Ok(key);
        }
    }

    // 2. Check DODEV_SESSION_TOKEN environment variable
    if let Ok(token) = env::var("DODEV_SESSION_TOKEN") {
        if !token.is_empty() {
            tracing::debug!("Using session token from DODEV_SESSION_TOKEN env var");
            return Ok(token);
        }
    }

    // 3. Check config file for session_token
    if let Some(session) = config::load_session() {
        tracing::debug!("Using session token from config file");
        return Ok(session.session_token);
    }

    // 4. Check config file for legacy api_key
    if let Some(cfg) = config::load_config() {
        if let Some(auth) = cfg.auth {
            if let Some(key) = auth.api_key {
                if !key.is_empty() {
                    tracing::debug!("Using API key from config file");
                    return Ok(key);
                }
            }
        }
    }

    Err(AuthError::NotFound)
}
