//! The signed release manifests: which system and app versions are current.
//!
//! The system's comes from this project's releases (below); the app's from the
//! app's own releases (box-app.json, see app_source.rs), checked at the same
//! time and combined into one `Check`. A system manifest's own `app` entry
//! (older releases had one) is ignored: the app is whatever the box pinned.
//!
//! `manifest.json` and `manifest.json.sig` (Ed25519 over the file's exact
//! bytes) are published with each release of this repo. The address and the
//! public key are baked into the read-only OS image:
//!   /etc/box/manifest-url       https://.../releases/latest/download/manifest.json
//!   /etc/box/manifest-key.pub   base64 of the raw 32-byte public key
//! Anything that doesn't verify is ignored; the box keeps its last good answer.
//!
//! Test versions: boxes whose owner opts in ("Get test versions early") also
//! read /etc/box/manifest-testing-url, the `testing` pre-release that
//! tools/publish-release.sh --prerelease keeps pointing at the newest
//! pre-release, and take whichever manifest has the newer system. GitHub's
//! "latest" never includes pre-releases, so other boxes don't see them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

const SCHEMA: u32 = 1;
const FIRST_CHECK: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct Manifest {
    pub schema: u32,
    pub os: Option<OsRelease>,
    pub app: Option<AppRelease>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct OsRelease {
    pub version: String,
    /// RAUC bundle; RAUC checks its signature itself
    pub bundle_url: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct AppRelease {
    pub name: String,
    pub version: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the tarball at `url`
    pub sha256: String,
    /// The binary inside the tarball; defaults to the name with - as _
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Starting environment, written on first install (missing keys added on updates)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Environment variables the box fills with a random value on first install
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// What the box page shows for signing in to the app the first time
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_login: Option<FirstLogin>,
    /// The publisher's key (base64), which signs box-app.json
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct FirstLogin {
    pub username: String,
    /// The secret holding the password
    pub password: String,
}

impl AppRelease {
    pub fn command(&self) -> String {
        self.command.clone().unwrap_or_else(|| self.name.replace('-', "_"))
    }
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Check {
    /// The last manifest that verified
    pub latest: Option<Manifest>,
    /// Unix time of the last successful check
    pub checked_at: Option<u64>,
    /// Why the most recent check failed, if it did
    pub error: Option<String>,
    pub checking: bool,
    /// The system in `latest` is a test version (the owner opted in)
    pub testing: bool,
    /// Why the app's releases couldn't be checked, if they couldn't
    pub app_error: Option<String>,
    /// The app in `latest` is a test version
    pub app_testing: bool,
}

pub struct Config {
    pub url: String,
    /// Where the newest test version's manifest is
    pub testing_url: Option<String>,
    pub key: VerifyingKey,
}

impl Config {
    /// None if this image wasn't built with a manifest address and key
    pub fn device() -> Option<Self> {
        let url = std::fs::read_to_string("/etc/box/manifest-url").ok()?.trim().to_owned();
        let key = std::fs::read_to_string("/etc/box/manifest-key.pub").ok()?;
        let testing_url = std::fs::read_to_string("/etc/box/manifest-testing-url")
            .ok()
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty());
        Some(Self { url, testing_url, key: parse_key(&key).ok()? })
    }
}

pub fn parse_key(b64: &str) -> Result<VerifyingKey, String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).map_err(|e| e.to_string())?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| "manifest key isn't 32 bytes")?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| e.to_string())
}

/// Checks the signature and parses; the only way a manifest gets used.
pub fn verify(key: &VerifyingKey, body: &[u8], sig: &[u8]) -> Result<Manifest, String> {
    let sig = Signature::from_slice(sig).map_err(|_| "manifest signature is malformed")?;
    key.verify(body, &sig).map_err(|_| "manifest signature doesn't match this box's key")?;
    let manifest: Manifest = serde_json::from_slice(body).map_err(|e| format!("manifest: {e}"))?;
    if manifest.schema != SCHEMA {
        return Err(format!("manifest schema {} isn't supported (expected {SCHEMA})", manifest.schema));
    }
    Ok(manifest)
}

/// A newer system version (see version.rs: 0.51 > 0.50; old date versions are older than any)
pub fn os_newer(candidate: &str, running: &str) -> bool {
    crate::version::newer(candidate, running)
}

fn fetch_one(key: &VerifyingKey, url: &str) -> Result<Manifest, String> {
    let body = crate::http::get(url, 64 << 10)?;
    let sig = crate::http::get(&format!("{url}.sig"), 1 << 10)?;
    verify(key, &body, &sig)
}

fn os_version(m: &Manifest) -> &str {
    m.os.as_ref().map(|os| os.version.as_str()).unwrap_or("")
}

/// The regular manifest, or the test one if it has a newer system. A missing
/// or broken test manifest never stands in the way of regular updates.
/// Returns the manifest and whether it's the test one.
pub fn pick(
    regular: Result<Manifest, String>,
    test: Option<Result<Manifest, String>>,
) -> Result<(Manifest, bool), String> {
    match (regular, test) {
        (Ok(r), Some(Ok(t))) if os_newer(os_version(&t), os_version(&r)) => Ok((t, true)),
        (Ok(r), _) => Ok((r, false)),
        (Err(_), Some(Ok(t))) => Ok((t, true)),
        (Err(e), _) => Err(e),
    }
}

fn fetch(config: &Config, testing: bool) -> Result<(Manifest, bool), String> {
    let regular = fetch_one(&config.key, &config.url);
    let test = config.testing_url.as_deref().filter(|_| testing).map(|url| fetch_one(&config.key, url));
    pick(regular, test)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone)]
pub struct Manifests {
    config: Option<Arc<Config>>,
    state: Arc<Mutex<Check>>,
    /// The owner opted in to test versions
    testing: Arc<AtomicBool>,
    /// The app this box runs, once chosen
    app_source: Arc<Mutex<Option<crate::app_source::AppSource>>>,
}

impl Manifests {
    pub fn new(config: Option<Config>) -> Self {
        Self {
            config: config.map(Arc::new),
            state: Arc::new(Mutex::new(Check::default())),
            testing: Default::default(),
            app_source: Default::default(),
        }
    }

    pub fn set_testing(&self, on: bool) {
        self.testing.store(on, Ordering::Relaxed);
    }

    pub fn configured(&self) -> bool {
        self.config.is_some()
    }

    pub fn state(&self) -> Check {
        self.state.lock().unwrap().clone()
    }

    pub fn set_app_source(&self, source: Option<crate::app_source::AppSource>) {
        *self.app_source.lock().unwrap() = source;
    }

    pub fn app_source(&self) -> Option<crate::app_source::AppSource> {
        self.app_source.lock().unwrap().clone()
    }

    /// Starts a check in the background (the system's releases and the app's);
    /// false if one is already running or there's nothing to check.
    pub fn check_now(&self) -> bool {
        let config = self.config.clone();
        let source = self.app_source();
        if config.is_none() && source.is_none() {
            return false;
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.checking {
                return false;
            }
            state.checking = true;
        }
        let state = self.state.clone();
        let testing = self.testing.load(Ordering::Relaxed);
        tokio::spawn(async move {
            let has_source = source.is_some();
            let result = tokio::task::spawn_blocking(move || {
                let os = config.map(|c| fetch(&c, testing));
                let app = source.map(|s| crate::app_source::fetch(&s, testing));
                (os, app)
            })
            .await;
            let mut s = state.lock().unwrap();
            s.checking = false;
            let (os, app) = match result {
                Ok(r) => r,
                Err(e) => {
                    s.error = Some(format!("checker crashed: {e}"));
                    return;
                }
            };
            // Each half keeps its last good answer when its check fails
            let mut latest = s.latest.take().unwrap_or(Manifest { schema: SCHEMA, os: None, app: None });
            let mut os_error = None;
            match os {
                Some(Ok((manifest, from_testing))) => {
                    latest.os = manifest.os;
                    s.testing = from_testing;
                }
                Some(Err(e)) => os_error = Some(e),
                None => {}
            }
            let mut app_error = None;
            match app {
                Some(Ok((release, from_testing))) => {
                    latest.app = Some(release);
                    s.app_testing = from_testing;
                }
                Some(Err(e)) => app_error = Some(e),
                None if !has_source => latest.app = None,
                None => {}
            }
            s.latest = Some(latest);
            if os_error.is_none() {
                s.checked_at = Some(now());
            }
            s.error = os_error;
            s.app_error = app_error;
        });
        true
    }

    /// Checks shortly after start-up, then about every 12 hours. The spacing
    /// varies a little per box so a fleet doesn't hit the server at once.
    pub fn spawn_periodic(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FIRST_CHECK).await;
            loop {
                this.check_now();
                let jitter = Duration::from_secs(now() % 3600);
                tokio::time::sleep(CHECK_EVERY + jitter).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed(key: &SigningKey, body: &str) -> (Vec<u8>, Vec<u8>) {
        (body.as_bytes().to_vec(), key.sign(body.as_bytes()).to_bytes().to_vec())
    }

    const BODY: &str = r#"{"schema":1,
        "os":{"version":"2026.10.20.0100-gabc1234","bundle_url":"https://example.com/box.raucb"},
        "app":{"name":"kid-phone-server","version":"0.18.6","url":"https://example.com/a.tgz","sha256":"00ff"}}"#;

    fn with_os(version: &str) -> Manifest {
        Manifest {
            schema: 1,
            os: Some(OsRelease { version: version.into(), bundle_url: format!("https://x/{version}.raucb") }),
            app: None,
        }
    }

    #[test]
    fn test_versions_only_when_newer() {
        let stable = || Ok(with_os("2026.11.01.0300-gaaaaaaa"));
        let newer = || Ok(with_os("2026.11.05.0300-gbbbbbbb"));
        let older = || Ok(with_os("2026.10.20.0300-gccccccc"));
        assert_eq!(pick(stable(), None).unwrap(), (with_os("2026.11.01.0300-gaaaaaaa"), false));
        assert_eq!(pick(stable(), Some(newer())).unwrap(), (with_os("2026.11.05.0300-gbbbbbbb"), true));
        // Once a regular release overtakes the test one, back to regular releases
        assert!(!pick(stable(), Some(older())).unwrap().1);
        // No test release (yet): regular updates carry on
        assert!(!pick(stable(), Some(Err("404".into()))).unwrap().1);
        assert!(pick(Err("offline".into()), Some(newer())).unwrap().1);
        assert_eq!(pick(Err("offline".into()), Some(Err("404".into()))), Err("offline".into()));
    }

    #[test]
    fn accepts_a_signed_manifest() {
        let key = SigningKey::from_bytes(&[3; 32]);
        let (body, sig) = signed(&key, BODY);
        let m = verify(&key.verifying_key(), &body, &sig).unwrap();
        assert_eq!(m.os.unwrap().version, "2026.10.20.0100-gabc1234");
        assert_eq!(m.app.unwrap().version, "0.18.6");
    }

    #[test]
    fn rejects_tampering_and_other_keys() {
        let key = SigningKey::from_bytes(&[3; 32]);
        let (body, sig) = signed(&key, BODY);
        let tampered = String::from_utf8(body.clone()).unwrap().replace("0.18.6", "6.6.6");
        assert!(verify(&key.verifying_key(), tampered.as_bytes(), &sig).is_err());
        let stranger = SigningKey::from_bytes(&[4; 32]).verifying_key();
        assert!(verify(&stranger, &body, &sig).is_err());
        assert!(verify(&key.verifying_key(), &body, b"short").is_err());
    }

    #[test]
    fn rejects_unknown_schema() {
        let key = SigningKey::from_bytes(&[3; 32]);
        let (body, sig) = signed(&key, r#"{"schema":2,"os":null,"app":null}"#);
        let err = verify(&key.verifying_key(), &body, &sig).unwrap_err();
        assert!(err.contains("schema 2"), "{err}");
    }

    #[test]
    fn os_version_order() {
        assert!(os_newer("2026.10.20.0100-gabc", "2026.10.19.2244-g77c-dirty"));
        assert!(!os_newer("2026.10.19.2244-g77c", "2026.10.19.2244-g77c-dirty"));
        assert!(!os_newer("2026.9.30.2359-gaaa", "2026.10.1.0000-gbbb"));
    }

    #[test]
    fn key_parsing() {
        let key = SigningKey::from_bytes(&[3; 32]).verifying_key();
        let b64 = base64::engine::general_purpose::STANDARD.encode(key.as_bytes());
        assert_eq!(parse_key(&format!("{b64}\n")).unwrap(), key);
        assert!(parse_key("bm90IGEga2V5").is_err());
    }

    #[tokio::test]
    async fn checks_over_http() {
        use crate::app_source::tests::{key_b64, signed_app};
        use axum::{Router, routing::get};
        let key = SigningKey::from_bytes(&[3; 32]);
        let (body, sig) = signed(&key, BODY);
        let (test_body, test_sig) = signed(&key, &BODY.replace("2026.10.20.0100", "2026.11.02.0100"));
        // The app's own releases, signed by its publisher
        let publisher = SigningKey::from_bytes(&[9; 32]);
        let (app, app_sig) = signed_app(&publisher, "0.19.0", "https://x/a.tgz", &"ab".repeat(32));
        let (app_test, app_test_sig) =
            signed_app(&publisher, "0.20.0-rc", "https://x/b.tgz", &"cd".repeat(32));
        let app_routes = Router::new()
            .route("/app/box-app.json", get(move || async move { app }))
            .route("/app/box-app.json.sig", get(move || async move { app_sig }))
            .route("/app-testing/box-app.json", get(move || async move { app_test }))
            .route("/app-testing/box-app.json.sig", get(move || async move { app_test_sig }));
        let router = Router::new()
            .route("/manifest.json", get(move || async move { body }))
            .route("/manifest.json.sig", get(move || async move { sig }))
            .route("/testing/manifest.json", get(move || async move { test_body }))
            .route("/testing/manifest.json.sig", get(move || async move { test_sig }))
            .merge(app_routes);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let m = Manifests::new(Some(Config {
            url: format!("http://{addr}/manifest.json"),
            testing_url: Some(format!("http://{addr}/testing/manifest.json")),
            key: key.verifying_key(),
        }));
        let check = |m: &Manifests| {
            assert!(m.check_now());
            let m = m.clone();
            async move {
                for _ in 0..100 {
                    if !m.state().checking {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                m.state()
            }
        };
        let state = check(&m).await;
        assert_eq!(state.error, None);
        assert_eq!(os_version(state.latest.as_ref().unwrap()), "2026.10.20.0100-gabc1234");
        assert!(state.checked_at.is_some() && !state.testing);
        // No app chosen: the system manifest's own app entry doesn't count
        assert_eq!(state.latest.as_ref().unwrap().app, None);

        m.set_app_source(Some(crate::app_source::AppSource {
            link: "test".into(),
            manifest_url: format!("http://{addr}/app/box-app.json"),
            testing_url: Some(format!("http://{addr}/app-testing/box-app.json")),
            public_key: key_b64(&publisher),
            name: "kid-phone-server".into(),
        }));
        let state = check(&m).await;
        assert_eq!(state.app_error, None);
        assert_eq!(state.latest.as_ref().unwrap().app.as_ref().unwrap().version, "0.19.0");

        // Opted in: the newer test versions of both
        m.set_testing(true);
        let state = check(&m).await;
        assert_eq!(os_version(state.latest.as_ref().unwrap()), "2026.11.02.0100-gabc1234");
        assert!(state.testing && state.app_testing);
        assert_eq!(state.latest.as_ref().unwrap().app.as_ref().unwrap().version, "0.20.0-rc");

        // Pinned to another publisher: the app check fails, the last good answer stays
        m.set_app_source(Some(crate::app_source::AppSource {
            public_key: key_b64(&SigningKey::from_bytes(&[10; 32])),
            ..m.app_source().unwrap()
        }));
        let state = check(&m).await;
        assert!(state.app_error.unwrap().contains("isn't signed with this box's app key"));
        assert_eq!(state.latest.as_ref().unwrap().app.as_ref().unwrap().version, "0.20.0-rc");
        assert_eq!(state.error, None, "the system check is unaffected");
    }

    #[test]
    fn not_configured_does_nothing() {
        let m = Manifests::new(None);
        assert!(!m.configured());
        // check_now must not need a runtime when there's nothing to do
        assert!(!m.check_now());
    }
}
