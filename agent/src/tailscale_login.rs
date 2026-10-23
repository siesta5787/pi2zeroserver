//! "Connect Tailscale": runs `tailscale up` and shows its sign-in link, as a
//! link (on the phone you're using) and a QR code (to scan with another).
//!
//! `tailscale up` waits until someone signs in through the link, up to
//! [`LOGIN_TIMEOUT`]. The link comes from its output, or else from
//! `tailscale status --json` (`AuthURL`), whichever shows up first.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::io::AsyncBufReadExt;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Login {
    #[default]
    Idle,
    /// Waiting for Tailscale to come up with a sign-in link
    Starting,
    /// Someone needs to open `url` and sign in
    Waiting {
        url: String,
    },
    Failed {
        error: String,
    },
}

impl Login {
    pub fn busy(&self) -> bool {
        matches!(self, Login::Starting | Login::Waiting { .. })
    }
}

#[derive(Clone)]
pub struct TailscaleLogin {
    program: String,
    state: Arc<Mutex<Login>>,
}

/// The first sign-in link in a line of `tailscale up` output
fn find_url(line: &str) -> Option<&str> {
    line.split_whitespace().find(|w| w.starts_with("https://"))
}

impl TailscaleLogin {
    pub fn new(program: &str) -> Self {
        Self { program: program.into(), state: Default::default() }
    }

    pub fn state(&self) -> Login {
        self.state.lock().unwrap().clone()
    }

    /// Signs the box out of the tailnet (before a factory reset); best effort
    pub async fn logout(&self) {
        let run = tokio::process::Command::new(&self.program).arg("logout").kill_on_drop(true).status();
        if let Ok(Err(e)) = tokio::time::timeout(Duration::from_secs(20), run).await {
            eprintln!("tailscale logout: {e}");
        }
    }

    fn found(&self, url: &str) {
        let mut state = self.state.lock().unwrap();
        if *state == Login::Starting {
            *state = Login::Waiting { url: url.to_owned() };
        }
    }

    /// Starts signing in, unless that's already going on; false if it was
    pub fn start(&self) -> bool {
        {
            let mut state = self.state.lock().unwrap();
            if state.busy() {
                return false;
            }
            *state = Login::Starting;
        }
        let login = self.clone();
        tokio::spawn(async move {
            let result = login.run().await;
            *login.state.lock().unwrap() = match result {
                Ok(()) => Login::Idle,
                Err(error) => Login::Failed { error },
            };
        });
        true
    }

    async fn run(&self) -> Result<(), String> {
        let timeout = format!("--timeout={}s", LOGIN_TIMEOUT.as_secs());
        // Join the tailnet under the box's own name (box.local → box.<tailnet>.ts.net)
        let hostname = format!("--hostname={}", crate::hostname::current());
        let mut child = tokio::process::Command::new(&self.program)
            .args(["up", &timeout, &hostname])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("couldn't start tailscale: {e}"))?;
        let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let mut stderr = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
        let mut last_error = String::new();
        let mut poll = tokio::time::interval(Duration::from_secs(2));
        let (mut out_open, mut err_open) = (true, true);
        let status = loop {
            tokio::select! {
                line = stdout.next_line(), if out_open => match line {
                    Ok(Some(line)) => if let Some(url) = find_url(&line) { self.found(url) },
                    _ => out_open = false,
                },
                line = stderr.next_line(), if err_open => match line {
                    Ok(Some(line)) => match find_url(&line) {
                        Some(url) => self.found(url),
                        None if !line.trim().is_empty() => last_error = line.trim().to_owned(),
                        None => {}
                    },
                    _ => err_open = false,
                },
                _ = poll.tick(), if *self.state.lock().unwrap() == Login::Starting => {
                    if let Some(url) = self.auth_url().await {
                        self.found(&url);
                    }
                }
                status = child.wait(), if !out_open && !err_open => break status,
            }
        };
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(_) if !last_error.is_empty() => Err(last_error),
            Ok(s) => Err(format!("tailscale up failed ({s})")),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn auth_url(&self) -> Option<String> {
        let run = tokio::process::Command::new(&self.program)
            .args(["status", "--json"])
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(Duration::from_secs(10), run).await.ok()?.ok()?;
        let status: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        status["AuthURL"].as_str().filter(|u| u.starts_with("https://")).map(str::to_owned)
    }
}

/// The sign-in link as an inline SVG QR code
pub fn qr_svg(url: &str) -> Option<String> {
    let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
    let svg = code.render::<qrcode::render::svg::Color>().min_dimensions(220, 220).quiet_zone(true).build();
    // Inline in HTML: drop the XML declaration
    Some(svg[svg.find("<svg")?..].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_sign_in_link() {
        assert_eq!(
            find_url("\thttps://login.tailscale.com/a/1b2c3d4e5f"),
            Some("https://login.tailscale.com/a/1b2c3d4e5f")
        );
        assert_eq!(find_url("To authenticate, visit:"), None);
    }

    #[test]
    fn makes_a_qr_code() {
        let svg = qr_svg("https://login.tailscale.com/a/1b2c3d4e5f").unwrap();
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
    }

    /// A stand-in for the tailscale CLI: prints a link like `tailscale up`, then exits with `code`
    fn fake_tailscale(dir: &std::path::Path, code: i32) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("tailscale");
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = up ] || exit 1\necho 'To authenticate, visit:' >&2\n\
             echo '\thttps://login.tailscale.com/a/abc' >&2\nsleep 1\n\
             [ {code} = 0 ] || echo 'backend error: boom' >&2\nexit {code}\n"
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_owned()
    }

    async fn wait_until_done(login: &TailscaleLogin) -> Login {
        for _ in 0..100 {
            if !login.state().busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        login.state()
    }

    #[tokio::test]
    async fn shows_the_link_until_signed_in() {
        let dir = tempfile::tempdir().unwrap();
        let login = TailscaleLogin::new(&fake_tailscale(dir.path(), 0));
        assert!(login.start());
        assert!(!login.start(), "one sign-in at a time");
        for _ in 0..50 {
            if matches!(login.state(), Login::Waiting { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(login.state(), Login::Waiting { url: "https://login.tailscale.com/a/abc".into() });
        assert_eq!(wait_until_done(&login).await, Login::Idle);
    }

    #[tokio::test]
    async fn reports_why_it_failed() {
        let dir = tempfile::tempdir().unwrap();
        let login = TailscaleLogin::new(&fake_tailscale(dir.path(), 1));
        login.start();
        assert_eq!(wait_until_done(&login).await, Login::Failed { error: "backend error: boom".into() });
    }
}
