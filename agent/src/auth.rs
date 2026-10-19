//! Admin password, login sessions, and the checks every /_system request goes
//! through.
//!
//! - The password is stored as an Argon2id hash in `<data dir>/admin-password`.
//!   Until one is set, the only page is "choose a password".
//! - Logging in creates a random session token, sent as an HttpOnly,
//!   SameSite=Strict cookie scoped to /_system. Sessions live in memory, so
//!   restarting the agent logs everyone out.
//! - Repeated wrong passwords from one address lock it out for a while.
//! - Form posts must come from this box's own pages (Origin/Referer check).
//! - Requests from localhost get no special trust: `tailscale serve` forwards
//!   remote visitors from 127.0.0.1.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::AppState;

pub const COOKIE: &str = "box_session";
const SESSION_LIFETIME: Duration = Duration::from_secs(30 * 24 * 3600);
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
pub const MIN_PASSWORD_LEN: usize = 8;

pub struct Auth {
    password_file: PathBuf,
    sessions: Mutex<HashMap<String, Instant>>,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

pub enum LoginError {
    Wrong,
    LockedOut(Duration),
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("reading /dev/urandom");
    bytes
}

fn random_hex<const N: usize>() -> String {
    random_bytes::<N>().iter().map(|b| format!("{b:02x}")).collect()
}

/// OWASP's minimum for Argon2id; about half a second on a Pi Zero 2 W
fn argon2() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(19 * 1024, 2, 1, None).unwrap())
}

fn hash_password(password: &str) -> String {
    let salt = SaltString::encode_b64(&random_bytes::<16>()).expect("16-byte salt");
    argon2().hash_password(password.as_bytes(), &salt).expect("hashing password").to_string()
}

impl Auth {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            password_file: data_dir.join("admin-password"),
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    pub fn has_password(&self) -> bool {
        self.password_file.exists()
    }

    /// Replaces the password and logs out every session.
    pub async fn set_password(&self, password: String) -> std::io::Result<()> {
        let hash = tokio::task::spawn_blocking(move || hash_password(&password)).await.unwrap();
        let dir = self.password_file.parent().unwrap().to_owned();
        let tmp = dir.join(".admin-password.tmp");
        std::fs::create_dir_all(&dir)?;
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(hash.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.password_file)?;
        self.sessions.lock().unwrap().clear();
        Ok(())
    }

    /// Checks the password without counting failures (for "change password").
    pub async fn password_matches(&self, password: String) -> bool {
        let Ok(stored) = std::fs::read_to_string(&self.password_file) else {
            return false;
        };
        tokio::task::spawn_blocking(move || {
            PasswordHash::new(stored.trim())
                .map(|hash| argon2().verify_password(password.as_bytes(), &hash).is_ok())
                .unwrap_or(false)
        })
        .await
        .unwrap()
    }

    /// Checks a login attempt; returns a new session token on success.
    pub async fn login(&self, ip: IpAddr, password: String) -> Result<String, LoginError> {
        if let Some((count, last)) = self.failures.lock().unwrap().get(&ip)
            && *count >= MAX_FAILURES
            && last.elapsed() < LOCKOUT
        {
            return Err(LoginError::LockedOut(LOCKOUT - last.elapsed()));
        }
        if !self.password_matches(password).await {
            let mut failures = self.failures.lock().unwrap();
            failures.retain(|_, (_, last)| last.elapsed() < LOCKOUT);
            let entry = failures.entry(ip).or_insert((0, Instant::now()));
            *entry = (entry.0 + 1, Instant::now());
            return Err(LoginError::Wrong);
        }
        self.failures.lock().unwrap().remove(&ip);
        Ok(self.new_session())
    }

    pub fn new_session(&self) -> String {
        let token = random_hex::<32>();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, expires| *expires > Instant::now());
        sessions.insert(token.clone(), Instant::now() + SESSION_LIFETIME);
        token
    }

    pub fn session_valid(&self, token: &str) -> bool {
        self.sessions.lock().unwrap().get(token).is_some_and(|expires| *expires > Instant::now())
    }

    pub fn end_session(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }
}

/// The session token from the request's cookies, if any
pub fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, value)| value.to_owned())
}

pub fn session_cookie(token: &str) -> String {
    format!(
        "{COOKIE}={token}; Path=/_system; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_LIFETIME.as_secs()
    )
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/_system; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// True if a state-changing request came from a page served by this box:
/// browsers send Origin (or at least Referer) on form posts.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let source = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|v| v.to_str().ok());
    match source {
        Some(url) => {
            let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"));
            rest.map(|r| r.split('/').next() == Some(host)).unwrap_or(false)
        }
        None => false,
    }
}

/// Paths reachable without logging in (the bare /_system only redirects)
fn is_public(path: &str) -> bool {
    matches!(path, "/_system" | "/_system/api/health" | "/_system/login")
}

/// Runs in front of every /_system route.
pub async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let is_api = path.starts_with("/_system/api/");

    if req.method() != Method::GET && req.method() != Method::HEAD && !same_origin(req.headers()) {
        return (StatusCode::FORBIDDEN, "Request did not come from this box's own pages").into_response();
    }

    let deny = |status: StatusCode, page: &'static str| {
        if is_api { status.into_response() } else { Redirect::to(page).into_response() }
    };
    if !state.auth.has_password() {
        // First run: choosing a password is the only thing to do
        return if matches!(path.as_str(), "/_system" | "/_system/password" | "/_system/api/health") {
            next.run(req).await
        } else {
            deny(StatusCode::UNAUTHORIZED, "/_system/password")
        };
    }
    if is_public(&path) {
        return next.run(req).await;
    }
    match session_token(req.headers()) {
        Some(token) if state.auth.session_valid(&token) => next.run(req).await,
        _ => deny(StatusCode::UNAUTHORIZED, "/_system/login"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn origin_check() {
        assert!(same_origin(&headers(&[("host", "box.local"), ("origin", "http://box.local")])));
        assert!(same_origin(&headers(&[("host", "box.local"), ("referer", "http://box.local/_system/")])));
        assert!(!same_origin(&headers(&[("host", "box.local"), ("origin", "http://evil.example")])));
        assert!(!same_origin(&headers(&[("host", "box.local"), ("origin", "http://box.local.evil.example")])));
        assert!(!same_origin(&headers(&[("host", "box.local")])));
    }

    #[test]
    fn finds_session_cookie_among_others() {
        let h = headers(&[("cookie", "a=1; box_session=abc123; b=2")]);
        assert_eq!(session_token(&h).as_deref(), Some("abc123"));
        assert_eq!(session_token(&headers(&[("cookie", "box_sessionx=1")])), None);
    }

    #[tokio::test]
    async fn password_and_lockout() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::new(dir.path().to_owned());
        assert!(!auth.has_password());
        auth.set_password("correct horse".into()).await.unwrap();
        let ip: IpAddr = "192.168.0.9".parse().unwrap();

        let token = auth.login(ip, "correct horse".into()).await.ok().unwrap();
        assert!(auth.session_valid(&token));
        for _ in 0..MAX_FAILURES {
            assert!(matches!(auth.login(ip, "nope".into()).await, Err(LoginError::Wrong)));
        }
        assert!(matches!(auth.login(ip, "correct horse".into()).await, Err(LoginError::LockedOut(_))));
        // Other addresses aren't affected
        assert!(auth.login("192.168.0.10".parse().unwrap(), "correct horse".into()).await.is_ok());

        auth.set_password("battery staple".into()).await.unwrap();
        assert!(!auth.session_valid(&token), "changing the password logs everyone out");
    }
}
