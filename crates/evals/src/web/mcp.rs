//! The MCP server's panel: the tool list as the model sees it, a form that calls any tool and
//! shows the JSON-RPC exchange, the policy limits that stand in front of the engine, the
//! resources and the prompt. Every call goes over the Streamable HTTP endpoint like a client's.

use super::App;
use super::html::{self, chip, esc};
use bytes::Bytes;
use http_body_util::Full;
use hyper::Response;
use mcp_server::units::{eth, usdc_from_micro};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Instant;

type Html = Response<Full<Bytes>>;

/// Example arguments per tool, so a click on a tool name gives a form that already runs.
fn example_args(tool: &str) -> &'static str {
    match tool {
        "get_order_book" => r#"{"depth": 5}"#,
        "get_quote" => r#"{"side": "buy", "quantity_eth": "0.5"}"#,
        "place_limit_order" => r#"{"side": "buy", "price_usdc": "2999.50", "quantity_eth": "0.1"}"#,
        "cancel_order" | "get_order" => r#"{"order_id": "1"}"#,
        "list_orders" => r#"{"status": "open", "limit": 10}"#,
        "list_trades" => r#"{"limit": 10}"#,
        _ => "{}",
    }
}

/// One-click calls in three groups: reads, actions that change the book, and refusals.
const PRESETS: [(&str, &str, &str, &str); 8] = [
    ("read", "market summary", "get_market_summary", "{}"),
    (
        "read",
        "quote: buy 0.5 ETH",
        "get_quote",
        r#"{"side": "buy", "quantity_eth": "0.5"}"#,
    ),
    ("read", "balances", "get_balances", "{}"),
    (
        "act",
        "buy 0.1 ETH at 2999.50: rests",
        "place_limit_order",
        r#"{"side": "buy", "price_usdc": "2999.50", "quantity_eth": "0.1"}"#,
    ),
    (
        "act",
        "sell 0.2 ETH at 2999.00: fills",
        "place_limit_order",
        r#"{"side": "sell", "price_usdc": "2999.00", "quantity_eth": "0.2"}"#,
    ),
    ("act", "cancel all", "cancel_all_orders", "{}"),
    (
        "refused by policy",
        "buy 100 ETH: size limit",
        "place_limit_order",
        r#"{"side": "buy", "price_usdc": "3000.00", "quantity_eth": "100"}"#,
    ),
    (
        "refused by policy",
        "buy 0.1 ETH at 5000: price collar",
        "place_limit_order",
        r#"{"side": "buy", "price_usdc": "5000.00", "quantity_eth": "0.1"}"#,
    ),
];

fn preset_group(group: &str) -> String {
    let buttons: String = PRESETS
        .iter()
        .filter(|(g, ..)| *g == group)
        .map(|(_, label, tool, args)| {
            format!(
                "<button type=\"button\" class=\"small\" data-tool=\"{tool}\" data-args=\"{}\" data-go>{label}</button>",
                esc(args)
            )
        })
        .collect();
    format!("<div class=\"group\"><span class=\"lbl\">{group}</span>{buttons}</div>")
}

pub fn section(app: &App) -> String {
    let cfg = app.policy.config();
    let tools = html::panel(
        "Tools",
        "tools/list, as the model receives it",
        "The eleven tools with the name, description and JSON schema the model reads. Arguments and results are in human units, USDC with two decimals and ETH with four, as strings. Click a name to load it into the form with example arguments; the full description is shown on hover.",
        &html::live("mcp-tools", "/ui/mcp/tools", "30s", "never"),
    );
    let call = html::panel(
        "Call a tool",
        "tools/call over JSON-RPC",
        "One JSON-RPC 2.0 request on the Streamable HTTP endpoint, as a client sends it. A refusal by the policy comes back as an ordinary result marked <code>rejected</code>, with a code and a hint that tell the model what to do instead; tool errors are kept for bad arguments and outages. Every call moves the panels on this page as it would move the model's view.",
        &format!(
            r##"{reads}{actions}{refusals}
<form hx-post="/ui/mcp/call" hx-target="#mcp-result" hx-indicator="#mcp-ind" class="note">
  <div class="actions"><input type="text" id="tool" name="tool" value="get_market_summary" class="grow" spellcheck="false"><button type="submit" class="accent">call</button><span id="mcp-ind" class="htmx-indicator">calling</span></div>
  <textarea id="args" name="args" spellcheck="false" class="short">{{}}</textarea>
</form>
<div id="mcp-result" class="result"></div>"##,
            reads = preset_group("read"),
            actions = preset_group("act"),
            refusals = preset_group("refused by policy"),
        ),
    );
    let policy = html::panel(
        "Policy",
        "policy.rs, before the engine",
        "Deterministic rules applied to every action before it reaches the engine, each answered with a code and a hint: MAX_ORDER_SIZE, MAX_ORDER_VALUE, PRICE_COLLAR, MAX_OPEN_ORDERS, RATE_LIMIT, SESSION_CAP and HALTED. The engine adds its own checks underneath: balances back every order, and self-trades are prevented.",
        &format!(
            r##"<dl class="kv">
  <dt>largest order</dt><dd class="num">{max_eth} ETH</dd>
  <dt>largest order value</dt><dd class="num">{max_usdc} USDC</dd>
  <dt>price collar</dt><dd class="num">{collar} bps from mid</dd>
  <dt>open orders per account</dt><dd class="num">{max_open}</dd>
  <dt>actions per minute</dt><dd class="num">{rate}</dd>
  <dt>session notional cap</dt><dd class="num">{cap} USDC</dd>
  <dt>trading halted</dt><dd>{halted}</dd>
</dl>"##,
            max_eth = eth(cfg.max_order_lots),
            max_usdc = usdc_from_micro(cfg.max_order_notional_micro),
            collar = cfg.collar_bps,
            max_open = cfg.max_open_orders,
            rate = cfg.actions_per_minute,
            cap = usdc_from_micro(cfg.session_notional_cap_micro),
            halted = if cfg.halted {
                chip("bad", "yes")
            } else {
                chip("ok", "no")
            },
        ),
    );
    let resources = html::panel(
        "Resources and prompt",
        "resources/read, prompts/get",
        "Three resources, a template that takes a depth, and the standing instructions a client may load as a prompt. Clients may subscribe to a resource; the stdio transport pushes <code>notifications/resources/updated</code> when the engine's events touch it.",
        &format!(
            "{}<div id=\"resource-result\" class=\"result\"></div>{}",
            html::live("mcp-resources", "/ui/mcp/resources", "30s", "never"),
            html::live("mcp-prompt", "/ui/mcp/prompt", "300s", "never")
        ),
    );
    let body = format!(
        r##"<div class="cols even">
  <div class="stack">{tools}{policy}</div>
  <div class="stack">{call}{resources}</div>
</div>"##
    );
    html::tab(
        "mcp",
        "MCP server",
        "eleven tools, three resources and a prompt over Streamable HTTP",
        "The model perceives and acts only through these tools, and a deterministic policy checks every action before it reaches the engine.",
        "The same server speaks stdio for Claude Desktop and Claude Code, and is checked against the official MCP client in CI. The protocol layer is written by hand on top of serde_json: JSON-RPC 2.0 with a small set of methods.",
        &body,
        true,
    )
}

pub async fn tools(app: &App) -> anyhow::Result<Html> {
    let tools = app.mcp.list_tools().await?;
    let mut out = String::from("<ul class=\"list tools\">");
    for t in &tools {
        let name = t["name"].as_str().unwrap_or("?");
        let desc = t["description"].as_str().unwrap_or("");
        let first_sentence = desc.split(". ").next().unwrap_or(desc);
        let required: Vec<&str> = t["inputSchema"]["required"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        out.push_str(&format!(
            "<li><span class=\"k\"><a href=\"#mcp\" data-tool=\"{n}\" data-args=\"{a}\">{n}</a></span><span title=\"{d}\">{s}{r}</span></li>",
            n = esc(name),
            a = esc(example_args(name)),
            d = esc(desc),
            s = esc(first_sentence),
            r = if required.is_empty() {
                String::new()
            } else {
                format!(" <span class=\"muted\">needs {}</span>", esc(&required.join(", ")))
            }
        ));
    }
    out.push_str(&format!(
        "</ul><p class=\"muted small\">{} tools. Click a name to load it into the form with example arguments; the full description each tool carries is shown on hover.</p>",
        tools.len()
    ));
    Ok(html::html(out))
}

pub async fn call(app: &App, form: &HashMap<String, String>) -> anyhow::Result<Html> {
    let tool = form.get("tool").map(|s| s.trim()).unwrap_or("");
    let raw = form
        .get("args")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("{}");
    let args: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return Ok(html::error(&format!("arguments are not valid JSON: {e}"))),
    };
    let params = json!({ "name": tool, "arguments": args });
    let t0 = Instant::now();
    let outcome = app.mcp.request("tools/call", params.clone()).await;
    let ms = t0.elapsed().as_millis();
    let request = serde_json::to_string_pretty(&json!({ "jsonrpc": "2.0", "method": "tools/call", "params": params }))?;
    let mut out = String::new();
    match outcome {
        Ok(result) => {
            let is_error = result["isError"].as_bool().unwrap_or(false);
            let structured = result.get("structuredContent");
            let rejected = structured
                .filter(|s| s["rejected"].as_bool().unwrap_or(false))
                .map(|s| s["code"].as_str().unwrap_or("REJECTED").to_string());
            let text: String = result["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            // A policy rejection is a normal result with `rejected: true`: data the model reads and
            // acts on, not a protocol error. A tool error is reserved for bad arguments and outages.
            let verdict = match (&rejected, is_error) {
                (Some(code), _) => chip("warn", &format!("rejected by policy: {code}")),
                (None, true) => chip("bad", "tool error"),
                (None, false) => chip("ok", "ok"),
            };
            out.push_str(&format!(
                "<p>{verdict} <span class=\"muted small\">{ms} ms round trip</span></p>"
            ));
            if let Some(s) = structured.filter(|s| s["rejected"].as_bool().unwrap_or(false)) {
                out.push_str(&format!(
                    "<p>{}<br><span class=\"muted\">hint for the model: {}</span></p>",
                    esc(s["message"].as_str().unwrap_or("")),
                    esc(s["hint"].as_str().unwrap_or(""))
                ));
            }
            match structured {
                Some(s) => out.push_str(&format!(
                    "<details open><summary>structuredContent</summary><pre>{}</pre></details>",
                    esc(&serde_json::to_string_pretty(s)?)
                )),
                None => out.push_str(&format!("<pre>{}</pre>", esc(&text))),
            }
        }
        Err(e) => out.push_str(&format!(
            "<p>{} <span class=\"muted small\">{ms} ms</span></p><pre>{}</pre>",
            chip("bad", "JSON-RPC error"),
            esc(&e.to_string())
        )),
    }
    out.push_str(&format!(
        "<details><summary>request as sent</summary><pre>{}</pre></details>",
        esc(&request)
    ));
    // Any tool may have moved the book; the engine panels refresh at once.
    Ok(html::html_trigger(out, "engine"))
}

pub async fn resources(app: &App) -> anyhow::Result<Html> {
    let listed = app.mcp.request("resources/list", json!({})).await?;
    let mut out = String::from("<ul class=\"list\">");
    for r in listed["resources"].as_array().into_iter().flatten() {
        let uri = r["uri"].as_str().unwrap_or("");
        out.push_str(&format!(
            "<li><span class=\"k\">{}</span><span><code>{}</code> <button type=\"button\" class=\"small\" hx-post=\"/ui/mcp/read\" hx-vals='{{\"uri\": \"{}\"}}' hx-target=\"#resource-result\">read</button></span></li>",
            esc(r["name"].as_str().unwrap_or("")),
            esc(uri),
            esc(uri)
        ));
    }
    out.push_str(
        "<li><span class=\"k\">order_book_depth</span><span><code>market://ETH-USDC/book/{depth}</code> <button type=\"button\" class=\"small\" hx-post=\"/ui/mcp/read\" hx-vals='{\"uri\": \"market://ETH-USDC/book/10\"}' hx-target=\"#resource-result\">read depth 10</button></span></li></ul><p class=\"muted small\">Clients may subscribe to a resource; the stdio transport pushes <code>notifications/resources/updated</code> when the engine's events touch it.</p>",
    );
    Ok(html::html(out))
}

pub async fn read(app: &App, form: &HashMap<String, String>) -> anyhow::Result<Html> {
    let uri = form.get("uri").map(String::as_str).unwrap_or("");
    let t0 = Instant::now();
    let result = app.mcp.request("resources/read", json!({ "uri": uri })).await;
    let ms = t0.elapsed().as_millis();
    Ok(html::html(match result {
        Ok(r) => {
            let text = r["contents"][0]["text"].as_str().unwrap_or("");
            let pretty = serde_json::from_str::<Value>(text)
                .and_then(|v| serde_json::to_string_pretty(&v))
                .unwrap_or_else(|_| text.to_string());
            format!(
                "<p class=\"muted small\"><code>{}</code> · {ms} ms</p><pre>{}</pre>",
                esc(uri),
                esc(&pretty)
            )
        }
        Err(e) => format!(
            "<p>{}</p><pre>{}</pre>",
            chip("bad", "JSON-RPC error"),
            esc(&e.to_string())
        ),
    }))
}

pub async fn prompt(app: &App) -> anyhow::Result<Html> {
    let r = app
        .mcp
        .request("prompts/get", json!({ "name": "trading_assistant", "arguments": {} }))
        .await?;
    let text = r["messages"][0]["content"]["text"].as_str().unwrap_or("");
    Ok(html::html(format!(
        "<details><summary>prompt <code>trading_assistant</code>: the standing instructions a client may load ({} words)</summary><pre class=\"prewrap\">{}</pre></details>",
        text.split_whitespace().count(),
        esc(text)
    )))
}
