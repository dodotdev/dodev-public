use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Top-level config structure stored at `~/.dodev/config.toml`.
/// Shared across all dodev CLI subcommands.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub auth: Option<AuthConfig>,
    pub defaults: Option<DefaultsConfig>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    /// Legacy API key (backwards compat with DODEV_API_KEY)
    pub api_key: Option<String>,
    /// Session token from browser-based OAuth login
    pub session_token: Option<String>,
    /// User ID from the identity provider
    pub user_id: Option<String>,
    /// User email address
    pub email: Option<String>,
    /// User display name
    pub name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct DefaultsConfig {
    pub relay_url: Option<String>,
}

/// Session info returned from the config file.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_token: String,
    pub user_id: String,
    pub email: String,
    pub name: String,
}

/// Return the path to the config file: `~/.dodev/config.toml`
/// (or the platform-appropriate config directory).
pub fn config_path() -> Option<PathBuf> {
    // Prefer ~/.dodev for simplicity and cross-platform consistency.
    if let Some(home) = dirs::home_dir() {
        return Some(home.join(".dodev").join("config.toml"));
    }
    // Fallback to dirs::config_dir() / "localdev" / "config.toml"
    dirs::config_dir().map(|d| d.join("localdev").join("config.toml"))
}

/// Load the config file. Returns `None` if the file does not exist or cannot be parsed.
pub fn load_config() -> Option<AppConfig> {
    let path = config_path()?;
    let content = fs::read_to_string(&path).ok()?;
    match toml::from_str::<AppConfig>(&content) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!("Failed to parse config at {}: {}", path.display(), e);
            None
        }
    }
}

/// Save a session token and user info to the config file.
///
/// Preserves existing config values (e.g. `defaults.relay_url`, `auth.api_key`).
pub fn save_session(
    token: &str,
    user_id: &str,
    email: &str,
    name: &str,
) -> Result<(), ConfigError> {
    let path = config_path().ok_or(ConfigError::NoConfigDir)?;

    let mut config = load_config().unwrap_or_default();

    let auth = config.auth.get_or_insert_with(AuthConfig::default);
    auth.session_token = Some(token.to_string());
    auth.user_id = Some(user_id.to_string());
    auth.email = Some(email.to_string());
    auth.name = Some(name.to_string());

    write_config(&path, &config)
}

/// Load the saved session info from the config file.
///
/// Returns `None` if no session token is stored.
pub fn load_session() -> Option<SessionInfo> {
    let cfg = load_config()?;
    let auth = cfg.auth?;
    let session_token = auth.session_token?;

    if session_token.is_empty() {
        return None;
    }

    Some(SessionInfo {
        session_token,
        user_id: auth.user_id.unwrap_or_default(),
        email: auth.email.unwrap_or_default(),
        name: auth.name.unwrap_or_default(),
    })
}

/// Remove all session fields from the config file.
///
/// Preserves other config values (e.g. `defaults.relay_url`, `auth.api_key`).
pub fn clear_session() -> Result<(), ConfigError> {
    let path = config_path().ok_or(ConfigError::NoConfigDir)?;

    if !path.exists() {
        return Ok(());
    }

    let mut config = load_config().unwrap_or_default();

    if let Some(ref mut auth) = config.auth {
        auth.session_token = None;
        auth.user_id = None;
        auth.email = None;
        auth.name = None;
    }

    write_config(&path, &config)
}

/// Save an API key to the config file.
///
/// Preserves existing config values (e.g. `defaults.relay_url`).
pub fn save_api_key(key: &str) -> Result<(), ConfigError> {
    let path = config_path().ok_or(ConfigError::NoConfigDir)?;

    // Load existing config or create a new one
    let mut config = load_config().unwrap_or_default();

    // Update the auth section
    let auth = config.auth.get_or_insert_with(AuthConfig::default);
    auth.api_key = Some(key.to_string());

    write_config(&path, &config)
}

/// Remove the API key from the config file.
pub fn clear_api_key() -> Result<(), ConfigError> {
    let path = config_path().ok_or(ConfigError::NoConfigDir)?;

    if !path.exists() {
        // Nothing to clear
        return Ok(());
    }

    let mut config = load_config().unwrap_or_default();

    if let Some(ref mut auth) = config.auth {
        auth.api_key = None;
    }

    write_config(&path, &config)
}

/// Write the config struct to disk, creating directories as needed.
fn write_config(path: &PathBuf, config: &AppConfig) -> Result<(), ConfigError> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| ConfigError::Io(e.to_string()))?;
    }

    let content = toml::to_string_pretty(config).map_err(|e| ConfigError::Serialize(e.to_string()))?;
    fs::write(path, content).map_err(|e| ConfigError::Io(e.to_string()))?;

    // Set restrictive permissions on Unix (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, perms).map_err(|e| ConfigError::Io(e.to_string()))?;
    }

    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Cannot determine config directory")]
    NoConfigDir,

    #[error("IO error: {0}")]
    Io(String),

    #[error("Serialization error: {0}")]
    Serialize(String),
}
