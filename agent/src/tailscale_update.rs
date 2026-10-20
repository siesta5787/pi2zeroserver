//! Updating Tailscale without an OS update.
//!
//! 1. Ask pkgs.tailscale.com for the latest version (`/<track>/?mode=json`).
//! 2. Fetch the current signing keys (`/distsign.pub`) and check they're signed
//!    by one of Tailscale's root keys, pinned in `src/tailscale-roots/`.
//! 3. Download `tailscale_<v>_arm64.tgz`, hashing it as it arrives, and check
//!    its `.sig`: Ed25519 by a signing key over BLAKE2s-256(file) || u64le(len).
//! 4. Unpack just `tailscale` and `tailscaled` to `/data/tailscale/releases/<v>/`
//!    (renamed into place only once complete).
//! 5. Run `tailscale-activate <v>`, which switches over, waits for the daemon
//!    and switches back if it doesn't come up.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use blake2::Blake2s256;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Serialize;

const ROOT_KEYS: [&str; 2] = [
    include_str!("tailscale-roots/crawshaw-root.pem"),
    include_str!("tailscale-roots/distsign-prod-root-1-pub.pem"),
];

const MAX_TARBALL: u64 = 200 << 20;
/// Space needed on /data: the download plus the unpacked binaries
const MIN_FREE_SPACE: u64 = 250 << 20;

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Update {
    Idle,
    Checking,
    Downloading { version: String, percent: u8 },
    Installing { version: String },
    Updated { version: String },
    UpToDate { version: String },
    Failed { error: String },
}

impl Update {
    fn busy(&self) -> bool {
        matches!(self, Update::Checking | Update::Downloading { .. } | Update::Installing { .. })
    }
}

pub struct Config {
    /// e.g. https://pkgs.tailscale.com
    pub pkgs: String,
    /// "stable" or "unstable"
    pub track: String,
    pub releases: PathBuf,
    /// /usr/libexec/box/tailscale-activate
    pub activate: PathBuf,
    /// The CLI of the running release, to learn its version
    pub cli: PathBuf,
    pub roots: Vec<VerifyingKey>,
}

impl Config {
    pub fn device() -> Self {
        Self {
            pkgs: "https://pkgs.tailscale.com".into(),
            track: "stable".into(),
            releases: "/data/tailscale/releases".into(),
            activate: "/usr/libexec/box/tailscale-activate".into(),
            cli: "/usr/bin/tailscale".into(),
            roots: ROOT_KEYS.iter().flat_map(|pem| parse_keys(pem, "ROOT PUBLIC KEY").unwrap()).collect(),
        }
    }
}

#[derive(Clone)]
pub struct TailscaleUpdate {
    config: Arc<Config>,
    state: Arc<Mutex<Update>>,
}

impl TailscaleUpdate {
    pub fn new(config: Config) -> Self {
        Self { config: Arc::new(config), state: Arc::new(Mutex::new(Update::Idle)) }
    }

    pub fn state(&self) -> Update {
        self.state.lock().unwrap().clone()
    }

    /// Checks for a newer release and installs it; false if one is already running.
    pub fn start(&self) -> bool {
        {
            let mut state = self.state.lock().unwrap();
            if state.busy() {
                return false;
            }
            *state = Update::Checking;
        }
        let (config, state) = (self.config.clone(), self.state.clone());
        tokio::spawn(async move {
            let s = state.clone();
            let outcome = tokio::task::spawn_blocking(move || update(&config, &s))
                .await
                .unwrap_or_else(|e| Err(format!("updater crashed: {e}")));
            *state.lock().unwrap() = match outcome {
                Ok(done) => done,
                Err(error) => Update::Failed { error },
            };
        });
        true
    }
}

/// Parses PEM blocks of the given type, each holding a base64 Ed25519 key.
pub fn parse_keys(pem: &str, kind: &str) -> Result<Vec<VerifyingKey>, String> {
    let (begin, end) = (format!("-----BEGIN {kind}-----"), format!("-----END {kind}-----"));
    let mut keys = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(&begin) {
        let after = &rest[start + begin.len()..];
        let stop = after.find(&end).ok_or("unterminated PEM block")?;
        let b64: String = after[..stop].split_whitespace().collect();
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| e.to_string())?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| "key isn't 32 bytes")?;
        keys.push(VerifyingKey::from_bytes(&bytes).map_err(|e| e.to_string())?);
        rest = &after[stop + end.len()..];
    }
    if keys.is_empty() { Err(format!("no {kind} found")) } else { Ok(keys) }
}

fn verify_any(keys: &[VerifyingKey], msg: &[u8], sig: &[u8]) -> bool {
    let Ok(sig) = Signature::from_slice(sig) else {
        return false;
    };
    keys.iter().any(|k| k.verify(msg, &sig).is_ok())
}

/// The signing keys currently in use, if the root keys vouch for them
pub fn signing_keys(
    roots: &[VerifyingKey],
    bundle: &[u8],
    bundle_sig: &[u8],
) -> Result<Vec<VerifyingKey>, String> {
    if !verify_any(roots, bundle, bundle_sig) {
        return Err("Tailscale's signing keys aren't signed by a known root key".into());
    }
    parse_keys(std::str::from_utf8(bundle).map_err(|e| e.to_string())?, "SIGNING PUBLIC KEY")
}

/// What a package signature covers
fn package_message(hash: &[u8], len: u64) -> Vec<u8> {
    let mut msg = hash.to_vec();
    msg.extend_from_slice(&len.to_le_bytes());
    msg
}

fn version_numbers(v: &str) -> Vec<u64> {
    v.split('.').map(|n| n.parse().unwrap_or(0)).collect()
}

/// Unpacks tailscale and tailscaled from the tarball into `releases/<version>`.
fn unpack(tarball: &Path, version: &str, releases: &Path) -> Result<(), String> {
    let partial = releases.join(format!(".partial-{version}"));
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial).map_err(|e| e.to_string())?;
    let file = std::fs::File::open(tarball).map_err(|e| e.to_string())?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let prefix = format!("tailscale_{version}_arm64/");
    let mut found = 0;
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
        let Some(name @ ("tailscale" | "tailscaled")) = path.strip_prefix(&prefix) else {
            continue;
        };
        let dest = partial.join(name);
        let mut out = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        out.write_all(&buf).and_then(|_| out.sync_all()).map_err(|e| e.to_string())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        found += 1;
    }
    if found != 2 {
        return Err("the download doesn't contain tailscale and tailscaled".into());
    }
    std::fs::write(partial.join("VERSION"), format!("{version}\n")).map_err(|e| e.to_string())?;
    let final_dir = releases.join(version);
    let _ = std::fs::remove_dir_all(&final_dir);
    std::fs::rename(&partial, &final_dir).map_err(|e| e.to_string())
}

fn running_version(cli: &Path) -> Option<String> {
    let out = std::process::Command::new(cli).arg("version").output().ok()?;
    String::from_utf8(out.stdout).ok()?.lines().next().map(|l| l.trim().to_owned())
}

fn free_space(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: c is a valid C string; st is written by statvfs before use.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

fn update(config: &Config, state: &Mutex<Update>) -> Result<Update, String> {
    let set = |s: Update| *state.lock().unwrap() = s;
    let base = config.pkgs.trim_end_matches('/');

    let index = crate::http::get(&format!("{base}/{}/?mode=json", config.track), 1 << 20)?;
    let index: serde_json::Value =
        serde_json::from_slice(&index).map_err(|e| format!("version list: {e}"))?;
    let latest = index["TarballsVersion"].as_str().ok_or("version list has no TarballsVersion")?.to_owned();
    let tarball = index["Tarballs"]["arm64"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("tailscale_{latest}_arm64.tgz"));
    let running = running_version(&config.cli).unwrap_or_default();
    if version_numbers(&latest) <= version_numbers(&running) {
        return Ok(Update::UpToDate { version: running });
    }

    let bundle = crate::http::get(&format!("{base}/distsign.pub"), 64 << 10)?;
    let bundle_sig = crate::http::get(&format!("{base}/distsign.pub.sig"), 1 << 10)?;
    let keys = signing_keys(&config.roots, &bundle, &bundle_sig)?;

    std::fs::create_dir_all(&config.releases).map_err(|e| e.to_string())?;
    if free_space(&config.releases).is_some_and(|free| free < MIN_FREE_SPACE) {
        return Err("not enough free space on the data partition".into());
    }
    let download_path = config.releases.join(format!(".download-{latest}.tgz"));
    set(Update::Downloading { version: latest.clone(), percent: 0 });
    let url = format!("{base}/{}/{tarball}", config.track);
    let result = crate::http::download::<Blake2s256>(&url, &download_path, MAX_TARBALL, |percent| {
        set(Update::Downloading { version: latest.clone(), percent });
    })
    .and_then(|(hash, len)| {
        let sig = crate::http::get(&format!("{url}.sig"), 1 << 10)?;
        if !verify_any(&keys, &package_message(&hash, len), &sig) {
            return Err("the download's signature doesn't check out; nothing was installed".into());
        }
        set(Update::Installing { version: latest.clone() });
        unpack(&download_path, &latest, &config.releases)
    });
    let _ = std::fs::remove_file(&download_path);
    result?;

    let status = std::process::Command::new(&config.activate)
        .arg(&latest)
        .status()
        .map_err(|e| format!("couldn't start {}: {e}", config.activate.display()))?;
    match status.code() {
        Some(0) => Ok(Update::Updated { version: latest }),
        Some(1) => Err(format!("Tailscale {latest} didn't start, so the previous version was kept")),
        _ => Err(format!("Tailscale {latest} couldn't be installed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use blake2::Digest;
    use ed25519_dalek::{Signer, SigningKey};
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[test]
    fn real_tailscale_key_chain() {
        // distsign.pub and its signature as served by pkgs.tailscale.com on 2026-10-19
        let roots = Config::device().roots;
        assert_eq!(roots.len(), 2);
        let keys = signing_keys(
            &roots,
            include_bytes!("../testdata/tailscale-distsign.pub"),
            include_bytes!("../testdata/tailscale-distsign.pub.sig"),
        )
        .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(keys[0].as_bytes());
        assert_eq!(b64, "5MyXV+cvN4Rfe0mwuBrQV40ngYQ8XhG7hpCRux2OyCI=");

        let mut tampered = include_bytes!("../testdata/tailscale-distsign.pub").to_vec();
        tampered[40] ^= 1;
        assert!(
            signing_keys(&roots, &tampered, include_bytes!("../testdata/tailscale-distsign.pub.sig"))
                .is_err()
        );
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(version_numbers("1.104.1") > version_numbers("1.99.9"));
        assert!(version_numbers("1.104.1") == version_numbers("1.104.1"));
    }

    fn pem(kind: &str, key: &VerifyingKey) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(key.as_bytes());
        format!("-----BEGIN {kind}-----\n{b64}\n-----END {kind}-----\n")
    }

    fn script(path: &Path, body: &str) -> PathBuf {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_owned()
    }

    /// A fake pkgs.tailscale.com offering `version`; returns its base URL.
    async fn fake_pkgs(version: &str, tamper_tarball_sig: bool) -> (String, VerifyingKey) {
        let root = SigningKey::from_bytes(&[1; 32]);
        let signer = SigningKey::from_bytes(&[2; 32]);
        let bundle = pem("SIGNING PUBLIC KEY", &signer.verifying_key());
        let bundle_sig = root.sign(bundle.as_bytes()).to_bytes().to_vec();

        let mut tgz = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut tgz, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            for (name, body) in [("tailscale", "echo cli"), ("tailscaled", "echo daemon"), ("README", "x")] {
                let data = format!("#!/bin/sh\n{body}\n");
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(&mut header, format!("tailscale_{version}_arm64/{name}"), data.as_bytes())
                    .unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        let hash = Blake2s256::digest(&tgz);
        let mut sig = signer.sign(&package_message(&hash, tgz.len() as u64)).to_bytes().to_vec();
        if tamper_tarball_sig {
            sig[0] ^= 1;
        }

        let name = format!("tailscale_{version}_arm64.tgz");
        let index = serde_json::json!({"TarballsVersion": version, "Tarballs": {"arm64": name}}).to_string();
        let app = Router::new()
            .route("/stable/", get(move || async move { index }))
            .route("/distsign.pub", get(move || async move { bundle }))
            .route("/distsign.pub.sig", get(move || async move { bundle_sig }))
            .route(&format!("/stable/{name}"), get(move || async move { tgz }))
            .route(&format!("/stable/{name}.sig"), get(move || async move { sig }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), root.verifying_key())
    }

    fn config(dir: &Path, pkgs: String, root: VerifyingKey, running: &str, activate_exit: i32) -> Config {
        Config {
            pkgs,
            track: "stable".into(),
            releases: dir.join("releases"),
            activate: script(
                &dir.join("activate"),
                &format!("echo \"$1\" > {}/activated\nexit {activate_exit}", dir.display()),
            ),
            cli: script(&dir.join("tailscale"), &format!("echo {running}")),
            roots: vec![root],
        }
    }

    async fn finish(u: &TailscaleUpdate) -> Update {
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
    async fn installs_a_newer_signed_release() {
        let dir = tempfile::tempdir().unwrap();
        let (pkgs, root) = fake_pkgs("1.200.0", false).await;
        let u = TailscaleUpdate::new(config(dir.path(), pkgs, root, "1.104.1", 0));
        assert!(u.start());
        assert!(!u.start(), "only one update at a time");
        assert_eq!(finish(&u).await, Update::Updated { version: "1.200.0".into() });
        let rel = dir.path().join("releases/1.200.0");
        assert_eq!(std::fs::read_to_string(rel.join("VERSION")).unwrap(), "1.200.0\n");
        let mode = std::fs::metadata(rel.join("tailscaled")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(!rel.join("README").exists(), "only the two binaries are unpacked");
        assert_eq!(std::fs::read_to_string(dir.path().join("activated")).unwrap(), "1.200.0\n");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("releases")).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "download and partial dirs are cleaned up");
    }

    #[tokio::test]
    async fn refuses_a_bad_signature() {
        let dir = tempfile::tempdir().unwrap();
        let (pkgs, root) = fake_pkgs("1.200.0", true).await;
        let u = TailscaleUpdate::new(config(dir.path(), pkgs, root, "1.104.1", 0));
        u.start();
        let Update::Failed { error } = finish(&u).await else { panic!("expected failure") };
        assert!(error.contains("signature"), "{error}");
        assert!(!dir.path().join("releases/1.200.0").exists());
        assert!(!dir.path().join("activated").exists(), "nothing was activated");
    }

    #[tokio::test]
    async fn refuses_keys_from_an_unknown_root() {
        let dir = tempfile::tempdir().unwrap();
        let (pkgs, _) = fake_pkgs("1.200.0", false).await;
        let stranger = SigningKey::from_bytes(&[9; 32]).verifying_key();
        let u = TailscaleUpdate::new(config(dir.path(), pkgs, stranger, "1.104.1", 0));
        u.start();
        let Update::Failed { error } = finish(&u).await else { panic!("expected failure") };
        assert!(error.contains("root key"), "{error}");
    }

    #[tokio::test]
    async fn nothing_to_do_when_up_to_date() {
        let dir = tempfile::tempdir().unwrap();
        let (pkgs, root) = fake_pkgs("1.104.1", false).await;
        let u = TailscaleUpdate::new(config(dir.path(), pkgs, root, "1.104.1", 0));
        u.start();
        assert_eq!(finish(&u).await, Update::UpToDate { version: "1.104.1".into() });
    }

    #[tokio::test]
    async fn reports_a_rolled_back_activation() {
        let dir = tempfile::tempdir().unwrap();
        let (pkgs, root) = fake_pkgs("1.200.0", false).await;
        let u = TailscaleUpdate::new(config(dir.path(), pkgs, root, "1.104.1", 1));
        u.start();
        let Update::Failed { error } = finish(&u).await else { panic!("expected failure") };
        assert!(error.contains("previous version was kept"), "{error}");
    }
}
