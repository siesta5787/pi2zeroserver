//! Installing and updating the app (kid-phone-server) from the release manifest.
//!
//! 1. Download the tarball the manifest names, hashing it with SHA-256 as it
//!    arrives; anything that doesn't match the manifest is thrown away.
//! 2. Unpack it to `/data/app/releases/.partial-<v>/` (nothing can land outside),
//!    link `app` to the binary the manifest names, write VERSION, and rename
//!    it to `releases/<v>/` once complete.
//! 3. On first install, write the app's environment (`/data/app/env`) from the
//!    template in the image plus a generated admin password, which the status
//!    page shows until the owner says they've signed in.
//! 4. Run `app-activate <v>`: snapshot the database, switch, health-check,
//!    and restore and switch back if the new version doesn't come up.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::Sha256;

use crate::manifest::AppRelease;
use crate::progress::Progress;

const MAX_TARBALL: u64 = 200 << 20;
/// The download plus the unpacked release
const MIN_FREE_SPACE: u64 = 100 << 20;

pub struct Config {
    /// /data/app
    pub root: PathBuf,
    /// /usr/libexec/box/app-activate
    pub activate: PathBuf,
    /// /etc/box/app.env
    pub env_template: PathBuf,
}

impl Config {
    pub fn device() -> Self {
        Self {
            root: "/data/app".into(),
            activate: "/usr/libexec/box/app-activate".into(),
            env_template: "/etc/box/app.env".into(),
        }
    }
}

#[derive(Clone)]
pub struct AppUpdate {
    config: Arc<Config>,
    state: Arc<Mutex<Progress>>,
}

/// Dotted numbers compared numerically: 0.18.10 is newer than 0.18.9
pub fn app_newer(candidate: &str, installed: &str) -> bool {
    let key = |v: &str| v.split('.').map(|n| n.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    key(candidate) > key(installed)
}

impl AppUpdate {
    pub fn new(config: Config) -> Self {
        Self { config: Arc::new(config), state: Arc::new(Mutex::new(Progress::Idle)) }
    }

    pub fn state(&self) -> Progress {
        self.state.lock().unwrap().clone()
    }

    /// The release that's running, if any
    pub fn installed_version(&self) -> Option<String> {
        let v = std::fs::read_to_string(self.config.root.join("current/VERSION")).ok()?;
        Some(v.trim().to_owned())
    }

    /// The generated first-login password, until the owner dismisses it
    pub fn initial_admin_password(&self) -> Option<String> {
        let p = std::fs::read_to_string(self.config.root.join("initial-admin-password")).ok()?;
        Some(p.trim().to_owned())
    }

    pub fn forget_initial_admin_password(&self) {
        let _ = std::fs::remove_file(self.config.root.join("initial-admin-password"));
    }

    /// Installs `release` if it's newer than what's running; false if a job
    /// is already running.
    pub fn start(&self, release: AppRelease) -> bool {
        {
            let mut state = self.state.lock().unwrap();
            if state.busy() {
                return false;
            }
            *state = Progress::Checking;
        }
        let (config, state) = (self.config.clone(), self.state.clone());
        tokio::spawn(async move {
            let s = state.clone();
            let outcome = tokio::task::spawn_blocking(move || update(&config, &release, &s))
                .await
                .unwrap_or_else(|e| Err(format!("updater crashed: {e}")));
            *state.lock().unwrap() = match outcome {
                Ok(done) => done,
                Err(error) => Progress::Failed { error },
            };
        });
        true
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_password() -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut bytes = [0u8; 20];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("reading /dev/urandom");
    bytes.iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect()
}

fn write_private(path: &Path, contents: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    f.write_all(contents.as_bytes()).and_then(|_| f.sync_all()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// First install only: the app's environment, with a fresh admin password
fn ensure_env(config: &Config) -> Result<(), String> {
    let env = config.root.join("env");
    if env.exists() {
        return Ok(());
    }
    let template = std::fs::read_to_string(&config.env_template)
        .map_err(|e| format!("{}: {e}", config.env_template.display()))?;
    let password = random_password();
    write_private(&config.root.join("initial-admin-password"), &format!("{password}\n"))?;
    write_private(&env, &format!("{}\nADMIN_PASSWORD={password}\n", template.trim_end()))
}

/// Unpacks the tarball into `releases/<version>` with `app` linked to `command`.
fn unpack(tarball: &Path, version: &str, command: &str, releases: &Path) -> Result<(), String> {
    if command.contains('/') || command.is_empty() || command == ".." {
        return Err(format!("bad app command {command:?}"));
    }
    let partial = releases.join(format!(".partial-{version}"));
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial).map_err(|e| e.to_string())?;
    let file = std::fs::File::open(tarball).map_err(|e| e.to_string())?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    // Refuses entries that would land outside `partial` (absolute paths, "..")
    archive.unpack(&partial).map_err(|e| format!("unpacking: {e}"))?;

    let binary = partial.join(command);
    if !binary.is_file() {
        return Err(format!("the download doesn't contain {command}"));
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::os::unix::fs::symlink(command, partial.join("app")).map_err(|e| e.to_string())?;
    std::fs::write(partial.join("VERSION"), format!("{version}\n")).map_err(|e| e.to_string())?;

    let final_dir = releases.join(version);
    let _ = std::fs::remove_dir_all(&final_dir);
    std::fs::rename(&partial, &final_dir).map_err(|e| e.to_string())
}

fn free_space(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: c is a valid C string; st is written by statvfs before use.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

fn update(config: &Config, release: &AppRelease, state: &Mutex<Progress>) -> Result<Progress, String> {
    let set = |s: Progress| *state.lock().unwrap() = s;
    let installed = std::fs::read_to_string(config.root.join("current/VERSION")).ok();
    if let Some(installed) = installed.as_deref().map(str::trim)
        && !app_newer(&release.version, installed)
    {
        return Ok(Progress::UpToDate { version: installed.to_owned() });
    }

    let releases = config.root.join("releases");
    std::fs::create_dir_all(&releases).map_err(|e| e.to_string())?;
    if free_space(&releases).is_some_and(|free| free < MIN_FREE_SPACE) {
        return Err("not enough free space on the data partition".into());
    }
    let version = release.version.clone();
    let download_path = releases.join(format!(".download-{version}.tgz"));
    set(Progress::Downloading { version: version.clone(), percent: 0 });
    let result = crate::http::download::<Sha256>(&release.url, &download_path, MAX_TARBALL, |percent| {
        set(Progress::Downloading { version: version.clone(), percent });
    })
    .and_then(|(hash, _)| {
        if hex(&hash) != release.sha256.to_ascii_lowercase() {
            return Err("the download doesn't match the release manifest; nothing was installed".into());
        }
        set(Progress::Installing { version: version.clone() });
        unpack(&download_path, &version, &release.command(), &releases)
    });
    let _ = std::fs::remove_file(&download_path);
    result?;
    ensure_env(config)?;

    let status = std::process::Command::new(&config.activate)
        .arg(&version)
        .status()
        .map_err(|e| format!("couldn't start {}: {e}", config.activate.display()))?;
    match status.code() {
        Some(0) => Ok(Progress::Updated { version }),
        Some(1) => {
            Err(format!("version {version} didn't start, so the previous version and its data were kept"))
        }
        _ => Err(format!("version {version} couldn't be installed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[test]
    fn versions() {
        assert!(app_newer("0.18.10", "0.18.9"));
        assert!(!app_newer("0.18.6", "0.18.6"));
        assert!(!app_newer("0.9.0", "0.18.6"));
    }

    fn tarball(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut tgz = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut tgz, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            for (name, body) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_entry_type(tar::EntryType::Regular);
                // set_path refuses "..", so write the raw name for the escape test
                let raw = &mut header.as_old_mut().name;
                raw[..name.len()].copy_from_slice(name.as_bytes());
                header.set_cksum();
                tar.append(&header, body.as_bytes()).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        tgz
    }

    /// Serves `tgz` over HTTP; returns its URL and SHA-256.
    async fn serve(tgz: Vec<u8>) -> (String, String) {
        let sha = hex(&Sha256::digest(&tgz));
        let app = axum::Router::new().route("/app.tgz", axum::routing::get(move || async move { tgz }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/app.tgz"), sha)
    }

    fn config(dir: &Path, activate_exit: i32) -> Config {
        let activate = dir.join("activate");
        let root = dir.join("app");
        std::fs::create_dir_all(&root).unwrap();
        // Like the real script: point current at the release
        std::fs::write(
            &activate,
            format!(
                "#!/bin/sh\necho \"$1\" >> {d}/activated\n[ {activate_exit} = 0 ] && ln -sfn releases/$1 {r}/current\nexit {activate_exit}\n",
                d = dir.display(),
                r = root.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&activate, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env_template = dir.join("app.env");
        std::fs::write(&env_template, "BIND_ADDR=127.0.0.1:3100\nADMIN_USERNAME=admin\n").unwrap();
        Config { root, activate, env_template }
    }

    fn release(version: &str, url: String, sha256: String) -> AppRelease {
        AppRelease { name: "kid-phone-server".into(), version: version.into(), url, sha256, command: None }
    }

    async fn finish(u: &AppUpdate) -> Progress {
        for _ in 0..200 {
            let s = u.state();
            if !s.busy() {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("update didn't finish: {:?}", u.state())
    }

    #[tokio::test]
    async fn first_install_then_update() {
        let dir = tempfile::tempdir().unwrap();
        let u = AppUpdate::new(config(dir.path(), 0));
        let (url, sha) =
            serve(tarball(&[("kid_phone_server", "#!/bin/sh\n"), ("static/style.css", "body{}")])).await;
        assert!(u.start(release("0.18.6", url.clone(), sha.clone())));
        assert_eq!(finish(&u).await, Progress::Updated { version: "0.18.6".into() });

        let rel = dir.path().join("app/releases/0.18.6");
        assert_eq!(std::fs::read_link(rel.join("app")).unwrap(), Path::new("kid_phone_server"));
        assert_eq!(
            std::fs::metadata(rel.join("kid_phone_server")).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(rel.join("static/style.css").exists());
        assert_eq!(u.installed_version().as_deref(), Some("0.18.6"));

        let env = std::fs::read_to_string(dir.path().join("app/env")).unwrap();
        let password = u.initial_admin_password().unwrap();
        assert!(
            env.contains("BIND_ADDR=127.0.0.1:3100") && env.contains(&format!("ADMIN_PASSWORD={password}"))
        );
        assert_eq!(password.len(), 20);
        let mode = std::fs::metadata(dir.path().join("app/env")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        u.forget_initial_admin_password();
        assert_eq!(u.initial_admin_password(), None);

        // Same version again: nothing to do; the environment isn't regenerated
        u.start(release("0.18.6", url, sha));
        assert_eq!(finish(&u).await, Progress::UpToDate { version: "0.18.6".into() });
        assert_eq!(std::fs::read_to_string(dir.path().join("app/env")).unwrap(), env);
    }

    #[tokio::test]
    async fn refuses_a_download_that_does_not_match() {
        let dir = tempfile::tempdir().unwrap();
        let u = AppUpdate::new(config(dir.path(), 0));
        let (url, _) = serve(tarball(&[("kid_phone_server", "#!/bin/sh\n")])).await;
        u.start(release("0.18.6", url, "00".repeat(32)));
        let Progress::Failed { error } = finish(&u).await else { panic!("expected failure") };
        assert!(error.contains("doesn't match"), "{error}");
        assert!(!dir.path().join("activated").exists());
        assert!(!dir.path().join("app/env").exists(), "nothing set up for a rejected download");
    }

    #[tokio::test]
    async fn nothing_escapes_the_release_dir() {
        let dir = tempfile::tempdir().unwrap();
        let u = AppUpdate::new(config(dir.path(), 0));
        let (url, sha) = serve(tarball(&[("kid_phone_server", "#!/bin/sh\n"), ("../../evil", "x")])).await;
        u.start(release("0.18.6", url, sha));
        // The tar crate skips such entries; whatever happens, nothing lands outside
        finish(&u).await;
        for outside in
            [dir.path().join("evil"), dir.path().join("app/evil"), dir.path().join("app/releases/evil")]
        {
            assert!(!outside.exists(), "{} was written", outside.display());
        }
    }

    #[tokio::test]
    async fn reports_a_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let u = AppUpdate::new(config(dir.path(), 1));
        let (url, sha) = serve(tarball(&[("kid_phone_server", "#!/bin/sh\n")])).await;
        u.start(release("0.18.7", url, sha));
        let Progress::Failed { error } = finish(&u).await else { panic!("expected failure") };
        assert!(error.contains("previous version and its data were kept"), "{error}");
    }

    #[tokio::test]
    async fn missing_binary_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let u = AppUpdate::new(config(dir.path(), 0));
        let (url, sha) = serve(tarball(&[("static/style.css", "x")])).await;
        u.start(release("0.18.6", url, sha));
        let Progress::Failed { error } = finish(&u).await else { panic!("expected failure") };
        assert!(error.contains("doesn't contain kid_phone_server"), "{error}");
    }
}
