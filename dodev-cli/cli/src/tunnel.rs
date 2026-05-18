// v1 tunnel client — talks to the Cloudflare Worker + Durable Object via
// WebSocket using the JSON-envelope protocol defined in
// backends/workers-local/src/tunnel-do.ts.
//
// Wire format (text frames, JSON-encoded):
//   inbound  : {"type":"request",  id, method, url, headers, body(base64|null)}
//   outbound : {"type":"response", id, status, headers, body(base64|null)}
//
// Auth: Authorization: Bearer <token> on the WebSocket upgrade. The Worker
// validates the token against convex-local's cli_sessions before accepting
// the upgrade; a 401 here is a hard auth failure (no retry).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, http::HeaderValue, Message,
};
use uuid::Uuid;

use crate::display;
use crate::proxy::{self, LocalRequest};

pub struct TunnelConfig {
    pub ws_url: String,    // wss://<subdomain>.local.dev/_dodev/connect
    pub token: String,     // bearer token from `dodev login`
    pub subdomain: String, // for the active-tunnel banner
    pub local_host: String,
    pub local_port: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("Authentication failed: {0}")]
    AuthFailed(String),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("Maximum reconnection attempts ({0}) exceeded")]
    MaxReconnectsExceeded(u32),
}

const MAX_RECONNECT_ATTEMPTS: u32 = 20;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Envelope {
    Request {
        id: String,
        method: String,
        url: String,
        headers: HashMap<String, String>,
        body: Option<String>, // base64
    },
    Response {
        id: String,
        status: u16,
        headers: HashMap<String, String>,
        body: Option<String>, // base64
    },
}

pub async fn run_tunnel(config: TunnelConfig) -> Result<(), TunnelError> {
    let http_client = Arc::new(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| TunnelError::WebSocket(format!("client init: {}", e)))?,
    );
    let local_host = Arc::new(config.local_host.clone());

    let mut reconnect_attempt: u32 = 0;
    loop {
        match connect_and_run(&config, &local_host, &http_client, &mut reconnect_attempt).await {
            Ok(()) => {
                display::print_disconnected();
                return Ok(());
            }
            Err(TunnelError::AuthFailed(reason)) => {
                return Err(TunnelError::AuthFailed(reason));
            }
            Err(e) => {
                reconnect_attempt += 1;
                if reconnect_attempt > MAX_RECONNECT_ATTEMPTS {
                    display::print_error(&format!(
                        "Gave up after {} reconnection attempts",
                        MAX_RECONNECT_ATTEMPTS
                    ));
                    return Err(TunnelError::MaxReconnectsExceeded(MAX_RECONNECT_ATTEMPTS));
                }
                tracing::warn!("Tunnel disconnected: {}. Reconnecting...", e);
                display::print_reconnecting(reconnect_attempt, MAX_RECONNECT_ATTEMPTS);
                let base_secs = std::cmp::min(1u64 << (reconnect_attempt - 1), 30);
                let jitter_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| u64::from(d.subsec_millis()))
                    .unwrap_or(0);
                tokio::time::sleep(Duration::from_millis(base_secs * 1000 + jitter_ms)).await;
            }
        }
    }
}

async fn connect_and_run(
    config: &TunnelConfig,
    local_host: &Arc<String>,
    http_client: &Arc<reqwest::Client>,
    reconnect_attempt: &mut u32,
) -> Result<(), TunnelError> {
    // Build a WS upgrade request with Authorization: Bearer in the headers
    // (NOT the URL query string — per doc/local/ARCHITECTURE.md §Tunnel
    // lifecycle, tokens in URLs leak to access logs).
    let mut request = config
        .ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| TunnelError::WebSocket(format!("invalid ws url: {}", e)))?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", config.token))
            .map_err(|e| TunnelError::AuthFailed(format!("invalid token: {}", e)))?,
    );

    tracing::info!("Connecting to {}", config.ws_url);
    let (ws, response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| classify_connect_error(e))?;

    if response.status() != 101 {
        return Err(TunnelError::WebSocket(format!(
            "unexpected upgrade status {}",
            response.status()
        )));
    }

    // Connection succeeded — reset the consecutive-failure counter so a later
    // drop starts fresh backoff rather than accumulating toward the cap.
    *reconnect_attempt = 0;
    display::print_tunnel_active(&config.subdomain, config.local_port, "local.dev");

    let (mut ws_write, mut ws_read) = ws.split();
    let (tx, mut rx) = mpsc::channel::<Message>(256);

    // Writer task drains the channel into the WS sink so spawned per-request
    // tasks can send their response envelopes concurrently.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_write.send(msg).await.is_err() {
                break;
            }
        }
    });

    let local_port = config.local_port;
    while let Some(msg) = ws_read.next().await {
        let msg = msg.map_err(|e| TunnelError::WebSocket(e.to_string()))?;
        match msg {
            Message::Text(text) => {
                let envelope: Envelope = match serde_json::from_str(&text) {
                    Ok(env) => env,
                    Err(e) => {
                        tracing::warn!("malformed envelope: {}", e);
                        continue;
                    }
                };
                let (id, method, url, headers, body) = match envelope {
                    Envelope::Request { id, method, url, headers, body } => {
                        (id, method, url, headers, body)
                    }
                    Envelope::Response { .. } => {
                        tracing::debug!("ignoring response envelope from server");
                        continue;
                    }
                };

                let local_host_arc = Arc::clone(local_host);
                let http_client_arc = Arc::clone(http_client);
                let tx = tx.clone();
                tokio::spawn(async move {
                    handle_request(id, method, url, headers, body, local_host_arc, local_port, http_client_arc, tx).await;
                });
            }
            Message::Close(_) => {
                drop(tx);
                let _ = writer.await;
                return Ok(());
            }
            Message::Ping(payload) => {
                if tx.send(Message::Pong(payload)).await.is_err() {
                    break;
                }
            }
            _ => {}
        }
    }

    drop(tx);
    let _ = writer.await;
    Err(TunnelError::WebSocket("connection closed unexpectedly".to_string()))
}

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    id: String,
    method: String,
    url: String,
    headers: HashMap<String, String>,
    body: Option<String>,
    local_host: Arc<String>,
    local_port: u16,
    http_client: Arc<reqwest::Client>,
    tx: mpsc::Sender<Message>,
) {
    let body_bytes = body.and_then(|b| BASE64.decode(b).ok());
    let req = LocalRequest { method: method.clone(), url: url.clone(), headers, body: body_bytes };

    let started = std::time::Instant::now();
    let resp = proxy::proxy_request(req, &local_host, local_port, &http_client).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let path = url::Url::parse(&url)
        .ok()
        .map(|u| u.path().to_string())
        .unwrap_or_else(|| url.clone());
    display::print_request_log(&method, &path, resp.status, elapsed_ms);

    let envelope = Envelope::Response {
        id,
        status: resp.status,
        headers: resp.headers,
        body: if resp.body.is_empty() {
            None
        } else {
            Some(BASE64.encode(&resp.body))
        },
    };
    if let Ok(payload) = serde_json::to_string(&envelope) {
        let _ = tx.send(Message::Text(payload.into())).await;
    }
}

// Map a tungstenite connect error to AuthFailed when the server returned 401,
// otherwise WebSocket. Auth failures are not retried by the outer loop.
fn classify_connect_error(err: tokio_tungstenite::tungstenite::Error) -> TunnelError {
    use tokio_tungstenite::tungstenite::Error as TErr;
    if let TErr::Http(resp) = &err {
        if resp.status() == 401 {
            let body = resp
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).trim().to_string())
                .unwrap_or_else(|| "unauthorized".to_string());
            return TunnelError::AuthFailed(body);
        }
        if resp.status() == 403 {
            let body = resp
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).trim().to_string())
                .unwrap_or_else(|| "forbidden".to_string());
            return TunnelError::AuthFailed(format!("forbidden: {}", body));
        }
    }
    TunnelError::WebSocket(err.to_string())
}

// Silence "unused import" if we ever stop generating UUIDs client-side; today
// the DO assigns ids so we don't need this, but keep it imported for future
// inverse-call shapes (e.g. CLI-initiated probes).
#[allow(dead_code)]
fn _client_id() -> String {
    Uuid::new_v4().to_string()
}
