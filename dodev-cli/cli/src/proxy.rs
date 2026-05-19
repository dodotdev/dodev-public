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

    let mut saw_accept_encoding = false;
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
        // Rewrite Accept-Encoding to only what our Worker can decompress
        // (gzip/deflate). DecompressionStream in CF Workers doesn't
        // support brotli, so requesting "br" from upstream would mean
        // forwarding br bytes that CF strips the header from, producing
        // garbage in the browser.
        if lower == "accept-encoding" {
            req_builder = req_builder.header("accept-encoding", "gzip, deflate");
            saw_accept_encoding = true;
            continue;
        }
        req_builder = req_builder.header(name.as_str(), value.as_str());
    }
    if !saw_accept_encoding {
        // If the browser didn't ask for compression, ask anyway — keeps
        // the WS hop small. Worker decompresses before browser sees it.
        req_builder = req_builder.header("accept-encoding", "gzip, deflate");
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
            local_server_down_response(local_port)
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

/// HTML page served when the tunnel is healthy but the user's local
/// server isn't responding (connection refused). Most common situation:
/// they killed `pnpm dev` and re-loaded the URL, or pointed the tunnel
/// at the wrong port. Plain text is confusing in a browser — explain
/// what happened and link back to local.dev for context.
fn local_server_down_response(port: u16) -> LocalResponse {
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex,nofollow">
<title>local.dev — local server is down</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
    max-width: 560px; margin: 6rem auto; padding: 0 1.5rem;
    line-height: 1.55; color: #1a1a1a; background: #fafafa;
  }}
  @media (prefers-color-scheme: dark) {{
    body {{ color: #e5e7eb; background: #0a0a0a; }}
    code, pre {{ background: #1a1a1a; color: #d1fae5; }}
    .card {{ background: #111; border-color: #1f2937; }}
  }}
  .logo {{
    display: inline-flex; align-items: center; gap: .5rem;
    color: #047857; font-weight: 600; text-decoration: none;
  }}
  .logo-mark {{
    width: 1.75rem; height: 1.75rem; border-radius: .375rem;
    background: #047857; display: inline-flex;
    align-items: center; justify-content: center;
    color: white; font-size: .85rem; font-family: "SF Mono", Menlo, monospace;
  }}
  h1 {{ margin-top: 1.5rem; font-size: 1.5rem; }}
  .card {{
    margin-top: 1.5rem; padding: 1.25rem 1.5rem;
    border: 1px solid #e5e7eb; background: #fff; border-radius: .5rem;
  }}
  code, pre {{
    font-family: "SF Mono", Menlo, Consolas, monospace;
    background: #f3f4f6; padding: .15rem .4rem; border-radius: .25rem;
    font-size: .9em;
  }}
  pre {{ padding: .75rem 1rem; overflow-x: auto; }}
  a {{ color: #047857; }}
  .muted {{ color: #6b7280; font-size: .9rem; margin-top: 2rem; }}
</style>
</head>
<body>
<a href="https://local.dev" class="logo">
  <span class="logo-mark">›_</span><span>local.dev</span>
</a>

<h1>Your local server is down.</h1>
<p>
  The tunnel is up — but nothing is listening on
  <code>localhost:{port}</code>. Start your dev server, then refresh
  this page.
</p>

<div class="card">
  <p style="margin: 0 0 .5rem;"><strong>Common fixes</strong></p>
  <ul style="margin: 0; padding-left: 1.25rem;">
    <li>Run <code>pnpm dev</code> (or equivalent) in another terminal</li>
    <li>Make sure it's bound to port <code>{port}</code></li>
    <li>Wrong port? Restart your tunnel:
      <pre>dodev local http &lt;your-port&gt;</pre></li>
  </ul>
</div>

<p class="muted">
  Powered by <a href="https://local.dev">local.dev</a> — OAuth-ready
  HTTPS tunnels with real <code>.dev</code> hostnames.
  <a href="https://local.dev">What is local.dev? →</a>
</p>
</body>
</html>
"#,
        port = port,
    );

    let mut headers = HashMap::new();
    headers.insert("content-type".to_string(), "text/html; charset=utf-8".to_string());
    headers.insert("cache-control".to_string(), "no-store".to_string());
    LocalResponse {
        status: 502,
        headers,
        body: html.into_bytes(),
    }
}
