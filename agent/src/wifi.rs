//! Wi-Fi: joining networks from the system pages, and the setup hotspot.
//!
//! The agent drives iwd over D-Bus with `busctl` (part of systemd), whose JSON
//! output is easy to parse; no D-Bus library needed.
//!
//! Joining: the password goes into an iwd network file in iwd's storage
//! (/data/iwd, bind-mounted into iwd, which watches the folder), then
//! `Network.Connect` uses it, so no iwd agent is needed. A failed join puts the
//! previous file back (or removes the new one).
//!
//! Setup hotspot: with no Wi-Fi network saved, or after a few minutes without a
//! connection, the radio switches to access point mode as `Box-Setup-XXXX`
//! (password [`HOTSPOT_PASSWORD`]). networkd gives it [`HOTSPOT_ADDRESS`] and runs
//! DHCP (10-setup-hotspot.network); captive_dns answers every name with that
//! address, so phones open the setup page by themselves. With saved networks the
//! hotspot only stays up a few minutes at a time (longer while someone is
//! connected) and gives way as soon as its scans see a saved network.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

const IWD: &str = "net.connman.iwd";
const DEVICE: &str = "net.connman.iwd.Device";
const STATION: &str = "net.connman.iwd.Station";
const ACCESS_POINT: &str = "net.connman.iwd.AccessPoint";
const NETWORK: &str = "net.connman.iwd.Network";
const KNOWN_NETWORK: &str = "net.connman.iwd.KnownNetwork";

/// The hotspot's name is this plus the end of the box's Wi-Fi address
pub const HOTSPOT_PREFIX: &str = "Box-Setup-";
/// Fixed and published in the setup guide: the box has no screen or label to
/// show a per-box one. The setup pages still need the admin password.
pub const HOTSPOT_PASSWORD: &str = "boxsetup";
/// The box's address on its hotspot (see 10-setup-hotspot.network)
pub const HOTSPOT_ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);

#[derive(Clone, Debug)]
pub struct Config {
    pub busctl: String,
    /// iwd's network files
    pub storage: PathBuf,
    /// The Wi-Fi interface the box uses
    pub interface: String,
    /// Disconnected this long with saved networks: hotspot
    pub offline_grace: Duration,
    /// How long a hotspot stays up when there are saved networks...
    pub hotspot_window: Duration,
    /// ...or while someone is connected to it
    pub hotspot_window_busy: Duration,
    pub tick: Duration,
}

impl Config {
    pub fn device() -> Self {
        Self {
            busctl: "busctl".into(),
            storage: "/data/iwd".into(),
            interface: "wlan0".into(),
            offline_grace: Duration::from_secs(180),
            hotspot_window: Duration::from_secs(300),
            hotspot_window_busy: Duration::from_secs(900),
            tick: Duration::from_secs(5),
        }
    }
}

/// A network seen in the last scan
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Visible {
    pub name: String,
    /// iwd's security type: "open", "psk", "8021x" (enterprise; not supported here)
    pub kind: String,
    /// dBm
    pub signal: i32,
    pub saved: bool,
    pub connected: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Hotspot {
    pub ssid: String,
    pub password: String,
    /// No network saved yet (first setup) rather than a fallback while offline
    pub setup: bool,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Join {
    #[default]
    Idle,
    Joining {
        ssid: String,
    },
    Joined {
        ssid: String,
    },
    Failed {
        ssid: String,
        error: String,
    },
}

/// What the pages and `/_system/api/wifi` show
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct View {
    /// iwd answered and the Wi-Fi device exists
    pub available: bool,
    /// Name of the network the box is on
    pub connected: Option<String>,
    /// iwd's station state ("connected", "disconnected", "connecting", ...)
    pub state: Option<String>,
    pub scanning: bool,
    pub hotspot: Option<Hotspot>,
    pub networks: Vec<Visible>,
    pub saved: Vec<String>,
    pub join: Join,
}

#[derive(Debug, PartialEq)]
pub enum JoinError {
    Busy,
    BadName,
    BadPassword,
}

// --- iwd's objects, as GetManagedObjects reports them ---

#[derive(Clone, Debug, Default, PartialEq)]
struct Objects {
    devices: Vec<DeviceObj>,
    networks: Vec<NetworkObj>,
    known: Vec<KnownObj>,
}

#[derive(Clone, Debug, PartialEq)]
struct DeviceObj {
    path: String,
    name: String,
    address: String,
    powered: bool,
    /// "station" or "ap"
    mode: String,
    station: Option<StationObj>,
    ap: Option<ApObj>,
}

#[derive(Clone, Debug, PartialEq)]
struct StationObj {
    state: String,
    connected_network: Option<String>,
    scanning: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct ApObj {
    started: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct NetworkObj {
    path: String,
    name: String,
    kind: String,
    device: String,
    known: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct KnownObj {
    path: String,
    name: String,
}

/// A property value from busctl's JSON (`{"type": "s", "data": ...}`)
fn prop<'a>(ifaces: &'a Value, iface: &str, name: &str) -> Option<&'a Value> {
    ifaces.get(iface)?.get(name)?.get("data")
}

fn prop_str(ifaces: &Value, iface: &str, name: &str) -> Option<String> {
    prop(ifaces, iface, name)?.as_str().map(str::to_owned)
}

fn prop_bool(ifaces: &Value, iface: &str, name: &str) -> bool {
    prop(ifaces, iface, name).and_then(Value::as_bool).unwrap_or(false)
}

fn parse_objects(reply: &Value) -> Objects {
    let mut objects = Objects::default();
    let Some(map) = reply["data"][0].as_object() else {
        return objects;
    };
    for (path, ifaces) in map {
        if ifaces.get(DEVICE).is_some() {
            objects.devices.push(DeviceObj {
                path: path.clone(),
                name: prop_str(ifaces, DEVICE, "Name").unwrap_or_default(),
                address: prop_str(ifaces, DEVICE, "Address").unwrap_or_default(),
                powered: prop_bool(ifaces, DEVICE, "Powered"),
                mode: prop_str(ifaces, DEVICE, "Mode").unwrap_or_default(),
                station: ifaces.get(STATION).map(|_| StationObj {
                    state: prop_str(ifaces, STATION, "State").unwrap_or_default(),
                    connected_network: prop_str(ifaces, STATION, "ConnectedNetwork"),
                    scanning: prop_bool(ifaces, STATION, "Scanning"),
                }),
                ap: ifaces
                    .get(ACCESS_POINT)
                    .map(|_| ApObj { started: prop_bool(ifaces, ACCESS_POINT, "Started") }),
            });
        }
        if ifaces.get(NETWORK).is_some() {
            objects.networks.push(NetworkObj {
                path: path.clone(),
                name: prop_str(ifaces, NETWORK, "Name").unwrap_or_default(),
                kind: prop_str(ifaces, NETWORK, "Type").unwrap_or_default(),
                device: prop_str(ifaces, NETWORK, "Device").unwrap_or_default(),
                known: prop_str(ifaces, NETWORK, "KnownNetwork").is_some(),
            });
        }
        if ifaces.get(KNOWN_NETWORK).is_some() {
            objects.known.push(KnownObj {
                path: path.clone(),
                name: prop_str(ifaces, KNOWN_NETWORK, "Name").unwrap_or_default(),
            });
        }
    }
    objects.devices.sort_by(|a, b| a.name.cmp(&b.name));
    objects
}

impl Objects {
    fn device(&self, name: &str) -> Option<&DeviceObj> {
        self.devices.iter().find(|d| d.name == name)
    }

    fn network(&self, path: &str) -> Option<&NetworkObj> {
        self.networks.iter().find(|n| n.path == path)
    }

    fn find_network(&self, device: &str, name: &str) -> Option<&NetworkObj> {
        self.networks.iter().find(|n| n.device == device && n.name == name)
    }

    /// Saved networks, leaving out setup hotspots (e.g. another box's)
    fn saved(&self) -> Vec<String> {
        let mut names: Vec<String> =
            self.known.iter().map(|k| k.name.clone()).filter(|n| !n.starts_with(HOTSPOT_PREFIX)).collect();
        names.sort();
        names.dedup();
        names
    }
}

/// `Station.GetOrderedNetworks` → visible networks (signal in 100 * dBm)
fn parse_station_networks(reply: &Value, objects: &Objects, connected: Option<&str>) -> Vec<Visible> {
    let mut list = Vec::new();
    for entry in reply["data"][0].as_array().into_iter().flatten() {
        let (Some(path), Some(signal)) = (entry[0].as_str(), entry[1].as_i64()) else {
            continue;
        };
        if let Some(net) = objects.network(path) {
            list.push(Visible {
                name: net.name.clone(),
                kind: net.kind.clone(),
                signal: (signal / 100) as i32,
                saved: net.known,
                connected: connected == Some(path),
            });
        }
    }
    tidy(list)
}

/// `AccessPoint.GetOrderedNetworks` → visible networks
fn parse_ap_networks(reply: &Value, saved: &[String]) -> Vec<Visible> {
    let mut list = Vec::new();
    for entry in reply["data"][0].as_array().into_iter().flatten() {
        let field = |name: &str| entry.get(name).and_then(|v| v.get("data"));
        let (Some(name), Some(kind)) =
            (field("Name").and_then(Value::as_str), field("Type").and_then(Value::as_str))
        else {
            continue;
        };
        list.push(Visible {
            name: name.to_owned(),
            kind: kind.to_owned(),
            signal: field("SignalStrength").and_then(Value::as_i64).map(|s| (s / 100) as i32).unwrap_or(-100),
            saved: saved.iter().any(|s| s == name),
            connected: false,
        });
    }
    tidy(list)
}

/// Strongest first, one entry per name and type, no setup hotspots
fn tidy(mut list: Vec<Visible>) -> Vec<Visible> {
    list.retain(|n| !n.name.is_empty() && !n.name.starts_with(HOTSPOT_PREFIX));
    list.sort_by_key(|n| std::cmp::Reverse(n.signal));
    let mut seen = std::collections::HashSet::new();
    list.retain(|n| seen.insert((n.name.clone(), n.kind.clone())));
    list
}

/// The hotspot's name, from the device's MAC address ("…:ee:ff" → "Box-Setup-EEFF")
fn hotspot_ssid(address: &str) -> String {
    let hex: String = address.chars().filter(char::is_ascii_hexdigit).collect();
    let tail = &hex[hex.len().saturating_sub(4)..];
    format!("{HOTSPOT_PREFIX}{}", tail.to_ascii_uppercase())
}

/// iwd's file name for a network: the SSID if it's plain, else `=` + hex
fn network_file(storage: &Path, ssid: &str, kind: &str) -> PathBuf {
    let plain = ssid.chars().all(|c| c.is_ascii_alphanumeric() || "-_ ".contains(c));
    let name = if plain {
        ssid.to_owned()
    } else {
        format!("={}", ssid.bytes().map(|b| format!("{b:02x}")).collect::<String>())
    };
    storage.join(format!("{name}.{kind}"))
}

/// A passphrase as an iwd settings value: backslashes and leading spaces escaped
fn escape_setting(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut leading = true;
    for c in value.chars() {
        match c {
            ' ' if leading => out.push_str("\\s"),
            '\\' => {
                leading = false;
                out.push_str("\\\\");
            }
            _ => {
                leading = false;
                out.push(c);
            }
        }
    }
    out
}

pub fn valid_ssid(ssid: &str) -> bool {
    (1..=32).contains(&ssid.len()) && !ssid.chars().any(char::is_control)
}

/// WPA passphrases: 8 to 63 printable ASCII characters
pub fn valid_passphrase(p: &str) -> bool {
    (8..=63).contains(&p.len()) && p.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// Friendlier words for iwd's errors when joining
fn join_error(ssid: &str, kind: &str, iwd_error: &str) -> String {
    match kind {
        "psk" => format!("Couldn't join \"{ssid}\". Check the password and try again. ({iwd_error})"),
        _ => format!("Couldn't join \"{ssid}\" ({iwd_error})."),
    }
}

struct Inner {
    view: View,
    started: Instant,
    /// When the box last had a connection (or stopped trying with the hotspot)
    online_at: Instant,
    hotspot_since: Option<Instant>,
    hotspot_failed_at: Option<Instant>,
    last_ap_scan: Option<Instant>,
}

#[derive(Clone)]
pub struct Wifi {
    config: Arc<Config>,
    inner: Arc<Mutex<Inner>>,
    /// One iwd operation at a time (mode switches, joins)
    op: Arc<tokio::sync::Mutex<()>>,
}

const SHORT: Duration = Duration::from_secs(10);
/// Before the first hotspot: give iwd a moment to load saved networks
const SETUP_DELAY: Duration = Duration::from_secs(10);
/// A hotspot that didn't start is retried after this
const HOTSPOT_RETRY: Duration = Duration::from_secs(60);
const AP_SCAN_EVERY: Duration = Duration::from_secs(60);
/// Lets the page that started a join from the hotspot arrive before the hotspot goes
const LEAVE_HOTSPOT_DELAY: Duration = Duration::from_secs(3);

impl Wifi {
    pub fn new(config: Config) -> Self {
        let now = Instant::now();
        Self {
            config: Arc::new(config),
            inner: Arc::new(Mutex::new(Inner {
                view: View::default(),
                started: now,
                online_at: now,
                hotspot_since: None,
                hotspot_failed_at: None,
                last_ap_scan: None,
            })),
            op: Default::default(),
        }
    }

    pub fn view(&self) -> View {
        self.inner.lock().unwrap().view.clone()
    }

    /// Pretends iwd reported `view` (tests of the pages around it)
    #[cfg(test)]
    pub fn set_view(&self, view: View) {
        self.inner.lock().unwrap().view = view;
    }

    pub fn hotspot_active(&self) -> bool {
        self.inner.lock().unwrap().view.hotspot.is_some()
    }

    async fn busctl(&self, args: &[&str], timeout: Duration) -> Result<Value, String> {
        let mut cmd = tokio::process::Command::new(&self.config.busctl);
        // "--" so SSIDs starting with "-" aren't read as options
        cmd.arg("--json=short").arg(format!("--timeout={}", timeout.as_secs())).arg("--").args(args);
        let run = cmd.kill_on_drop(true).output();
        let out = tokio::time::timeout(timeout + Duration::from_secs(5), run)
            .await
            .map_err(|_| "iwd didn't answer".to_owned())?
            .map_err(|e| format!("busctl: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let line = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("busctl failed");
            return Err(line.strip_prefix("Call failed: ").unwrap_or(line).to_owned());
        }
        if out.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&out.stdout).map_err(|e| format!("busctl output: {e}"))
    }

    async fn call(
        &self,
        path: &str,
        iface: &str,
        method: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Value, String> {
        let mut all = vec!["call", IWD, path, iface, method];
        all.extend_from_slice(args);
        self.busctl(&all, timeout).await
    }

    async fn objects(&self) -> Result<Objects, String> {
        let reply =
            self.call("/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", &[], SHORT).await?;
        Ok(parse_objects(&reply))
    }

    async fn set_mode(&self, device: &str, mode: &str) -> Result<(), String> {
        self.busctl(&["set-property", IWD, device, DEVICE, "Mode", "s", mode], SHORT).await.map(drop)
    }

    /// Waits until `ready` holds for our device, for up to `limit`
    async fn wait_for(
        &self,
        limit: Duration,
        ready: impl Fn(&Objects, &DeviceObj) -> bool,
    ) -> Option<Objects> {
        let deadline = Instant::now() + limit;
        loop {
            if let Ok(objects) = self.objects().await
                && objects.device(&self.config.interface).is_some_and(|d| ready(&objects, d))
            {
                return Some(objects);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn station_networks(&self, device: &DeviceObj, objects: &Objects) -> Vec<Visible> {
        let connected = device.station.as_ref().and_then(|s| s.connected_network.as_deref());
        match self.call(&device.path, STATION, "GetOrderedNetworks", &[], SHORT).await {
            Ok(reply) => parse_station_networks(&reply, objects, connected),
            Err(_) => Vec::new(),
        }
    }

    /// Station scan, waiting for it to finish (up to 15 s)
    async fn scan_and_wait(&self, device: &str) -> Option<Objects> {
        let _ = self.call(device, STATION, "Scan", &[], SHORT).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        self.wait_for(Duration::from_secs(15), |_, d| d.station.as_ref().is_some_and(|s| !s.scanning)).await
    }

    /// Switches to the hotspot; remembers what's around first, since the AP may not be able to scan
    async fn start_hotspot(&self, device: &DeviceObj, setup: bool) {
        if device.mode == "station"
            && let Some(objects) = self.scan_and_wait(&device.path).await
            && let Some(dev) = objects.device(&self.config.interface)
        {
            let networks = self.station_networks(dev, &objects).await;
            self.inner.lock().unwrap().view.networks = networks;
        }
        let ssid = hotspot_ssid(&device.address);
        let result = async {
            if device.mode != "ap" {
                self.set_mode(&device.path, "ap").await?;
                self.wait_for(Duration::from_secs(10), |_, d| d.ap.is_some())
                    .await
                    .ok_or("no access point mode")?;
            }
            self.call(&device.path, ACCESS_POINT, "Start", &["ss", &ssid, HOTSPOT_PASSWORD], SHORT).await
        }
        .await;
        match result {
            Ok(_) => {
                eprintln!("wifi: setup hotspot {ssid} started");
                let mut inner = self.inner.lock().unwrap();
                inner.hotspot_since = Some(Instant::now());
                inner.last_ap_scan = None;
                inner.view.hotspot = Some(Hotspot { ssid, password: HOTSPOT_PASSWORD.into(), setup });
                inner.view.connected = None;
            }
            Err(e) => {
                eprintln!("wifi: setup hotspot didn't start: {e}");
                self.inner.lock().unwrap().hotspot_failed_at = Some(Instant::now());
                let _ = self.set_mode(&device.path, "station").await;
            }
        }
    }

    async fn stop_hotspot(&self, device: &str) {
        let _ = self.call(device, ACCESS_POINT, "Stop", &[], SHORT).await;
        if let Err(e) = self.set_mode(device, "station").await {
            eprintln!("wifi: back to station mode: {e}");
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.view.hotspot.take().is_some() {
            eprintln!("wifi: setup hotspot stopped");
        }
        inner.hotspot_since = None;
        inner.online_at = Instant::now();
    }

    /// Number of devices connected to the hotspot
    async fn hotspot_clients(&self, device: &str) -> usize {
        let reply =
            self.call(device, "net.connman.iwd.AccessPointDiagnostic", "GetDiagnostics", &[], SHORT).await;
        reply.ok().and_then(|r| r["data"][0].as_array().map(Vec::len)).unwrap_or(0)
    }

    fn set_unavailable(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.view.available = false;
        inner.view.connected = None;
        inner.view.state = None;
    }

    /// One look at the Wi-Fi, deciding whether the hotspot should come or go
    async fn tick(&self) {
        let _op = self.op.lock().await;
        let Ok(objects) = self.objects().await else {
            return self.set_unavailable();
        };
        let Some(device) = objects.device(&self.config.interface).cloned() else {
            return self.set_unavailable();
        };
        if !device.powered {
            let _ = self
                .busctl(&["set-property", IWD, &device.path, DEVICE, "Powered", "b", "true"], SHORT)
                .await;
            return;
        }
        let saved = objects.saved();
        let now = Instant::now();
        match (device.mode.as_str(), &device.station, &device.ap) {
            ("station", Some(station), _) => {
                let connected = station
                    .connected_network
                    .as_deref()
                    .and_then(|p| objects.network(p))
                    .map(|n| n.name.clone());
                let networks = self.station_networks(&device, &objects).await;
                let start = {
                    let mut inner = self.inner.lock().unwrap();
                    let v = &mut inner.view;
                    v.available = true;
                    v.state = Some(station.state.clone());
                    v.scanning = station.scanning;
                    v.connected = connected.clone();
                    v.saved = saved.clone();
                    v.hotspot = None;
                    if !networks.is_empty() {
                        v.networks = networks;
                    }
                    if connected.is_some() || station.state == "connecting" {
                        inner.online_at = now;
                    }
                    inner.hotspot_since = None;
                    let retry_ok = inner.hotspot_failed_at.is_none_or(|t| now - t >= HOTSPOT_RETRY);
                    let due = if saved.is_empty() {
                        now - inner.started >= SETUP_DELAY
                    } else {
                        now - inner.online_at >= self.config.offline_grace
                    };
                    connected.is_none() && retry_ok && due
                };
                if start {
                    self.start_hotspot(&device, saved.is_empty()).await;
                }
            }
            ("ap", _, Some(ap)) => {
                if !ap.started {
                    // Not ours, or iwd restarted it: (re)start it as the hotspot
                    return self.start_hotspot(&device, saved.is_empty()).await;
                }
                let (since, scan_due) = {
                    let mut inner = self.inner.lock().unwrap();
                    inner.view.available = true;
                    inner.view.state = Some("hotspot".into());
                    inner.view.saved = saved.clone();
                    inner.view.connected = None;
                    if inner.view.hotspot.is_none() {
                        // The agent restarted while the hotspot was on
                        let ssid = hotspot_ssid(&device.address);
                        inner.view.hotspot = Some(Hotspot {
                            ssid,
                            password: HOTSPOT_PASSWORD.into(),
                            setup: saved.is_empty(),
                        });
                    }
                    if let Some(h) = inner.view.hotspot.as_mut() {
                        h.setup = saved.is_empty();
                    }
                    let since = *inner.hotspot_since.get_or_insert(now);
                    (since, inner.last_ap_scan.is_none_or(|t| now - t >= AP_SCAN_EVERY))
                };
                if let Ok(reply) =
                    self.call(&device.path, ACCESS_POINT, "GetOrderedNetworks", &[], SHORT).await
                {
                    let networks = parse_ap_networks(&reply, &saved);
                    if !networks.is_empty() {
                        self.inner.lock().unwrap().view.networks = networks;
                    }
                }
                if saved.is_empty() || self.join_busy() {
                    return;
                }
                // Fallback hotspot: scan now and then, leave when a saved network is back or time's up
                if scan_due {
                    self.inner.lock().unwrap().last_ap_scan = Some(now);
                    let _ = self.call(&device.path, ACCESS_POINT, "Scan", &[], SHORT).await;
                }
                let saved_nearby = self.view().networks.iter().any(|n| n.saved);
                let window = if self.hotspot_clients(&device.path).await > 0 {
                    self.config.hotspot_window_busy
                } else {
                    self.config.hotspot_window
                };
                if saved_nearby || now - since >= window {
                    self.stop_hotspot(&device.path).await;
                }
            }
            _ => self.set_unavailable(),
        }
    }

    pub fn spawn(&self) {
        let wifi = self.clone();
        tokio::spawn(async move {
            loop {
                wifi.tick().await;
                tokio::time::sleep(wifi.config.tick).await;
            }
        });
    }

    fn join_busy(&self) -> bool {
        matches!(self.inner.lock().unwrap().view.join, Join::Joining { .. })
    }

    /// Asks for a fresh list of networks (shown on the next look, a few seconds later)
    pub fn scan(&self) {
        let wifi = self.clone();
        tokio::spawn(async move {
            let _op = wifi.op.lock().await;
            let Ok(objects) = wifi.objects().await else { return };
            if let Some(device) = objects.device(&wifi.config.interface) {
                let iface = if device.mode == "ap" { ACCESS_POINT } else { STATION };
                let _ = wifi.call(&device.path, iface, "Scan", &[], SHORT).await;
            }
        });
    }

    /// Joins `ssid` in the background (leaving the hotspot if it's on); see `view().join`
    pub fn join(&self, ssid: String, passphrase: String) -> Result<(), JoinError> {
        if !valid_ssid(&ssid) {
            return Err(JoinError::BadName);
        }
        if !passphrase.is_empty() && !valid_passphrase(&passphrase) {
            return Err(JoinError::BadPassword);
        }
        let from_hotspot = {
            let mut inner = self.inner.lock().unwrap();
            if matches!(inner.view.join, Join::Joining { .. }) {
                return Err(JoinError::Busy);
            }
            inner.view.join = Join::Joining { ssid: ssid.clone() };
            inner.view.hotspot.is_some()
        };
        let wifi = self.clone();
        tokio::spawn(async move {
            if from_hotspot {
                tokio::time::sleep(LEAVE_HOTSPOT_DELAY).await;
            }
            let _op = wifi.op.lock().await;
            let result = wifi.connect(&ssid, &passphrase).await;
            if let Err(e) = &result {
                eprintln!("wifi: joining {ssid:?} failed: {e}");
            }
            wifi.inner.lock().unwrap().view.join = match result {
                Ok(()) => Join::Joined { ssid },
                Err(error) => Join::Failed { ssid, error },
            };
            // Back to the hotspot straight away, so whoever's setting up sees what went wrong
            if from_hotspot
                && let Ok(objects) = wifi.objects().await
                && let Some(device) = objects.device(&wifi.config.interface)
                && device.station.as_ref().is_some_and(|s| s.connected_network.is_none())
            {
                let setup = objects.saved().is_empty();
                wifi.start_hotspot(device, setup).await;
            }
        });
        Ok(())
    }

    async fn connect(&self, ssid: &str, passphrase: &str) -> Result<(), String> {
        let objects = self.objects().await?;
        let device = objects.device(&self.config.interface).ok_or("The box has no Wi-Fi device.")?.clone();
        // Where to go back to if this doesn't work out
        let previous = device.station.as_ref().and_then(|s| s.connected_network.clone());
        if device.mode != "station" {
            self.stop_hotspot(&device.path).await;
            self.wait_for(Duration::from_secs(10), |_, d| d.station.is_some())
                .await
                .ok_or("The Wi-Fi didn't switch back from the hotspot.")?;
        }
        let objects = match self.objects().await?.find_network(&device.path, ssid) {
            Some(_) => self.objects().await?,
            None => self.scan_and_wait(&device.path).await.unwrap_or_default(),
        };
        let net = objects
            .find_network(&device.path, ssid)
            .cloned()
            .ok_or_else(|| format!("Couldn't find a network called \"{ssid}\" nearby."))?;
        let mut restore = None;
        match net.kind.as_str() {
            "open" => {}
            // Saved already: rejoin with the saved password
            "psk" if passphrase.is_empty() && net.known => {}
            "psk" if passphrase.is_empty() => return Err(format!("\"{ssid}\" needs a password.")),
            "psk" => {
                let path = network_file(&self.config.storage, ssid, "psk");
                restore = Some((path.clone(), std::fs::read(&path).ok()));
                let contents = format!("[Security]\nPassphrase={}\n", escape_setting(passphrase));
                write_private(&path, contents.as_bytes())
                    .map_err(|e| format!("Couldn't save the password: {e}"))?;
                // iwd notices the file by itself
                let net_path = net.path.clone();
                self.wait_for(Duration::from_secs(5), |o, _| o.network(&net_path).is_some_and(|n| n.known))
                    .await;
            }
            _ => return Err(format!("\"{ssid}\" uses enterprise sign-in, which the box doesn't support.")),
        }
        if restore.is_some() && previous.as_deref() == Some(net.path.as_str()) {
            // A new password for the network it's on: iwd answers "connected" without
            // trying it, so drop the connection first (the old password is restored on failure)
            let _ = self.call(&device.path, STATION, "Disconnect", &[], SHORT).await;
            self.wait_for(Duration::from_secs(10), |_, d| {
                d.station.as_ref().is_some_and(|s| s.connected_network.is_none())
            })
            .await;
        }
        match self.connect_network(&net.path).await {
            Ok(()) => {
                eprintln!("wifi: joined {ssid:?}");
                self.inner.lock().unwrap().online_at = Instant::now();
                Ok(())
            }
            Err(e) => {
                if let Some((path, old)) = restore {
                    let _ = match old {
                        Some(old) => write_private(&path, &old),
                        None => std::fs::remove_file(&path),
                    };
                }
                // iwd doesn't rejoin by itself after a failed handshake on that network
                if let Some(previous) = previous {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let _ = self.connect_network(&previous).await;
                }
                Err(join_error(ssid, &net.kind, &e))
            }
        }
    }

    async fn connect_network(&self, path: &str) -> Result<(), String> {
        let connect = || self.call(path, NETWORK, "Connect", &[], Duration::from_secs(60));
        match connect().await {
            // After a failed handshake iwd asks an agent for the password on the next
            // try (and stops asking after that); there is no agent, the file has it
            Err(e) if e.contains("No Agent") => connect().await.map(drop),
            result => result.map(drop),
        }
    }

    /// Forgets a saved network (disconnecting if it's the current one)
    pub async fn forget(&self, name: &str) -> Result<(), String> {
        let _op = self.op.lock().await;
        let objects = self.objects().await?;
        let known: Vec<&KnownObj> = objects.known.iter().filter(|k| k.name == name).collect();
        if known.is_empty() {
            return Err(format!("\"{name}\" isn't saved."));
        }
        for k in known {
            self.call(&k.path, KNOWN_NETWORK, "Forget", &[], SHORT).await?;
        }
        eprintln!("wifi: forgot {name:?}");
        let mut inner = self.inner.lock().unwrap();
        inner.view.saved.retain(|s| s != name);
        if inner.view.connected.as_deref() == Some(name) {
            inner.view.connected = None;
        }
        Ok(())
    }
}

/// Writes a file only root can read, replacing it in one step
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    std::fs::create_dir_all(dir)?;
    // iwd ignores files without a network extension, so it won't read this half-written
    let tmp = dir.join(".box-agent-new");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(contents)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GetManagedObjects as busctl prints it: one device in station mode,
    /// connected to "Home", and two networks in range
    fn managed_objects() -> Value {
        serde_json::json!({"type": "a{oa{sa{sv}}}", "data": [{
            "/net/connman/iwd": {"net.connman.iwd.AgentManager": {}},
            "/net/connman/iwd/0": {"net.connman.iwd.Adapter": {"Powered": {"type": "b", "data": true}}},
            "/net/connman/iwd/0/4": {
                "net.connman.iwd.Device": {
                    "Name": {"type": "s", "data": "wlan0"},
                    "Address": {"type": "s", "data": "b8:27:eb:12:ab:cd"},
                    "Powered": {"type": "b", "data": true},
                    "Adapter": {"type": "o", "data": "/net/connman/iwd/0"},
                    "Mode": {"type": "s", "data": "station"}
                },
                "net.connman.iwd.Station": {
                    "State": {"type": "s", "data": "connected"},
                    "ConnectedNetwork": {"type": "o", "data": "/net/connman/iwd/0/4/486f6d65_psk"},
                    "Scanning": {"type": "b", "data": false}
                }
            },
            "/net/connman/iwd/0/4/486f6d65_psk": {"net.connman.iwd.Network": {
                "Name": {"type": "s", "data": "Home"},
                "Connected": {"type": "b", "data": true},
                "Device": {"type": "o", "data": "/net/connman/iwd/0/4"},
                "Type": {"type": "s", "data": "psk"},
                "KnownNetwork": {"type": "o", "data": "/net/connman/iwd/486f6d65_psk"}
            }},
            "/net/connman/iwd/0/4/436166c3a9_open": {"net.connman.iwd.Network": {
                "Name": {"type": "s", "data": "Café"},
                "Connected": {"type": "b", "data": false},
                "Device": {"type": "o", "data": "/net/connman/iwd/0/4"},
                "Type": {"type": "s", "data": "open"}
            }},
            "/net/connman/iwd/486f6d65_psk": {"net.connman.iwd.KnownNetwork": {
                "Name": {"type": "s", "data": "Home"},
                "Type": {"type": "s", "data": "psk"}
            }},
            "/net/connman/iwd/426f782d53657475702d31323334_psk": {"net.connman.iwd.KnownNetwork": {
                "Name": {"type": "s", "data": "Box-Setup-1234"},
                "Type": {"type": "s", "data": "psk"}
            }}
        }]})
    }

    #[test]
    fn reads_iwd_objects() {
        let o = parse_objects(&managed_objects());
        let dev = o.device("wlan0").unwrap();
        assert_eq!(
            (dev.path.as_str(), dev.mode.as_str(), dev.powered),
            ("/net/connman/iwd/0/4", "station", true)
        );
        let station = dev.station.as_ref().unwrap();
        assert_eq!(station.state, "connected");
        assert_eq!(o.network(station.connected_network.as_deref().unwrap()).unwrap().name, "Home");
        assert!(dev.ap.is_none());
        assert_eq!(o.find_network(&dev.path, "Café").unwrap().kind, "open");
        assert!(o.find_network(&dev.path, "Home").unwrap().known);
        // Setup hotspots (another box's, or the test's) don't count as saved
        assert_eq!(o.saved(), vec!["Home".to_owned()]);
        assert!(
            parse_objects(&serde_json::json!({"type": "a{oa{sa{sv}}}", "data": [{}]})).devices.is_empty()
        );
    }

    #[test]
    fn lists_networks_strongest_first() {
        let o = parse_objects(&managed_objects());
        let reply = serde_json::json!({"type": "a(on)", "data": [[
            ["/net/connman/iwd/0/4/436166c3a9_open", -7200],
            ["/net/connman/iwd/0/4/486f6d65_psk", -4500],
            ["/net/connman/iwd/0/4/gone_psk", -3000]
        ]]});
        let list = parse_station_networks(&reply, &o, Some("/net/connman/iwd/0/4/486f6d65_psk"));
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0],
            Visible { name: "Home".into(), kind: "psk".into(), signal: -45, saved: true, connected: true }
        );
        assert_eq!((list[1].name.as_str(), list[1].signal, list[1].connected), ("Café", -72, false));
    }

    #[test]
    fn lists_networks_seen_by_the_hotspot() {
        let entry = |name: &str, signal: i64, kind: &str| {
            serde_json::json!({"Name": {"type": "s", "data": name},
                "SignalStrength": {"type": "n", "data": signal}, "Type": {"type": "s", "data": kind}})
        };
        let reply = serde_json::json!({"type": "aa{sv}", "data": [[
            entry("Home", -6000, "psk"), entry("Home", -5000, "psk"), entry("Box-Setup-9999", -2000, "psk"),
            entry("Office", -8000, "8021x")
        ]]});
        let list = parse_ap_networks(&reply, &["Home".to_owned()]);
        assert_eq!(
            list.iter().map(|n| (n.name.as_str(), n.signal)).collect::<Vec<_>>(),
            [("Home", -50), ("Office", -80)]
        );
        assert!(list[0].saved && !list[1].saved);
    }

    #[test]
    fn names_files_like_iwd() {
        let dir = Path::new("/data/iwd");
        assert_eq!(network_file(dir, "My Home-5G_x", "psk"), dir.join("My Home-5G_x.psk"));
        assert_eq!(network_file(dir, "Café", "psk"), dir.join("=436166c3a9.psk"));
        assert_eq!(network_file(dir, "a.b", "open"), dir.join("=612e62.open"));
    }

    #[test]
    fn escapes_passphrases_for_iwd() {
        assert_eq!(escape_setting("plain pass"), "plain pass");
        assert_eq!(escape_setting("  lead\\ing "), "\\s\\slead\\\\ing ");
    }

    #[test]
    fn checks_names_and_passwords() {
        assert!(valid_ssid("Home") && valid_ssid(&"x".repeat(32)));
        assert!(!valid_ssid("") && !valid_ssid(&"x".repeat(33)) && !valid_ssid("a\nb"));
        assert!(valid_passphrase("12345678") && valid_passphrase(&"x".repeat(63)));
        assert!(
            !valid_passphrase("1234567")
                && !valid_passphrase(&"x".repeat(64))
                && !valid_passphrase("pässword")
        );
    }

    #[test]
    fn hotspot_named_after_the_address() {
        assert_eq!(hotspot_ssid("b8:27:eb:12:ab:cd"), "Box-Setup-ABCD");
        assert_eq!(hotspot_ssid(""), "Box-Setup-");
    }

    #[test]
    fn writes_network_files_privately() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = network_file(dir.path(), "Home", "psk");
        write_private(&path, b"[Security]\nPassphrase=x\n").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temporary file left behind");
    }

    #[tokio::test]
    async fn rejects_bad_join_requests() {
        let wifi = Wifi::new(Config { busctl: "false".into(), ..Config::device() });
        assert_eq!(wifi.join(String::new(), String::new()), Err(JoinError::BadName));
        assert_eq!(wifi.join("Home".into(), "short".into()), Err(JoinError::BadPassword));
    }

    #[tokio::test]
    async fn no_iwd_means_no_wifi() {
        let wifi = Wifi::new(Config { busctl: "false".into(), ..Config::device() });
        wifi.tick().await;
        let view = wifi.view();
        assert!(!view.available && view.hotspot.is_none());
    }
}
