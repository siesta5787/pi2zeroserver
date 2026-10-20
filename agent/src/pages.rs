//! HTML for the system pages. Plain language, works on a phone, no JavaScript.

use std::fmt::Write;

use crate::manifest::{Check, os_newer};
use crate::os_update::Install;
use crate::progress::Progress;
use crate::status::{Status, Tailscale};
use crate::tailscale_login::Login;
use crate::wifi;

/// Everything besides `Status` that the status page shows: update jobs, and
/// whether the app is published over Tailscale
pub struct Updates<'a> {
    pub serve: &'a crate::tailscale_serve::Serve,
    pub install: &'a Install,
    pub tailscale: &'a Progress,
    pub manifest: &'a Check,
    /// False if this image has no manifest address/key
    pub checks_configured: bool,
    pub app: &'a Progress,
    pub app_installed: Option<&'a str>,
    /// The generated first-login password, until dismissed
    pub app_initial_password: Option<&'a str>,
    pub login: &'a Login,
    pub wifi: &'a wifi::View,
}

/// Escapes text for HTML element content and attribute values.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const STYLE: &str = "\
body{font-family:system-ui,sans-serif;max-width:40rem;margin:0 auto;padding:1rem;\
line-height:1.5;color:#1b1b1b;background:#fafafa}\
h1{font-size:1.5rem}section{background:#fff;border:1px solid #ddd;border-radius:.5rem;\
padding:.75rem 1rem;margin:1rem 0}h2{font-size:1.1rem;margin:0 0 .25rem}\
p{margin:.25rem 0}.ok{color:#11632a}.warn{color:#8a4b00}.muted{color:#555;font-size:.9rem}\
a{color:#0b57d0}";

fn page(title: &str, body: &str) -> String {
    page_refreshing(title, body, None)
}

/// A page that reloads itself every `refresh` seconds (progress without JavaScript)
fn page_refreshing(title: &str, body: &str, refresh: Option<(u32, &str)>) -> String {
    let refresh = refresh
        .map(|(secs, url)| format!("<meta http-equiv=\"refresh\" content=\"{secs};url={}\">", esc(url)))
        .unwrap_or_default();
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">{refresh}\
         <title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        esc(title)
    )
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

fn duration(seconds: u64) -> String {
    let (d, h, m) = (seconds / 86400, seconds / 3600 % 24, seconds / 60 % 60);
    match (d, h) {
        (0, 0) => format!("{m} min"),
        (0, _) => format!("{h} h {m} min"),
        _ => format!("{d} days {h} h"),
    }
}

fn update_section(u: &Updates, running: &str) -> String {
    let body = match u.install {
        Install::Running { percent, step } => format!(
            "<p class=\"warn\">Installing an update: {percent}%</p><p class=\"muted\">{}</p>\
             <p>You can keep using the app. The box restarts when you say so.</p>",
            esc(step)
        ),
        Install::Installed => "<p class=\"ok\">An update is installed. Restart to start using it.</p>\
             <form method=\"post\" action=\"/_system/restart\"><button>Restart now</button></form>"
            .to_owned(),
        Install::Failed { .. } | Install::Idle => {
            let mut b = match u.install {
                Install::Failed { error } => format!(
                    "<p class=\"warn\">The last update didn't install. Nothing was changed.</p>\
                     <p class=\"muted\">{}</p>",
                    esc(error)
                ),
                _ => String::new(),
            };
            b.push_str(&availability(u, running));
            b
        }
    };
    let advanced = if matches!(u.install, Install::Running { .. }) {
        ""
    } else {
        "<details><summary class=\"muted\">Install from an address</summary>\
         <form method=\"post\" action=\"/_system/os/install\"><p><input name=\"source\" \
         type=\"text\" inputmode=\"url\" placeholder=\"https://.../box-....raucb\" required \
         style=\"width:100%\"></p><p><button>Install</button></p></form></details>"
    };
    format!("<section><h2>Updates</h2>{body}{advanced}</section>")
}

fn app_section(s: &Status, u: &Updates) -> String {
    let mut b = String::from("<section><h2>App</h2>");
    match u.app_installed {
        Some(v) if s.app_running => {
            let _ = write!(b, "<p class=\"ok\">kid-phone-server {} is running.</p>", esc(v));
        }
        Some(v) => {
            let _ = write!(b, "<p class=\"warn\">kid-phone-server {} is installed but not running.</p>", esc(v));
        }
        None => b.push_str(
            "<p class=\"warn\">Not installed yet. It installs by itself once the box can reach the internet.</p>",
        ),
    }
    if let Some(name) =
        s.tailscale.dns_name.as_deref().filter(|_| u.app_installed.is_some() && u.serve.configured)
    {
        let _ = write!(b, "<p><a href=\"https://{0}/\">Open the app at https://{0}/</a></p>", esc(name));
    }
    if let Some(password) = u.app_initial_password {
        let _ = write!(
            b,
            "<p>First sign-in to the app: username <b>admin</b>, password <b>{}</b>. \
             It asks you to choose your own straight away.</p>\
             <form method=\"post\" action=\"/_system/app/initial-password/forget\">\
             <button>I've signed in, hide this</button></form>",
            esc(password)
        );
    }
    let newer = u
        .manifest
        .latest
        .as_ref()
        .and_then(|m| m.app.as_ref())
        .filter(|app| u.app_installed.is_some_and(|v| crate::app_update::app_newer(&app.version, v)));
    match u.app {
        Progress::Checking => b.push_str("<p class=\"muted\">Getting ready…</p>"),
        Progress::Downloading { version, percent } => {
            let _ = write!(b, "<p class=\"warn\">Downloading version {}: {percent}%</p>", esc(version));
        }
        Progress::Installing { version } => {
            let _ = write!(b, "<p class=\"warn\">Installing version {}…</p>", esc(version));
        }
        Progress::Failed { error } => {
            let _ = write!(b, "<p class=\"warn\">The app wasn't updated: {}</p>", esc(error));
        }
        Progress::Updated { version } => {
            let _ = write!(b, "<p class=\"ok\">Updated to version {}.</p>", esc(version));
        }
        Progress::Idle | Progress::UpToDate { .. } => {}
    }
    if let Some(app) = newer.filter(|_| !u.app.busy()) {
        let _ = write!(
            b,
            "<p class=\"warn\">An app update is available: {}</p>\
             <form method=\"post\" action=\"/_system/app/update\"><button>Update the app</button></form>",
            esc(&app.version)
        );
    }
    b.push_str("</section>");
    b
}

/// "Update available" / "up to date" from the release manifest
fn availability(u: &Updates, running: &str) -> String {
    if !u.checks_configured {
        return "<p class=\"muted\">This build doesn't check for updates.</p>".to_owned();
    }
    let check = u.manifest;
    if check.checking {
        return "<p class=\"muted\">Checking for updates…</p>".to_owned();
    }
    let mut b = match check.latest.as_ref().and_then(|m| m.os.as_ref()) {
        Some(os) if os_newer(&os.version, running) => format!(
            "<p class=\"warn\">A system update is available: {}</p>\
             <form method=\"post\" action=\"/_system/os/update\"><button>Install update</button></form>",
            esc(&os.version)
        ),
        Some(_) => "<p class=\"ok\">The system is up to date.</p>".to_owned(),
        None => String::new(),
    };
    if let Some(error) = &check.error {
        let _ = write!(b, "<p class=\"muted\">Couldn't check for updates: {}</p>", esc(error));
    }
    b.push_str("<form method=\"post\" action=\"/_system/check\"><button>Check now</button></form>");
    b
}

fn tailscale_update_line(update: &Progress) -> String {
    let button = "<form method=\"post\" action=\"/_system/tailscale/update\">\
                  <button>Check for a Tailscale update</button></form>";
    match update {
        Progress::Idle => button.to_owned(),
        Progress::Checking => "<p class=\"muted\">Checking for a newer version…</p>".to_owned(),
        Progress::Downloading { version, percent } => {
            format!("<p class=\"warn\">Downloading Tailscale {}: {percent}%</p>", esc(version))
        }
        Progress::Installing { version } => {
            format!("<p class=\"warn\">Installing Tailscale {}…</p>", esc(version))
        }
        Progress::Updated { version } => {
            format!("<p class=\"ok\">Updated to Tailscale {}.</p>", esc(version))
        }
        Progress::UpToDate { version } => {
            format!("<p class=\"ok\">Tailscale {} is the latest version.</p>{button}", esc(version))
        }
        Progress::Failed { error } => {
            format!("<p class=\"warn\">Tailscale wasn't updated: {}</p>{button}", esc(error))
        }
    }
}

fn signal_words(dbm: i32) -> &'static str {
    match dbm {
        -60.. => "strong signal",
        -72.. => "good signal",
        _ => "weak signal",
    }
}

fn join_line(join: &wifi::Join) -> String {
    match join {
        wifi::Join::Idle => String::new(),
        wifi::Join::Joining { ssid } => format!("<p class=\"warn\">Joining <b>{}</b>…</p>", esc(ssid)),
        wifi::Join::Joined { ssid } => format!("<p class=\"ok\">Joined <b>{}</b>.</p>", esc(ssid)),
        wifi::Join::Failed { error, .. } => format!("<p class=\"warn\" role=\"alert\">{}</p>", esc(error)),
    }
}

/// The status page's Wi-Fi summary; the setup prompt while the hotspot is on
fn wifi_section(w: &wifi::View) -> String {
    let body = match (&w.hotspot, &w.connected) {
        (Some(h), _) => format!(
            "<p class=\"warn\">{}</p><p><a href=\"/_system/wifi\"><b>Choose your Wi-Fi network</b></a></p>",
            if h.setup {
                "The box isn't on your Wi-Fi yet."
            } else {
                "The box couldn't reach your Wi-Fi, so it opened its setup hotspot. It keeps trying your saved networks."
            }
        ),
        _ if !w.available => "<p class=\"muted\">No Wi-Fi found on this box.</p>".to_owned(),
        (None, Some(name)) => format!(
            "<p class=\"ok\">Connected to <b>{}</b></p><p><a href=\"/_system/wifi\">Change Wi-Fi</a></p>",
            esc(name)
        ),
        (None, None) => {
            "<p class=\"warn\">Not connected.</p><p><a href=\"/_system/wifi\">Choose a Wi-Fi network</a></p>"
                .to_owned()
        }
    };
    format!("<section><h2>Wi-Fi</h2>{}{body}</section>", join_line(&w.join))
}

/// The Wi-Fi page: networks nearby, joining, saved networks
pub fn wifi(w: &wifi::View, error: Option<&str>) -> String {
    let mut b = String::from("<h1>Wi-Fi</h1>");
    b.push_str(&error_line(error));
    if let Some(h) = &w.hotspot {
        let _ = write!(
            b,
            "<p>Pick your home Wi-Fi network and enter its password. The box then leaves its \
             setup hotspot (<b>{}</b>) and joins your network.</p>",
            esc(&h.ssid)
        );
    } else if let Some(name) = &w.connected {
        let _ = write!(b, "<p class=\"ok\">The box is on <b>{}</b>.</p>", esc(name));
    }
    b.push_str(&join_line(&w.join));

    b.push_str("<section><h2>Networks nearby</h2>");
    if w.networks.is_empty() {
        b.push_str("<p class=\"muted\">No networks found yet.</p>");
    }
    for n in &w.networks {
        let mut notes = vec![signal_words(n.signal)];
        if n.connected {
            notes.push("connected");
        } else if n.saved {
            notes.push("saved");
        }
        let label = format!("<b>{}</b> <span class=\"muted\">{}</span>", esc(&n.name), notes.join(" · "));
        let password = match n.kind.as_str() {
            "open" => {
                "<p class=\"muted\">No password (anyone nearby can see what's sent on it).</p>".to_owned()
            }
            "psk" => format!(
                "<p><label>Password{}<br><input type=\"password\" name=\"password\" minlength=\"8\" \
                 maxlength=\"63\" autocomplete=\"off\"{}></label></p>",
                if n.saved { " (leave empty to use the saved one)" } else { "" },
                if n.saved { "" } else { " required" }
            ),
            _ => {
                let _ = write!(
                    b,
                    "<p>{label}<br><span class=\"muted\">Uses enterprise sign-in, which the box doesn't support.</span></p>"
                );
                continue;
            }
        };
        let _ = write!(
            b,
            "<details><summary>{label}</summary><form method=\"post\" action=\"/_system/wifi/join\">\
             <input type=\"hidden\" name=\"ssid\" value=\"{}\">{password}<p><button>Join</button></p></form></details>",
            esc(&n.name)
        );
    }
    b.push_str(
        "<form method=\"post\" action=\"/_system/wifi/scan\"><p><button>Look again</button></p></form>\
         <details><summary>Network not listed?</summary><form method=\"post\" action=\"/_system/wifi/join\">\
         <p><label>Network name<br><input name=\"ssid\" maxlength=\"32\" required autocapitalize=\"off\"></label></p>\
         <p><label>Password (empty if it has none)<br><input type=\"password\" name=\"password\" \
         maxlength=\"63\" autocomplete=\"off\"></label></p><p><button>Join</button></p></form></details></section>",
    );

    if !w.saved.is_empty() {
        b.push_str("<section><h2>Saved networks</h2><p class=\"muted\">The box joins these by itself when they're in range.</p>");
        for name in &w.saved {
            let current = w.connected.as_deref() == Some(name.as_str());
            let _ = write!(
                b,
                "<form method=\"post\" action=\"/_system/wifi/forget\"><p><b>{0}</b> \
                 <input type=\"hidden\" name=\"name\" value=\"{0}\"><button>Forget</button>{1}</p></form>",
                esc(name),
                if current {
                    " <span class=\"muted\">(the box is on it now and will disconnect)</span>"
                } else {
                    ""
                }
            );
        }
        b.push_str("</section>");
    }
    b.push_str("<p><a href=\"/_system/\">Back</a></p>");
    let busy = matches!(w.join, wifi::Join::Joining { .. }) || w.scanning;
    page_refreshing("Wi-Fi", &b, busy.then_some((3, "/_system/wifi")))
}

/// After choosing a network from the setup hotspot, which is about to go away
pub fn wifi_leaving(ssid: &str) -> String {
    let ssid = esc(ssid);
    page(
        "Joining your Wi-Fi",
        &format!(
            "<h1>Joining {ssid}…</h1><p>The box is leaving its setup hotspot to join <b>{ssid}</b>, \
             so this page will lose its connection. Next:</p>\
             <ol><li>Connect this phone back to <b>{ssid}</b>.</li>\
             <li>Open <a href=\"http://box.local/_system/\">http://box.local/_system/</a> \
             and bookmark it.</li></ol>\
             <p class=\"muted\">If the box can't join (say, the password was wrong), its setup hotspot \
             comes back within a couple of minutes: connect to it again to see what went wrong.</p>\
             <p class=\"muted\">If box.local doesn't open (some Android phones can't use .local names), \
             look for \"box\" in your router's list of devices and open its address instead.</p>"
        ),
    )
}

/// The Tailscale section's sign-in part, while Tailscale isn't connected
fn tailscale_connect(ts: &Tailscale, u: &Updates) -> String {
    let signed_out = matches!(ts.backend_state.as_deref(), Some("NeedsLogin" | "NoState" | "Stopped"));
    let button = "<p>Tailscale lets your own devices reach the app securely from anywhere. \
                  You'll need a (free) Tailscale account.</p>\
                  <form method=\"post\" action=\"/_system/tailscale/login\"><button>Connect Tailscale</button></form>";
    match u.login {
        Login::Starting => "<p class=\"muted\">Getting a sign-in link from Tailscale…</p>".to_owned(),
        Login::Waiting { url } => format!(
            "<p>Sign in to Tailscale to add the box to your devices:</p>\
             <p><a href=\"{0}\" target=\"_blank\" rel=\"noopener\"><b>Sign in to Tailscale</b></a></p>\
             <p class=\"muted\">Or scan this with another phone or tablet:</p>{1}\
             <p class=\"muted\">This page updates by itself once you've signed in.</p>",
            esc(url),
            crate::tailscale_login::qr_svg(url)
                .map(|svg| format!("<div style=\"max-width:240px\">{svg}</div>"))
                .unwrap_or_default()
        ),
        _ if ts.running && !signed_out => String::new(),
        _ if u.wifi.hotspot.is_some() => {
            "<p class=\"muted\">Connect the box to your Wi-Fi first.</p>".to_owned()
        }
        _ if !ts.running => String::new(),
        Login::Failed { error } => format!("<p class=\"warn\">Couldn't connect: {}</p>{button}", esc(error)),
        Login::Idle => button.to_owned(),
    }
}

pub fn status(s: &Status, updates: &Updates) -> String {
    let mut b = String::from("<h1>Box</h1>");
    b.push_str(&wifi_section(updates.wifi));

    // System
    let backup = s.os.slots.iter().find(|slot| Some(&slot.name) != s.os.booted_slot.as_ref());
    let backup_line = match backup {
        Some(slot) if slot.boot_status == "good" => {
            "A backup copy of the system is ready if an update ever fails."
        }
        _ => "No backup copy yet; one is made with the first update.",
    };
    let _ = write!(
        b,
        "<section><h2>System</h2><p class=\"ok\">Running version {}</p><p>{}</p>\
         <p class=\"muted\">Copy {} in use · up {}</p></section>",
        esc(&s.os.version),
        backup_line,
        esc(s.os.booted_slot.as_deref().unwrap_or("?")),
        duration(s.uptime_seconds),
    );

    // Tailscale
    let ts = &s.tailscale;
    let (class, line) = match (ts.running, ts.backend_state.as_deref(), ts.dns_name.as_deref()) {
        (false, _, _) => ("warn", "Remote access isn't running.".to_owned()),
        (true, Some("Running"), Some(name)) => ("ok", format!("Connected as <b>{}</b>", esc(name))),
        (true, Some("Running"), None) => ("ok", "Connected.".to_owned()),
        (true, Some("NeedsLogin"), _) => ("warn", "Not set up yet.".to_owned()),
        (true, Some("Stopped"), _) => ("warn", "Turned off.".to_owned()),
        (true, other, _) => ("warn", format!("Starting ({})", esc(other.unwrap_or("unknown")))),
    };
    let _ = write!(
        b,
        "<section><h2>Remote access (Tailscale)</h2><p class=\"{class}\">{line}</p>\
         {}<p class=\"muted\">Version {}</p>{}</section>",
        tailscale_connect(ts, updates),
        esc(ts.version.as_deref().unwrap_or("unknown")),
        tailscale_update_line(updates.tailscale),
    );
    if ts.backend_state.as_deref() == Some("Running")
        && let Some(error) = updates.serve.error.as_deref()
    {
        let _ = write!(
            b,
            "<section><h2>Opening the app over Tailscale</h2><p class=\"warn\">Not working yet. \
             In the Tailscale admin console, open the DNS page and turn on MagicDNS and HTTPS \
             certificates. The box retries every minute.</p><p class=\"muted\">{}</p></section>",
            esc(error)
        );
    }

    // Storage
    let storage = match &s.data {
        Some(d) => format!("<p>{} free of {}</p>", gib(d.free_bytes), gib(d.total_bytes)),
        None => "<p class=\"warn\">The data area isn't available.</p>".to_owned(),
    };
    b.push_str(&app_section(s, updates));
    let _ = write!(b, "<section><h2>Storage</h2>{storage}</section>");
    b.push_str(&update_section(updates, &s.os.version));

    b.push_str(
        "<p class=\"muted\"><a href=\"/_system/password\">Change password</a></p>\
         <form method=\"post\" action=\"/_system/logout\"><button>Sign out</button></form>",
    );
    let busy = matches!(updates.install, Install::Running { .. })
        || updates.manifest.checking
        || updates.app.busy()
        || matches!(updates.wifi.join, wifi::Join::Joining { .. })
        || matches!(
            updates.tailscale,
            Progress::Checking | Progress::Downloading { .. } | Progress::Installing { .. }
        );
    // Waiting for a Tailscale sign-in: refresh gently, the user is busy on another page
    let refresh =
        if busy { Some((3, "/_system/")) } else { updates.login.busy().then_some((5, "/_system/")) };
    page_refreshing("Box", &b, refresh)
}

pub fn message(title: &str, text: &str) -> String {
    page(title, &format!("<h1>{}</h1><p>{}</p><p><a href=\"/_system/\">Back</a></p>", esc(title), esc(text)))
}

pub fn restarting() -> String {
    page_refreshing(
        "Restarting",
        "<h1>Restarting…</h1><p>The box is restarting. This page will reload by itself \
         in about a minute.</p>",
        Some((60, "/_system/")),
    )
}

fn error_line(error: Option<&str>) -> String {
    error.map(|e| format!("<p class=\"warn\" role=\"alert\">{}</p>", esc(e))).unwrap_or_default()
}

pub fn login(error: Option<&str>) -> String {
    page(
        "Sign in",
        &format!(
            "<h1>Sign in to Box</h1>{}\
             <form method=\"post\" action=\"/_system/login\">\
             <p><label>Password<br><input type=\"password\" name=\"password\" \
             autocomplete=\"current-password\" required autofocus></label></p>\
             <p><button>Sign in</button></p></form>",
            error_line(error)
        ),
    )
}

/// `first_run`: no password yet, so no "current password" field
pub fn set_password(first_run: bool, error: Option<&str>) -> String {
    let (title, intro, current) = if first_run {
        (
            "Choose a password",
            "<p>Pick a password for this box. You'll need it to change settings and install updates.</p>",
            "",
        )
    } else {
        (
            "Change password",
            "",
            "<p><label>Current password<br><input type=\"password\" name=\"current\" \
             autocomplete=\"current-password\" required></label></p>",
        )
    };
    page(
        title,
        &format!(
            "<h1>{title}</h1>{intro}{}<form method=\"post\" action=\"/_system/password\">{current}\
             <p><label>New password (at least {min} characters)<br>\
             <input type=\"password\" name=\"password\" minlength=\"{min}\" \
             autocomplete=\"new-password\" required></label></p>\
             <p><label>New password again<br><input type=\"password\" name=\"confirm\" \
             minlength=\"{min}\" autocomplete=\"new-password\" required></label></p>\
             <p><button>Save password</button></p></form>",
            error_line(error),
            min = crate::auth::MIN_PASSWORD_LEN,
        ),
    )
}

/// What the home network sees outside /_system/: the app is only on the tailnet
pub fn app_on_tailnet(dns_name: Option<&str>) -> String {
    let link = match dns_name {
        Some(name) => format!("<p><a href=\"https://{0}/\">Open https://{0}/</a></p>", esc(name)),
        None => "<p>Connect Tailscale on the <a href=\"/_system/\">box's page</a> first.</p>".to_owned(),
    };
    page(
        "Open the app over Tailscale",
        &format!(
            "<h1>Open the app over Tailscale</h1>\
             <p>For privacy, the app is only reachable through Tailscale (with HTTPS), \
             from devices signed in to your tailnet.</p>{link}\
             <p class=\"muted\">The box's own settings are at <a href=\"/_system/\">/_system/</a>.</p>"
        ),
    )
}

pub fn app_down() -> String {
    page(
        "App not running",
        "<h1>The app isn't running right now</h1>\
         <p>It may be starting up or updating. Try again in a minute.</p>\
         <p>If it stays like this, check the <a href=\"/_system/\">system page</a>.</p>",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{Disk, Os, Slot, Tailscale};

    #[test]
    fn escapes_markup() {
        assert_eq!(esc(r#"<a href="x">&'"#), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }

    #[test]
    fn status_page_escapes_reported_values() {
        let s = Status {
            os: Os {
                version: "<script>".into(),
                booted_slot: Some("A".into()),
                slots: vec![Slot { name: "B".into(), state: "inactive".into(), boot_status: "good".into() }],
            },
            tailscale: Tailscale {
                running: true,
                backend_state: Some("Running".into()),
                dns_name: Some("evil<b>".into()),
                ..Default::default()
            },
            data: Some(Disk { total_bytes: 30_000_000_000, free_bytes: 29_500_000_000 }),
            uptime_seconds: 3 * 3600 + 5 * 60,
            app_running: true,
        };
        let html = status(
            &s,
            &Updates {
                install: &Install::Idle,
                tailscale: &Progress::Idle,
                manifest: &Check::default(),
                checks_configured: false,
                app: &Progress::Idle,
                app_installed: None,
                app_initial_password: None,
                serve: &Default::default(),
                login: &Login::Idle,
                wifi: &Default::default(),
            },
        );
        assert!(!html.contains("<script>") && html.contains("&lt;script&gt;"));
        assert!(html.contains("evil&lt;b&gt;"));
        assert!(html.contains("A backup copy of the system is ready"));
        assert!(html.contains("29.5 GB free of 30.0 GB"));
        assert!(html.contains("up 3 h 5 min"));
    }

    fn section(install: &Install, manifest: &Check) -> String {
        let u = Updates {
            install,
            tailscale: &Progress::Idle,
            manifest,
            checks_configured: true,
            app: &Progress::Idle,
            app_installed: None,
            app_initial_password: None,
            serve: &Default::default(),
            login: &Login::Idle,
            wifi: &Default::default(),
        };
        update_section(&u, "2026.10.19.2244-g77ccad4")
    }

    #[test]
    fn update_progress_refreshes_and_escapes() {
        let none = Check::default();
        let running = section(&Install::Running { percent: 40, step: "<Copying>".into() }, &none);
        assert!(
            running.contains("40%") && running.contains("&lt;Copying&gt;") && !running.contains("<details>")
        );
        let failed = section(&Install::Failed { error: "bad <sig>".into() }, &none);
        assert!(failed.contains("bad &lt;sig&gt;") && failed.contains("<details>"));
        assert!(section(&Install::Installed, &none).contains("/_system/restart"));
    }

    #[test]
    fn offers_a_newer_system_from_the_manifest() {
        use crate::manifest::{Manifest, OsRelease};
        let check = |version: &str| Check {
            latest: Some(Manifest {
                schema: 1,
                os: Some(OsRelease { version: version.into(), bundle_url: "https://x/b.raucb".into() }),
                app: None,
            }),
            ..Default::default()
        };
        let newer = section(&Install::Idle, &check("2026.10.20.0100-gabc"));
        assert!(newer.contains("A system update is available: 2026.10.20.0100-gabc"));
        assert!(newer.contains("/_system/os/update"));
        let same = section(&Install::Idle, &check("2026.10.19.2244-g77ccad4"));
        assert!(same.contains("up to date") && !same.contains("/_system/os/update"));
        let failed =
            section(&Install::Idle, &Check { error: Some("offline <x>".into()), ..Default::default() });
        assert!(failed.contains("offline &lt;x&gt;") && failed.contains("Check now"));
    }

    #[test]
    fn app_section_offers_updates_and_first_login() {
        use crate::manifest::{AppRelease, Manifest};
        let status = |dns: Option<&str>| Status {
            os: Default::default(),
            tailscale: Tailscale { dns_name: dns.map(Into::into), ..Default::default() },
            data: None,
            uptime_seconds: 0,
            app_running: true,
        };
        let manifest = Check {
            latest: Some(Manifest {
                schema: 1,
                os: None,
                app: Some(AppRelease {
                    name: "kid-phone-server".into(),
                    version: "0.19.0".into(),
                    url: "https://x/a.tgz".into(),
                    sha256: "00".into(),
                    command: None,
                }),
            }),
            ..Default::default()
        };
        let updates = |installed: Option<&'static str>, password: Option<&'static str>| Updates {
            install: &Install::Idle,
            tailscale: &Progress::Idle,
            manifest: Box::leak(Box::new(manifest.clone())),
            checks_configured: true,
            app: &Progress::Idle,
            app_installed: installed,
            app_initial_password: password,
            serve: Box::leak(Box::new(crate::tailscale_serve::Serve { configured: true, error: None })),
            login: &Login::Idle,
            wifi: Box::leak(Box::new(wifi::View::default())),
        };
        let html = app_section(&status(Some("box.tail1.ts.net")), &updates(Some("0.18.6"), Some("Ab<c")));
        assert!(html.contains("0.18.6 is running") && html.contains("An app update is available: 0.19.0"));
        assert!(html.contains("password <b>Ab&lt;c</b>") && html.contains("https://box.tail1.ts.net/"));
        let current = app_section(&status(None), &updates(Some("0.19.0"), None));
        assert!(!current.contains("update is available") && !current.contains("First sign-in"));
        let fresh = app_section(&status(None), &updates(None, None));
        assert!(fresh.contains("Not installed yet") && !fresh.contains("update is available"));
    }

    #[test]
    fn durations() {
        assert_eq!(duration(59), "0 min");
        assert_eq!(duration(3600 * 26 + 60), "1 days 2 h");
    }
}
