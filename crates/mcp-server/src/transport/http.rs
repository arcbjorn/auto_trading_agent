//! Streamable HTTP transport (MCP 2025-03-26 and later), stateless variant.
//!
//! * `POST /mcp` with a JSON-RPC request answers `application/json`; a notification answers
//!   `202 Accepted` with no body.
//! * `GET /mcp` answers `405`: this server never opens a server-to-client stream, which the
//!   specification allows.
//! * No session id is issued, so clients never need to send one.
//! * `resources/subscribe` is refused with an explanation: there is no stream to deliver
//!   notifications on. The stdio transport delivers them.
//! * The `Origin` header, when present, must be a loopback origin (DNS-rebinding protection for a
//!   server meant to run locally), and an `MCP-Protocol-Version` header, when present, must name
//!   a supported version.

use crate::protocol::{McpServer, SUPPORTED_PROTOCOL_VERSIONS};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, header};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

pub const MAX_BODY_BYTES: usize = 1 << 20;
pub const MCP_PATH: &str = "/mcp";

fn text(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("static response")
}

fn origin_is_local(origin: &str) -> bool {
    let host = origin
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host = host.split('/').next().unwrap_or("");
    let host = host
        .strip_prefix('[')
        .map(|h| h.split(']').next().unwrap_or(""))
        .unwrap_or_else(|| host.split(':').next().unwrap_or(""));
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "null") || host.ends_with(".localhost")
}

async fn handle(req: Request<Incoming>, server: Arc<McpServer>) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path();
    if path == "/healthz" && req.method() == Method::GET {
        return Ok(text(StatusCode::OK, "ok"));
    }
    if path == "/metrics" && req.method() == Method::GET {
        // The engine's counters are fetched on each scrape; a stalled engine leaves them out.
        let engine = server
            .tools()
            .engine_client()
            .get_stats(clob_proto::v1::GetStatsRequest {})
            .await
            .ok()
            .map(|r| r.into_inner());
        return Ok(text(StatusCode::OK, &server.metrics().render(engine.as_ref())));
    }
    if path != MCP_PATH {
        return Ok(text(StatusCode::NOT_FOUND, "not found"));
    }
    if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if !origin_is_local(origin) {
            return Ok(text(StatusCode::FORBIDDEN, "origin not allowed"));
        }
    }
    if let Some(v) = req.headers().get("mcp-protocol-version").and_then(|v| v.to_str().ok()) {
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&v) {
            return Ok(text(StatusCode::BAD_REQUEST, "unsupported MCP-Protocol-Version"));
        }
    }
    match *req.method() {
        Method::POST => {}
        Method::GET | Method::DELETE => {
            return Ok(Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .header(header::ALLOW, "POST")
                .body(Full::new(Bytes::new()))
                .expect("static response"));
        }
        _ => return Ok(text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")),
    }
    let body = match Limited::new(req.into_body(), MAX_BODY_BYTES).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return Ok(text(StatusCode::PAYLOAD_TOO_LARGE, "body too large or unreadable")),
    };
    // Stateless HTTP has no server-to-client stream, so a subscription could never be honoured:
    // say so instead of accepting it silently.
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
        if v["method"] == "resources/subscribe" {
            let id = v.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let err = crate::jsonrpc::failure(
                id,
                crate::jsonrpc::RpcError::invalid_params(
                    "subscriptions need a server-to-client stream; this stateless HTTP transport has none, use stdio",
                ),
            );
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(
                    serde_json::to_vec(&err).expect("serialisable reply"),
                )))
                .expect("json response"));
        }
    }
    match server.handle_bytes(&body).await {
        Some(reply) => Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(
                serde_json::to_vec(&reply).expect("serialisable reply"),
            )))
            .expect("json response")),
        None => Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Full::new(Bytes::new()))
            .expect("static response")),
    }
}

pub struct HttpServerHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl HttpServerHandle {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.task.await;
    }
}

/// Binds `addr` (port 0 picks a free port) and serves the MCP endpoint at `/mcp` plus `/healthz`.
pub async fn serve_http(addr: SocketAddr, server: Arc<McpServer>) -> anyhow::Result<(SocketAddr, HttpServerHandle)> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let (tx, mut rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let server = Arc::clone(&server);
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req| handle(req, Arc::clone(&server)));
                        if let Err(e) = http1::Builder::new().keep_alive(true).serve_connection(io, service).await {
                            tracing::debug!(error = %e, "connection closed");
                        }
                    });
                }
            }
        }
    });
    Ok((
        bound,
        HttpServerHandle {
            shutdown: Some(tx),
            task,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::origin_is_local;

    #[test]
    fn loopback_origins_only() {
        assert!(origin_is_local("http://localhost:3000"));
        assert!(origin_is_local("http://127.0.0.1"));
        assert!(origin_is_local("http://[::1]:8000"));
        assert!(origin_is_local("null"));
        assert!(!origin_is_local("https://evil.example"));
        assert!(!origin_is_local("http://localhost.evil.example"));
    }
}
