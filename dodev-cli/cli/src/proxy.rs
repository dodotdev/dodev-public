// HTTP proxy to the user's local server. Receives a decoded request shape
// from tunnel.rs, forwards to http://<local_host>:<local_port><path>, and
// returns the response decoded into bytes. Connection / timeout / parse
// errors return a synthetic 502 or 504 so the tunnel always sends a
// response envelope back to the Worker.

use std::collections::HashMap;
use reqwest::Client;

pub struct LocalRequest {
    pub method: String,
    pub url: String, // full URL as the external caller sent it (origin = lab.local.dev etc.)
    pub headers: HashMap<String, String>,
    pub body: Option<Vec<u8>>,
}

pub struct LocalResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

pub async fn proxy_request(
    request: LocalRequest,
    local_host: &str,
    local_port: u16,
    client: &Client,
) -> LocalResponse {
    // Rewrite the URL: keep path + query, replace origin with localhost.
    let parsed = match url::Url::parse(&request.url) {
        Ok(u) => u,
        Err(_) => return error_response(400, &format!("invalid request url: {}", request.url)),
    };
    let path_and_query = if let Some(q) = parsed.query() {
        format!("{}?{}", parsed.path(), q)
    } else {
        parsed.path().to_string()
    };
    let local_url = format!("http://{}:{}{}", local_host, local_port, path_and_query);

    let method = match reqwest::Method::from_bytes(request.method.as_bytes()) {
        Ok(m) => m,
        Err(_) => return error_response(400, &format!("unsupported method: {}", request.method)),
    };

    let mut req_builder = client.request(method.clone(), &local_url);

    for (name, value) in &request.headers {
        // Drop hop-by-hop / connection-specific headers. reqwest manages
        // Host based on the URL; content-length must be derived from the
        // body we set; we don't forward the original Authorization header
        // because it was for the tunnel auth, not the local app.
        let lower = name.to_lowercase();
        if matches!(
            lower.as_str(),
            "host"
                | "content-length"
                | "connection"
                | "keep-alive"
                | "transfer-encoding"
                | "upgrade"
                | "proxy-authorization"
                | "proxy-authenticate"
                | "te"
                | "trailer"
        ) {
            continue;
        }
        req_builder = req_builder.header(name.as_str(), value.as_str());
    }

    // GET and HEAD must not carry a body even if one was sent.
    if let Some(body) = request.body {
        if method != reqwest::Method::GET && method != reqwest::Method::HEAD {
            req_builder = req_builder.body(body);
        }
    }

    match req_builder.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let mut headers = HashMap::new();
            for (name, value) in response.headers().iter() {
                headers.insert(
                    name.as_str().to_string(),
                    value.to_str().unwrap_or("").to_string(),
                );
            }
            match response.bytes().await {
                Ok(body) => LocalResponse { status, headers, body: body.to_vec() },
                Err(e) => {
                    tracing::error!("Failed to read response body: {}", e);
                    error_response(502, &format!("failed to read response body: {}", e))
                }
            }
        }
        Err(e) if e.is_connect() => {
            tracing::debug!("Connection refused to {}:{}", local_host, local_port);
            error_response(
                502,
                &format!(
                    "Local server not running on port {}. Start your server and try again.",
                    local_port
                ),
            )
        }
        Err(e) if e.is_timeout() => {
            tracing::debug!("Timeout connecting to {}:{}", local_host, local_port);
            error_response(504, &format!("Local server timeout on port {}", local_port))
        }
        Err(e) => {
            tracing::error!("Proxy error: {}", e);
            error_response(502, &format!("proxy error: {}", e))
        }
    }
}

fn error_response(status: u16, message: &str) -> LocalResponse {
    let mut headers = HashMap::new();
    headers.insert("content-type".to_string(), "text/plain; charset=utf-8".to_string());
    LocalResponse { status, headers, body: message.as_bytes().to_vec() }
}
