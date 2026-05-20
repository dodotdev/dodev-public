mod active;
mod auth;
mod config;
mod display;
mod proxy;
mod tunnel;

use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(
    name = "dodev",
    version,
    about = format!("do.dev CLI v{} — local.dev tunnels and more", env!("CARGO_PKG_VERSION"))
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Enable verbose logging
    #[arg(long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate with your do.dev account
    Login,

    /// Remove saved authentication
    Logout,

    /// Show current auth status
    Status,

    /// local.dev tunnel service
    Local {
        #[command(subcommand)]
        command: LocalCommands,

        /// Override the WebSocket URL. By default this is derived from
        /// --subdomain as `wss://<subdomain>.local.dev/_dodev/connect`.
        /// Only set this if you need to point at a non-prod environment.
        #[arg(long, global = true)]
        ws_url: Option<String>,
    },

    /// Update dodev to the latest release. Only works for binaries
    /// installed via the install.sh / install.ps1 installer.
    Update,
}

#[derive(Subcommand)]
enum LocalCommands {
    /// Expose a local HTTP port (e.g. `dodev local http 3000`)
    Http {
        /// Local port to expose
        #[arg(default_value = "3000")]
        port: u16,

        /// Request a specific subdomain
        #[arg(short, long)]
        subdomain: Option<String>,

        /// Local host to forward to
        #[arg(long, default_value = "localhost")]
        host: String,
    },

    /// Show configuration
    Config,
}

fn init_tracing(verbose: bool) {
    // Default level is WARN so axoupdater's "INFO exec env …" and other
    // dependency chatter doesn't clutter user output. The CLI's own
    // human-facing status (e.g. `dodev update` progress) uses
    // `println!`/`display::*`, not tracing, so those still show up.
    // `--verbose` opens it up to DEBUG for actual diagnostics.
    let level = if verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::WARN
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .init();
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            println!();
            return;
        }
    };

    let result = match command {
        Commands::Login => cmd_login().await,
        Commands::Logout => cmd_logout(),
        Commands::Status => cmd_status(),
        Commands::Local { command, ws_url } => match command {
            LocalCommands::Http {
                port,
                subdomain,
                host,
            } => cmd_start(ws_url, port, subdomain, host).await,
            LocalCommands::Config => cmd_config(ws_url),
        },
        Commands::Update => cmd_update().await,
    };

    if let Err(e) = result {
        display::print_error(&e);
        std::process::exit(1);
    }
}

/// Self-update via axoupdater. Reads the install receipt that
/// cargo-dist's installer wrote and replaces the running binary
/// with the latest GitHub release.
async fn cmd_update() -> Result<(), String> {
    use axoupdater::AxoUpdater;

    let mut updater = AxoUpdater::new_for("dodev-cli");

    // load_receipt() looks for the install receipt cargo-dist's
    // installer wrote. Missing => not installed via the official
    // installer (cargo install, source build, manual copy, etc).
    if let Err(e) = updater.load_receipt() {
        return Err(format!(
            "No install receipt found ({}). `dodev update` only works for binaries installed via https://local.dev/install.sh — reinstall via that script to enable self-update.",
            e
        ));
    }

    // Suppress the subprocess installer's chatter (downloading, paths,
    // "everything's installed!"). The CLI prints its own clean status.
    updater.disable_installer_output();

    print!("  Updating…");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    let result = updater.run().await;
    println!();

    match result {
        Ok(Some(result)) => {
            println!("  \u{2714} Updated dodev to v{}", result.new_version);
            println!();
            Ok(())
        }
        Ok(None) => {
            println!("  \u{2714} Already on the latest release.");
            println!();
            Ok(())
        }
        Err(e) => Err(format!("Update failed: {}", e)),
    }
}

/// Start a tunnel connection.
///
/// Subdomain selection:
/// 1. --ws-url override (advanced/non-prod) wins absolutely.
/// 2. Else, hit /api/cli/me to find out which subdomains this user owns.
/// 3. If --subdomain was passed and the user owns it → use it.
/// 4. If --subdomain was passed but not owned → fail fast with a friendly
///    message listing what they DO own.
/// 5. Else (no flag) → use the user's first assigned random subdomain.
async fn cmd_start(
    ws_url_override: Option<String>,
    port: u16,
    subdomain: Option<String>,
    host: String,
) -> Result<(), String> {
    let token = auth::get_auth_token().map_err(|e| e.to_string())?;

    // --ws-url is for power users hitting a non-prod env. It bypasses the
    // me lookup entirely — they specified an exact URL, we honour it.
    if let Some(ws_url) = ws_url_override {
        let chosen = subdomain.unwrap_or_else(|| "lab".to_string());
        return tunnel::run_tunnel(tunnel::TunnelConfig {
            ws_url,
            token,
            subdomain: chosen,
            local_host: host,
            local_port: port,
        })
        .await
        .map_err(|e| e.to_string());
    }

    let me = fetch_me(&token).await.map_err(|e| e.to_string())?;
    let owned: Vec<String> = me
        .assigned_subdomains
        .iter()
        .chain(me.reserved_subdomains.iter())
        .cloned()
        .collect();

    let chosen = match subdomain {
        Some(requested) => {
            // Accept either a flat owned subdomain ("dev") or a single-level
            // nested form under an owned namespace ("talk.dev" or "pbx.dev").
            // The Worker keys each nested hostname to its own Durable Object,
            // so different leaves can run on different ports from different
            // CLI processes — exactly the higher-plan upsell pitch.
            let parts: Vec<&str> = requested.split('.').collect();
            let owns_it = match parts.as_slice() {
                [single] => owned.iter().any(|s| s.eq_ignore_ascii_case(single)),
                [_leaf, namespace] => owned.iter().any(|s| s.eq_ignore_ascii_case(namespace)),
                _ => false, // 3+ levels deep — Worker rejects, fail fast here too
            };
            if !owns_it {
                return Err(format!(
                    "You don't own '{}.local.dev'. Subdomains you can use:\n{}\n\nFor nested subdomains, claim a namespace at https://local.dev/dashboard/subdomains then run `dodev local http <port> -s <leaf>.<namespace>` — e.g. `talk.dev`, `pbx.dev`.",
                    requested,
                    if owned.is_empty() {
                        "  (none yet — try `dodev login` then re-run)".to_string()
                    } else {
                        owned
                            .iter()
                            .map(|s| format!("  - {}.local.dev (and *.{}.local.dev)", s, s))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                ));
            }
            if active::is_in_use(&requested) {
                return Err(format!(
                    "'{}.local.dev' is already in use by another dodev tunnel on this machine. Stop that one (Ctrl+C) or pick a different subdomain with -s.",
                    requested
                ));
            }
            requested
        }
        None => {
            // Auto-pick: first owned subdomain not currently in use.
            // Reserved names (claimed namespaces) come AFTER assigned ones
            // because they're typically more memorable — we want random
            // subdomains used for ephemeral tunnels and reserved ones
            // saved for user-facing work.
            let free: Vec<&String> = owned.iter().filter(|s| !active::is_in_use(s)).collect();
            match free.first() {
                Some(s) => (*s).clone(),
                None if owned.is_empty() => {
                    return Err(
                        "No subdomains assigned to your account yet. Try `dodev login` to refresh, or contact support@do.dev.".to_string()
                    );
                }
                None => {
                    return Err(format!(
                        "All {} of your subdomains are in use by other dodev tunnels on this machine:\n{}\n\nStop one with Ctrl+C, or upgrade for more concurrent tunnels: https://local.dev/pricing",
                        owned.len(),
                        owned
                            .iter()
                            .map(|s| format!("  - {}.local.dev", s))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
            }
        }
    };

    // Acquire the per-subdomain lockfile so a second `dodev local http` in
    // another terminal will skip this one. Held for the lifetime of the
    // tunnel; cleaned up via Drop on graceful exit. If acquire fails
    // (filesystem error), log a warning but proceed — the tunnel still
    // works, we just can't track it.
    let _lock = match active::acquire(&chosen) {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!("Couldn't write tunnel lock for '{}': {}", chosen, e);
            None
        }
    };

    println!("  Using subdomain: {}.local.dev", chosen);

    let ws_url = format!("wss://{}.local.dev/_dodev/connect", chosen);
    tunnel::run_tunnel(tunnel::TunnelConfig {
        ws_url,
        token,
        subdomain: chosen,
        local_host: host,
        local_port: port,
    })
    .await
    .map_err(|e| e.to_string())
}

#[derive(serde::Deserialize)]
struct MeResponse {
    #[allow(dead_code)]
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "assignedSubdomains")]
    assigned_subdomains: Vec<String>,
    #[serde(default, rename = "reservedSubdomains")]
    reserved_subdomains: Vec<String>,
}

async fn fetch_me(token: &str) -> Result<MeResponse, String> {
    let url = format!("{}/api/cli/me", AUTH_BASE_URL);
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("Failed to reach {}: {}", url, e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Account lookup failed ({}): {}",
            status,
            body.chars().take(200).collect::<String>()
        ));
    }

    resp.json::<MeResponse>()
        .await
        .map_err(|e| format!("Failed to parse account response: {}", e))
}

// ---------------------------------------------------------------
// Login via browser-based OAuth
// ---------------------------------------------------------------

/// The auth server base URL.
const AUTH_BASE_URL: &str = "https://local.dev";

/// Data received from the OAuth callback.
struct CallbackData {
    token: String,
    email: String,
    name: String,
    user_id: String,
}

/// Authenticate via browser-based OAuth flow.
///
/// 1. Start a local HTTP server on a random port.
/// 2. Open the browser to local.dev/auth/cli?port=PORT.
/// 3. Wait for the callback with session token and user info.
/// 4. Save the session to the config file.
async fn cmd_login() -> Result<(), String> {
    // 1. Start local callback server
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("Failed to start callback server: {}", e))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Failed to get listener address: {}", e))?
        .port();

    let auth_url = format!("{}/auth/cli?port={}", AUTH_BASE_URL, port);

    // 2. Open browser
    println!();
    println!("  Opening browser for authentication...");
    println!();

    if let Err(e) = open_browser(&auth_url) {
        tracing::debug!("Failed to open browser: {}", e);
    }

    println!("  If the browser doesn't open, visit:");
    println!("  {}", auth_url);
    println!();
    println!("  Waiting for authentication...");

    // 3. Wait for callback (120 second timeout)
    let callback = wait_for_callback(listener, 120).await?;

    // 4. Save session to config
    config::save_session(&callback.token, &callback.user_id, &callback.email, &callback.name)
        .map_err(|e| format!("Failed to save session: {}", e))?;

    let display_name = if callback.email.is_empty() {
        "authenticated user".to_string()
    } else {
        callback.email
    };

    println!();
    println!("  {} Logged in as {}", "\u{2714}", display_name);
    println!("  Run `dodev local http 3000` to create a tunnel.");
    println!();

    Ok(())
}

/// Wait for the OAuth callback on the local server with a timeout.
async fn wait_for_callback(listener: TcpListener, timeout_secs: u64) -> Result<CallbackData, String> {
    let result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        accept_callback(listener),
    )
    .await;

    match result {
        Ok(Ok(data)) => Ok(data),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("Login timed out. Please try again.".to_string()),
    }
}

/// Accept a single HTTP request on the callback server and extract session data.
async fn accept_callback(listener: TcpListener) -> Result<CallbackData, String> {
    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|e| format!("Failed to accept connection: {}", e))?;

    let mut buf = vec![0u8; 8192];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| format!("Failed to read request: {}", e))?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse the GET request line: "GET /callback?token=...&email=...&name=... HTTP/1.1"
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| "Invalid HTTP request".to_string())?;

    // Parse query params from the path
    let full_url = format!("http://localhost{}", path);
    let parsed = url::Url::parse(&full_url).map_err(|e| format!("Failed to parse callback URL: {}", e))?;

    let params: HashMap<String, String> = parsed
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let token = params
        .get("token")
        .ok_or_else(|| "Missing token in callback".to_string())?
        .clone();
    let email = params.get("email").cloned().unwrap_or_default();
    let name = params.get("name").cloned().unwrap_or_default();
    let user_id = params.get("user_id").cloned().unwrap_or_default();

    // Send success HTML response
    let html = r#"<!DOCTYPE html><html><head><title>dodev CLI</title><style>
        body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; display: flex; align-items: center; justify-content: center; min-height: 100vh; margin: 0; background: #f8fafc; color: #1e293b; }
        .container { text-align: center; }
        h1 { color: #16a34a; font-size: 1.5rem; }
        p { color: #64748b; }
    </style></head><body><div class="container">
        <h1>Authenticated</h1>
        <p>You can close this tab and return to the terminal.</p>
    </div></body></html>"#;

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| format!("Failed to send response: {}", e))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("Failed to flush response: {}", e))?;

    Ok(CallbackData {
        token,
        email,
        name,
        user_id,
    })
}

/// Open a URL in the default browser.
fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/c", "start", url])
            .spawn()
            .map_err(|e| format!("Failed to open browser: {}", e))?;
    }

    Ok(())
}

// ---------------------------------------------------------------
// Logout / Status / Config
// ---------------------------------------------------------------

/// Remove saved authentication.
fn cmd_logout() -> Result<(), String> {
    config::clear_session().map_err(|e| e.to_string())?;
    config::clear_api_key().map_err(|e| e.to_string())?;
    println!();
    println!("  Logged out.");
    println!();
    Ok(())
}

/// Show current authentication status.
fn cmd_status() -> Result<(), String> {
    println!();

    // Check for session first
    if let Some(session) = config::load_session() {
        let display = if session.email.is_empty() {
            session.user_id.clone()
        } else {
            session.email.clone()
        };
        println!("  Authenticated:  yes ({})", display);
        if !session.name.is_empty() {
            println!("  Name:           {}", session.name);
        }
        println!("  Auth method:    session token");
    } else {
        match auth::get_auth_token() {
            Ok(key) => {
                let prefix = if key.len() > 12 { &key[..12] } else { &key };
                println!("  Authenticated:  yes ({}...)", prefix);
                println!("  Auth method:    API key");
            }
            Err(_) => {
                println!("  Authenticated:  no");
                println!("  Run `dodev login` to authenticate.");
            }
        }
    }

    println!();
    Ok(())
}

/// Show the current configuration.
fn cmd_config(ws_url_override: Option<String>) -> Result<(), String> {
    println!();

    let path = config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    println!("  Config file:    {}", path);

    match config::load_config() {
        Some(cfg) => {
            let has_session = cfg
                .auth
                .as_ref()
                .and_then(|a| a.session_token.as_ref())
                .is_some();
            let has_key = cfg
                .auth
                .as_ref()
                .and_then(|a| a.api_key.as_ref())
                .is_some();

            if has_session {
                println!("  Session:        set");
            }
            if has_key {
                println!("  API key:        set");
            }
            if !has_session && !has_key {
                println!("  Auth:           not configured");
            }
        }
        None => {
            println!("  (no config file found)");
        }
    }

    match ws_url_override {
        Some(url) => println!("  WS URL:         {} (override)", url),
        None => println!("  WS URL:         wss://<subdomain>.local.dev/_dodev/connect (derived from --subdomain, default: lab)"),
    }

    println!();
    Ok(())
}

// Legacy: defaults.relay_url in the config file is no longer consulted —
// v1 derives the WS URL from --subdomain. The field is left in
// DefaultsConfig so we don't break parsing of older config files.
#[allow(dead_code)]
fn _resolve_relay_url_legacy(cli_value: &str) -> String {
    let default = "wss://relay.local.dev/ws/connect";
    if cli_value == default {
        if let Some(cfg) = config::load_config() {
            if let Some(defaults) = cfg.defaults {
                if let Some(url) = defaults.relay_url {
                    if !url.is_empty() {
                        return url;
                    }
                }
            }
        }
    }
    cli_value.to_string()
}
