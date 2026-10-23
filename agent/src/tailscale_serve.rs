//! Publishes the agent's tailnet front over HTTPS with `tailscale serve`.
//!
//! Once Tailscale is connected, `tailscale serve --bg --https=443
//! http://127.0.0.1:8081` makes https://<box>.<tailnet>.ts.net/ reach the
//! tailnet front (box pages and the app), with a real certificate. Tailscale
//! keeps that setting in its state on /data, so this normally runs once per
//! box. It needs HTTPS certificates turned on for the tailnet; until they are,
//! the error is shown on the status page.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Serve {
    pub configured: bool,
    /// Why the last attempt failed
    pub error: Option<String>,
    /// The box's name on the tailnet (box.tailXXXX.ts.net), once connected
    pub dns_name: Option<String>,
}

pub type Shared = Arc<Mutex<Serve>>;

#[derive(Debug, PartialEq)]
enum Config {
    /// `name`:443 proxies to the target
    Ours,
    /// Nothing set up
    Missing,
    /// Set up for something else, e.g. the box's old name after a rename
    Stale,
}

/// What `tailscale serve status --json` says about `name`:443 → `target`
fn serve_config(status_json: &str, name: &str, target: &str) -> Config {
    let status: serde_json::Value = serde_json::from_str(status_json).unwrap_or_default();
    let web = status["Web"].as_object();
    let ours = web
        .and_then(|w| w.get(&format!("{name}:443")))
        .is_some_and(|handlers| handlers.to_string().contains(&format!("\"{target}\"")));
    match (ours, web.is_some_and(|w| !w.is_empty())) {
        (true, _) => Config::Ours,
        (false, true) => Config::Stale,
        (false, false) => Config::Missing,
    }
}

async fn output(args: &[&str]) -> Result<std::process::Output, String> {
    let run = tokio::process::Command::new("tailscale").args(args).kill_on_drop(true).output();
    match tokio::time::timeout(Duration::from_secs(30), run).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("tailscale didn't answer".into()),
    }
}

/// The box's tailnet name from `tailscale status --json`, if it's connected
async fn dns_name() -> Result<String, String> {
    let status = output(&["status", "--json"]).await?;
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap_or_default();
    if status["BackendState"] != "Running" {
        return Err("Tailscale isn't connected yet".into());
    }
    status["Self"]["DNSName"]
        .as_str()
        .map(|n| n.trim_end_matches('.').to_owned())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| "Tailscale hasn't given the box a name yet".into())
}

async fn ensure(name: &str, target: &str) -> Result<(), String> {
    let current = output(&["serve", "status", "--json"]).await?;
    match serve_config(&String::from_utf8_lossy(&current.stdout), name, target) {
        Config::Ours => return Ok(()),
        Config::Stale => {
            output(&["serve", "reset"]).await?;
        }
        Config::Missing => {}
    }
    let out = output(&["serve", "--bg", "--https=443", target]).await?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("tailscale serve failed").trim().to_owned())
    }
}

/// Keeps checking every minute, so it takes effect as soon as Tailscale is
/// connected (and comes back if someone resets it).
pub fn spawn(shared: Shared, tailnet_front: SocketAddr) {
    let target = format!("http://{tailnet_front}");
    tokio::spawn(async move {
        loop {
            let name = dns_name().await;
            let result = match &name {
                Ok(name) => ensure(name, &target).await,
                Err(e) => Err(e.clone()),
            };
            *shared.lock().unwrap() =
                Serve { configured: result.is_ok(), error: result.err(), dns_name: name.ok() };
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_an_existing_config() {
        let json = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"box.tail1.ts.net:443":
            {"Handlers":{"/":{"Proxy":"http://127.0.0.1:8081"}}}}}"#;
        let target = "http://127.0.0.1:8081";
        assert_eq!(serve_config(json, "box.tail1.ts.net", target), Config::Ours);
        assert_eq!(serve_config(json, "box.tail1.ts.net", "http://127.0.0.1:808"), Config::Stale);
        // Renamed: the old name's config has to go
        assert_eq!(serve_config(json, "kitchen.tail1.ts.net", target), Config::Stale);
        assert_eq!(serve_config("{}", "box.tail1.ts.net", target), Config::Missing);
        assert_eq!(serve_config("", "box.tail1.ts.net", target), Config::Missing);
    }
}
