//! box-agent: the appliance's front door on port 80.
//!
//! - `/_system/...`: system pages and JSON API, behind the admin password
//!   (see auth.rs): status, updates, Wi-Fi, Tailscale sign-in
//! - on the tailnet front, everything else is proxied to the user's app, with
//!   a friendly page when the app isn't answering; the home network gets a
//!   pointer to the app's Tailscale address instead (or, on the setup
//!   hotspot, a redirect to the setup page)
//!
//! It lives in the read-only OS image and only changes with OS updates, so a
//! bad app update can never take away the page used to fix it.

mod app_update;
mod auth;
mod autoupdate;
mod box_api;
mod captive_dns;
mod hostname;
mod http;
mod manifest;
mod os_update;
mod pages;
mod progress;
mod proxy;
mod status;
mod tailscale_login;
mod tailscale_serve;
mod tailscale_update;
mod wifi;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Form, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use axum::{Router, middleware};
use serde::Deserialize;

#[derive(Clone)]
struct AppState {
    proxy: proxy::Proxy,
    auth: Arc<auth::Auth>,
    os_update: os_update::OsUpdate,
    tailscale_update: tailscale_update::TailscaleUpdate,
    manifests: manifest::Manifests,
    app_update: app_update::AppUpdate,
    tailscale_serve: tailscale_serve::Shared,
    tailscale_login: tailscale_login::TailscaleLogin,
    wifi: wifi::Wifi,
    /// Command that restarts the box (replaced in tests)
    reboot: Arc<Vec<String>>,
    /// Asks box-recovery.service to erase the box at the next start
    factory_reset_flag: Arc<PathBuf>,
    autoupdate: autoupdate::AutoUpdate,
    namer: hostname::Namer,
}

impl AppState {
    fn new(app: SocketAddr, data_dir: PathBuf) -> Self {
        Self {
            proxy: proxy::Proxy::new(app),
            // On the device the data dir lives on the /data partition
            auth: Arc::new(auth::Auth::new(
                data_dir.clone(),
                data_dir.starts_with("/data").then(|| PathBuf::from("/data")),
            )),
            os_update: os_update::OsUpdate::new("rauc".into()),
            manifests: manifest::Manifests::new(manifest::Config::device()),
            app_update: app_update::AppUpdate::new(app_update::Config::device()),
            tailscale_serve: Default::default(),
            tailscale_update: tailscale_update::TailscaleUpdate::new(tailscale_update::Config::device()),
            tailscale_login: tailscale_login::TailscaleLogin::new("tailscale"),
            wifi: wifi::Wifi::new(wifi::Config::device()),
            reboot: Arc::new(vec!["systemctl".into(), "reboot".into()]),
            factory_reset_flag: Arc::new("/data/.factory-reset".into()),
            autoupdate: autoupdate::AutoUpdate::new(&data_dir),
            namer: hostname::Namer::new(&data_dir),
        }
    }
}

struct Args {
    listen: SocketAddr,
    tailnet_listen: SocketAddr,
    app: SocketAddr,
    data_dir: PathBuf,
    api_socket: PathBuf,
}

const USAGE: &str = "usage: box-agent [--listen ADDR:PORT] [--tailnet-listen ADDR:PORT] [--app ADDR:PORT] [--data-dir DIR] \
     [--api-socket PATH]";

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        listen: "0.0.0.0:80".parse().unwrap(),
        tailnet_listen: "127.0.0.1:8081".parse().unwrap(),
        // kid-phone-server's default
        app: "127.0.0.1:3100".parse().unwrap(),
        data_dir: "/data/agent".into(),
        api_socket: "/run/box/api.sock".into(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let value = it.next().ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))?;
        let bad = |e: &dyn std::fmt::Display| format!("{arg} {value:?}: {e}");
        match arg.as_str() {
            "--listen" => args.listen = value.parse().map_err(|e| bad(&e))?,
            "--tailnet-listen" => args.tailnet_listen = value.parse().map_err(|e| bad(&e))?,
            "--app" => args.app = value.parse().map_err(|e| bad(&e))?,
            "--data-dir" => args.data_dir = value.into(),
            "--api-socket" => args.api_socket = value.into(),
            _ => return Err(format!("unknown argument {arg:?}\n{USAGE}")),
        }
    }
    Ok(args)
}

/// Which way a request came in
#[derive(Clone, Copy, PartialEq, Debug)]
enum Front {
    /// Port 80 on the home network: the box's own pages only
    Lan,
    /// 127.0.0.1:8081, reached only through `tailscale serve` (HTTPS): box pages and the app
    Tailnet,
}

fn router(state: AppState, front: Front) -> Router {
    let system = Router::new()
        .route("/_system", get(|| async { Redirect::permanent("/_system/") }))
        .route("/_system/", get(status_page))
        .route("/_system/login", get(login_page).post(login_submit))
        .route("/_system/logout", post(logout))
        .route("/_system/password", get(password_page).post(password_submit))
        .route("/_system/api/health", get(|| async { "ok" }))
        .route("/_system/api/status", get(status_json))
        .route("/_system/api/os/install", get(os_install_state).post(os_install_api))
        .route("/_system/os/install", post(os_install_form))
        .route("/_system/restart", post(restart))
        .route("/_system/factory-reset", post(factory_reset))
        .route("/_system/name", post(rename))
        .route("/_system/settings/updates", post(update_settings))
        .route("/_system/api/settings/updates", get(update_settings_state))
        .route("/_system/check", post(check_form))
        .route("/_system/os/update", post(os_update_form))
        .route("/_system/api/updates", get(updates_state))
        .route("/_system/api/updates/check", post(check_api))
        .route("/_system/app/update", post(app_update_form))
        .route("/_system/app/initial-password/forget", post(forget_initial_password))
        .route("/_system/api/app/update", get(app_update_state).post(app_update_api))
        .route("/_system/api/tailscale/update", get(tailscale_update_state).post(tailscale_update_api))
        .route("/_system/tailscale/update", post(tailscale_update_form))
        .route("/_system/tailscale/login", post(tailscale_login_form))
        .route("/_system/api/tailscale/login", get(tailscale_login_state))
        .route("/_system/wifi", get(wifi_page))
        .route("/_system/wifi/join", post(wifi_join))
        .route("/_system/wifi/forget", post(wifi_forget))
        .route("/_system/wifi/scan", post(wifi_scan))
        .route("/_system/api/wifi", get(wifi_state))
        // Unknown system paths stay with the agent rather than reaching the app
        .route("/_system/{*rest}", any(|| async { StatusCode::NOT_FOUND }))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::guard));
    match front {
        Front::Lan => Router::new()
            .merge(system)
            .fallback(app_on_tailnet)
            .layer(middleware::from_fn_with_state(state.clone(), tailnet_to_https))
            .with_state(state),
        Front::Tailnet => Router::new().merge(system).fallback(proxy::forward).with_state(state),
    }
}

/// Tailscale's addresses: 100.64.0.0/10 and fd7a:115c:a1e0::/48
fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && b & 0xc0 == 64
        }
        IpAddr::V6(v6) => {
            v6.to_ipv4_mapped().is_some_and(|v4| is_tailscale(v4.into()))
                || v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0]
        }
    }
}

/// Where a tailnet visitor of the home-network front should go instead: port 80
/// also answers over Tailscale (as http://box/ with MagicDNS); once the HTTPS
/// address works, send them there
fn tailnet_https_url(peer: IpAddr, serve: &tailscale_serve::Serve, path: &str) -> Option<String> {
    let name = serve.dns_name.as_deref().filter(|_| serve.configured && is_tailscale(peer))?;
    Some(format!("https://{name}{path}"))
}

async fn tailnet_to_https(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: middleware::Next,
) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
        let serve = state.tailscale_serve.lock().unwrap().clone();
        if let Some(url) = tailnet_https_url(peer.ip(), &serve, path) {
            return (StatusCode::FOUND, [(header::LOCATION, url)]).into_response();
        }
    }
    next.run(req).await
}

async fn app_on_tailnet(State(state): State<AppState>) -> Response {
    if state.wifi.hotspot_active() {
        // Captive portal: phones checking for internet land on the setup page
        let setup = format!("http://{}/_system/", wifi::HOTSPOT_ADDRESS);
        return (StatusCode::FOUND, [(header::LOCATION, setup)]).into_response();
    }
    let s = status::gather().await;
    Html(pages::app_on_tailnet(s.tailscale.dns_name.as_deref())).into_response()
}

async fn status_page(State(state): State<AppState>) -> impl IntoResponse {
    let (install, tailscale, manifest, app) = (
        state.os_update.state(),
        state.tailscale_update.state(),
        state.manifests.state(),
        state.app_update.state(),
    );
    let (app_installed, app_initial_password) =
        (state.app_update.installed_version(), state.app_update.initial_admin_password());
    let serve = state.tailscale_serve.lock().unwrap().clone();
    let (login, wifi) = (state.tailscale_login.state(), state.wifi.view());
    let (auto, last_night) = (state.autoupdate.settings(), state.autoupdate.last_run());
    let zones = state.autoupdate.zones();
    let updates = pages::Updates {
        install: &install,
        tailscale: &tailscale,
        manifest: &manifest,
        checks_configured: state.manifests.configured(),
        app: &app,
        app_installed: app_installed.as_deref(),
        app_initial_password: app_initial_password.as_deref(),
        serve: &serve,
        login: &login,
        wifi: &wifi,
        auto: &auto,
        last_night: last_night.as_ref(),
        zones: &zones,
    };
    Html(pages::status(&status::gather().await, &updates))
}

async fn status_json() -> impl IntoResponse {
    axum::Json(status::gather().await)
}

#[derive(Deserialize)]
struct InstallForm {
    source: String,
}

async fn os_install_state(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(state.os_update.state())
}

/// Starts an install; 202 with the new state, or why it can't start
async fn os_install_api(State(state): State<AppState>, Form(form): Form<InstallForm>) -> Response {
    match state.os_update.start(&form.source) {
        Ok(()) => (StatusCode::ACCEPTED, axum::Json(state.os_update.state())).into_response(),
        Err(os_update::StartError::AlreadyRunning) => {
            (StatusCode::CONFLICT, "an update is already installing").into_response()
        }
        Err(os_update::StartError::BadSource) => {
            (StatusCode::BAD_REQUEST, "not an update address").into_response()
        }
    }
}

async fn check_form(State(state): State<AppState>) -> Redirect {
    state.manifests.check_now();
    Redirect::to("/_system/")
}

async fn check_api(State(state): State<AppState>) -> Response {
    let status = if state.manifests.check_now() { StatusCode::ACCEPTED } else { StatusCode::CONFLICT };
    (status, axum::Json(state.manifests.state())).into_response()
}

async fn updates_state(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(state.manifests.state())
}

/// The app release the manifest names, if any
fn manifest_app(state: &AppState) -> Option<manifest::AppRelease> {
    state.manifests.state().latest.and_then(|m| m.app)
}

async fn app_update_form(State(state): State<AppState>) -> Redirect {
    if let Some(app) = manifest_app(&state) {
        state.app_update.start(app);
    }
    Redirect::to("/_system/")
}

async fn app_update_state(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(state.app_update.state())
}

async fn app_update_api(State(state): State<AppState>) -> Response {
    let Some(app) = manifest_app(&state) else {
        return (StatusCode::CONFLICT, "no app release known yet; check for updates first").into_response();
    };
    let status = if state.app_update.start(app) { StatusCode::ACCEPTED } else { StatusCode::CONFLICT };
    (status, axum::Json(state.app_update.state())).into_response()
}

async fn forget_initial_password(State(state): State<AppState>) -> Redirect {
    state.app_update.forget_initial_admin_password();
    Redirect::to("/_system/")
}

#[derive(Deserialize)]
struct RenameForm {
    name: String,
}

async fn rename(State(state): State<AppState>, Form(form): Form<RenameForm>) -> Response {
    let signed_in = status::gather().await.tailscale.backend_state.as_deref() == Some("Running");
    match state.namer.rename(&form.name, signed_in).await {
        Ok(_) => Redirect::to("/_system/").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Html(pages::message("Not renamed", &e))).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateSettingsForm {
    /// The checkbox: present ("on") when ticked
    #[serde(default)]
    auto: String,
    tz: String,
    /// Checkbox: test versions early
    #[serde(default)]
    testing: String,
}

async fn update_settings(State(state): State<AppState>, Form(form): Form<UpdateSettingsForm>) -> Response {
    let testing = form.testing == "on";
    let was_testing = state.autoupdate.settings().testing;
    match state.autoupdate.save(form.auto == "on", form.tz.trim(), testing) {
        Ok(()) => {
            state.manifests.set_testing(testing);
            if testing != was_testing {
                // Show what's available on the new footing straight away
                state.manifests.check_now();
            }
            Redirect::to("/_system/").into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Html(pages::message("Not saved", &e))).into_response(),
    }
}

async fn update_settings_state(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "settings": state.autoupdate.settings(),
        "last_night": state.autoupdate.last_run(),
    }))
}

/// Waits while `busy()` holds, for up to `limit`
async fn wait_while(busy: impl Fn() -> bool, limit: std::time::Duration) {
    let deadline = std::time::Instant::now() + limit;
    while busy() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Tonight's updates (see autoupdate.rs): check, Tailscale, then the system
/// with a restart into it. Returns what happened, in a sentence or two.
async fn night_updates(state: &AppState) -> String {
    use std::time::Duration;
    let mut notes = Vec::new();
    state.manifests.check_now();
    wait_while(|| state.manifests.state().checking, Duration::from_secs(300)).await;
    if let Some(error) = state.manifests.state().error {
        notes.push(format!("Couldn't check for system updates: {error}"));
    }
    if state.tailscale_update.start() {
        wait_while(|| state.tailscale_update.state().busy(), Duration::from_secs(1800)).await;
        match state.tailscale_update.state() {
            progress::Progress::Updated { version } => notes.push(format!("Updated Tailscale to {version}.")),
            progress::Progress::Failed { error } => notes.push(format!("Tailscale wasn't updated: {error}")),
            _ => {}
        }
    }
    let running = std::fs::read_to_string("/etc/box-version").unwrap_or_default().trim().to_owned();
    let newer = state
        .manifests
        .state()
        .latest
        .and_then(|m| m.os)
        .filter(|os| manifest::os_newer(&os.version, &running));
    let mut restart = state.os_update.state() == os_update::Install::Installed;
    if let Some(os) = newer.filter(|_| !restart)
        && state.os_update.start(&os.bundle_url).is_ok()
    {
        wait_while(
            || matches!(state.os_update.state(), os_update::Install::Running { .. }),
            Duration::from_secs(5400),
        )
        .await;
        match state.os_update.state() {
            os_update::Install::Installed => {
                notes.push(format!("Installed system {}.", os.version));
                restart = true;
            }
            os_update::Install::Failed { error } => {
                notes.push(format!("The system update didn't install: {error}"))
            }
            _ => {}
        }
    }
    if restart {
        notes.push("Restarted to finish.".into());
        schedule_restart(state);
    }
    if notes.is_empty() { "Nothing new.".into() } else { notes.join(" ") }
}

/// Runs the night updates once a night, inside the window
fn spawn_night_updates(state: AppState) {
    tokio::spawn(async move {
        let mut done: Option<String> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            let settings = state.autoupdate.settings();
            let Some((date, minute)) = autoupdate::local_now(settings.timezone.as_deref()).await else {
                continue;
            };
            let start = autoupdate::start_minute(&date);
            if !autoupdate::due(&settings, &date, minute, start, done.as_deref()) {
                continue;
            }
            done = Some(date.clone());
            let summary = night_updates(&state).await;
            state.autoupdate.record(&date, summary);
        }
    });
}

/// Installs the app as soon as a manifest names one, if none is installed yet.
/// Retries hourly after a failure (say, the download broke off).
fn spawn_first_app_install(state: AppState) {
    tokio::spawn(async move {
        let mut last_attempt: Option<std::time::Instant> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if state.app_update.installed_version().is_some() || state.app_update.state().busy() {
                continue;
            }
            if last_attempt.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3600)) {
                continue;
            }
            if let Some(app) = manifest_app(&state) {
                last_attempt = Some(std::time::Instant::now());
                state.app_update.start(app);
            }
        }
    });
}

/// Installs the OS release the manifest names
async fn os_update_form(State(state): State<AppState>) -> Response {
    let bundle = state.manifests.state().latest.and_then(|m| m.os).map(|os| os.bundle_url);
    match bundle {
        Some(url) => {
            // Already installing counts too: the page shows the progress
            let _ = state.os_update.start(&url);
            Redirect::to("/_system/").into_response()
        }
        None => {
            let page = pages::message("No update", "There's no update to install right now.");
            (StatusCode::CONFLICT, Html(page)).into_response()
        }
    }
}

async fn os_install_form(State(state): State<AppState>, Form(form): Form<InstallForm>) -> Response {
    match state.os_update.start(&form.source) {
        Ok(()) | Err(os_update::StartError::AlreadyRunning) => Redirect::to("/_system/").into_response(),
        Err(os_update::StartError::BadSource) => (
            StatusCode::BAD_REQUEST,
            Html(pages::message("Can't install that", "That isn't an update address.")),
        )
            .into_response(),
    }
}

async fn tailscale_update_state(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(state.tailscale_update.state())
}

/// Checks for and installs a newer Tailscale; 202, or 409 if already running
async fn tailscale_update_api(State(state): State<AppState>) -> Response {
    let status = if state.tailscale_update.start() { StatusCode::ACCEPTED } else { StatusCode::CONFLICT };
    (status, axum::Json(state.tailscale_update.state())).into_response()
}

async fn tailscale_update_form(State(state): State<AppState>) -> Redirect {
    state.tailscale_update.start();
    Redirect::to("/_system/")
}

async fn tailscale_login_form(State(state): State<AppState>) -> Redirect {
    state.tailscale_login.start();
    Redirect::to("/_system/")
}

async fn tailscale_login_state(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(state.tailscale_login.state())
}

async fn wifi_page(State(state): State<AppState>) -> impl IntoResponse {
    Html(pages::wifi(&state.wifi.view(), None))
}

async fn wifi_state(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(state.wifi.view())
}

#[derive(Deserialize)]
struct JoinForm {
    ssid: String,
    #[serde(default)]
    password: String,
}

async fn wifi_join(State(state): State<AppState>, Form(form): Form<JoinForm>) -> Response {
    let leaving_hotspot = state.wifi.hotspot_active();
    let problem = match state.wifi.join(form.ssid.clone(), form.password) {
        Ok(()) if leaving_hotspot => return Html(pages::wifi_leaving(&form.ssid)).into_response(),
        Ok(()) => return Redirect::to("/_system/wifi").into_response(),
        Err(wifi::JoinError::Busy) => (StatusCode::CONFLICT, "The box is already joining a network."),
        Err(wifi::JoinError::BadName) => (StatusCode::BAD_REQUEST, "That isn't a Wi-Fi network name."),
        Err(wifi::JoinError::BadPassword) => {
            (StatusCode::BAD_REQUEST, "Wi-Fi passwords have 8 to 63 characters (letters, numbers, symbols).")
        }
    };
    (problem.0, Html(pages::wifi(&state.wifi.view(), Some(problem.1)))).into_response()
}

#[derive(Deserialize)]
struct ForgetForm {
    name: String,
}

async fn wifi_forget(State(state): State<AppState>, Form(form): Form<ForgetForm>) -> Response {
    match state.wifi.forget(&form.name).await {
        Ok(()) => Redirect::to("/_system/wifi").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Html(pages::wifi(&state.wifi.view(), Some(&e)))).into_response(),
    }
}

async fn wifi_scan(State(state): State<AppState>) -> Redirect {
    state.wifi.scan();
    Redirect::to("/_system/wifi")
}

/// Restarts the box a second from now, so the caller still gets its answer.
fn schedule_restart(state: &AppState) {
    let cmd = state.reboot.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if let Err(e) = tokio::process::Command::new(&cmd[0]).args(&cmd[1..]).status().await {
            eprintln!("restarting: {e}");
        }
    });
}

async fn restart(State(state): State<AppState>) -> impl IntoResponse {
    schedule_restart(&state);
    Html(pages::restarting())
}

#[derive(Deserialize)]
struct FactoryResetForm {
    #[serde(default)]
    confirm: String,
}

/// Erases the box: box-recovery.service does the erasing at the next start,
/// after the box has signed out of Tailscale
async fn factory_reset(State(state): State<AppState>, Form(form): Form<FactoryResetForm>) -> Response {
    if form.confirm != "yes" {
        let page = pages::message("Nothing erased", "Tick the box to confirm, then try again.");
        return (StatusCode::BAD_REQUEST, Html(page)).into_response();
    }
    if let Err(e) = std::fs::write(state.factory_reset_flag.as_path(), b"requested from the box's page\n") {
        eprintln!("factory reset: {e}");
        let page = pages::message("Nothing erased", "The box couldn't start erasing. Try again.");
        return (StatusCode::INTERNAL_SERVER_ERROR, Html(page)).into_response();
    }
    eprintln!("factory reset requested; signing out of Tailscale and restarting");
    let s = state.clone();
    tokio::spawn(async move {
        s.tailscale_login.logout().await;
        schedule_restart(&s);
    });
    Html(pages::erasing()).into_response()
}

/// Response that logs the browser in and continues to the status page
fn signed_in(token: &str) -> Response {
    ([(header::SET_COOKIE, auth::session_cookie(token))], Redirect::to("/_system/")).into_response()
}

async fn login_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match auth::session_token(&headers) {
        Some(token) if state.auth.session_valid(&token) => Redirect::to("/_system/").into_response(),
        _ => Html(pages::login(None)).into_response(),
    }
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn login_submit(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Form(form): Form<LoginForm>,
) -> Response {
    match state.auth.login(peer.ip(), form.password).await {
        Ok(token) => signed_in(&token),
        Err(auth::LoginError::Wrong) => {
            (StatusCode::UNAUTHORIZED, Html(pages::login(Some("That password isn't right.")))).into_response()
        }
        Err(auth::LoginError::LockedOut(wait)) => {
            let msg = format!("Too many tries. Wait {} seconds and try again.", wait.as_secs() + 1);
            (StatusCode::TOO_MANY_REQUESTS, Html(pages::login(Some(&msg)))).into_response()
        }
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = auth::session_token(&headers) {
        state.auth.end_session(&token);
    }
    ([(header::SET_COOKIE, auth::clear_cookie())], Redirect::to("/_system/login")).into_response()
}

async fn password_page(State(state): State<AppState>) -> impl IntoResponse {
    Html(pages::set_password(!state.auth.has_password(), None))
}

#[derive(Deserialize)]
struct PasswordForm {
    #[serde(default)]
    current: String,
    password: String,
    confirm: String,
}

async fn password_submit(State(state): State<AppState>, Form(form): Form<PasswordForm>) -> Response {
    let first_run = !state.auth.has_password();
    let problem = if form.password.chars().count() < auth::MIN_PASSWORD_LEN {
        Some(format!("The new password needs at least {} characters.", auth::MIN_PASSWORD_LEN))
    } else if form.password != form.confirm {
        Some("The two new passwords don't match.".to_owned())
    } else if !first_run && !state.auth.password_matches(form.current).await {
        Some("The current password isn't right.".to_owned())
    } else {
        None
    };
    if let Some(problem) = problem {
        return (StatusCode::BAD_REQUEST, Html(pages::set_password(first_run, Some(&problem))))
            .into_response();
    }
    if let Err(e) = state.auth.set_password(form.password).await {
        eprintln!("saving the admin password: {e}");
        let msg = "The password couldn't be saved. Try again.";
        return (StatusCode::INTERNAL_SERVER_ERROR, Html(pages::set_password(first_run, Some(msg))))
            .into_response();
    }
    signed_in(&state.auth.new_session())
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let state = AppState::new(args.app, args.data_dir);
    state.manifests.set_testing(state.autoupdate.settings().testing);
    state.manifests.spawn_periodic();
    spawn_first_app_install(state.clone());
    spawn_night_updates(state.clone());
    state.wifi.spawn();
    captive_dns::spawn(wifi::HOTSPOT_ADDRESS);
    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("can't listen on {}: {e}", args.listen);
            std::process::exit(1);
        }
    };
    // The app's way in; a failure here mustn't take down the system pages
    match box_api::listen(&args.api_socket, Some("app")) {
        Ok(api) => {
            let api_router = box_api::router(state.clone());
            tokio::spawn(async move {
                if let Err(e) = axum::serve(api, api_router).await {
                    eprintln!("box API server error: {e}");
                }
            });
        }
        Err(e) => eprintln!("box API not available at {}: {e}", args.api_socket.display()),
    }
    match tokio::net::TcpListener::bind(args.tailnet_listen).await {
        Ok(tailnet) => {
            let service =
                router(state.clone(), Front::Tailnet).into_make_service_with_connect_info::<SocketAddr>();
            tokio::spawn(async move {
                if let Err(e) = axum::serve(tailnet, service).await {
                    eprintln!("tailnet server error: {e}");
                }
            });
        }
        Err(e) => eprintln!("can't listen on {} for tailscale serve: {e}", args.tailnet_listen),
    }
    tailscale_serve::spawn(state.tailscale_serve.clone(), args.tailnet_listen);
    eprintln!(
        "box-agent listening on {} (home network) and {} (tailnet), app at {}",
        args.listen, args.tailnet_listen, args.app
    );
    let app = router(state, Front::Lan).into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    struct Agent {
        addr: SocketAddr,
        _data: tempfile::TempDir,
    }

    /// Agent state for tests: data in `dir`, and restarting does nothing.
    pub(crate) fn test_state(dir: &std::path::Path) -> AppState {
        let mut state = AppState::new("127.0.0.1:9".parse().unwrap(), dir.to_owned());
        state.reboot = Arc::new(vec!["true".into()]);
        // No iwd or Tailscale of this machine's
        state.wifi = wifi::Wifi::new(wifi::Config {
            busctl: "false".into(),
            systemctl: "false".into(),
            ..wifi::Config::device()
        });
        state.tailscale_login = tailscale_login::TailscaleLogin::new("false");
        state.factory_reset_flag = Arc::new(dir.join(".factory-reset"));
        state.namer = hostname::Namer::inert(dir);
        state
    }

    /// Starts the agent's tailnet front on a free port, proxying to `app`, with an empty data dir.
    async fn start_agent(app: SocketAddr) -> Agent {
        start_front(app, Front::Tailnet).await
    }

    async fn start_front(app: SocketAddr, front: Front) -> Agent {
        start_with_wifi(app, front, wifi::View::default()).await
    }

    /// An agent whose Wi-Fi looks like `wifi`
    async fn start_with_wifi(app: SocketAddr, front: Front, wifi: wifi::View) -> Agent {
        let data = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut state = test_state(data.path());
        state.proxy = proxy::Proxy::new(app);
        state.wifi.set_view(wifi);
        let service = router(state, front).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
        Agent { addr, _data: data }
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: String,
    }

    impl Reply {
        fn location(&self) -> &str {
            self.headers.get("location").map(|v| v.to_str().unwrap()).unwrap_or("")
        }
        /// "box_session=..." from Set-Cookie, ready to send back
        fn cookie(&self) -> String {
            let set = self.headers.get("set-cookie").unwrap().to_str().unwrap();
            set.split(';').next().unwrap().to_owned()
        }
    }

    /// Sends a request the way a browser on this box's own page would.
    async fn send(
        agent: &Agent,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        form: Option<&str>,
    ) -> Reply {
        let mut req = hyper::Request::builder()
            .method(method)
            .uri(format!("http://{}{path}", agent.addr))
            .header("host", agent.addr.to_string());
        if let Some(c) = cookie {
            req = req.header("cookie", c);
        }
        let body = match form {
            Some(f) => {
                req = req
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("origin", format!("http://{}", agent.addr));
                Body::from(f.to_owned())
            }
            None => Body::empty(),
        };
        let client = Client::builder(TokioExecutor::new()).build_http::<Body>();
        let resp = client.request(req.body(body).unwrap()).await.unwrap();
        let (parts, body) = resp.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    async fn fetch(agent: &Agent, path: &str, cookie: Option<&str>) -> Reply {
        send(agent, "GET", path, cookie, None).await
    }

    async fn submit(agent: &Agent, path: &str, cookie: Option<&str>, form: &str) -> Reply {
        send(agent, "POST", path, cookie, Some(form)).await
    }

    /// Sets the first password; returns the session cookie it logs in with.
    async fn set_up(agent: &Agent) -> String {
        let r =
            submit(agent, "/_system/password", None, "password=correct+horse&confirm=correct+horse").await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
        assert_eq!(r.location(), "/_system/");
        r.cookie()
    }

    /// A free port with nothing listening on it
    async fn unused_port() -> SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    }

    #[tokio::test]
    async fn health_is_public() {
        let agent = start_agent(unused_port().await).await;
        let r = fetch(&agent, "/_system/api/health", None).await;
        assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, "ok"));
    }

    #[tokio::test]
    async fn first_run_asks_for_a_password() {
        let agent = start_agent(unused_port().await).await;
        assert_eq!(fetch(&agent, "/_system/", None).await.location(), "/_system/password");
        assert_eq!(fetch(&agent, "/_system/api/status", None).await.status, StatusCode::UNAUTHORIZED);
        let r = fetch(&agent, "/_system/password", None).await;
        assert!(r.body.contains("Choose a password") && !r.body.contains("Current password"));
    }

    #[tokio::test]
    async fn rejects_weak_or_mismatched_first_password() {
        let agent = start_agent(unused_port().await).await;
        let r = submit(&agent, "/_system/password", None, "password=short&confirm=short").await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        let r =
            submit(&agent, "/_system/password", None, "password=correct+horse&confirm=correct+hors").await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(r.body.contains("don&#39;t match"));
    }

    #[tokio::test]
    async fn login_logout_flow() {
        let agent = start_agent(unused_port().await).await;
        let setup_cookie = set_up(&agent).await;
        let r = fetch(&agent, "/_system/api/status", Some(&setup_cookie)).await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.body.contains("\"tailscale\""));

        // A fresh browser has to sign in
        assert_eq!(fetch(&agent, "/_system/", None).await.location(), "/_system/login");
        let r = submit(&agent, "/_system/login", None, "password=wrong+guess").await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        let r = submit(&agent, "/_system/login", None, "password=correct+horse").await;
        assert_eq!(r.location(), "/_system/");
        let cookie = r.cookie();
        let r = fetch(&agent, "/_system/", Some(&cookie)).await;
        assert!(r.body.contains("Remote access"), "{}", r.body);

        let r = submit(&agent, "/_system/logout", Some(&cookie), "").await;
        assert_eq!(r.location(), "/_system/login");
        assert_eq!(fetch(&agent, "/_system/", Some(&cookie)).await.location(), "/_system/login");
    }

    #[tokio::test]
    async fn changing_the_password_needs_the_current_one() {
        let agent = start_agent(unused_port().await).await;
        let cookie = set_up(&agent).await;
        // Without a session it's the login page, not the form
        let r = submit(&agent, "/_system/password", None, "password=new+password&confirm=new+password").await;
        assert_eq!(r.location(), "/_system/login");
        let form = "current=nope&password=new+password&confirm=new+password";
        assert_eq!(
            submit(&agent, "/_system/password", Some(&cookie), form).await.status,
            StatusCode::BAD_REQUEST
        );
        let form = "current=correct+horse&password=new+password&confirm=new+password";
        assert_eq!(submit(&agent, "/_system/password", Some(&cookie), form).await.location(), "/_system/");
        let r = submit(&agent, "/_system/login", None, "password=new+password").await;
        assert_eq!(r.location(), "/_system/");
    }

    #[tokio::test]
    async fn cross_site_posts_are_refused() {
        let agent = start_agent(unused_port().await).await;
        let cookie = set_up(&agent).await;
        let req = hyper::Request::post(format!("http://{}/_system/logout", agent.addr))
            .header("host", agent.addr.to_string())
            .header("origin", "http://evil.example")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let client = Client::builder(TokioExecutor::new()).build_http::<Body>();
        assert_eq!(client.request(req).await.unwrap().status(), StatusCode::FORBIDDEN);
        assert_eq!(fetch(&agent, "/_system/api/status", Some(&cookie)).await.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn os_install_api_needs_login_and_a_valid_source() {
        let agent = start_agent(unused_port().await).await;
        let cookie = set_up(&agent).await;
        let r =
            submit(&agent, "/_system/api/os/install", None, "source=https%3A%2F%2Fx.example%2Fa.raucb").await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        let r = submit(&agent, "/_system/api/os/install", Some(&cookie), "source=%2Fetc%2Fshadow").await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        let r = fetch(&agent, "/_system/api/os/install", Some(&cookie)).await;
        assert_eq!(r.body, r#"{"state":"idle"}"#);
    }

    #[tokio::test]
    async fn restart_answers_before_restarting() {
        let agent = start_agent(unused_port().await).await;
        let cookie = set_up(&agent).await;
        let r = submit(&agent, "/_system/restart", Some(&cookie), "").await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.body.contains("Restarting"));
    }

    #[tokio::test]
    async fn factory_reset_needs_confirmation() {
        let agent = start_agent(unused_port().await).await;
        let cookie = set_up(&agent).await;
        let flag = agent._data.path().join(".factory-reset");
        let r = submit(&agent, "/_system/factory-reset", None, "confirm=yes").await;
        assert_eq!(r.location(), "/_system/login", "needs the password");
        assert_eq!(
            submit(&agent, "/_system/factory-reset", Some(&cookie), "").await.status,
            StatusCode::BAD_REQUEST
        );
        assert!(!flag.exists());
        let r = submit(&agent, "/_system/factory-reset", Some(&cookie), "confirm=yes").await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.body.contains("Erasing") && flag.exists(), "{}", r.body);
    }

    #[tokio::test]
    async fn saves_automatic_update_settings() {
        let agent = start_agent(unused_port().await).await;
        let cookie = set_up(&agent).await;
        let r = fetch(&agent, "/_system/", Some(&cookie)).await;
        assert!(r.body.contains("Automatic updates") && r.body.contains("name=\"tz\""), "{}", r.body);
        let r = submit(&agent, "/_system/settings/updates", Some(&cookie), "tz=..%2Fetc%2Fpasswd").await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        // Unticked checkbox: not sent at all
        let r = submit(&agent, "/_system/settings/updates", Some(&cookie), "tz=UTC").await;
        assert_eq!(r.location(), "/_system/");
        let r = fetch(&agent, "/_system/api/settings/updates", Some(&cookie)).await;
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["settings"], serde_json::json!({"enabled": false, "timezone": "UTC", "testing": false}));
        let r = submit(&agent, "/_system/settings/updates", Some(&cookie), "auto=on&tz=UTC&testing=on").await;
        assert_eq!(r.location(), "/_system/");
        let r = fetch(&agent, "/_system/api/settings/updates", Some(&cookie)).await;
        assert!(r.body.contains("\"testing\":true"), "{}", r.body);
    }

    #[tokio::test]
    async fn renaming_checks_the_name() {
        let agent = start_agent(unused_port().await).await;
        let cookie = set_up(&agent).await;
        let r = fetch(&agent, "/_system/", Some(&cookie)).await;
        assert!(r.body.contains("action=\"/_system/name\""), "{}", r.body);
        let r = submit(&agent, "/_system/name", Some(&cookie), "name=%21%21%21").await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        let r = submit(&agent, "/_system/name", Some(&cookie), "name=Kitchen+Box").await;
        assert_eq!(r.location(), "/_system/");
        let saved = std::fs::read_to_string(agent._data.path().join("hostname")).unwrap();
        assert_eq!(saved, "kitchen-box\n");
    }

    #[tokio::test]
    async fn unknown_system_paths_stay_with_the_agent() {
        let agent = start_agent(unused_port().await).await;
        let cookie = set_up(&agent).await;
        assert_eq!(fetch(&agent, "/_system/nope", Some(&cookie)).await.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn proxies_to_the_app_without_login() {
        let app_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_addr = app_listener.local_addr().unwrap();
        let app = Router::new().route(
            "/hello",
            get(|headers: HeaderMap, uri: axum::http::Uri| async move {
                let fwd = headers.get("x-forwarded-for").map(|v| v.to_str().unwrap().to_owned());
                ([("x-app", "yes")], format!("{} {}", uri, fwd.unwrap_or_default()))
            }),
        );
        tokio::spawn(async move { axum::serve(app_listener, app).await.unwrap() });

        let agent = start_agent(app_addr).await;
        let r = fetch(&agent, "/hello?x=1", None).await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.headers.get("x-app").unwrap(), "yes");
        assert_eq!(r.body, "/hello?x=1 127.0.0.1");
    }

    #[tokio::test]
    async fn home_network_gets_box_pages_but_not_the_app() {
        let app_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_addr = app_listener.local_addr().unwrap();
        let app = Router::new().route("/devices", get(|| async { "secret app page" }));
        tokio::spawn(async move { axum::serve(app_listener, app).await.unwrap() });

        let lan = start_front(app_addr, Front::Lan).await;
        let r = fetch(&lan, "/devices", None).await;
        assert!(!r.body.contains("secret app page"), "the app must not be reachable from the LAN");
        assert!(r.body.contains("Tailscale"), "{}", r.body);
        assert_eq!(fetch(&lan, "/_system/api/health", None).await.body, "ok");

        let tailnet = start_front(app_addr, Front::Tailnet).await;
        assert_eq!(fetch(&tailnet, "/devices", None).await.body, "secret app page");
    }

    #[tokio::test]
    async fn tailnet_front_tells_the_app_it_is_https() {
        let app_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_addr = app_listener.local_addr().unwrap();
        let app = Router::new().route(
            "/proto",
            get(|headers: HeaderMap| async move {
                headers.get("x-forwarded-proto").map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default()
            }),
        );
        tokio::spawn(async move { axum::serve(app_listener, app).await.unwrap() });
        let tailnet = start_agent(app_addr).await;
        assert_eq!(fetch(&tailnet, "/proto", None).await.body, "https");
    }

    #[tokio::test]
    async fn friendly_page_when_app_is_down() {
        let agent = start_agent(unused_port().await).await;
        let r = fetch(&agent, "/anything", None).await;
        assert_eq!(r.status, StatusCode::BAD_GATEWAY);
        assert!(r.headers.get("content-type").unwrap().to_str().unwrap().starts_with("text/html"));
        assert!(r.body.contains("/_system/"), "page links to the system page: {}", r.body);
    }

    fn hotspot_view() -> wifi::View {
        wifi::View {
            available: true,
            hotspot: Some(wifi::Hotspot {
                ssid: "Box-Setup-ABCD".into(),
                password: "boxsetup".into(),
                setup: true,
            }),
            networks: vec![wifi::Visible {
                name: "Home <5G>".into(),
                kind: "psk".into(),
                signal: -55,
                saved: false,
                connected: false,
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn setup_hotspot_sends_phones_to_the_setup_page() {
        let lan = start_with_wifi(unused_port().await, Front::Lan, hotspot_view()).await;
        // What Android and iOS fetch to see whether the network has internet
        for path in ["/generate_204", "/hotspot-detect.html"] {
            let r = fetch(&lan, path, None).await;
            assert_eq!(
                (r.status, r.location()),
                (StatusCode::FOUND, "http://192.168.4.1/_system/"),
                "{path}"
            );
        }
        assert_eq!(fetch(&lan, "/_system/api/health", None).await.body, "ok");
        // Without the hotspot, the home network gets the pointer to Tailscale
        let home = start_front(unused_port().await, Front::Lan).await;
        assert_eq!(fetch(&home, "/generate_204", None).await.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn wifi_setup_needs_the_password_and_checks_input() {
        let lan = start_with_wifi(unused_port().await, Front::Lan, hotspot_view()).await;
        // First the admin password, then everything else
        assert_eq!(fetch(&lan, "/_system/wifi", None).await.location(), "/_system/password");
        let cookie = set_up(&lan).await;
        let r = fetch(&lan, "/_system/", Some(&cookie)).await;
        assert!(r.body.contains("isn't on your Wi-Fi yet") && r.body.contains("/_system/wifi"), "{}", r.body);
        assert!(
            r.body.contains("Connect the box to your Wi-Fi first"),
            "no Tailscale sign-in without internet"
        );
        let r = fetch(&lan, "/_system/wifi", Some(&cookie)).await;
        assert!(r.body.contains("Home &lt;5G&gt;") && r.body.contains("Box-Setup-ABCD"), "{}", r.body);
        let r = fetch(&lan, "/_system/api/wifi", Some(&cookie)).await;
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["hotspot"]["ssid"], "Box-Setup-ABCD");

        let r = submit(&lan, "/_system/wifi/join", Some(&cookie), "ssid=Home&password=short").await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(r.body.contains("8 to 63 characters"));
        let r = submit(&lan, "/_system/wifi/join", Some(&cookie), "ssid=&password=").await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        // A real join leaves the hotspot: the page says what to do next
        let r = submit(&lan, "/_system/wifi/join", Some(&cookie), "ssid=Home+%3C5G%3E&password=long+enough")
            .await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.body.contains("Connect this phone back to <b>Home &lt;5G&gt;</b>"), "{}", r.body);
        let r = submit(&lan, "/_system/wifi/join", Some(&cookie), "ssid=Other&password=long+enough").await;
        assert_eq!(r.status, StatusCode::CONFLICT, "one join at a time");
    }

    #[test]
    fn tailnet_visitors_go_to_https() {
        let serve = tailscale_serve::Serve {
            configured: true,
            error: None,
            dns_name: Some("box.tail1234.ts.net".into()),
        };
        let ts: IpAddr = "100.101.102.103".parse().unwrap();
        assert_eq!(
            tailnet_https_url(ts, &serve, "/_system/?x=1").as_deref(),
            Some("https://box.tail1234.ts.net/_system/?x=1")
        );
        assert!(tailnet_https_url("fd7a:115c:a1e0::5".parse().unwrap(), &serve, "/").is_some());
        // The home network stays on the box's own address
        assert!(tailnet_https_url("192.168.1.20".parse().unwrap(), &serve, "/").is_none());
        assert!(tailnet_https_url("100.128.0.1".parse().unwrap(), &serve, "/").is_none());
        // Until HTTPS works, tailnet visitors are served here
        let no_https = tailscale_serve::Serve { configured: false, ..serve };
        assert!(tailnet_https_url(ts, &no_https, "/").is_none());
    }

    #[tokio::test]
    async fn system_redirects_to_slash() {
        let agent = start_agent(unused_port().await).await;
        let r = fetch(&agent, "/_system", None).await;
        assert_eq!((r.status, r.location()), (StatusCode::PERMANENT_REDIRECT, "/_system/"));
    }
}
