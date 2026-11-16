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

/// Time left at the rate the battery has been falling since it last went up
/// (a charge, or a fresh battery). None until it has dropped a few percent
/// over at least an hour, so a single noisy reading gives no estimate.
pub fn time_left(log: &Log) -> Option<Duration> {
    let (last_at, last) = *log.last()?;
    let mut start = log.len() - 1;
    while start > 0 && log[start - 1].1 >= log[start].1 {
        start -= 1;
    }
    let (first_at, first) = log[start];
    let dropped = first.saturating_sub(last);
    let span = last_at.duration_since(first_at).ok()?;
    if dropped < 3 || span < Duration::from_secs(60 * 60) {
        return None;
    }
    let per_second = f64::from(dropped) / span.as_secs_f64();
    Some(Duration::from_secs_f64(f64::from(last) / per_second))
}

/// "about 5 h left", "about 2 days left".
pub fn describe_left(left: Duration) -> String {
    let hours = left.as_secs() / 3600;
    match hours {
        0 => "under an hour left".to_string(),
        1..=47 => format!("about {hours} h left"),
        _ => format!("about {} days left", (hours + 12) / 24),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: u64, percent: u8) -> (SystemTime, u8) {
        (UNIX_EPOCH + Duration::from_secs(1_000_000 + hours * 3600), percent)
    }

    #[test]
    fn estimates_from_the_current_discharge_only() {
        // Charged to 100 at hour 10, then 10 % in 10 hours: 90 % left is ~90 h.
        let log = vec![at(0, 40), at(5, 30), at(10, 100), at(15, 95), at(20, 90)];
        let left = time_left(&log).unwrap();
        assert_eq!(left.as_secs() / 3600, 90);
        assert_eq!(describe_left(left), "about 4 days left");
    }

    #[test]
    fn no_estimate_from_too_little() {
        assert!(time_left(&vec![at(0, 80), at(5, 79)]).is_none());
        assert!(time_left(&vec![at(0, 80)]).is_none());
    }

    #[test]
    fn samples_spread_over_the_log() {
        let log: Log = (0..100).map(|i| at(i, (100 - i) as u8)).collect();
        let picked = sample(&log, 10);
        assert_eq!(picked.len(), 10);
        assert_eq!(picked[0], 100);
        assert_eq!(picked[9], 1);
    }
}
