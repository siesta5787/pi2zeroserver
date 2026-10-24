//! Installing and updating the app from its signed release (box-app.json, see
//! app_source.rs).
//!
//! 1. Download the tarball the release names (or take the copy a preinstalled
//!    image carries), hashing it with SHA-256; anything that doesn't match the
//!    release is thrown away.
//! 2. Unpack it to `/data/app/releases/.partial-<v>/` (nothing can land outside),
//!    link `app` to the binary the release names, write VERSION and NAME, and
//!    rename it to `releases/<v>/` once complete.
//! 3. Write the app's environment (`/data/app/env`): the release's `env`, the
//!    box's listening address (BIND_ADDR, PORT) and a random value for each of
//!    its `secrets`, on first install; later releases only add settings that are
//!    missing, so the owner's and generated values stay. The first-login details
//!    the release names are kept for the status page until the owner dismisses them.
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

/// Where the app listens; the agent proxies to it (see main.rs --app)
pub const BIND_ADDR: &str = "127.0.0.1:3100";

pub struct Config {
    /// /data/app
    pub root: PathBuf,
    /// /usr/libexec/box/app-activate
    pub activate: PathBuf,
}

impl Config {
    pub fn device() -> Self {
        Self { root: "/data/app".into(), activate: "/usr/libexec/box/app-activate".into() }
    }
}

/// Where the tarball comes from
enum Tarball {
    Download,
    /// Already on the box (a preinstalled image's copy)
    Local(PathBuf),
}

#[derive(Clone)]
pub struct AppUpdate {
    config: Arc<Config>,
    state: Arc<Mutex<Progress>>,
}

/// Dotted numbers compared numerically: 0.18.10 is newer than 0.18.9
pub fn app_newer(candidate: &str, installed: &str) -> bool {
    crate::version::newer(candidate, installed)
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

    /// The installed app's name (from its release)
    pub fn installed_name(&self) -> Option<String> {
        let v = std::fs::read_to_string(self.config.root.join("current/NAME")).ok()?;
        Some(v.trim().to_owned()).filter(|n| !n.is_empty())
    }

    /// Username and generated password for signing in to the app the first
    /// time, until the owner dismisses them
    pub fn first_login(&self) -> Option<(String, String)> {
        let text = std::fs::read_to_string(self.config.root.join("first-login")).ok();
        if let Some(login) = text.and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) {
            return Some((login["username"].as_str()?.to_owned(), login["password"].as_str()?.to_owned()));
        }
        // Boxes set up before box-app.json: kid-phone-server's admin password
        let p = std::fs::read_to_string(self.config.root.join("initial-admin-password")).ok()?;
        Some(("admin".to_owned(), p.trim().to_owned()))
    }

    pub fn forget_first_login(&self) {
        let _ = std::fs::remove_file(self.config.root.join("first-login"));
        let _ = std::fs::remove_file(self.config.root.join("initial-admin-password"));
    }

    /// Installs `release` if it's newer than what's running; false if a job
    /// is already running.
    pub fn start(&self, release: AppRelease) -> bool {
        self.run(release, Tarball::Download)
    }

    /// Like `start`, from a tarball already on the box
    pub fn start_local(&self, release: AppRelease, tarball: PathBuf) -> bool {
        self.run(release, Tarball::Local(tarball))
    }

    fn run(&self, release: AppRelease, tarball: Tarball) -> bool {
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
            let outcome = tokio::task::spawn_blocking(move || update(&config, &release, &tarball, &s))
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

/// The app's environment: on first install the release's settings, the box's
/// listening address and fresh secrets; on updates only what's missing is added
fn ensure_env(config: &Config, release: &AppRelease) -> Result<(), String> {
    let path = config.root.join("env");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let has = |key: &str| existing.lines().any(|l| l.split_once('=').is_some_and(|(k, _)| k.trim() == key));
    let port = BIND_ADDR.rsplit(':').next().unwrap_or("3100");
    let mut added = String::new();
    let mut add = |key: &str, value: &str| {
        if !has(key) {
            added.push_str(&format!("{key}={value}\n"));
        }
    };
    // The box decides where the app listens: the agent proxies there
    add("BIND_ADDR", BIND_ADDR);
    add("PORT", port);
    for (key, value) in &release.env {
        add(key, value);
    }
    let mut generated = std::collections::BTreeMap::new();
    for secret in &release.secrets {
        if !has(secret) {
            let value = random_password();
            add(secret, &value);
            generated.insert(secret.clone(), value);
        }
    }
    if added.is_empty() {
        return Ok(());
    }
    if let Some(login) = &release.first_login
        && let Some(password) = generated.get(&login.password)
    {
        let json = serde_json::json!({"username": login.username, "password": password});
        write_private(&config.root.join("first-login"), &json.to_string())?;
    }
    let header = if existing.is_empty() {
        "# The app's environment (set up by the box from the app's release; edits are kept)\n"
    } else {
        ""
    };
    write_private(&path, &format!("{header}{existing}{added}"))
}

/// Unpacks the tarball into `releases/<version>` with `app` linked to `command`.
fn unpack(tarball: &Path, release: &AppRelease, releases: &Path) -> Result<(), String> {
    let (version, command) = (release.version.as_str(), release.command());
    let command = command.as_str();
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
    std::fs::write(partial.join("NAME"), format!("{}\n", release.name)).map_err(|e| e.to_string())?;

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

fn update(
    config: &Config,
    release: &AppRelease,
    tarball: &Tarball,
    state: &Mutex<Progress>,
) -> Result<Progress, String> {
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
    let check = |hash: &[u8]| {
        if hex(hash) == release.sha256.to_ascii_lowercase() {
            Ok(())
        } else {
            Err("the download doesn't match the app's release; nothing was installed".to_owned())
        }
    };
    let result = match tarball {
        Tarball::Download => {
            set(Progress::Downloading { version: version.clone(), percent: 0 });
            crate::http::download::<Sha256>(&release.url, &download_path, MAX_TARBALL, |percent| {
                set(Progress::Downloading { version: version.clone(), percent });
            })
            .and_then(|(hash, _)| check(&hash))
            .map(|()| download_path.clone())
        }
        Tarball::Local(path) => {
            use sha2::Digest;
            let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let mut hasher = Sha256::new();
            std::io::copy(&mut file, &mut hasher).map_err(|e| e.to_string())?;
            check(&hasher.finalize()).map(|()| path.clone())
        }
    }
    .and_then(|tgz| {
        set(Progress::Installing { version: version.clone() });
        unpack(&tgz, release, &releases)
    });
    let _ = std::fs::remove_file(&download_path);
    result?;
    ensure_env(config, release)?;

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
        Config { root, activate }
    }

    /// kid-phone-server's release, as its box-app.json describes it
    fn release(version: &str, url: String, sha256: String) -> AppRelease {
        AppRelease {
            name: "kid-phone-server".into(),
            version: version.into(),
            url,
            sha256,
            command: None,
            env: [("ADMIN_USERNAME".to_owned(), "admin".to_owned())].into(),
            secrets: vec!["ADMIN_PASSWORD".into()],
            first_login: Some(crate::manifest::FirstLogin {
                username: "admin".into(),
                password: "ADMIN_PASSWORD".into(),
            }),
            public_key: None,
        }
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

        assert_eq!(u.installed_name().as_deref(), Some("kid-phone-server"));
        let env = std::fs::read_to_string(dir.path().join("app/env")).unwrap();
        let (user, password) = u.first_login().unwrap();
        assert_eq!(user, "admin");
        for line in [
            "BIND_ADDR=127.0.0.1:3100",
            "PORT=3100",
            "ADMIN_USERNAME=admin",
            &format!("ADMIN_PASSWORD={password}"),
        ] {
            assert!(env.lines().any(|l| l == line), "{line} missing from {env}");
        }
        assert_eq!(password.len(), 20);
        let mode = std::fs::metadata(dir.path().join("app/env")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        u.forget_first_login();
        assert_eq!(u.first_login(), None);

        // Same version again: nothing to do; the environment isn't regenerated
        u.start(release("0.18.6", url.clone(), sha.clone()));
        assert_eq!(finish(&u).await, Progress::UpToDate { version: "0.18.6".into() });
        assert_eq!(std::fs::read_to_string(dir.path().join("app/env")).unwrap(), env);

        // A newer release with a new setting and secret: only those are added
        let mut newer = release("0.19.0", url, sha);
        newer.env.insert("ADMIN_USERNAME".into(), "changed".into());
        newer.env.insert("NEW_SETTING".into(), "1".into());
        newer.secrets.push("SESSION_KEY".into());
        u.start(newer);
        assert_eq!(finish(&u).await, Progress::Updated { version: "0.19.0".into() });
        let env2 = std::fs::read_to_string(dir.path().join("app/env")).unwrap();
        assert!(env2.starts_with(&env), "existing settings kept as they were");
        assert!(env2.contains("NEW_SETTING=1\n") && env2.contains("SESSION_KEY="));
        assert!(!env2.contains("ADMIN_USERNAME=changed"));
        assert_eq!(u.first_login(), None, "no new first-login password for an update");
    }

    #[tokio::test]
    async fn installs_a_preloaded_copy() {
        let dir = tempfile::tempdir().unwrap();
        let u = AppUpdate::new(config(dir.path(), 0));
        let tgz = tarball(&[("kid_phone_server", "#!/bin/sh\n")]);
        let path = dir.path().join("app.tgz");
        std::fs::write(&path, &tgz).unwrap();
        let sha = hex(&Sha256::digest(&tgz));
        // The URL is never used for a local copy
        assert!(u.start_local(
            release("0.18.6", "https://unused.invalid/a.tgz".into(), "00".repeat(32)),
            path.clone()
        ));
        let Progress::Failed { error } = finish(&u).await else { panic!("a wrong hash must fail") };
        assert!(error.contains("doesn't match"), "{error}");
        u.start_local(release("0.18.6", "https://unused.invalid/a.tgz".into(), sha), path);
        assert_eq!(finish(&u).await, Progress::Updated { version: "0.18.6".into() });
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
