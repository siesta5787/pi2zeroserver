//! Installing an OS update: runs `rauc install` and keeps its progress.
//!
//! RAUC does the real work in its own service (streaming the signed bundle
//! into the other slot); this just starts it, follows its progress output and
//! remembers the outcome for the status page. Only one install runs at a time.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Install {
    Idle,
    Running {
        percent: u8,
        step: String,
    },
    /// Installed into the other slot; takes effect after a restart
    Installed,
    Failed {
        error: String,
    },
}

#[derive(Clone)]
pub struct OsUpdate {
    rauc: PathBuf,
    state: Arc<Mutex<Install>>,
}

#[derive(Debug, PartialEq)]
pub enum StartError {
    AlreadyRunning,
    BadSource,
}

/// Where a bundle may come from: the network, or a file on /data (for testing
/// and offline installs). Never arbitrary local paths.
pub fn valid_source(source: &str) -> bool {
    let source = source.trim();
    let network = (source.starts_with("https://") || source.starts_with("http://"))
        && !source.contains(char::is_whitespace);
    let data_file = source.starts_with("/data/") && !source.contains("/../") && source.ends_with(".raucb");
    network || data_file
}

/// One line of `rauc install` output, e.g. "  20% Checking bundle done."
fn parse_progress(line: &str) -> Option<(u8, String)> {
    let line = line.trim();
    let (percent, step) = line.split_once("% ")?;
    Some((percent.trim().parse().ok()?, step.trim().to_owned()))
}

impl OsUpdate {
    pub fn new(rauc: PathBuf) -> Self {
        Self { rauc, state: Arc::new(Mutex::new(Install::Idle)) }
    }

    pub fn state(&self) -> Install {
        self.state.lock().unwrap().clone()
    }

    pub fn start(&self, source: &str) -> Result<(), StartError> {
        if !valid_source(source) {
            return Err(StartError::BadSource);
        }
        {
            let mut state = self.state.lock().unwrap();
            if matches!(*state, Install::Running { .. }) {
                return Err(StartError::AlreadyRunning);
            }
            *state = Install::Running { percent: 0, step: "Starting".into() };
        }
        let (rauc, state, source) = (self.rauc.clone(), self.state.clone(), source.trim().to_owned());
        tokio::spawn(async move {
            let outcome = run(&rauc, &source, &state).await;
            *state.lock().unwrap() = outcome;
        });
        Ok(())
    }
}

async fn run(rauc: &PathBuf, source: &str, state: &Mutex<Install>) -> Install {
    let mut child = match tokio::process::Command::new(rauc)
        .args(["install", source])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Install::Failed { error: format!("couldn't start rauc: {e}") },
    };
    let mut last_error = String::new();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    while let Ok(Some(line)) = stdout.next_line().await {
        if let Some((percent, step)) = parse_progress(&line) {
            *state.lock().unwrap() = Install::Running { percent, step };
        } else if let Some(error) = line.trim().strip_prefix("LastError:") {
            last_error = error.trim().to_owned();
        }
    }
    let mut stderr_text = String::new();
    if let Some(stderr) = child.stderr.take() {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            stderr_text = line;
        }
    }
    match child.wait().await {
        Ok(status) if status.success() => Install::Installed,
        _ => Install::Failed {
            error: [last_error, stderr_text]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or_else(|| "install failed".into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[test]
    fn progress_lines() {
        assert_eq!(parse_progress("  20% Checking bundle done."), Some((20, "Checking bundle done.".into())));
        assert_eq!(parse_progress("100% Installing done."), Some((100, "Installing done.".into())));
        assert_eq!(parse_progress("installing"), None);
        assert_eq!(parse_progress("idle"), None);
    }

    #[test]
    fn sources() {
        assert!(valid_source("https://example.com/box-1.raucb"));
        assert!(valid_source("/data/update.raucb"));
        assert!(!valid_source("/etc/shadow"));
        assert!(!valid_source("/data/../etc/x.raucb"));
        assert!(!valid_source("file:///data/update.raucb"));
        assert!(!valid_source("https://example.com/a b"));
    }

    /// A stand-in for `rauc` that prints `output` and exits with `code`.
    fn fake_rauc(dir: &std::path::Path, output: &str, code: i32) -> PathBuf {
        let path = dir.join("rauc");
        std::fs::write(&path, format!("#!/bin/sh\nsleep 0.2\ncat <<'EOF'\n{output}\nEOF\nexit {code}\n"))
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn wait_until_done(u: &OsUpdate) -> Install {
        for _ in 0..100 {
            let s = u.state();
            if !matches!(s, Install::Running { .. }) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("install didn't finish")
    }

    #[tokio::test]
    async fn successful_install() {
        let dir = tempfile::tempdir().unwrap();
        let rauc = fake_rauc(
            dir.path(),
            "installing\n  0% Installing\n 50% Copying image\n100% Installing done.\nidle",
            0,
        );
        let u = OsUpdate::new(rauc);
        u.start("/data/update.raucb").unwrap();
        assert!(matches!(u.state(), Install::Running { .. }));
        assert_eq!(u.start("/data/update.raucb"), Err(StartError::AlreadyRunning));
        assert_eq!(wait_until_done(&u).await, Install::Installed);
    }

    #[tokio::test]
    async fn failed_install_keeps_rauc_error() {
        let dir = tempfile::tempdir().unwrap();
        let out = " 20% Checking bundle failed.\nLastError: signature verification failed\nidle";
        let u = OsUpdate::new(fake_rauc(dir.path(), out, 1));
        u.start("https://example.com/box.raucb").unwrap();
        assert_eq!(
            wait_until_done(&u).await,
            Install::Failed { error: "signature verification failed".into() }
        );
        // A new attempt is allowed after a failure
        assert!(u.start("https://example.com/box.raucb").is_ok());
    }
}
