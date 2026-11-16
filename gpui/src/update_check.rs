//! Updates from the `latest.json` each GitHub Release publishes (every build
//! of this fork publishes one, see `.github/workflows/build-gpui-windows.yml`).
//! A newer version's installer is downloaded in the background and checked
//! against its SHA-256; "Restart to update" (or the next launch) hands over to
//! it with `--update`, which installs silently and starts the app again.

use std::time::Duration;

use gpui_kit::{App, Entity};
use serde::Deserialize;

use crate::app::AppView;

const LATEST_JSON_URL: &str = "https://github.com/NiteRix/Desktop/releases/latest/download/latest.json";
/// Where release notes and manual downloads live.
pub const RELEASES_URL: &str = "https://github.com/NiteRix/Desktop/releases/latest";
/// The installer's silent mode (`openmouse_installer::UPDATE_ARG`).
const INSTALLER_UPDATE_ARG: &str = "--update";
/// Background cadence. There's no rush — a flaky network shouldn't nag, and
/// the startup check plus the manual "Check for updates" button cover the
/// cases where someone actually wants a fresh answer right now.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub platforms: std::collections::HashMap<String, Asset>,
}

/// One platform's installer in `latest.json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
}

impl UpdateInfo {
    /// This machine's installer, if the release has one.
    pub fn asset(&self) -> Option<&Asset> {
        if cfg!(windows) {
            self.platforms.get("windows-x86_64")
        } else {
            None
        }
    }
}

/// A downloaded, verified installer waiting to be run.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct Pending {
    pub version: String,
    pub path: std::path::PathBuf,
}

fn update_dir() -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|dir| dir.join("openmouse").join("update"))
}

fn sha256_of(path: &std::path::Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    Some(Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

/// Downloads the release's installer (unless it already is) and checks its
/// SHA-256. Blocking; run off the UI thread. Records it as pending, so the
/// next launch installs it even if nobody presses "Restart to update".
pub fn download(info: &UpdateInfo) -> Result<Pending, String> {
    let asset = info.asset().ok_or("no installer for this platform")?;
    let dir = update_dir().ok_or("no cache folder")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("OpenMouseDesktopSetup-{}.exe", info.version));
    let want = asset.sha256.to_ascii_lowercase();
    if sha256_of(&path).as_deref() != Some(want.as_str()) {
        let part = path.with_extension("part");
        let status = crate::proc::quiet(
            std::process::Command::new("curl")
                .args(["-sSfL", "-m", "900", "-o"])
                .arg(&part)
                .arg(&asset.url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
        )
        .status()
        .map_err(|e| e.to_string())?;
        if !status.success() {
            let _ = std::fs::remove_file(&part);
            return Err("download failed".into());
        }
        if sha256_of(&part).as_deref() != Some(want.as_str()) {
            let _ = std::fs::remove_file(&part);
            return Err("the download did not match its checksum".into());
        }
        std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
    }
    // Older downloads are no use any more.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p != path && p.extension().is_some_and(|e| e == "exe" || e == "part") {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    let pending = Pending { version: info.version.clone(), path };
    if let Ok(text) = serde_json::to_string(&pending) {
        let _ = std::fs::write(dir.join("pending.json"), text);
    }
    Ok(pending)
}

/// The pending update, if it is newer than `current` and still on disk.
/// Taking it clears the record, so a failing installer cannot loop.
pub fn take_pending(current: &str) -> Option<Pending> {
    let record = update_dir()?.join("pending.json");
    let pending: Pending = serde_json::from_str(&std::fs::read_to_string(&record).ok()?).ok()?;
    let _ = std::fs::remove_file(&record);
    (is_newer(current, &pending.version) && pending.path.exists()).then_some(pending)
}

/// Starts the downloaded installer in its silent update mode. The caller
/// quits straight after, so the installer can replace the app's .exe.
pub fn launch_installer(pending: &Pending, autostart: bool) -> Result<(), String> {
    let mut command = std::process::Command::new(&pending.path);
    command.arg(INSTALLER_UPDATE_ARG);
    if autostart {
        command.arg(crate::autostart::ARG);
    }
    command.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Fetches `latest.json` via the system `curl` — the same dependency-free
/// convention `games::prefetch_art` uses for network access, rather than
/// pulling in an HTTP client crate for one small GET. Blocking; callers must
/// run this off the UI thread. The per-platform `url`/`signature` fields in
/// the updater JSON are ignored entirely at this stage.
pub fn fetch_latest() -> Option<UpdateInfo> {
    let output = crate::proc::quiet(
        std::process::Command::new("curl")
            .args(["-sSfL", "-m", "10"])
            .arg(LATEST_JSON_URL)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    )
    .output()
    .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// True when `latest` is a newer version than `current`. Both are bare
/// `x.y.z` strings (no prerelease tag, see `APP_VERSION`'s usage in
/// `app.rs`); a missing/non-numeric component compares as `0`, so a
/// malformed string never panics — it just sorts low.
pub fn is_newer(current: &str, latest: &str) -> bool {
    fn parts(v: &str) -> Vec<u32> {
        v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    }
    parts(latest) > parts(current)
}

/// Startup check plus a 6-hour background poll, mirroring
/// `theme::spawn_wallpaper_watcher`'s shape: the fetch runs on the
/// background executor, the result lands on `AppView` via the foreground
/// executor. Detached — ends on its own once the view is dropped.
pub fn spawn_update_watcher(cx: &mut App, view: &Entity<AppView>) {
    let view = view.downgrade();
    cx.spawn(async move |cx| {
        loop {
            let info = cx.background_executor().spawn(async { fetch_latest() }).await;
            if let Some(info) = info {
                if view.update(cx, |view, cx| view.note_update_available(info, cx)).is_err() {
                    break;
                }
            } else if view.upgrade().is_none() {
                break;
            }
            cx.background_executor().timer(CHECK_INTERVAL).await;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_numeric_components_not_strings() {
        assert!(is_newer("0.0.29", "0.0.30"));
        assert!(is_newer("0.0.9", "0.0.10"));
        assert!(is_newer("0.0.29", "0.1.0"));
        assert!(!is_newer("0.0.29", "0.0.29"));
        assert!(!is_newer("0.1.0", "0.0.99"));
    }

    #[test]
    fn malformed_versions_do_not_panic() {
        assert!(!is_newer("not-a-version", ""));
        assert!(is_newer("", "0.0.1"));
    }
}
