//! `evals web`: the whole stack in one process behind a browser page, so every part can be driven
//! by hand and its effect watched. The engine's book, trades and wallets; the MCP tools and the
//! policy in front of them; the agent's chat with every gate decision; the evaluation runs.
//!
//! The page is server-rendered HTML with htmx. Each panel is a fragment the server renders from
//! the same gRPC, MCP and HTTP calls any client would make, so what the page shows is what the
//! interfaces return. Nothing here reaches the engine except through gRPC.
//!
//! Chat needs a model key; every other panel works without one.

mod engine;
mod html;

use crate::Args;
use crate::cases::Funding;
use crate::harness::{ACCOUNT, MAKER, Stack};
use agent_service::{Agent, AgentConfig, Audit, McpClient, ModelClient, NoteChannel};
use bytes::Bytes;
use clob_proto::v1::engine_client::EngineClient;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::transport::Channel;

const HTMX: &str = include_str!("../../web/htmx.min.js");
const CSS: &str = include_str!("../../web/style.css");
const JS: &str = include_str!("../../web/app.js");

/// Everything the page handlers share. Cloning the gRPC client is cheap.
pub struct App {
    pub engine: EngineClient<Channel>,
    pub engine_addr: SocketAddr,
    pub mcp_url: String,
    /// The in-process agent-service, when a model key was found.
    pub agent_url: Option<String>,
    /// The model's description, or the reason chat is off.
    pub model: Result<String, String>,
    /// The newest engine events, filled by [`engine::watch_events`].
    pub events: Mutex<VecDeque<engine::EventRow>>,
    /// Every account this process has funded: the reset cancels their orders and the load test
    /// checks that USDC and ETH are conserved across them.
    pub accounts: Mutex<BTreeSet<String>>,
    pub reseeds: AtomicU64,
    pub started: Instant,
}

/// The running stack behind the page; dropped in order on shutdown.
pub struct Booted {
    pub app: Arc<App>,
    stack: Stack,
    agent: Option<agent_service::http::HttpServerHandle>,
    /// The event-stream follower; stopped first, since its open `Subscribe` stream would
    /// otherwise hold up the engine's graceful shutdown.
    watcher: tokio::task::JoinHandle<()>,
}

impl Booted {
    pub async fn shutdown(self) {
        self.watcher.abort();
        let _ = self.watcher.await;
        if let Some(agent) = self.agent {
            agent.shutdown().await;
        }
        self.stack.shutdown().await;
    }
}

/// Starts the engine and the MCP server, funds and seeds the demo book, and starts the
/// agent-service when a model key is present.
pub async fn boot(out_dir: &Path) -> anyhow::Result<Booted> {
    let mut stack = Stack::start().await?;
    stack.fund(&Funding::default()).await?;
    stack.seed(&crate::demo::seed_book()).await?;
    let (agent_url, model, agent) = match ModelClient::from_env() {
        Ok(model) => {
            let describe = model.describe();
            let cfg = AgentConfig {
                note_channel: NoteChannel::for_model(model.model_id()),
                ..AgentConfig::default()
            };
            std::fs::create_dir_all(out_dir)?;
            let audit = Audit::new(Some(out_dir.join("web-audit.jsonl")))?;
            let agent = Agent::new(model, McpClient::connect(&stack.mcp_url).await?, cfg, audit).await?;
            let state = Arc::new(agent_service::http::State::new(agent));
            let (addr, handle) = agent_service::http::serve("127.0.0.1:0".parse()?, state).await?;
            (Some(format!("http://{addr}")), Ok(describe), Some(handle))
        }
        Err(e) => (None, Err(e.to_string()), None),
    };
    let app = Arc::new(App {
        engine: stack.engine.clone(),
        engine_addr: stack.engine_addr,
        mcp_url: stack.mcp_url.clone(),
        agent_url,
        model,
        events: Mutex::new(VecDeque::new()),
        accounts: Mutex::new([ACCOUNT.to_string(), MAKER.to_string()].into_iter().collect()),
        reseeds: AtomicU64::new(0),
        started: Instant::now(),
    });
    let watcher = tokio::spawn(engine::watch_events(Arc::clone(&app)));
    Ok(Booted {
        app,
        stack,
        agent,
        watcher,
    })
}

pub struct WebHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl WebHandle {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.task.await;
    }
}

pub async fn serve(addr: SocketAddr, app: Arc<App>) -> anyhow::Result<(SocketAddr, WebHandle)> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let (tx, mut rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let app = Arc::clone(&app);
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let service = service_fn(move |req| handle(req, Arc::clone(&app)));
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
        WebHandle {
            shutdown: Some(tx),
            task,
        },
    ))
}

pub async fn run(args: &Args) -> anyhow::Result<()> {
    let booted = boot(&args.out_dir).await?;
    let (addr, handle) = serve(args.addr.parse()?, Arc::clone(&booted.app)).await?;
    let app = &booted.app;
    println!("web demo   http://{addr}");
    println!("engine     {} (gRPC)", app.engine_addr);
    println!("mcp        {}", app.mcp_url);
    match (&app.model, &app.agent_url) {
        (Ok(model), Some(url)) => println!("chat       {model} through {url}/chat"),
        (Ok(_), None) => println!("chat       off"),
        (Err(why), _) => println!("chat       off ({why})"),
    }
    println!("ctrl-c to stop");
    tokio::signal::ctrl_c().await?;
    handle.shutdown().await;
    booted.shutdown().await;
    Ok(())
}

async fn handle(req: Request<Incoming>, app: Arc<App>) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let form = if method == Method::POST {
        match Limited::new(req.into_body(), 256 << 10).collect().await {
            Ok(b) => form(&b.to_bytes()),
            Err(_) => return Ok(html::error("request body too large")),
        }
    } else {
        HashMap::new()
    };
    let res = route(&app, method, &path, &form).await;
    Ok(res.unwrap_or_else(|e| html::error(&e.to_string())))
}

async fn route(
    app: &Arc<App>,
    method: Method,
    path: &str,
    form: &HashMap<String, String>,
) -> anyhow::Result<Response<Full<Bytes>>> {
    Ok(match (method, path) {
        (Method::GET, "/") => html::html(page(app)),
        (Method::GET, "/htmx.min.js") => html::asset("application/javascript", HTMX),
        (Method::GET, "/style.css") => html::asset("text/css", CSS),
        (Method::GET, "/app.js") => html::asset("application/javascript", JS),
        (Method::GET, "/healthz") => html::respond(StatusCode::OK, "application/json", r#"{"ok":true}"#),
        (Method::GET, "/ui/engine/market") => engine::market(app).await?,
        (Method::GET, "/ui/engine/book") => engine::book(app).await?,
        (Method::GET, "/ui/engine/trades") => engine::trades(app).await?,
        (Method::GET, "/ui/engine/accounts") => engine::accounts(app).await?,
        (Method::GET, "/ui/engine/stats") => engine::stats(app).await?,
        (Method::GET, "/ui/engine/events") => engine::events(app),
        (Method::POST, "/ui/engine/reset") => engine::reset(app).await?,
        (Method::POST, "/ui/engine/load") => engine::load(app, form).await?,
        _ => html::not_found(),
    })
}

fn page(app: &App) -> String {
    let chat = match &app.model {
        Ok(m) => format!("chat <b>{}</b>", html::esc(m)),
        Err(_) => "chat <b>off</b> (no model key)".to_string(),
    };
    let status = format!(
        "engine <b>{}</b> · mcp <b>{}</b> · {chat}",
        html::esc(&app.engine_addr.to_string()),
        html::esc(app.mcp_url.trim_start_matches("http://"))
    );
    let sections = engine::section();
    html::page(&status, &sections)
}

/// `application/x-www-form-urlencoded`, which is what htmx sends.
pub fn form(body: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in body.split(|b| *b == b'&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, |b| *b == b'=');
        let key = it.next().unwrap_or(&[]);
        let value = it.next().unwrap_or(&[]);
        out.insert(percent_decode(key), percent_decode(value));
    }
    out
}

fn percent_decode(s: &[u8]) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < s.len() => {
                let hex = std::str::from_utf8(&s[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_decodes_percent_and_plus() {
        let f = form(b"message=Buy+0.5+ETH+%40+3000&clients=8&empty=&odd=%zz%4");
        assert_eq!(f["message"], "Buy 0.5 ETH @ 3000");
        assert_eq!(f["clients"], "8");
        assert_eq!(f["empty"], "");
        assert_eq!(f["odd"], "%zz%4", "a malformed escape is kept as it was");
    }

    /// The page and every engine panel render against a real in-process stack; the reset and the
    /// load test run and report their checks. No model key is needed for any of it.
    #[tokio::test(flavor = "multi_thread")]
    async fn page_and_engine_panels_render() {
        let dir = std::env::temp_dir().join(format!("web-test-{}", std::process::id()));
        let booted = boot(&dir).await.expect("boot");
        let (addr, handle) = serve("127.0.0.1:0".parse().unwrap(), Arc::clone(&booted.app))
            .await
            .expect("serve");
        let http = reqwest::Client::new();
        let base = format!("http://{addr}");
        let get = |path: &str| {
            let (http, url) = (http.clone(), format!("{base}{path}"));
            async move { http.get(url).send().await.expect("get").text().await.expect("body") }
        };
        let page = get("/").await;
        assert!(page.contains("auto_trading_agent") && page.contains("hx-get=\"/ui/engine/book\""));
        let book = get("/ui/engine/book").await;
        assert!(book.contains("2999.00") && book.contains("3001.00"), "{book}");
        assert!(get("/ui/engine/market").await.contains("best bid"));
        assert!(get("/ui/engine/accounts").await.contains("50000.00 USDC"));
        assert!(get("/ui/engine/stats").await.contains("commands applied"));
        let post = |path: &str, body: &str| {
            let (http, url, body) = (http.clone(), format!("{base}{path}"), body.to_string());
            async move {
                http.post(url)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(body)
                    .send()
                    .await
                    .expect("post")
            }
        };
        let load = post("/ui/engine/load", "clients=4&per=50").await;
        assert_eq!(load.headers()["hx-trigger"], "engine");
        let load = load.text().await.unwrap();
        assert!(
            load.contains("200 orders") && load.contains("book not crossed: holds"),
            "{load}"
        );
        assert!(load.contains("USDC conserved across 6 accounts: holds") && load.contains("ETH conserved: holds"));
        let reset = post("/ui/engine/reset", "").await.text().await.unwrap();
        assert!(reset.contains("rested 4 maker levels"), "{reset}");
        assert!(get("/ui/engine/book").await.contains("2998.50"));
        assert!(
            get("/ui/engine/trades").await.contains("load-"),
            "the load test's fills are on the tape"
        );
        assert!(http.get(format!("{base}/nope")).send().await.unwrap().status() == 404);
        handle.shutdown().await;
        booted.shutdown().await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
