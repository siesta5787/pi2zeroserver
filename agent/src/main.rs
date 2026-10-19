//! box-agent: the appliance's front door on port 80.
//!
//! - `/_system/...`: system pages and JSON API (status for now; updates,
//!   Wi-Fi and setup come later)
//! - everything else is proxied to the user's app on 127.0.0.1:8080, with a
//!   friendly page when the app isn't answering
//!
//! It lives in the read-only OS image and only changes with OS updates, so a
//! bad app update can never take away the page used to fix it.

mod pages;
mod proxy;
mod status;

use std::net::SocketAddr;

use axum::Router;
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::get;

#[derive(Clone)]
struct AppState {
    proxy: proxy::Proxy,
}

struct Args {
    listen: SocketAddr,
    app: SocketAddr,
}

const USAGE: &str = "usage: box-agent [--listen ADDR:PORT] [--app ADDR:PORT]";

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        listen: "0.0.0.0:80".parse().unwrap(),
        app: "127.0.0.1:8080".parse().unwrap(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let target = match arg.as_str() {
            "--listen" => &mut args.listen,
            "--app" => &mut args.app,
            _ => return Err(format!("unknown argument {arg:?}\n{USAGE}")),
        };
        let value = it.next().ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))?;
        *target = value.parse().map_err(|e| format!("{arg} {value:?}: {e}"))?;
    }
    Ok(args)
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/_system", get(|| async { Redirect::permanent("/_system/") }))
        .route("/_system/", get(status_page))
        .route("/_system/api/health", get(|| async { "ok" }))
        .route("/_system/api/status", get(status_json))
        .fallback(proxy::forward)
        .with_state(state)
}

async fn status_page() -> impl IntoResponse {
    Html(pages::status(&status::gather().await))
}

async fn status_json() -> impl IntoResponse {
    axum::Json(status::gather().await)
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
    let state = AppState { proxy: proxy::Proxy::new(args.app) };
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

    /// Starts the agent on a free port, proxying to `app`; returns its address.
    async fn start_agent(app: SocketAddr) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let service = router(AppState { proxy: proxy::Proxy::new(app) })
            .into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
        addr
    }

    async fn fetch(url: String) -> (hyper::StatusCode, hyper::HeaderMap, String) {
        let client = Client::builder(TokioExecutor::new()).build_http::<Body>();
        let resp = client.get(url.parse().unwrap()).await.unwrap();
        let (parts, body) = resp.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        (parts.status, parts.headers, String::from_utf8_lossy(&body).into_owned())
    }

    /// A free port with nothing listening on it
    async fn unused_port() -> SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    }

    #[tokio::test]
    async fn health() {
        let agent = start_agent(unused_port().await).await;
        let (status, _, body) = fetch(format!("http://{agent}/_system/api/health")).await;
        assert_eq!(status, 200);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn proxies_to_the_app() {
        let app_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app_addr = app_listener.local_addr().unwrap();
        let app = Router::new().route(
            "/hello",
            get(|headers: axum::http::HeaderMap, uri: axum::http::Uri| async move {
                let fwd = headers.get("x-forwarded-for").map(|v| v.to_str().unwrap().to_owned());
                ([("x-app", "yes")], format!("{} {}", uri, fwd.unwrap_or_default()))
            }),
        );
        tokio::spawn(async move { axum::serve(app_listener, app).await.unwrap() });

        let agent = start_agent(app_addr).await;
        let (status, headers, body) = fetch(format!("http://{agent}/hello?x=1")).await;
        assert_eq!(status, 200);
        assert_eq!(headers.get("x-app").unwrap(), "yes");
        assert_eq!(body, "/hello?x=1 127.0.0.1");
    }

    #[tokio::test]
    async fn friendly_page_when_app_is_down() {
        let agent = start_agent(unused_port().await).await;
        let (status, headers, body) = fetch(format!("http://{agent}/anything")).await;
        assert_eq!(status, 502);
        assert!(headers.get("content-type").unwrap().to_str().unwrap().starts_with("text/html"));
        assert!(body.contains("/_system/"), "page links to the system page: {body}");
    }

    #[tokio::test]
    async fn system_redirects_to_slash() {
        let agent = start_agent(unused_port().await).await;
        let (status, headers, _) = fetch(format!("http://{agent}/_system")).await;
        assert_eq!(status, 308);
        assert_eq!(headers.get("location").unwrap(), "/_system/");
    }
}
