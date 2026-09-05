//! HTML helpers: escaping, responses, the page shell. Every string that came from a user, a
//! model or the engine passes through [`esc`] before it is written into a page.

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode, header};

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn respond(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(body.into()))
        .expect("response")
}

pub fn html(body: String) -> Response<Full<Bytes>> {
    respond(StatusCode::OK, "text/html; charset=utf-8", body)
}

/// A fragment that also tells the page which panels to refresh (`hx-trigger="<event> from:body"`).
pub fn html_trigger(body: String, event: &str) -> Response<Full<Bytes>> {
    let mut r = html(body);
    r.headers_mut()
        .insert("HX-Trigger", event.parse().expect("header value"));
    r
}

pub fn asset(content_type: &str, body: &'static str) -> Response<Full<Bytes>> {
    let mut r = respond(StatusCode::OK, content_type, body);
    r.headers_mut()
        .insert(header::CACHE_CONTROL, "max-age=3600".parse().expect("header value"));
    r
}

/// Errors are rendered into the panel that asked, with status 200 so htmx swaps them in.
pub fn error(msg: &str) -> Response<Full<Bytes>> {
    html(format!("<p class=\"err\">{}</p>", esc(msg)))
}

pub fn not_found() -> Response<Full<Bytes>> {
    respond(StatusCode::NOT_FOUND, "text/plain", "not found")
}

pub fn chip(class: &str, text: &str) -> String {
    format!("<span class=\"chip {class}\">{}</span>", esc(text))
}

/// A panel that loads itself and then refreshes on a timer and on the named page event.
pub fn live(id: &str, url: &str, every: &str, event: &str) -> String {
    format!(
        "<div id=\"{id}\" hx-get=\"{url}\" hx-trigger=\"load, every {every}, {event} from:body\" hx-swap=\"innerHTML\"></div>"
    )
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The page: header, then one section per part of the task, each filled by fragments.
pub fn page(status: &str, sections: &str) -> String {
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>auto_trading_agent</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500;600;700&display=swap" rel="stylesheet">
<link rel="stylesheet" href="/style.css">
<script src="/htmx.min.js"></script>
<script src="/app.js" defer></script>
</head>
<body>
<div class="wrap">
<header class="top">
  <div class="logo"><span class="at">~</span> auto_trading_agent</div>
  <nav class="links">
    <a href="#engine">engine</a>
    <a href="#mcp">mcp</a>
    <a href="#agent">agent</a>
    <a href="#evals">evals</a>
    <a href="#results">results</a>
  </nav>
  <div class="status">{status}<br><button id="theme" class="small" type="button">dark</button></div>
</header>
<p class="sub">ETH/USDC. The engine, the MCP server, the agent and the evaluation harness run in this one process; every number on this page arrived over gRPC, MCP or the chat API.</p>
{sections}
</div>
</body>
</html>"##
    )
}
