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
    let level = if verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
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

    println!("  Checking for updates...");

    match updater.run().await {
        Ok(Some(result)) => {
            println!();
            println!("  \u{2714} Updated dodev to v{}", result.new_version);
            println!();
            Ok(())
        }
        Ok(None) => {
            println!();
            println!("  \u{2714} Already on the latest release.");
            println!();
            Ok(())
        }
        Err(e) => Err(format!("Update failed: {}", e)),
    }
}

/// Start a tunnel connection.
///
/// Subdomain selection: --subdomain flag wins; otherwise defaults to "lab"
/// (the v0/v1 dev subdomain). Auto-discovery of the user's assigned random
/// subdomains via the /cli/validate-session response is a TODO.
async fn cmd_start(
    ws_url_override: Option<String>,
    port: u16,
    subdomain: Option<String>,
    host: String,
) -> Result<(), String> {
    let token = auth::get_auth_token().map_err(|e| e.to_string())?;
    let subdomain = subdomain.unwrap_or_else(|| "lab".to_string());
    let ws_url = ws_url_override
        .unwrap_or_else(|| format!("wss://{}.local.dev/_dodev/connect", subdomain));

    let config = tunnel::TunnelConfig {
        ws_url,
        token,
        subdomain,
        local_host: host,
        local_port: port,
    };

    tunnel::run_tunnel(config)
        .await
        .map_err(|e| e.to_string())
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
