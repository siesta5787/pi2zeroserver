//! The signed release manifest: which OS and app versions are current.
//!
//! `manifest.json` and `manifest.json.sig` (Ed25519 over the file's exact
//! bytes) are published with each release of this repo. The address and the
//! public key are baked into the read-only OS image:
//!   /etc/box/manifest-url       https://.../releases/latest/download/manifest.json
//!   /etc/box/manifest-key.pub   base64 of the raw 32-byte public key
//! Anything that doesn't verify is ignored; the box keeps its last good answer.

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
}

pub struct Config {
    pub url: String,
    pub key: VerifyingKey,
}

impl Config {
    /// None if this image wasn't built with a manifest address and key
    pub fn device() -> Option<Self> {
        let url = std::fs::read_to_string("/etc/box/manifest-url").ok()?.trim().to_owned();
        let key = std::fs::read_to_string("/etc/box/manifest-key.pub").ok()?;
        Some(Self { url, key: parse_key(&key).ok()? })
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

/// OS versions look like 2026.10.19.2244-g77ccad4: compare the date/time part.
pub fn os_newer(candidate: &str, running: &str) -> bool {
    fn key(v: &str) -> Vec<u64> {
        v.split("-g").next().unwrap_or(v).split('.').map(|n| n.parse().unwrap_or(0)).collect()
    }
    key(candidate) > key(running)
}

fn fetch(config: &Config) -> Result<Manifest, String> {
    let body = crate::http::get(&config.url, 64 << 10)?;
    let sig = crate::http::get(&format!("{}.sig", config.url), 1 << 10)?;
    verify(&config.key, &body, &sig)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone)]
pub struct Manifests {
    config: Option<Arc<Config>>,
    state: Arc<Mutex<Check>>,
}

impl Manifests {
    pub fn new(config: Option<Config>) -> Self {
        Self { config: config.map(Arc::new), state: Arc::new(Mutex::new(Check::default())) }
    }

    pub fn configured(&self) -> bool {
        self.config.is_some()
    }

    pub fn state(&self) -> Check {
        self.state.lock().unwrap().clone()
    }

    /// Starts a check in the background; false if one is already running or
    /// update checks aren't configured.
    pub fn check_now(&self) -> bool {
        let Some(config) = self.config.clone() else {
            return false;
        };
        {
            let mut state = self.state.lock().unwrap();
            if state.checking {
                return false;
            }
            state.checking = true;
        }
        let state = self.state.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || fetch(&config))
                .await
                .unwrap_or_else(|e| Err(format!("checker crashed: {e}")));
            let mut s = state.lock().unwrap();
            s.checking = false;
            match result {
                Ok(manifest) => {
                    s.latest = Some(manifest);
                    s.checked_at = Some(now());
                    s.error = None;
                }
                Err(e) => s.error = Some(e),
            }
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
        use axum::{Router, routing::get};
        let key = SigningKey::from_bytes(&[3; 32]);
        let (body, sig) = signed(&key, BODY);
        let app = Router::new()
            .route("/manifest.json", get(move || async move { body }))
            .route("/manifest.json.sig", get(move || async move { sig }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let m = Manifests::new(Some(Config {
            url: format!("http://{addr}/manifest.json"),
            key: key.verifying_key(),
        }));
        assert!(m.check_now());
        for _ in 0..100 {
            if !m.state().checking {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let state = m.state();
        assert_eq!(state.error, None);
        assert_eq!(state.latest.unwrap().app.unwrap().name, "kid-phone-server");
        assert!(state.checked_at.is_some());
    }

    #[test]
    fn not_configured_does_nothing() {
        let m = Manifests::new(None);
        assert!(!m.configured());
        // check_now must not need a runtime when there's nothing to do
        assert!(!m.check_now());
    }
}
