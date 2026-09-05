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

/// One panel: title, the interface it reads from, a `?` that shows the explanation, the body.
/// `title` and `source` may carry HTML; `about` is a paragraph shown by the `?` or the explain
/// switch in the header.
pub fn panel(title: &str, source: &str, about: &str, body: &str) -> String {
    format!(
        "<div class=\"panel\"><h3><span class=\"t\">{title}</span><span class=\"src\">{source}</span><button type=\"button\" class=\"about-toggle\" aria-label=\"about this panel\" title=\"about this panel\">?</button></h3><p class=\"about\">{about}</p>{body}</div>"
    )
}

/// One tab under the hood: a heading, one sentence, an explanation behind the same `?`.
pub fn tab(id: &str, title: &str, subtitle: &str, lead: &str, about: &str, body: &str, hidden: bool) -> String {
    format!(
        "<section class=\"tab\" id=\"tab-{id}\"{h}><h2>{title} <small>{subtitle}</small><button type=\"button\" class=\"about-toggle\" aria-label=\"about this part\" title=\"about this part\">?</button></h2><p class=\"lead\">{lead}</p><p class=\"about\">{about}</p>{body}</section>",
        h = if hidden { " hidden" } else { "" }
    )
}

/// The page: the chat with the live book beside it, then a tab strip for the parts under the
/// hood. Tabs are plain sections toggled by `app.js`; the active one is kept in the URL hash.
pub fn page(status: &str, home: &str, sections: &str) -> String {
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
  <div class="tagline">ETH/USDC · an agent that may only ask; code decides</div>
  <div class="status">{status}<span class="switch"><button id="explain" class="small" type="button" title="show every explanation on the page">explain</button><button id="theme" class="small" type="button">dark</button></span></div>
</header>
<div id="ticker" class="ticker" hx-get="/ui/engine/market" hx-trigger="load, every 1s, engine from:body" hx-swap="innerHTML"></div>
{home}
<nav class="tabs" role="tablist">
  <span class="lbl">under the hood</span>
  <button type="button" role="tab" data-tab="engine">engine</button>
  <button type="button" role="tab" data-tab="mcp">MCP server</button>
  <button type="button" role="tab" data-tab="evals">evaluation</button>
  <button type="button" role="tab" data-tab="results">results</button>
</nav>
<main>
{sections}
</main>
<footer class="muted small">One process: the engine, the MCP server, the agent and the harness. Every number on this page arrived over gRPC, MCP or the chat API. The <b>?</b> on a panel explains it; <b>explain</b> in the header opens every explanation at once.</footer>
</div>
</body>
</html>"##
    )
}
