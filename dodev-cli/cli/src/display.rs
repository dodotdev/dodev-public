use colored::Colorize;

/// Print the tunnel-active banner after successful authentication.
///
/// `relay_host` should be the hostname portion of the relay URL (e.g. "local.dev").
pub fn print_tunnel_active(subdomain: &str, local_port: u16, relay_host: &str) {
    println!();
    println!(
        "  {} {}",
        "✔".green().bold(),
        "Tunnel active".green().bold()
    );
    println!();
    println!(
        "  {}   https://{}.{}",
        "Public URL:".white().bold(),
        subdomain,
        relay_host
    );
    println!(
        "  {}  https://{}.{} → http://localhost:{}",
        "Forwarding:".white().bold(),
        subdomain,
        relay_host,
        local_port
    );
    println!();
    println!("  Press {} to stop", "Ctrl+C".yellow().bold());
    println!();
}

/// Log an individual HTTP request that was proxied through the tunnel.
///
/// Colors the status code: green for 2xx, yellow for 3xx, red for 4xx/5xx.
pub fn print_request_log(method: &str, path: &str, status: u16, duration_ms: u64) {
    let status_colored = match status {
        200..=299 => format!("{}", status).green(),
        300..=399 => format!("{}", status).yellow(),
        400..=499 => format!("{}", status).red(),
        500..=599 => format!("{}", status).red().bold(),
        _ => format!("{}", status).white(),
    };

    println!(
        "  {} {} → {} ({}ms)",
        method.white().bold(),
        path.white(),
        status_colored,
        duration_ms
    );
}

/// Print a reconnection attempt message. Attempts 1-2 are silent — NAT
/// timeouts and Wi-Fi flickers are routine and the auto-recovery
/// usually completes in a second. We only narrate from attempt 3 on,
/// when something more persistent is wrong.
pub fn print_reconnecting(attempt: u32, max: u32) {
    if attempt < 3 {
        return;
    }
    println!(
        "  {} Reconnecting... (attempt {}/{})",
        "⟳".yellow().bold(),
        attempt,
        max
    );
}

/// One-line "back online" notice for routine reconnects. Used instead
/// of repeating the full Tunnel-active banner on every NAT timeout.
/// Says "refreshed" rather than "reconnected" because the latter
/// implies something went wrong — these are routine maintenance
/// recoveries, indistinguishable from a fresh tunnel from the user's
/// perspective.
pub fn print_reconnected_quietly() {
    println!("  {}", "↻ tunnel refreshed".dimmed());
}

/// Print an error message.
pub fn print_error(msg: &str) {
    eprintln!("  {} {}", "✗ Error:".red().bold(), msg);
}

/// Print a disconnection message.
pub fn print_disconnected() {
    println!();
    println!("  {}", "Tunnel disconnected.".dimmed());
}
