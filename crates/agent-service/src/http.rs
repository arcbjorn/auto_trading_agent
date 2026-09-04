//! The HTTP API: `POST /chat {session_id?, request_id?, message}`, `GET /sessions/{id}` and
//! `GET /healthz`. A `request_id` makes a retried POST return the earlier answer instead of running
//! the turn again; each session is limited to a number of turns per minute.

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
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

#[derive(Clone, Debug)]
pub struct SessionLimits {
    /// Sessions kept in memory; beyond this the least recently used ones are dropped.
    pub max_sessions: usize,
    /// A session untouched for this long is dropped once the store is full.
    pub idle_ttl: Duration,
    /// Turns one session may start per rolling minute; beyond it `POST /chat` answers 429.
    pub turns_per_minute: u32,
    /// Turns one session may hold in total; beyond it `POST /chat` answers 409 and the client
    /// starts a new session, so no conversation grows without bound.
    pub max_turns: u32,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_sessions: 1_000,
            idle_ttl: Duration::from_secs(3_600),
            turns_per_minute: 20,
            max_turns: 200,
        }
    }
}

/// Responses remembered per session for `request_id` retries.
const REMEMBERED_RESPONSES: usize = 16;

struct Entry {
    last_seen: Instant,
    session: Arc<tokio::sync::Mutex<Session>>,
}

pub struct State {
    pub agent: Agent,
    limits: SessionLimits,
    sessions: Mutex<HashMap<String, Entry>>,
}

impl State {
    pub fn new(agent: Agent) -> Self {
        Self::with_limits(agent, SessionLimits::default())
    }

    pub fn with_limits(agent: Agent, limits: SessionLimits) -> Self {
        Self {
            agent,
            limits,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.lock().expect("sessions lock").len()
    }

    /// Returns the session, creating it if needed. When the store is full, idle sessions are
    /// dropped first, then the least recently used one, so memory stays bounded.
    fn session(&self, id: &str) -> Arc<tokio::sync::Mutex<Session>> {
        let now = Instant::now();
        let mut map = self.sessions.lock().expect("sessions lock");
        if let Some(e) = map.get_mut(id) {
            e.last_seen = now;
            return Arc::clone(&e.session);
        }
        if map.len() >= self.limits.max_sessions.max(1) {
            let ttl = self.limits.idle_ttl;
            map.retain(|_, e| now.duration_since(e.last_seen) < ttl);
            if map.len() >= self.limits.max_sessions.max(1) {
                if let Some(oldest) = map.iter().min_by_key(|(_, e)| e.last_seen).map(|(k, _)| k.clone()) {
                    map.remove(&oldest);
                }
            }
        }
        let session = Arc::new(tokio::sync::Mutex::new(Session::new(id)));
        map.insert(
            id.to_string(),
            Entry {
                last_seen: now,
                session: Arc::clone(&session),
            },
        );
        session
    }
}

fn message_hash(message: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    message.hash(&mut h);
    h.finish()
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
            let session_id = match input["session_id"].as_str() {
                None => new_session_id(),
                Some(id) if valid_session_id(id) => id.to_string(),
                Some(_) => {
                    return Ok(respond(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "session_id must be 1-64 characters of letters, digits, '.', '_' or '-'" }),
                    ))
                }
            };
            let request_id = match input["request_id"].as_str() {
                None => None,
                Some(id) if valid_session_id(id) => Some(id.to_string()),
                Some(_) => {
                    return Ok(respond(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "request_id must be 1-64 characters of letters, digits, '.', '_' or '-'" }),
                    ))
                }
            };
            let session = state.session(&session_id);
            let mut s = session.lock().await;
            if let Some(id) = &request_id {
                if let Some((_, earlier_message, earlier)) = s.responses.iter().find(|(r, _, _)| r == id) {
                    // The same id with the same message replays; with a different message it is
                    // a client bug, and answering the old question would mislead.
                    if *earlier_message == message_hash(message) {
                        return Ok(respond(StatusCode::OK, earlier.clone()));
                    }
                    return Ok(respond(
                        StatusCode::CONFLICT,
                        json!({ "error": "request_id was already used with a different message", "session_id": session_id }),
                    ));
                }
            }
            if s.turns >= state.limits.max_turns {
                return Ok(respond(
                    StatusCode::CONFLICT,
                    json!({ "error": format!("this session reached its {} turns; start a new session", state.limits.max_turns), "session_id": session_id }),
                ));
            }
            let now = Instant::now();
            while s
                .turn_times
                .front()
                .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
            {
                s.turn_times.pop_front();
            }
            if s.turn_times.len() as u32 >= state.limits.turns_per_minute {
                return Ok(respond(
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({ "error": format!("this session may start {} turns per minute; wait before the next one", state.limits.turns_per_minute), "session_id": session_id }),
                ));
            }
            s.turn_times.push_back(now);
            match state.agent.chat_turn(&mut s, message).await {
                Ok(turn) => {
                    let body = serde_json::to_value(turn).unwrap_or_default();
                    if let Some(id) = request_id {
                        if s.responses.len() >= REMEMBERED_RESPONSES {
                            s.responses.remove(0);
                        }
                        s.responses.push((id, message_hash(message), body.clone()));
                    }
                    Ok(respond(StatusCode::OK, body))
                }
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

/// The session id becomes part of every idempotency key the service sends to the engine, and the
/// engine echoes those keys in listings the model reads. A bounded, plain id cannot carry text.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
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
