//! box-agent: the appliance's front door on port 80.
//!
//! - `/_system/...`: system pages and JSON API, behind the admin password
//!   (see auth.rs). Status for now; updates, Wi-Fi and setup come later.
//! - everything else is proxied to the user's app on 127.0.0.1:8080, with a
//!   friendly page when the app isn't answering
//!
//! It lives in the read-only OS image and only changes with OS updates, so a
//! bad app update can never take away the page used to fix it.

mod auth;
mod pages;
mod proxy;
mod status;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Form, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use axum::{Router, middleware};
use serde::Deserialize;

#[derive(Clone)]
struct AppState {
    proxy: proxy::Proxy,
    auth: Arc<auth::Auth>,
}

struct Args {
    listen: SocketAddr,
    app: SocketAddr,
    data_dir: PathBuf,
}

const USAGE: &str = "usage: box-agent [--listen ADDR:PORT] [--app ADDR:PORT] [--data-dir DIR]";

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        listen: "0.0.0.0:80".parse().unwrap(),
        app: "127.0.0.1:8080".parse().unwrap(),
        data_dir: "/data/agent".into(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let value = it.next().ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))?;
        let bad = |e: &dyn std::fmt::Display| format!("{arg} {value:?}: {e}");
        match arg.as_str() {
            "--listen" => args.listen = value.parse().map_err(|e| bad(&e))?,
            "--app" => args.app = value.parse().map_err(|e| bad(&e))?,
            "--data-dir" => args.data_dir = value.into(),
            _ => return Err(format!("unknown argument {arg:?}\n{USAGE}")),
        }
    }
    Ok(args)
}

fn router(state: AppState) -> Router {
    let system = Router::new()
        .route("/_system", get(|| async { Redirect::permanent("/_system/") }))
        .route("/_system/", get(status_page))
        .route("/_system/login", get(login_page).post(login_submit))
        .route("/_system/logout", post(logout))
        .route("/_system/password", get(password_page).post(password_submit))
        .route("/_system/api/health", get(|| async { "ok" }))
        .route("/_system/api/status", get(status_json))
        // Unknown system paths stay with the agent rather than reaching the app
        .route("/_system/{*rest}", any(|| async { StatusCode::NOT_FOUND }))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::guard));
    Router::new().merge(system).fallback(proxy::forward).with_state(state)
}

async fn status_page() -> impl IntoResponse {
    Html(pages::status(&status::gather().await))
}

async fn status_json() -> impl IntoResponse {
    axum::Json(status::gather().await)
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
        return (StatusCode::BAD_REQUEST, Html(pages::set_password(first_run, Some(&problem)))).into_response();
    }
    if let Err(e) = state.auth.set_password(form.password).await {
        eprintln!("saving the admin password: {e}");
        let msg = "The password couldn't be saved. Try again.";
        return (StatusCode::INTERNAL_SERVER_ERROR, Html(pages::set_password(first_run, Some(msg)))).into_response();
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
    let state = AppState {
        proxy: proxy::Proxy::new(args.app),
        auth: Arc::new(auth::Auth::new(args.data_dir)),
    };
    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("can't listen on {}: {e}", args.listen);
            std::process::exit(1);
        }
    };
    eprintln!("box-agent listening on {}, app at {}", args.listen, args.app);
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
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

    /// Starts the agent on a free port, proxying to `app`, with an empty data dir.
    async fn start_agent(app: SocketAddr) -> Agent {
        let data = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState {
            proxy: proxy::Proxy::new(app),
            auth: Arc::new(auth::Auth::new(data.path().to_owned())),
        };
        let service = router(state).into_make_service_with_connect_info::<SocketAddr>();
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
    async fn send(agent: &Agent, method: &str, path: &str, cookie: Option<&str>, form: Option<&str>) -> Reply {
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
        Reply { status: parts.status, headers: parts.headers, body: String::from_utf8_lossy(&body).into_owned() }
    }

    async fn fetch(agent: &Agent, path: &str, cookie: Option<&str>) -> Reply {
        send(agent, "GET", path, cookie, None).await
    }

    async fn submit(agent: &Agent, path: &str, cookie: Option<&str>, form: &str) -> Reply {
        send(agent, "POST", path, cookie, Some(form)).await
    }

    /// Sets the first password; returns the session cookie it logs in with.
    async fn set_up(agent: &Agent) -> String {
        let r = submit(agent, "/_system/password", None, "password=correct+horse&confirm=correct+horse").await;
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
        let r = submit(&agent, "/_system/password", None, "password=correct+horse&confirm=correct+hors").await;
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
        assert_eq!(submit(&agent, "/_system/password", Some(&cookie), form).await.status, StatusCode::BAD_REQUEST);
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
    async fn friendly_page_when_app_is_down() {
        let agent = start_agent(unused_port().await).await;
        let r = fetch(&agent, "/anything", None).await;
        assert_eq!(r.status, StatusCode::BAD_GATEWAY);
        assert!(r.headers.get("content-type").unwrap().to_str().unwrap().starts_with("text/html"));
        assert!(r.body.contains("/_system/"), "page links to the system page: {}", r.body);
    }

    #[tokio::test]
    async fn system_redirects_to_slash() {
        let agent = start_agent(unused_port().await).await;
        let r = fetch(&agent, "/_system", None).await;
        assert_eq!((r.status, r.location()), (StatusCode::PERMANENT_REDIRECT, "/_system/"));
    }
}
