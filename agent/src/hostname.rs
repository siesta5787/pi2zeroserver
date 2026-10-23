//! The box's name: `<name>.local` on the home network and the name Tailscale
//! gives it (`<name>.<tailnet>.ts.net`), so several boxes can live side by side.
//!
//! Saved in `hostname` in the agent's data folder (erased by a factory reset,
//! which brings back "box"). /usr/libexec/box/set-hostname applies it early at
//! boot, before networking and Tailscale start; a rename sets the kernel's host
//! name at once (what mDNS, DHCP and Tailscale read; systemd-hostnamed would
//! ignore it while /etc/hostname, read-only, says "box") and renames the box in
//! the tailnet if it's connected.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The name the image ships with
pub const DEFAULT: &str = "box";

/// A name as typed ("Kitchen Box") made into a host name ("kitchen-box"), or
/// None if nothing usable is left. Host names: a-z, 0-9 and "-", at most 63,
/// not starting or ending with "-".
pub fn normalize(input: &str) -> Option<String> {
    let mut name = String::new();
    for c in input.trim().chars() {
        match c {
            'a'..='z' | '0'..='9' => name.push(c),
            'A'..='Z' => name.push(c.to_ascii_lowercase()),
            ' ' | '-' | '_' | '.' if !name.ends_with('-') => name.push('-'),
            _ => {}
        }
    }
    let name = name.trim_matches('-');
    let name = &name[..name.len().min(63)];
    let name = name.trim_end_matches('-');
    (!name.is_empty()).then(|| name.to_owned())
}

/// The name the box goes by right now
pub fn current() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT.to_owned())
}

/// Renames the box; the side effects are switched off in tests
#[derive(Clone)]
pub struct Namer {
    data_dir: PathBuf,
    /// Set the kernel's host name (not in tests)
    apply: bool,
    tailscale: String,
}

fn set_kernel_hostname(name: &str) -> Result<(), String> {
    // SAFETY: a pointer and length to a live string
    let rc = unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("Couldn't apply the name: {}", std::io::Error::last_os_error()))
    }
}

impl Namer {
    pub fn new(data_dir: &Path) -> Self {
        Self { data_dir: data_dir.to_owned(), apply: true, tailscale: "tailscale".into() }
    }

    #[cfg(test)]
    pub fn inert(data_dir: &Path) -> Self {
        Self { data_dir: data_dir.to_owned(), apply: false, tailscale: "true".into() }
    }

    /// Saves and applies a new name; renames the box in the tailnet if it's
    /// signed in (`tailscale_running`). Returns the name as applied.
    pub async fn rename(&self, input: &str, tailscale_running: bool) -> Result<String, String> {
        let name = normalize(input).ok_or("Use letters, numbers and dashes.")?;
        std::fs::write(self.data_dir.join("hostname"), format!("{name}\n"))
            .map_err(|e| format!("Couldn't save the name: {e}"))?;
        if self.apply {
            set_kernel_hostname(&name)?;
        }
        if tailscale_running {
            // The tailnet name (and so the HTTPS address) follows; tailscale_serve re-links it
            run(&self.tailscale, &["set", &format!("--hostname={name}")]).await?;
        }
        eprintln!("renamed the box to {name:?}");
        Ok(name)
    }
}

async fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let cmd = tokio::process::Command::new(program).args(args).kill_on_drop(true).output();
    match tokio::time::timeout(Duration::from_secs(20), cmd).await {
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("failed").to_owned())
        }
        Ok(Err(e)) => Err(format!("{program}: {e}")),
        Err(_) => Err(format!("{program} didn't finish")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_host_names() {
        assert_eq!(normalize("Kitchen Box").as_deref(), Some("kitchen-box"));
        assert_eq!(normalize("  phones_2  ").as_deref(), Some("phones-2"));
        assert_eq!(normalize("--a  b--").as_deref(), Some("a-b"));
        assert_eq!(normalize("Café").as_deref(), Some("caf"));
        assert_eq!(normalize("box.local").as_deref(), Some("box-local"));
        assert_eq!(normalize("!!!"), None);
        assert_eq!(normalize(""), None);
        let long = normalize(&format!("{}-x", "a".repeat(62))).unwrap();
        assert!(long.len() <= 63 && !long.ends_with('-'), "{long}");
    }
}
