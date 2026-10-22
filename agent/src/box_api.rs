//! The box API for the app, on a Unix socket only the app's user can open
//! (root:app, mode 0660). File permissions are the access control, so there's
//! no login; nothing on the network can reach it. See docs/box-api.md.
//!
//!   GET  /v1/status            versions, what's available, progress
//!   POST /v1/check             look for updates now
//!   POST /v1/os/install        install the OS update the manifest names
//!   POST /v1/app/update        install the app update the manifest names
//!   POST /v1/tailscale/update  update Tailscale if there's a newer release
//!   POST /v1/restart           restart the box (e.g. after an OS install)

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;

use crate::AppState;
use crate::os_update::Install;
use crate::progress::Progress;

#[derive(Serialize)]
struct OsStatus {
    version: String,
    latest: Option<String>,
    update_available: bool,
    install: Install,
}

#[derive(Serialize)]
struct AppStatus {
    version: Option<String>,
    latest: Option<String>,
    update_available: bool,
    progress: Progress,
}

#[derive(Serialize)]
struct TailscaleStatus {
    version: Option<String>,
    backend_state: Option<String>,
    dns_name: Option<String>,
    progress: Progress,
}

#[derive(Serialize)]
struct BoxStatus {
    os: OsStatus,
    app: AppStatus,
    tailscale: TailscaleStatus,
    /// Unix time of the last successful update check
    checked_at: Option<u64>,
    check_error: Option<String>,
    checking: bool,
    /// An OS update is installed and waits for a restart
    restart_needed: bool,
    /// Night-time installs of system and Tailscale updates
    auto_updates: crate::autoupdate::Settings,
    last_night: Option<crate::autoupdate::LastRun>,
}

async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let s = crate::status::gather().await;
    let check = state.manifests.state();
    let latest_os = check.latest.as_ref().and_then(|m| m.os.as_ref()).map(|os| os.version.clone());
    let latest_app = check.latest.as_ref().and_then(|m| m.app.as_ref()).map(|app| app.version.clone());
    let app_version = state.app_update.installed_version();
    let install = state.os_update.state();
    axum::Json(BoxStatus {
        os: OsStatus {
            update_available: latest_os
                .as_deref()
                .is_some_and(|l| crate::manifest::os_newer(l, &s.os.version)),
            version: s.os.version,
            latest: latest_os,
            install: install.clone(),
        },
        app: AppStatus {
            update_available: match (&latest_app, &app_version) {
                (Some(latest), Some(running)) => crate::app_update::app_newer(latest, running),
                _ => false,
            },
            version: app_version,
            latest: latest_app,
            progress: state.app_update.state(),
        },
        tailscale: TailscaleStatus {
            version: s.tailscale.version,
            backend_state: s.tailscale.backend_state,
            dns_name: s.tailscale.dns_name,
            progress: state.tailscale_update.state(),
        },
        checked_at: check.checked_at,
        check_error: check.error,
        checking: check.checking,
        restart_needed: install == Install::Installed,
        auto_updates: state.autoupdate.settings(),
        last_night: state.autoupdate.last_run(),
    })
}

fn started(ok: bool) -> Response {
    if ok {
        StatusCode::ACCEPTED.into_response()
    } else {
        (StatusCode::CONFLICT, "already running").into_response()
    }
}

fn nothing_to_install() -> Response {
    (StatusCode::NOT_FOUND, "no release known yet; POST /v1/check first").into_response()
}

async fn check(State(state): State<AppState>) -> Response {
    started(state.manifests.check_now())
}

async fn os_install(State(state): State<AppState>) -> Response {
    match state.manifests.state().latest.and_then(|m| m.os) {
        Some(os) => started(state.os_update.start(&os.bundle_url).is_ok()),
        None => nothing_to_install(),
    }
}

async fn app_update(State(state): State<AppState>) -> Response {
    match state.manifests.state().latest.and_then(|m| m.app) {
        Some(app) => started(state.app_update.start(app)),
        None => nothing_to_install(),
    }
}

async fn tailscale_update(State(state): State<AppState>) -> Response {
    started(state.tailscale_update.start())
}

async fn restart(State(state): State<AppState>) -> Response {
    crate::schedule_restart(&state);
    StatusCode::ACCEPTED.into_response()
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/check", post(check))
        .route("/v1/os/install", post(os_install))
        .route("/v1/app/update", post(app_update))
        .route("/v1/tailscale/update", post(tailscale_update))
        .route("/v1/restart", post(restart))
        .with_state(state)
}

/// The group the app runs as, from /etc/group
fn group_id(name: &str) -> Option<u32> {
    let groups = std::fs::read_to_string("/etc/group").ok()?;
    groups.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == name).then(|| fields.nth(1)?.parse().ok())?
    })
}

/// Listens on `path`, readable and writable by root and `group` only.
pub fn listen(path: &Path, group: Option<&str>) -> std::io::Result<tokio::net::UnixListener> {
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    if let Some(group) = group {
        let gid = group_id(group)
            .ok_or_else(|| std::io::Error::other(format!("no group {group:?} for the box API socket")))?;
        std::os::unix::fs::chown(path, None, Some(gid))?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use std::os::unix::fs::MetadataExt;

    /// One HTTP/1.1 request over the Unix socket, the way an app would make it
    async fn request(socket: &Path, method: &str, path: &str) -> (StatusCode, String) {
        let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await.unwrap();
        tokio::spawn(conn);
        let req = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "box")
            .body(Body::empty())
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn status_and_actions_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("run/api.sock");
        let state = crate::tests::test_state(dir.path());
        let listener = listen(&socket, None).unwrap();
        assert_eq!(std::fs::metadata(&socket).unwrap().mode() & 0o777, 0o660);
        tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });

        let (code, body) = request(&socket, "GET", "/v1/status").await;
        assert_eq!(code, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["app"]["version"], serde_json::Value::Null);
        assert_eq!(v["os"]["install"]["state"], "idle");
        assert_eq!(v["restart_needed"], false);
        assert_eq!(v["auto_updates"]["enabled"], true);

        // No manifest yet: nothing to install, and update checks aren't configured in tests
        assert_eq!(request(&socket, "POST", "/v1/os/install").await.0, StatusCode::NOT_FOUND);
        assert_eq!(request(&socket, "POST", "/v1/app/update").await.0, StatusCode::NOT_FOUND);
        assert_eq!(request(&socket, "POST", "/v1/check").await.0, StatusCode::CONFLICT);
        assert_eq!(request(&socket, "POST", "/v1/restart").await.0, StatusCode::ACCEPTED);
        assert_eq!(request(&socket, "GET", "/v1/nope").await.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn finds_group_ids() {
        assert_eq!(group_id("root"), Some(0));
        assert_eq!(group_id("no-such-group-here"), None);
    }
}
