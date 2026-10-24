//! Which app a box runs, and where its releases come from.
//!
//! The box is a generic base for one web app. The app is chosen once: by the
//! owner on the setup page (an "app link", e.g. github.com/owner/repo), or
//! preset in a preinstalled image (tools/make-app-image.sh). Choosing pins the
//! app's release address and its publisher's public key in
//! /data/box/app-source.json; from then on the box only installs releases
//! signed with that key. A factory reset keeps the pin (only the app's data is
//! erased); changing apps means reflashing the card.
//!
//! Each app release carries `box-app.json` + `box-app.json.sig` (Ed25519 over
//! the file's bytes, made by tools/app-release.sh): download address, SHA-256,
//! the binary's name, starting environment, secrets to generate, and the
//! publisher's public key. When pinning, the box trusts the key in the file it
//! fetched (trust on first use) after the owner compares its fingerprint.
//!
//! Test versions: GitHub-hosted apps can also publish a rolling `box-testing`
//! pre-release; boxes whose owner opted in take it when it's newer.

use std::path::Path;

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::manifest::AppRelease;

pub const SOURCE_FILE: &str = "/data/box/app-source.json";
/// A release put there by tools/make-app-image.sh, installed on first boot
pub const PRELOAD_DIR: &str = "/data/app/preload";

const SCHEMA: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AppSource {
    /// What the owner gave, as shown on the page
    pub link: String,
    pub manifest_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub testing_url: Option<String>,
    /// base64 of the raw 32-byte Ed25519 key releases must be signed with
    pub public_key: String,
    /// The app's name when it was pinned
    pub name: String,
}

#[derive(Serialize, Deserialize)]
struct AppManifestFile {
    schema: u32,
    #[serde(flatten)]
    app: AppRelease,
}

/// An app link → (link as shown, manifest URL, test-version manifest URL).
/// `github.com/owner/repo` (any GitHub URL of the repo), or the https URL of a
/// box-app.json hosted anywhere.
pub fn parse_link(input: &str) -> Result<(String, String, Option<String>), String> {
    let input = input.trim().trim_end_matches('/');
    let bare = input.strip_prefix("https://").or_else(|| input.strip_prefix("http://")).unwrap_or(input);
    if let Some(path) = bare.strip_prefix("github.com/").or_else(|| bare.strip_prefix("www.github.com/")) {
        let mut parts = path.split('/');
        let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
            return Err("A GitHub link needs the owner and the repository: github.com/owner/repo".into());
        };
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
        if !ok(owner) || !ok(repo) || owner.starts_with('.') || repo.starts_with('.') {
            return Err("That doesn't look like a GitHub repository link.".into());
        }
        let base = format!("https://github.com/{owner}/{repo}/releases");
        return Ok((
            format!("github.com/{owner}/{repo}"),
            format!("{base}/latest/download/box-app.json"),
            Some(format!("{base}/download/box-testing/box-app.json")),
        ));
    }
    // Tests serve releases from a local HTTP server
    let local_test_server = cfg!(test) && input.starts_with("http://127.0.0.1:");
    if (input.starts_with("https://") || local_test_server)
        && input.ends_with(".json")
        && !input.contains(char::is_whitespace)
    {
        return Ok((input.to_owned(), input.to_owned(), None));
    }
    Err("Paste the app's GitHub page (github.com/owner/repo) or the https link to its box-app.json.".into())
}

pub fn parse_key(b64: &str) -> Result<VerifyingKey, String> {
    crate::manifest::parse_key(b64).map_err(|e| format!("the app's key: {e}"))
}

/// A key as people compare it: the start of its SHA-256, in groups of four
pub fn fingerprint(b64: &str) -> String {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).unwrap_or_default();
    let hex: String = sha2::Sha256::digest(&bytes).iter().take(10).map(|b| format!("{b:02x}")).collect();
    hex.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

fn env_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Checks a signed box-app.json with `key`; the app release it describes
pub fn verify(key: &VerifyingKey, body: &[u8], sig: &[u8]) -> Result<AppRelease, String> {
    let sig = Signature::from_slice(sig).map_err(|_| "the app's signature is malformed")?;
    key.verify(body, &sig).map_err(|_| "the app release isn't signed with this box's app key")?;
    let file: AppManifestFile = serde_json::from_slice(body).map_err(|e| format!("box-app.json: {e}"))?;
    if file.schema != SCHEMA {
        return Err(format!("box-app.json schema {} isn't supported (expected {SCHEMA})", file.schema));
    }
    let app = file.app;
    if let Some(embedded) = &app.public_key
        && parse_key(embedded)? != *key
    {
        return Err("box-app.json names a different publisher key".into());
    }
    let valid_name =
        |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if !valid_name(&app.name) || !valid_name(&app.version) || app.sha256.len() != 64 {
        return Err("box-app.json is missing its name, version or SHA-256".into());
    }
    let bad_env = app.env.iter().any(|(k, v)| !env_name_ok(k) || v.contains(['\n', '\r', '\0']));
    if bad_env || !app.secrets.iter().all(|s| env_name_ok(s)) {
        return Err("box-app.json has an unusable environment setting".into());
    }
    if let Some(login) = &app.first_login
        && !app.secrets.contains(&login.password)
    {
        return Err("box-app.json: first_login.password must name one of its secrets".into());
    }
    Ok(app)
}

/// A box-app.json not pinned yet: checked against the key it names itself
/// (trust on first use; the owner compares the fingerprint)
pub fn verify_unpinned(body: &[u8], sig: &[u8]) -> Result<(AppRelease, String), String> {
    #[derive(Deserialize)]
    struct KeyOnly {
        public_key: Option<String>,
    }
    let named: KeyOnly = serde_json::from_slice(body).map_err(|e| format!("box-app.json: {e}"))?;
    let key_b64 = named.public_key.ok_or("box-app.json doesn't name its publisher key")?;
    let app = verify(&parse_key(&key_b64)?, body, sig)?;
    Ok((app, key_b64.trim().to_owned()))
}

fn get(url: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    let body = crate::http::get(url, 64 << 10)?;
    let sig = crate::http::get(&format!("{url}.sig"), 1 << 10)?;
    Ok((body, sig))
}

/// Fetches an app's latest release for the owner to confirm (blocking)
pub fn fetch_unpinned(manifest_url: &str) -> Result<(AppRelease, String), String> {
    let (body, sig) = get(manifest_url)?;
    verify_unpinned(&body, &sig)
}

/// The pinned app's newest release (blocking); the test version if the owner
/// opted in and it's newer. Returns the release and whether it's a test version.
pub fn fetch(source: &AppSource, testing: bool) -> Result<(AppRelease, bool), String> {
    let key = parse_key(&source.public_key)?;
    let regular = get(&source.manifest_url).and_then(|(b, s)| verify(&key, &b, &s));
    let test = source
        .testing_url
        .as_deref()
        .filter(|_| testing)
        .map(|url| get(url).and_then(|(b, s)| verify(&key, &b, &s)));
    match (regular, test) {
        (Ok(r), Some(Ok(t))) if crate::app_update::app_newer(&t.version, &r.version) => Ok((t, true)),
        (Ok(r), _) => Ok((r, false)),
        (Err(_), Some(Ok(t))) => Ok((t, true)),
        (Err(e), _) => Err(e),
    }
}

pub fn load(path: &Path) -> Option<AppSource> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Pins the app; refuses if one is pinned already (that takes a reflash)
pub fn pin(path: &Path, source: &AppSource) -> Result<(), String> {
    if path.exists() {
        return Err("This box already has its app. Changing apps means reflashing the card.".into());
    }
    let dir = path.parent().ok_or("no folder for the app source")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_vec_pretty(source).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("Couldn't save the app choice: {e}"))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub fn key_b64(key: &SigningKey) -> String {
        base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
    }

    /// A signed box-app.json for `version`, naming `key`
    pub fn signed_app(key: &SigningKey, version: &str, url: &str, sha256: &str) -> (Vec<u8>, Vec<u8>) {
        let body = serde_json::json!({
            "schema": 1, "name": "kid-phone-server", "version": version, "url": url, "sha256": sha256,
            "env": {"DATABASE_URL": "sqlite://data/kidphone.db", "ADMIN_USERNAME": "admin"},
            "secrets": ["ADMIN_PASSWORD"],
            "first_login": {"username": "admin", "password": "ADMIN_PASSWORD"},
            "public_key": key_b64(key),
        })
        .to_string()
        .into_bytes();
        let sig = key.sign(&body).to_bytes().to_vec();
        (body, sig)
    }

    #[test]
    fn understands_app_links() {
        for link in [
            "github.com/siesta5787/kid-phone-server",
            "https://github.com/siesta5787/kid-phone-server/",
            "https://github.com/siesta5787/kid-phone-server/releases/tag/v0.18.6",
            "https://github.com/siesta5787/kid-phone-server.git",
        ] {
            let (shown, url, testing) = parse_link(link).unwrap();
            assert_eq!(shown, "github.com/siesta5787/kid-phone-server", "{link}");
            assert_eq!(
                url,
                "https://github.com/siesta5787/kid-phone-server/releases/latest/download/box-app.json"
            );
            assert_eq!(
                testing.as_deref(),
                Some(
                    "https://github.com/siesta5787/kid-phone-server/releases/download/box-testing/box-app.json"
                )
            );
        }
        let (_, url, testing) = parse_link("https://apps.example.com/thing/box-app.json").unwrap();
        assert_eq!((url.as_str(), testing), ("https://apps.example.com/thing/box-app.json", None));
        for bad in [
            "",
            "github.com/only-owner",
            "github.com/../x",
            "http://example.com/box-app.json",
            "kid-phone-server",
        ] {
            assert!(parse_link(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn trusts_the_named_key_once_then_only_the_pinned_one() {
        let publisher = SigningKey::from_bytes(&[7; 32]);
        let (body, sig) = signed_app(&publisher, "0.19.0", "https://x/a.tgz", &"ab".repeat(32));
        let (app, key) = verify_unpinned(&body, &sig).unwrap();
        assert_eq!((app.version.as_str(), key.as_str()), ("0.19.0", key_b64(&publisher).as_str()));
        assert_eq!(app.first_login.unwrap().username, "admin");
        assert_eq!(fingerprint(&key).split(' ').count(), 5);

        // Someone else's release, even naming their own key, isn't accepted once pinned
        let other = SigningKey::from_bytes(&[8; 32]);
        let (body2, sig2) = signed_app(&other, "0.20.0", "https://x/a.tgz", &"ab".repeat(32));
        assert!(verify_unpinned(&body2, &sig2).is_ok(), "fine on its own...");
        assert!(verify(&parse_key(&key).unwrap(), &body2, &sig2).is_err(), "...but not for this box");
        // Tampering breaks the signature
        let mut tampered = body.clone();
        let i = tampered.windows(6).position(|w| w == b"0.19.0").unwrap();
        tampered[i + 2] = b'9';
        assert!(verify(&parse_key(&key).unwrap(), &tampered, &sig).is_err());
    }

    #[test]
    fn refuses_odd_releases() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let sign = |v: serde_json::Value| {
            let body = v.to_string().into_bytes();
            let sig = key.sign(&body).to_bytes().to_vec();
            verify(&key.verifying_key(), &body, &sig)
        };
        let base = || {
            serde_json::json!({"schema": 1, "name": "app", "version": "1.0.0", "url": "https://x/a.tgz",
                "sha256": "ab".repeat(32)})
        };
        assert!(sign(base()).is_ok());
        let mut v = base();
        v["env"] = serde_json::json!({"lower": "x"});
        assert!(sign(v).is_err());
        let mut v = base();
        v["env"] = serde_json::json!({"A": "x\nB=y"});
        assert!(sign(v).is_err());
        let mut v = base();
        v["first_login"] = serde_json::json!({"username": "admin", "password": "NOT_A_SECRET"});
        assert!(sign(v).is_err());
        let mut v = base();
        v["schema"] = serde_json::json!(2);
        assert!(sign(v).is_err());
        let mut v = base();
        v["version"] = serde_json::json!("../1");
        assert!(sign(v).is_err());
    }

    #[test]
    fn fingerprint_matches_the_tools() {
        // tools/app-release.sh fingerprint, for the same key
        assert_eq!(fingerprint("nt5JS7eZl6BdZhM0wFbVrkukSWJDkUooK/TXq8Ayyxo="), "c6e5 8745 1c69 7c6d 0bab");
    }

    #[test]
    fn pins_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("box/app-source.json");
        let source = AppSource {
            link: "github.com/a/b".into(),
            manifest_url: "https://github.com/a/b/releases/latest/download/box-app.json".into(),
            testing_url: None,
            public_key: key_b64(&SigningKey::from_bytes(&[7; 32])),
            name: "b".into(),
        };
        pin(&path, &source).unwrap();
        assert_eq!(load(&path), Some(source.clone()));
        assert!(pin(&path, &source).unwrap_err().contains("reflashing"));
    }
}
