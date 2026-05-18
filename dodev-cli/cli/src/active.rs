// Per-subdomain lockfiles tracking which tunnels this user has running on
// this machine. Lets `cmd_start` skip subdomains already in use by another
// `dodev local http` invocation when auto-picking from the assigned list.
//
// Layout:
//   ~/.dodev/tunnels/<subdomain>.lock     contains the PID as plain text
//
// Stale cleanup: on every scan, we check if the recorded PID still exists
// (libc::kill(pid, 0) returns 0 → alive, -1 → ESRCH → dead). Dead locks
// are unlinked so a crashed CLI doesn't wedge a subdomain forever.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

pub struct TunnelLock {
    path: PathBuf,
}

impl Drop for TunnelLock {
    fn drop(&mut self) {
        // Best-effort cleanup on graceful exit. If the process is killed
        // hard (-9), the file stays around; PID check on next scan handles
        // that case.
        let _ = fs::remove_file(&self.path);
    }
}

fn tunnels_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".dodev").join("tunnels"))
}

fn lock_path(subdomain: &str) -> Option<PathBuf> {
    Some(tunnels_dir()?.join(format!("{}.lock", subdomain)))
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    // kill(pid, 0) is the standard "does this process exist?" probe.
    // Returns 0 on success (alive), -1 with ESRCH if not.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    // Windows fallback: assume the lock is live. Stale locks there will
    // need manual cleanup until we add proper handle-based tracking.
    true
}

/// True if the subdomain has an active lockfile owned by a live process.
pub fn is_in_use(subdomain: &str) -> bool {
    let path = match lock_path(subdomain) {
        Some(p) => p,
        None => return false,
    };
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let pid: i32 = match content.trim().parse() {
        Ok(p) => p,
        Err(_) => {
            // Garbled lock file — drop it and treat as free.
            let _ = fs::remove_file(&path);
            return false;
        }
    };
    if pid_alive(pid) {
        true
    } else {
        // Stale; clear it so future calls don't keep rechecking.
        let _ = fs::remove_file(&path);
        false
    }
}

/// Claim a subdomain by writing our PID to the lockfile. Returns a guard
/// that removes the lockfile on Drop. Returns Err if the directory can't
/// be created or the file can't be written — callers should treat that
/// as a soft error and proceed without a lock (the tunnel still works,
/// we just can't track it).
pub fn acquire(subdomain: &str) -> Result<TunnelLock, String> {
    let dir = tunnels_dir().ok_or_else(|| "no home dir".to_string())?;
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {}", dir.display(), e))?;
    let path = dir.join(format!("{}.lock", subdomain));
    let mut f = fs::File::create(&path).map_err(|e| format!("create {}: {}", path.display(), e))?;
    writeln!(f, "{}", std::process::id())
        .map_err(|e| format!("write {}: {}", path.display(), e))?;
    Ok(TunnelLock { path })
}
