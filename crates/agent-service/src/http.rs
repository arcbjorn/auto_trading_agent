//! The HTTP API: `POST /chat {session_id?, message}` and `GET /healthz`.

use crate::agent::{Agent, Session};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{header, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

pub struct State {
    pub agent: Agent,
    sessions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Session>>>>,
}

impl State {
    pub fn new(agent: Agent) -> Self {
        Self {
            agent,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn session(&self, id: &str) -> Arc<tokio::sync::Mutex<Session>> {
        let mut map = self.sessions.lock().expect("sessions lock");
        Arc::clone(
            map.entry(id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(Session::new(id)))),
        )
    }
}

fn respond(status: StatusCode, body: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("json response")
}

async fn handle(req: Request<Incoming>, state: Arc<State>) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path().to_string();
    match (req.method().clone(), path.as_str()) {
        (Method::GET, "/healthz") => Ok(respond(StatusCode::OK, json!({ "ok": true }))),
        (Method::GET, p) if p.starts_with("/sessions/") => {
            let id = p.trim_start_matches("/sessions/");
            let session = state.session(id);
            let s = session.lock().await;
            Ok(respond(
                StatusCode::OK,
                json!({ "session_id": s.id, "turns": s.turns, "messages": s.messages, "pending_confirmation": s.pending.is_some() }),
            ))
        }
        (Method::POST, "/chat") => {
            let body = match Limited::new(req.into_body(), 1 << 20).collect().await {
                Ok(b) => b.to_bytes(),
                Err(_) => {
                    return Ok(respond(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        json!({ "error": "body too large" }),
                    ))
                }
            };
            let input: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(respond(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": format!("invalid JSON: {e}") }),
                    ))
                }
            };
            let Some(message) = input["message"].as_str().filter(|m| !m.trim().is_empty()) else {
                return Ok(respond(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "message is required" }),
                ));
            };
            if message.len() > 4_000 {
                return Ok(respond(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "message is too long (max 4000 bytes)" }),
                ));
            }
            let session_id = input["session_id"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(new_session_id);
            let session = state.session(&session_id);
            let mut s = session.lock().await;
            match state.agent.chat_turn(&mut s, message).await {
                Ok(turn) => Ok(respond(StatusCode::OK, serde_json::to_value(turn).unwrap_or_default())),
                Err(e) => {
                    tracing::error!(error = %e, session = %session_id, "turn failed");
                    Ok(respond(
                        StatusCode::BAD_GATEWAY,
                        json!({ "error": e.to_string(), "session_id": session_id }),
                    ))
                }
            }
        }
        _ => Ok(respond(StatusCode::NOT_FOUND, json!({ "error": "not found" }))),
    }
}

fn new_session_id() -> String {
    format!(
        "s-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
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

pub async fn serve(addr: SocketAddr, state: Arc<State>) -> anyhow::Result<(SocketAddr, HttpServerHandle)> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let (tx, mut rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req| handle(req, Arc::clone(&state)));
                        if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
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
