//! What the status page and `/_system/api/status` report.

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, Debug, PartialEq)]
pub struct Status {
    pub os: Os,
    pub tailscale: Tailscale,
    /// None if /data isn't mounted
    pub data: Option<Disk>,
    pub uptime_seconds: u64,
}

#[derive(Serialize, Debug, PartialEq, Default)]
pub struct Os {
    pub version: String,
    /// "A" or "B"; None if RAUC didn't answer
    pub booted_slot: Option<String>,
    pub slots: Vec<Slot>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Slot {
    pub name: String,
    /// RAUC's view: "booted", "inactive", ...
    pub state: String,
    /// "good", "bad" or empty when unknown
    pub boot_status: String,
}

#[derive(Serialize, Debug, PartialEq, Default)]
pub struct Tailscale {
    /// tailscaled answered
    pub running: bool,
    pub version: Option<String>,
    /// e.g. "NeedsLogin", "Running", "Stopped"
    pub backend_state: Option<String>,
    /// Name on the tailnet, without the trailing dot
    pub dns_name: Option<String>,
    pub ips: Vec<String>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Disk {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs a command and parses its stdout as JSON; None on any failure.
async fn json_command(program: &str, args: &[&str]) -> Option<Value> {
    let run = tokio::process::Command::new(program).args(args).kill_on_drop(true).output();
    let out = tokio::time::timeout(COMMAND_TIMEOUT, run).await.ok()?.ok()?;
    // `tailscale status --json` exits non-zero when logged out but still prints JSON
    serde_json::from_slice(&out.stdout).ok()
}

pub async fn gather() -> Status {
    let (rauc, tailscale) = tokio::join!(
        json_command("rauc", &["status", "--output-format=json"]),
        json_command("tailscale", &["status", "--json"]),
    );
    let mut os = rauc.as_ref().map(parse_rauc).unwrap_or_default();
    os.version = read_trimmed("/etc/box-version").unwrap_or_else(|| "unknown".into());
    Status {
        os,
        tailscale: tailscale.as_ref().map(parse_tailscale).unwrap_or_default(),
        data: disk_usage("/data"),
        uptime_seconds: read_trimmed("/proc/uptime")
            .and_then(|s| s.split('.').next().and_then(|n| n.parse().ok()))
            .unwrap_or(0),
    }
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

/// `rauc status --output-format=json`:
/// {"booted":"A","slots":[{"rootfs.0":{"bootname":"A","state":"booted","boot_status":"good",...}},...]}
fn parse_rauc(v: &Value) -> Os {
    let mut slots: Vec<Slot> = v["slots"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.as_object()?.values().next())
        .filter(|slot| slot["class"] == "rootfs")
        .filter_map(|slot| {
            Some(Slot {
                name: slot["bootname"].as_str()?.to_owned(),
                state: slot["state"].as_str().unwrap_or("").to_owned(),
                boot_status: slot["boot_status"].as_str().unwrap_or("").to_owned(),
            })
        })
        .collect();
    slots.sort_by(|a, b| a.name.cmp(&b.name));
    Os { version: String::new(), booted_slot: v["booted"].as_str().map(str::to_owned), slots }
}

/// `tailscale status --json`: {"Version":"1.104.1-t..","BackendState":"Running",
/// "Self":{"DNSName":"box.tailnet.ts.net.","TailscaleIPs":["100.x.y.z",..]}}
fn parse_tailscale(v: &Value) -> Tailscale {
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    Tailscale {
        running: v["BackendState"].is_string(),
        version: v["Version"].as_str().and_then(|s| non_empty(s.split('-').next().unwrap_or(s))),
        backend_state: v["BackendState"].as_str().and_then(non_empty),
        dns_name: v["Self"]["DNSName"].as_str().and_then(|s| non_empty(s.trim_end_matches('.'))),
        ips: v["Self"]["TailscaleIPs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|ip| ip.as_str().map(str::to_owned))
            .collect(),
    }
}

fn disk_usage(path: &str) -> Option<Disk> {
    let c_path = std::ffi::CString::new(path).ok()?;
    // SAFETY: c_path is a valid C string; st is written by statvfs before use.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // Only report /data itself, not the directory under it when it isn't mounted
    let parent = std::ffi::CString::new("/").ok()?;
    let (mut a, mut b): (libc::stat, libc::stat) = unsafe { (std::mem::zeroed(), std::mem::zeroed()) };
    // SAFETY: as above
    if unsafe { libc::stat(c_path.as_ptr(), &mut a) != 0 || libc::stat(parent.as_ptr(), &mut b) != 0 } {
        return None;
    }
    if a.st_dev == b.st_dev {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some(Disk { total_bytes: st.f_blocks as u64 * frsize, free_bytes: st.f_bavail as u64 * frsize })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rauc_slots() {
        // Shape captured from the device
        let v = json!({"compatible":"box-zero2w","booted":"A","boot_primary":"rootfs.0","slots":[
            {"rootfs.1":{"class":"rootfs","device":"/dev/mmcblk0p3","bootname":"B","state":"inactive","boot_status":"good"}},
            {"rootfs.0":{"class":"rootfs","device":"/dev/mmcblk0p2","bootname":"A","state":"booted","boot_status":"good"}}]});
        let os = parse_rauc(&v);
        assert_eq!(os.booted_slot.as_deref(), Some("A"));
        assert_eq!(
            os.slots,
            vec![
                Slot { name: "A".into(), state: "booted".into(), boot_status: "good".into() },
                Slot { name: "B".into(), state: "inactive".into(), boot_status: "good".into() },
            ]
        );
    }

    #[test]
    fn tailscale_logged_out() {
        let v = json!({"Version":"1.104.1-t7f4efe814-g8da26756c","BackendState":"NeedsLogin","Self":{"DNSName":"","TailscaleIPs":null}});
        let ts = parse_tailscale(&v);
        assert_eq!(
            ts,
            Tailscale {
                running: true,
                version: Some("1.104.1".into()),
                backend_state: Some("NeedsLogin".into()),
                dns_name: None,
                ips: vec![],
            }
        );
    }

    #[test]
    fn tailscale_connected() {
        let v = json!({"Version":"1.104.1-tabc","BackendState":"Running",
            "Self":{"DNSName":"box.tail1234.ts.net.","TailscaleIPs":["100.101.102.103","fd7a::1"]}});
        let ts = parse_tailscale(&v);
        assert_eq!(ts.dns_name.as_deref(), Some("box.tail1234.ts.net"));
        assert_eq!(ts.ips, vec!["100.101.102.103", "fd7a::1"]);
    }

    #[test]
    fn missing_tools_give_defaults() {
        assert_eq!(parse_tailscale(&json!({})), Tailscale::default());
        assert_eq!(parse_rauc(&json!({})), Os::default());
    }
}
