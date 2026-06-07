//! Best-effort "is a newer release available?" check.
//!
//! Bruce has no HTTP stack (and we don't want to pull a heavy TLS dependency on
//! the user's slow connection), so the check shells out to `curl` — present on
//! Windows 10+, macOS and virtually all Linux. Everything here degrades quietly:
//! if `curl` is missing or offline, no badge is shown.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// GitHub repo whose releases we check.
const REPO: &str = "soydanielsierradev/bruce-tui";

/// The version this binary was built as, e.g. `"0.9.1"`.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Releases page URL (shown in the update dialog / opened in the browser).
pub fn releases_url() -> String {
    format!("https://github.com/{REPO}/releases")
}

/// A background version check. Spawns a thread immediately; the result is read
/// later, once, via [`Check::poll`].
pub struct Check {
    latest: Arc<Mutex<Option<String>>>,
    done: Arc<AtomicBool>,
}

impl Check {
    /// Start fetching the latest release tag in the background.
    pub fn spawn() -> Self {
        let latest = Arc::new(Mutex::new(None));
        let done = Arc::new(AtomicBool::new(false));
        let latest_t = Arc::clone(&latest);
        let done_t = Arc::clone(&done);
        std::thread::spawn(move || {
            let result = fetch_latest();
            if let Ok(mut slot) = latest_t.lock() {
                *slot = result;
            }
            done_t.store(true, Ordering::SeqCst);
        });
        Self { latest, done }
    }

    /// If the background fetch has finished, return its result once: the inner
    /// `Option` is the fetched version (or `None` when the fetch failed).
    /// Returns `None` while still in flight.
    pub fn poll(&self) -> Option<Option<String>> {
        if self.done.load(Ordering::SeqCst) {
            Some(self.latest.lock().ok().and_then(|g| g.clone()))
        } else {
            None
        }
    }
}

/// Ask GitHub for the latest release tag, returned without the leading `v`
/// (e.g. `"0.9.2"`). `None` on any failure (no curl, offline, rate-limited).
pub fn fetch_latest() -> Option<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let output = Command::new("curl")
        .args(["-fsSL", "-H", "User-Agent: bruce-tui", &url])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let tag = json.get("tag_name")?.as_str()?;
    Some(tag.trim_start_matches('v').trim().to_string())
}

/// True when `latest` is a strictly newer semver (major.minor.patch) than
/// `current`. Non-numeric parts are treated as 0 (best-effort for clean tags).
pub fn is_newer(latest: &str, current: &str) -> bool {
    semver(latest) > semver(current)
}

fn semver(v: &str) -> (u64, u64, u64) {
    let mut parts = v
        .split('.')
        .map(|p| p.trim().parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// Open `url` in the user's default browser. Best-effort, fire-and-forget.
pub fn open_in_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = cmd.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_detects_bumps() {
        assert!(is_newer("0.9.2", "0.9.1"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.9.1", "0.9.1"));
        assert!(!is_newer("0.9.0", "0.9.1"));
    }
}
