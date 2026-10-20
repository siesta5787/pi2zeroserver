//! Reverse proxy from the agent to the user's app.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::AppState;

#[derive(Clone)]
pub struct Proxy {
    app: SocketAddr,
    client: Client<HttpConnector, Body>,
}

impl Proxy {
    pub fn new(app: SocketAddr) -> Self {
        Self { app, client: Client::builder(TokioExecutor::new()).build_http() }
    }
}

/// Headers that describe one connection, not the request; never forwarded
/// (RFC 9110 section 7.6.1), plus any listed in the Connection header.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in listed {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
    headers.remove("keep-alive");
}

pub async fn forward(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut req: Request,
) -> Response {
    let proxy = &state.proxy;
    let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let uri: Uri = match format!("http://{}{}", proxy.app, path).parse() {
        Ok(uri) => uri,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    *req.uri_mut() = uri;

    let headers = req.headers_mut();
    strip_hop_by_hop(headers);
    let client_ip = peer.ip().to_string();
    let forwarded_for = match headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(prior) => format!("{prior}, {client_ip}"),
        None => client_ip,
    };
    if let Ok(v) = HeaderValue::from_str(&forwarded_for) {
        headers.insert("x-forwarded-for", v);
    }
    // Only the tailnet front proxies, and it's only reached through tailscale serve over HTTPS
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));

    match proxy.client.request(req).await {
        Ok(resp) => {
            let (mut parts, body) = resp.into_parts();
            strip_hop_by_hop(&mut parts.headers);
            Response::from_parts(parts, Body::new(body))
        }
        Err(_) => (StatusCode::BAD_GATEWAY, Html(crate::pages::app_down())).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_hop_by_hop_and_connection_listed_headers() {
        let mut h = HeaderMap::new();
        h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive, X-Secret"));
        h.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        h.insert("x-secret", HeaderValue::from_static("1"));
        h.insert(header::TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        strip_hop_by_hop(&mut h);
        assert_eq!(h.len(), 1);
        assert!(h.contains_key(header::CONTENT_TYPE));
    }
}
