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

    #[error("Quota exceeded: {0}")]
    QuotaExceeded(String),

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
    // Server-initiated heartbeat. Keeps the WS warm AND lets the server
    // confirm liveness via the ack we send back. Without this, a
    // silently-dead WS (NAT timeout, ISP cleanup) goes undetected until
    // the next browser request times out. See backends/workers-local
    // tunnel-do.ts WS_IDLE_TIMEOUT_MS for the server side.
    Heartbeat {
        #[serde(default)]
        ts: u64,
    },
    // Acknowledgement we send back on receiving a Heartbeat.
    #[serde(rename = "heartbeat-ack")]
    HeartbeatAck {
        #[serde(default)]
        ts: u64,
    },
    // WebSocket-proxy envelopes. When a browser opens ws://<sub>.local.dev/<path>,
    // the Worker accepts the upgrade and sends ws-open down the CLI tunnel.
    // The CLI then dials ws://localhost:<port><path>, ack's, and shuttles
    // frames in both directions until either side closes.
    #[serde(rename = "ws-open")]
    WsOpen {
        id: String,
        url: String, // path + query
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        protocols: Vec<String>,
    },
    #[serde(rename = "ws-open-ack")]
    WsOpenAck {
        id: String,
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    },
    #[serde(rename = "ws-msg")]
    WsMsg {
        id: String,
        kind: String, // "text" | "binary"
        data: String, // raw text or base64 for binary
    },
    #[serde(rename = "ws-close")]
    WsClose {
        id: String,
        #[serde(default)]
        code: Option<u16>,
        #[serde(default)]
        reason: Option<String>,
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
            Err(TunnelError::QuotaExceeded(reason)) => {
                // Quotas don't resolve on their own — bail instead of looping.
                return Err(TunnelError::QuotaExceeded(reason));
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
                // First few reconnects are normal — NAT timeouts, edge
                // rotations, brief Wi-Fi drops. Only escalate to WARN
                // once we've retried a handful of times in a row.
                if reconnect_attempt <= 3 {
                    tracing::info!("Tunnel disconnected: {}. Reconnecting...", e);
                } else {
                    tracing::warn!(
                        "Tunnel disconnected ({}/{}): {}. Reconnecting...",
                        reconnect_attempt, MAX_RECONNECT_ATTEMPTS, e
                    );
                }
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
    // Advertise that this CLI understands the ws-* envelope set so the
    // Worker is safe to proxy browser WebSockets to us. Old CLIs that
    // don't send this header get the 404-on-upgrade fallback instead of
    // a flood of "malformed envelope" warnings.
    request.headers_mut().insert(
        "x-dodev-supports-ws-proxy",
        HeaderValue::from_static("1"),
    );
    request.headers_mut().insert(
        "x-dodev-cli-version",
        HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
    );

    tracing::info!("Connecting to {}", config.ws_url);
    // Snapshot before we reset — lets us decide whether this connect
    // is the initial one (print full banner), a quiet recovery (print
    // a single dim line), or a recovery from sustained trouble (print
    // the full banner so the user knows we're back).
    let prior_attempt = *reconnect_attempt;
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
    if prior_attempt == 0 {
        display::print_tunnel_active(&config.subdomain, config.local_port, "local.dev");
    } else if prior_attempt < 3 {
        // Routine reconnect — one quiet line, no banner spam.
        display::print_reconnected_quietly();
    } else {
        // We escalated to WARN earlier; user deserves the full banner
        // back so they can confirm the tunnel is really up.
        display::print_tunnel_active(&config.subdomain, config.local_port, "local.dev");
    }

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

    // ws-proxy session table. Each entry is the inbound side of a local
    // ws://localhost dial — the read loop pushes ws-msg / ws-close
    // envelopes here and a per-session task forwards them to the local WS.
    // Removed when the local WS closes (either side).
    let ws_sessions: std::sync::Arc<std::sync::Mutex<HashMap<String, mpsc::Sender<WsLocalEvent>>>>
        = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Server sends a heartbeat every ~25s. If we haven't seen ANY frame
    // (heartbeat, request, ack) in 70s we treat the connection as dead
    // and reconnect. This covers the silent-TCP-death case — NAT timeout
    // or middlebox dropping the flow without sending FIN, where ws_read
    // just hangs forever otherwise and the CLI displays "connected" while
    // proxied requests get "tunnel offline" on the server side.
    const SERVER_SILENCE_TIMEOUT: Duration = Duration::from_secs(70);

    loop {
        let next = tokio::time::timeout(SERVER_SILENCE_TIMEOUT, ws_read.next()).await;
        let msg = match next {
            Err(_) => {
                tracing::warn!(
                    "No frame from server in {}s — treating as dead and reconnecting",
                    SERVER_SILENCE_TIMEOUT.as_secs()
                );
                return Err(TunnelError::WebSocket(format!(
                    "no server heartbeat in {}s",
                    SERVER_SILENCE_TIMEOUT.as_secs()
                )));
            }
            Ok(None) => break, // stream ended cleanly
            Ok(Some(m)) => m.map_err(|e| TunnelError::WebSocket(e.to_string()))?,
        };

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
                    Envelope::Heartbeat { ts } => {
                        let ack = Envelope::HeartbeatAck { ts };
                        if let Ok(payload) = serde_json::to_string(&ack) {
                            let _ = tx.send(Message::Text(payload.into())).await;
                        }
                        continue;
                    }
                    Envelope::HeartbeatAck { .. } => {
                        continue;
                    }
                    Envelope::WsOpen { id, url, headers, protocols } => {
                        let local_host_arc = Arc::clone(local_host);
                        let tx_clone = tx.clone();
                        let sessions_clone = std::sync::Arc::clone(&ws_sessions);
                        tokio::spawn(async move {
                            handle_ws_open(
                                id,
                                url,
                                headers,
                                protocols,
                                local_host_arc,
                                local_port,
                                tx_clone,
                                sessions_clone,
                            )
                            .await;
                        });
                        continue;
                    }
                    Envelope::WsMsg { id, kind, data } => {
                        let sender_opt = {
                            ws_sessions.lock().unwrap().get(&id).cloned()
                        };
                        if let Some(sender) = sender_opt {
                            let _ = sender.send(WsLocalEvent::Msg { kind, data }).await;
                        }
                        continue;
                    }
                    Envelope::WsClose { id, code, reason } => {
                        let sender_opt = {
                            ws_sessions.lock().unwrap().remove(&id)
                        };
                        if let Some(sender) = sender_opt {
                            let _ = sender.send(WsLocalEvent::Close { code, reason }).await;
                        }
                        continue;
                    }
                    Envelope::WsOpenAck { .. } => {
                        tracing::debug!("ignoring ws-open-ack from server");
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

// Events delivered to a per-session local-WS task from the main read loop.
#[derive(Debug)]
enum WsLocalEvent {
    Msg { kind: String, data: String },
    Close { code: Option<u16>, reason: Option<String> },
}

/// Browser opened a WebSocket through the tunnel. Dial the matching
/// ws://localhost:<port><url>, send ws-open-ack, then shuttle frames
/// in both directions until either side closes.
#[allow(clippy::too_many_arguments)]
async fn handle_ws_open(
    id: String,
    url: String,
    headers: HashMap<String, String>,
    protocols: Vec<String>,
    local_host: Arc<String>,
    local_port: u16,
    tx: mpsc::Sender<Message>,
    sessions: std::sync::Arc<std::sync::Mutex<HashMap<String, mpsc::Sender<WsLocalEvent>>>>,
) {
    let ws_url = format!("ws://{}:{}{}", local_host, local_port, url);
    let mut request = match ws_url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            let _ = send_envelope(
                &tx,
                &Envelope::WsOpenAck {
                    id: id.clone(),
                    ok: false,
                    error: Some(format!("invalid local ws url: {}", e)),
                },
            )
            .await;
            return;
        }
    };

    // Forward selected headers from the browser request. The Host header
    // is set to localhost:port automatically by tungstenite. Skip
    // hop-by-hop headers and anything related to the upstream upgrade.
    let req_headers = request.headers_mut();
    for (k, v) in &headers {
        let kl = k.to_lowercase();
        if matches!(
            kl.as_str(),
            "host"
                | "connection"
                | "upgrade"
                | "sec-websocket-key"
                | "sec-websocket-version"
                | "sec-websocket-accept"
                | "sec-websocket-protocol"
                | "sec-websocket-extensions"
                | "content-length"
        ) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            req_headers.insert(name, val);
        }
    }
    if !protocols.is_empty() {
        if let Ok(val) = HeaderValue::from_str(&protocols.join(", ")) {
            req_headers.insert("Sec-WebSocket-Protocol", val);
        }
    }

    let (local_ws, _resp) = match tokio_tungstenite::connect_async(request).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!("ws-proxy: dial {} failed: {}", ws_url, e);
            let _ = send_envelope(
                &tx,
                &Envelope::WsOpenAck {
                    id: id.clone(),
                    ok: false,
                    error: Some(format!("local dial failed: {}", e)),
                },
            )
            .await;
            return;
        }
    };

    let _ = send_envelope(&tx, &Envelope::WsOpenAck { id: id.clone(), ok: true, error: None }).await;

    let (mut local_write, mut local_read) = local_ws.split();
    let (local_event_tx, mut local_event_rx) = mpsc::channel::<WsLocalEvent>(256);
    sessions.lock().unwrap().insert(id.clone(), local_event_tx);

    // Inbound from upstream (browser via worker) → write to local WS.
    let id_for_write = id.clone();
    let local_write_task = tokio::spawn(async move {
        while let Some(ev) = local_event_rx.recv().await {
            match ev {
                WsLocalEvent::Msg { kind, data } => {
                    let frame = if kind == "binary" {
                        match BASE64.decode(&data) {
                            Ok(bytes) => Message::Binary(bytes.into()),
                            Err(_) => continue,
                        }
                    } else {
                        Message::Text(data.into())
                    };
                    if local_write.send(frame).await.is_err() {
                        break;
                    }
                }
                WsLocalEvent::Close { code, reason } => {
                    // Sanitize the close code before writing it to the
                    // local WS. RFC 6455 forbids 1005/1006/1015 in a
                    // Close frame — they're abnormal-closure markers
                    // generated internally by WS stacks, never sent.
                    // The Node `ws` library inside Next.js dev throws
                    // `WS_ERR_INVALID_CLOSE_CODE` when one of these
                    // arrives, polluting the dev server log with
                    // uncaughtException stack traces. Map to 1011.
                    let safe_code = match code {
                        Some(c) if c == 1005 || c == 1006 || c == 1015 => 1011,
                        Some(c) if (1000..=1014).contains(&c) && c != 1004 => c,
                        Some(c) if (3000..=4999).contains(&c) => c,
                        _ => 1011,
                    };
                    let cf = tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(safe_code),
                        reason: reason.unwrap_or_default().into(),
                    };
                    let _ = local_write.send(Message::Close(Some(cf))).await;
                    break;
                }
            }
        }
        tracing::debug!("ws-proxy: local writer done for session {}", id_for_write);
    });

    // Outbound from local WS → wrap as ws-msg envelopes.
    while let Some(msg) = local_read.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("ws-proxy: local read err: {}", e);
                break;
            }
        };
        match msg {
            Message::Text(text) => {
                let _ = send_envelope(
                    &tx,
                    &Envelope::WsMsg {
                        id: id.clone(),
                        kind: "text".into(),
                        data: text.to_string(),
                    },
                )
                .await;
            }
            Message::Binary(bin) => {
                let _ = send_envelope(
                    &tx,
                    &Envelope::WsMsg {
                        id: id.clone(),
                        kind: "binary".into(),
                        data: BASE64.encode(&bin),
                    },
                )
                .await;
            }
            Message::Close(cf) => {
                let (code, reason) = cf
                    .map(|c| (Some(u16::from(c.code)), Some(c.reason.to_string())))
                    .unwrap_or((None, None));
                let _ = send_envelope(
                    &tx,
                    &Envelope::WsClose { id: id.clone(), code, reason },
                )
                .await;
                break;
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }

    sessions.lock().unwrap().remove(&id);
    let _ = local_write_task.await;
    tracing::debug!("ws-proxy: session {} ended", id);
}

async fn send_envelope(tx: &mpsc::Sender<Message>, env: &Envelope) -> Result<(), ()> {
    let payload = serde_json::to_string(env).map_err(|_| ())?;
    tx.send(Message::Text(payload.into())).await.map_err(|_| ())
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
        if resp.status() == 429 {
            let body = resp
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).trim().to_string())
                .unwrap_or_else(|| "quota exceeded".to_string());
            // Worker prefixes the body with "quota exceeded:" — strip
            // before re-wrapping with our friendlier framing.
            let detail = body
                .strip_prefix("quota exceeded:")
                .unwrap_or(&body)
                .trim()
                .to_string();
            return TunnelError::QuotaExceeded(format!(
                "{}. Stop another tunnel with Ctrl+C, or upgrade at https://local.dev/pricing",
                if detail.is_empty() { "limit reached" } else { &detail }
            ));
        }
    }
    TunnelError::WebSocket(err.to_string())
}

