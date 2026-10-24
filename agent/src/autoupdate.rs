//! Automatic updates at night: between 2 and 5 am in the owner's time zone the
//! box checks for updates, installs a newer Tailscale, then a newer system and
//! restarts into it. App updates stay the owner's choice: a new app version can
//! change how things work. The owner can turn this off and sets the time zone
//! on the status page (stored in settings.json in the agent's data folder).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// The night window, in minutes after local midnight: 2:00 to 5:00
pub const WINDOW_START: u32 = 2 * 60;
pub const WINDOW_END: u32 = 5 * 60;
/// Each night starts somewhere in the first two hours of the window, so not
/// every box asks for updates at the same moment
const SPREAD: u32 = 120;

const ZONEINFO: &str = "/usr/share/zoneinfo";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    /// Install system and Tailscale updates at night
    pub enabled: bool,
    /// IANA time zone ("America/New_York"); None until the owner picks one (then UTC)
    pub timezone: Option<String>,
    /// Also take test versions (pre-releases), see manifest.rs
    #[serde(default)]
    pub testing: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true, timezone: None, testing: false }
    }
}

/// What the last night's run did, for the status page (kept on disk: a
/// system update ends with a restart)
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LastRun {
    /// Local date, YYYY-MM-DD
    pub date: String,
    pub summary: String,
}

#[derive(Clone)]
pub struct AutoUpdate {
    path: PathBuf,
    last_path: PathBuf,
    zoneinfo: PathBuf,
    settings: Arc<Mutex<Settings>>,
    last: Arc<Mutex<Option<LastRun>>>,
}

/// A time zone name that's safe to use as a path under zoneinfo
fn plausible_zone(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('/')
        && name.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "/_-+".contains(c))
}

/// The local date and minutes after midnight, from `date` in time zone `tz`
/// (a fresh process each time, so a changed zone takes effect at once)
pub async fn local_now(tz: Option<&str>) -> Option<(String, u32)> {
    let mut cmd = tokio::process::Command::new("date");
    cmd.arg("+%Y-%m-%d %H %M").env("TZ", tz.map(|z| format!(":{z}")).unwrap_or_else(|| "UTC".into()));
    let out = cmd.kill_on_drop(true).output().await.ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut parts = text.split_whitespace();
    let date = parts.next()?.to_owned();
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    Some((date, hour * 60 + minute))
}

/// Minutes after midnight when tonight's run may start (fixed per date)
pub fn start_minute(date: &str) -> u32 {
    // FNV-1a over the date and the Wi-Fi address: varies by night and by box
    let salt = std::fs::read_to_string("/sys/class/net/wlan0/address").unwrap_or_default();
    let mut h: u32 = 0x811c9dc5;
    for b in date.bytes().chain(salt.bytes()) {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    WINDOW_START + h % SPREAD
}

/// Whether tonight's run should start now
pub fn due(settings: &Settings, date: &str, minute: u32, start: u32, done: Option<&str>) -> bool {
    settings.enabled && done != Some(date) && minute >= start && minute < WINDOW_END
}

impl AutoUpdate {
    pub fn new(data_dir: &Path) -> Self {
        let path = data_dir.join("settings.json");
        let last_path = data_dir.join("last-night.json");
        let read = |p: &Path| std::fs::read(p).ok();
        let settings = read(&path).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let last = read(&last_path).and_then(|b| serde_json::from_slice(&b).ok());
        Self {
            path,
            last_path,
            zoneinfo: ZONEINFO.into(),
            settings: Arc::new(Mutex::new(settings)),
            last: Arc::new(Mutex::new(last)),
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    pub fn last_run(&self) -> Option<LastRun> {
        self.last.lock().unwrap().clone()
    }

    pub fn record(&self, date: &str, summary: String) {
        eprintln!("night updates: {summary}");
        let run = LastRun { date: date.to_owned(), summary };
        if let Ok(json) = serde_json::to_vec(&run) {
            let _ = std::fs::write(&self.last_path, json);
        }
        *self.last.lock().unwrap() = Some(run);
    }

    /// Time zones the box knows about (region/city names from zoneinfo)
    pub fn zones(&self) -> Vec<String> {
        const REGIONS: [&str; 9] = [
            "Africa",
            "America",
            "Antarctica",
            "Asia",
            "Atlantic",
            "Australia",
            "Europe",
            "Indian",
            "Pacific",
        ];
        let mut zones = vec!["UTC".to_owned()];
        for region in REGIONS {
            let mut stack = vec![(self.zoneinfo.join(region), region.to_owned())];
            while let Some((dir, prefix)) = stack.pop() {
                for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
                    match entry.file_type() {
                        Ok(t) if t.is_dir() => stack.push((entry.path(), name)),
                        Ok(_) if plausible_zone(&name) => zones.push(name),
                        _ => {}
                    }
                }
            }
        }
        zones[1..].sort();
        zones
    }

    pub fn valid_zone(&self, name: &str) -> bool {
        name == "UTC" || (plausible_zone(name) && self.zoneinfo.join(name).is_file())
    }

    /// Saves new settings; Err if the time zone isn't one the box knows
    pub fn save(&self, enabled: bool, timezone: &str, testing: bool) -> Result<(), String> {
        if !self.valid_zone(timezone) {
            return Err(format!("Unknown time zone {timezone:?}"));
        }
        self.store(Settings { enabled, timezone: Some(timezone.to_owned()), testing })
    }

    /// Turns test versions on or off, keeping the other settings
    pub fn set_testing(&self, testing: bool) -> Result<(), String> {
        self.store(Settings { testing, ..self.settings() })
    }

    fn store(&self, settings: Settings) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .map_err(|e| format!("Couldn't save the settings: {e}"))?;
        *self.settings.lock().unwrap() = settings;
        Ok(())
    }

    #[cfg(test)]
    fn with_zoneinfo(mut self, dir: &Path) -> Self {
        self.zoneinfo = dir.to_owned();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_once_a_night_inside_the_window() {
        let on = Settings { enabled: true, timezone: Some("Europe/Berlin".into()), testing: false };
        let start = 150;
        assert!(!due(&on, "2026-10-23", 149, start, None), "not before tonight's start");
        assert!(due(&on, "2026-10-23", 150, start, None));
        assert!(due(&on, "2026-10-23", 299, start, Some("2026-10-22")));
        assert!(!due(&on, "2026-10-23", 300, start, None), "not after 5 am");
        assert!(!due(&on, "2026-10-23", 200, start, Some("2026-10-23")), "once per night");
        let off = Settings { enabled: false, ..on };
        assert!(!due(&off, "2026-10-23", 200, start, None));
    }

    #[test]
    fn start_varies_but_stays_in_the_window() {
        let starts: Vec<u32> = (1..=28).map(|d| start_minute(&format!("2026-02-{d:02}"))).collect();
        assert!(starts.iter().all(|&s| (WINDOW_START..WINDOW_START + SPREAD).contains(&s)));
        assert!(starts.iter().any(|&s| s != starts[0]));
        assert_eq!(start_minute("2026-02-01"), starts[0], "fixed for a given night");
    }

    #[test]
    fn checks_time_zone_names() {
        assert!(plausible_zone("America/Argentina/Buenos_Aires") && plausible_zone("Etc/GMT+5"));
        for bad in ["", "/etc/shadow", "../../etc/passwd", "America/../../x", "Europe//Paris", "a b"] {
            assert!(!plausible_zone(bad), "{bad:?}");
        }
    }

    #[test]
    fn saves_settings_and_lists_zones() {
        let dir = tempfile::tempdir().unwrap();
        let zoneinfo = dir.path().join("zoneinfo");
        std::fs::create_dir_all(zoneinfo.join("America/Indiana")).unwrap();
        std::fs::create_dir_all(zoneinfo.join("Europe")).unwrap();
        for z in ["America/New_York", "America/Indiana/Knox", "Europe/Paris"] {
            std::fs::write(zoneinfo.join(z), b"TZif").unwrap();
        }
        let auto = AutoUpdate::new(dir.path()).with_zoneinfo(&zoneinfo);
        assert_eq!(auto.settings(), Settings::default());
        assert_eq!(auto.zones(), ["UTC", "America/Indiana/Knox", "America/New_York", "Europe/Paris"]);
        assert!(auto.save(true, "Mars/Olympus", false).is_err());
        assert!(auto.save(true, "../etc", false).is_err());
        auto.save(false, "Europe/Paris", false).unwrap();
        auto.set_testing(true).unwrap();
        auto.record("2026-10-23", "Nothing new.".into());
        let reloaded = AutoUpdate::new(dir.path());
        assert_eq!(
            reloaded.settings(),
            Settings { enabled: false, timezone: Some("Europe/Paris".into()), testing: true }
        );
        assert_eq!(reloaded.last_run().unwrap().summary, "Nothing new.");
    }

    #[tokio::test]
    async fn reads_the_local_time() {
        let (date, minute) = local_now(Some("UTC")).await.unwrap();
        assert_eq!(date.len(), 10);
        assert!(minute < 24 * 60);
        assert!(local_now(None).await.is_some());
    }
}
