//! HTML for the system pages. Plain language, works on a phone, no JavaScript.

use std::fmt::Write;

use crate::status::Status;

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
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
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

pub fn status(s: &Status) -> String {
    let mut b = String::from("<h1>Box</h1>");

    // System
    let backup = s.os.slots.iter().find(|slot| Some(&slot.name) != s.os.booted_slot.as_ref());
    let backup_line = match backup {
        Some(slot) if slot.boot_status == "good" => "A backup copy of the system is ready if an update ever fails.",
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
         <p class=\"muted\">Version {}</p></section>",
        esc(ts.version.as_deref().unwrap_or("unknown")),
    );

    // Storage
    let storage = match &s.data {
        Some(d) => format!("<p>{} free of {}</p>", gib(d.free_bytes), gib(d.total_bytes)),
        None => "<p class=\"warn\">The data area isn't available.</p>".to_owned(),
    };
    let _ = write!(b, "<section><h2>Storage</h2>{storage}</section>");

    b.push_str(
        "<p class=\"muted\"><a href=\"/\">Open the app</a> · \
         <a href=\"/_system/password\">Change password</a></p>\
         <form method=\"post\" action=\"/_system/logout\"><button>Sign out</button></form>",
    );
    page("Box", &b)
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
        };
        let html = status(&s);
        assert!(!html.contains("<script>") && html.contains("&lt;script&gt;"));
        assert!(html.contains("evil&lt;b&gt;"));
        assert!(html.contains("A backup copy of the system is ready"));
        assert!(html.contains("29.5 GB free of 30.0 GB"));
        assert!(html.contains("up 3 h 5 min"));
    }

    #[test]
    fn durations() {
        assert_eq!(duration(59), "0 min");
        assert_eq!(duration(3600 * 26 + 60), "1 days 2 h");
    }
}
