//! Battery readings kept across launches, for the Overview's "Battery over
//! time" panel (the web app keeps the past seven days the same way).
//!
//! Stored at `<config_dir>/openmouse/battery-history.json`, keyed by the
//! mouse's name so its cable and receiver connections share one history.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How far back readings are kept.
pub const KEEP: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A reading is stored when the level changes, or this long after the last one.
pub const EVERY: Duration = Duration::from_secs(10 * 60);
const MAX_PER_DEVICE: usize = 2000;

pub type Log = Vec<(SystemTime, u8)>;

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("openmouse").join("battery-history.json"))
}

fn secs(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Every device's readings from the last [`KEEP`].
pub fn load() -> HashMap<String, Log> {
    let stored: HashMap<String, Vec<(u64, u8)>> = path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let cutoff = secs(SystemTime::now()).saturating_sub(KEEP.as_secs());
    stored
        .into_iter()
        .map(|(name, readings)| {
            let log: Log = readings
                .into_iter()
                .filter(|(at, _)| *at >= cutoff)
                .map(|(at, percent)| (UNIX_EPOCH + Duration::from_secs(at), percent.min(100)))
                .collect();
            (name, log)
        })
        .filter(|(_, log)| !log.is_empty())
        .collect()
}

/// Adds a reading if it is news; returns whether the log changed.
pub fn record(log: &mut Log, percent: u8) -> bool {
    let now = SystemTime::now();
    let due = log
        .last()
        .is_none_or(|(at, p)| *p != percent || now.duration_since(*at).is_ok_and(|d| d >= EVERY));
    if !due {
        return false;
    }
    log.push((now, percent));
    let cutoff = now.checked_sub(KEEP).unwrap_or(UNIX_EPOCH);
    log.retain(|(at, _)| *at >= cutoff);
    if log.len() > MAX_PER_DEVICE {
        let extra = log.len() - MAX_PER_DEVICE;
        log.drain(..extra);
    }
    true
}

pub fn save(all: &HashMap<String, Log>) {
    let Some(path) = path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let stored: HashMap<&String, Vec<(u64, u8)>> =
        all.iter().map(|(name, log)| (name, log.iter().map(|(at, p)| (secs(*at), *p)).collect())).collect();
    if let Ok(text) = serde_json::to_string(&stored) {
        let _ = std::fs::write(path, text);
    }
}

/// At most `bars` readings spread evenly over the log, oldest first.
pub fn sample(log: &Log, bars: usize) -> Vec<u8> {
    if log.len() <= bars || bars < 2 {
        return log.iter().map(|(_, p)| *p).collect();
    }
    (0..bars)
        .map(|i| log[i * (log.len() - 1) / (bars - 1)].1)
        .collect()
}

/// "3 min ago", "5 h ago", "2 days ago".
pub fn ago(at: SystemTime) -> String {
    let minutes = at.elapsed().map(|d| d.as_secs() / 60).unwrap_or(0);
    match minutes {
        0..=59 => format!("{minutes} min ago"),
        60..=1439 => format!("{} h ago", minutes / 60),
        _ => {
            let days = minutes / 1440;
            if days == 1 { "1 day ago".to_string() } else { format!("{days} days ago") }
        }
    }
}
