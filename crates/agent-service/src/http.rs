//! The HTTP API: `POST /chat {session_id?, request_id?, message}`, `GET /sessions/{id}` and
//! `GET /healthz`. A `request_id` makes a retried POST return the earlier answer instead of running
//! the turn again; each session is limited to a number of turns per minute.

use crate::agent::{Agent, Session};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, header};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
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
    /// Prompt tokens the history may reach, measured from the model's own usage report; beyond
    /// it `POST /chat` answers 409. Turns bound the count, this bounds the size: a few turns
    /// with large tool results can fill a context window long before two hundred turns.
    pub max_context_tokens: u64,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_sessions: 1_000,
            idle_ttl: Duration::from_secs(3_600),
            turns_per_minute: 20,
            max_turns: 200,
            max_context_tokens: 150_000,
        }
    }
}

/// Responses remembered per session for `request_id` retries.
const REMEMBERED_RESPONSES: usize = 16;

struct Entry {
    last_seen: Instant,
    session: Arc<tokio::sync::Mutex<Session>>,
}

/// A session held for the length of a request.
///
/// While one exists the entry cannot be evicted, so a turn in flight never has its session
/// replaced underneath it. That would let a second turn of the same session run in parallel with
/// its limits reset.
pub struct SessionLease {
    session: Arc<tokio::sync::Mutex<Session>>,
}

impl SessionLease {
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, Session> {
        self.session.lock().await
    }
}

pub struct State {
    pub agent: Agent,
    limits: SessionLimits,
    sessions: Mutex<HashMap<String, Entry>>,
    pub metrics: crate::metrics::Metrics,
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
            metrics: crate::metrics::Metrics::default(),
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.lock().expect("sessions lock").len()
    }

    /// An existing session, without creating one. Used by reads, which must not bring a session
    /// into being or push another out.
    fn existing(self: &Arc<Self>, id: &str) -> Option<SessionLease> {
        let session = {
            let mut map = self.sessions.lock().expect("sessions lock");
            let e = map.get_mut(id)?;
            e.last_seen = Instant::now();
            Arc::clone(&e.session)
        };
        Some(SessionLease { session })
    }

    /// Returns the session, creating it if needed, and holds it for the caller's lifetime. When
    /// the store is full, idle sessions are dropped first, then the least recently used one that
    /// has no request in flight. When every session is busy the caller is refused rather than
    /// having a live session evicted underneath it.
    fn session(self: &Arc<Self>, id: &str) -> Result<SessionLease, Box<Response<Full<Bytes>>>> {
        let now = Instant::now();
        let session = {
            let mut map = self.sessions.lock().expect("sessions lock");
            if let Some(e) = map.get_mut(id) {
                e.last_seen = now;
                Arc::clone(&e.session)
            } else {
                if map.len() >= self.limits.max_sessions.max(1) {
                    let ttl = self.limits.idle_ttl;
                    map.retain(|_, e| now.duration_since(e.last_seen) < ttl || Arc::strong_count(&e.session) > 1);
                    if map.len() >= self.limits.max_sessions.max(1) {
                        let oldest = map
                            .iter()
                            .filter(|(_, e)| Arc::strong_count(&e.session) == 1)
                            .min_by_key(|(_, e)| e.last_seen)
                            .map(|(k, _)| k.clone());
                        match oldest {
                            Some(k) => {
                                map.remove(&k);
                            }
                            None => {
                                return Err(Box::new(respond(
                                    StatusCode::TOO_MANY_REQUESTS,
                                    json!({ "error": "every session is busy; retry shortly", "session_id": id }),
                                )));
                            }
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
        };
        Ok(SessionLease { session })
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
    if !mcp_server::transport::http::local_request_allowed(req.headers()) {
        return Ok(respond(
            StatusCode::FORBIDDEN,
            json!({ "error": "host or origin not allowed" }),
        ));
    }
    let path = req.uri().path().to_string();
    match (req.method().clone(), path.as_str()) {
        (Method::GET, "/healthz") => Ok(respond(StatusCode::OK, json!({ "ok": true }))),
        (Method::GET, "/metrics") => Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
            .body(Full::new(Bytes::from(state.metrics.render(state.session_count()))))
            .expect("metrics response")),
        (Method::GET, p) if p.starts_with("/sessions/") => {
            let id = p.trim_start_matches("/sessions/");
            // A read never creates a session, so it cannot evict one either.
            let Some(lease) = state.existing(id) else {
                return Ok(respond(
                    StatusCode::NOT_FOUND,
                    json!({ "error": "no such session", "session_id": id }),
                ));
            };
            let s = lease.lock().await;
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
                    ));
                }
            };
            let input: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(respond(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": format!("invalid JSON: {e}") }),
                    ));
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
            let session_id = match input.get("session_id") {
                None | Some(Value::Null) => new_session_id(),
                Some(Value::String(id)) if valid_session_id(id) => id.to_string(),
                Some(_) => {
                    return Ok(respond(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "session_id must be 1-64 characters of letters, digits, '.', '_' or '-'" }),
                    ));
                }
            };
            let request_id = match input.get("request_id") {
                None | Some(Value::Null) => None,
                Some(Value::String(id)) if valid_session_id(id) => Some(id.to_string()),
                Some(_) => {
                    return Ok(respond(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "request_id must be 1-64 characters of letters, digits, '.', '_' or '-'" }),
                    ));
                }
            };
            let lease = match state.session(&session_id) {
                Ok(l) => l,
                Err(response) => {
                    state.metrics.turn_refused("no_session_available");
                    return Ok(*response);
                }
            };
            let mut s = lease.lock().await;
            if let Some(id) = &request_id
                && let Some((_, earlier_message, status, earlier)) = s.responses.iter().find(|(r, _, _, _)| r == id)
            {
                // The same id with the same message replays; with a different message it is
                // a client bug, and answering the old question would mislead.
                if earlier_message == message {
                    return Ok(respond(
                        StatusCode::from_u16(*status).expect("stored status"),
                        earlier.clone(),
                    ));
                }
                return Ok(respond(
                    StatusCode::CONFLICT,
                    json!({ "error": "request_id was already used with a different message", "session_id": session_id }),
                ));
            }
            if s.context_tokens >= state.limits.max_context_tokens {
                state.metrics.turn_refused("context_full");
                return Ok(respond(
                    StatusCode::CONFLICT,
                    json!({ "error": format!("this session's history reached {} prompt tokens; start a new session", s.context_tokens), "session_id": session_id }),
                ));
            }
            if s.turns >= state.limits.max_turns {
                state.metrics.turn_refused("session_full");
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
            if s.turn_times.len() >= state.limits.turns_per_minute as usize {
                return Ok(respond(
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({ "error": format!("this session may start {} turns per minute; wait before the next one", state.limits.turns_per_minute), "session_id": session_id }),
                ));
            }
            s.turn_times.push_back(now);
            // Reserve before awaiting: even cancellation of this HTTP handler must not turn a
            // retry into a second order after an uncertain first attempt.
            if let Some(id) = &request_id {
                if s.responses.len() >= REMEMBERED_RESPONSES {
                    s.responses.remove(0);
                }
                s.responses.push((id.clone(), message.to_string(), 409, json!({
                    "error": "request outcome is unknown; inspect the session and engine before submitting another action",
                    "session_id": session_id
                })));
            }
            let (status, body) = match state.agent.chat_turn(&mut s, message).await {
                Ok(turn) => {
                    state.metrics.turn(&turn);
                    (StatusCode::OK, serde_json::to_value(turn).expect("serializable turn"))
                }
                Err(e) => {
                    state.metrics.turn_refused("error");
                    tracing::error!(error = %e, session = %session_id, "turn failed");
                    (
                        StatusCode::BAD_GATEWAY,
                        json!({ "error": e.to_string(), "session_id": session_id }),
                    )
                }
            };
            if let Some(id) = &request_id {
                let entry = s
                    .responses
                    .iter_mut()
                    .find(|(r, _, _, _)| r == id)
                    .expect("reserved request");
                entry.2 = status.as_u16();
                entry.3 = body.clone();
            }
            Ok(respond(status, body))
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
