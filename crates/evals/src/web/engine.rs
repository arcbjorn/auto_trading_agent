//! The engine's panels, all from gRPC: market, ladder, tape, wallets and statement, statistics,
//! the event stream. Plus a reset that restores the demo book and a load test that shows
//! concurrent placement with the invariants checked afterwards.

use super::App;
use super::html::{self, chip, esc, thousands};
use crate::harness::{ACCOUNT, MAKER};
use bytes::Bytes;
use clob_proto::v1::engine_client::EngineClient;
use clob_proto::v1::engine_event::Event;
use clob_proto::v1::{
    CancelAllOrdersRequest, DepositRequest, EngineEvent, GetBalancesRequest, GetMarketRequest, GetOrderBookRequest,
    GetStatementRequest, GetStatsRequest, ListTradesRequest, PlaceOrderRequest, Side, SubscribeRequest, TimeInForce,
};
use http_body_util::Full;
use hyper::Response;
use mcp_server::units::{eth, parse_price, parse_qty, signed_usdc_from_micro, usdc, usdc_from_micro};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

type Html = Response<Full<Bytes>>;

/// Proto prices and quantities are `int64`; the engine never emits a negative one.
fn u(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn side_name(side: i32) -> &'static str {
    if side == Side::Buy as i32 { "buy" } else { "sell" }
}

pub struct EventRow {
    pub seq: u64,
    pub kind: &'static str,
    pub text: String,
}

const KEPT_EVENTS: usize = 60;
const SHOWN_EVENTS: usize = 10;
const DEPTH: u32 = 8;

pub fn section() -> String {
    let book = html::panel(
        "Order book",
        &format!("GetOrderBook, depth {DEPTH}"),
        "Resting liquidity by price level, best first on each side: teal bids, orange asks, bars proportional to size. Prices are integer ticks (0.01 USDC) and quantities integer lots (0.0001 ETH); matching is price-time priority, a self-trade is prevented, and every level is backed by its account's balance. Reset cancels every open order of every account this process funded and rests the demo levels again; balances stay.",
        &format!(
            "{}<div class=\"actions\"><button type=\"button\" class=\"small\" hx-post=\"/ui/engine/reset\" hx-target=\"#reset-result\">reset book</button><span id=\"reset-result\" class=\"muted small\"></span></div>",
            html::live("book", "/ui/engine/book", "1s", "engine")
        ),
    );
    let trades = html::panel(
        "Trades",
        "ListTrades, newest first",
        "Every fill with its sequence number, the taker's side and both accounts. A trade prints at the maker's price, whatever the taker's limit was.",
        &html::live("trades", "/ui/engine/trades", "1s", "engine"),
    );
    let events = html::panel(
        "Event stream",
        "Subscribe",
        "The engine's event stream as a client receives it: accepted, trade, cancelled, rejected, deposit and withdrawal, each with the sequence number that replaying the journal reproduces. The newest few dozen are kept.",
        &html::live("events", "/ui/engine/events", "1s", "engine"),
    );
    let accounts = html::panel(
        "Wallets and statement",
        "GetBalances, GetStatement",
        "Available and reserved USDC and ETH per account. Reserved backs the account's live orders and is released on a cancel or a fill. The statement is the demo account's ledger on this venue: what was bought and sold, the average cost of the inventory bought here, and the realised P&amp;L.",
        &html::live("accounts", "/ui/engine/accounts", "1s", "engine"),
    );
    let load = html::panel(
        "Concurrent placement",
        "PlaceOrder from N connections",
        "Each client is its own gRPC connection placing GTC orders inside the current spread, so they trade with each other and leave the demo levels alone; their leftovers are cancelled afterwards. Then the book must not be crossed, and USDC and ETH must be conserved across every account: the sum of the wallets equals the sum of the deposits.",
        &format!(
            r##"<form hx-post="/ui/engine/load" hx-target="#load-result" hx-indicator="#load-ind" class="actions">
  <label class="inline">clients <input type="text" name="clients" value="8"></label>
  <label class="inline">orders each <input type="text" name="per" value="1000"></label>
  <button class="accent" type="submit">fire</button>
  {running}
</form>
<div id="load-result" class="result flow scrollbox small"></div>"##,
            running = html::indicator("load-ind", "running"),
        ),
    );
    let stats = format!(
        "<details class=\"panel\"><summary><h3><span class=\"t\">Engine statistics</span><span class=\"src\">GetStats, click to open</span></h3></summary>{}</details>",
        html::live("stats", "/ui/engine/stats", "1s", "engine")
    );
    let body = format!(
        r##"<div class="cols even">
  <div class="stack">{book}{trades}{events}</div>
  <div class="stack">{accounts}{load}{stats}</div>
</div>"##
    );
    html::tab(
        "engine",
        "Matching engine",
        "deterministic ETH/USDC limit order book behind gRPC",
        "One thread owns the book, commands arrive over a bounded channel and are applied in batches, and every event carries a sequence number.",
        "Wallets back every order and self-trades are prevented inside the matcher. A write-ahead journal with snapshot compaction makes a restart replay to the same state, and the sequence numbers are what that replay reproduces. These panels poll the gRPC API once a second and refresh at once after any action on this page.",
        &body,
        false,
    )
}

/// The market bar: the line under the header that every action on the page moves.
pub async fn market(app: &App) -> anyhow::Result<Html> {
    let mut engine = app.engine.clone();
    let m = engine.get_market(GetMarketRequest {}).await?.into_inner();
    let w = engine
        .get_balances(GetBalancesRequest {
            account_id: ACCOUNT.into(),
        })
        .await?
        .into_inner();
    let (bid, ask) = (u(m.best_bid_ticks), u(m.best_ask_ticks));
    let price = |t: u64| if t == 0 { "–".to_string() } else { usdc(t) };
    let (spread, mid) = if bid > 0 && ask > 0 {
        (usdc(ask.saturating_sub(bid)), mcp_server::units::mid(bid, ask))
    } else {
        ("–".into(), "–".into())
    };
    let cell = |k: &str, v: String, class: &str| {
        format!("<span><span class=\"k\">{k}</span><b class=\"num {class}\">{v}</b></span>")
    };
    // Reserved amounts back live orders; they are shown only while there are some.
    let reserved = match (w.usdc_reserved_micro, w.eth_reserved_lots) {
        (0, 0) => String::new(),
        (usdc_r, 0) => format!(
            " <span class=\"muted\">({} USDC reserved)</span>",
            usdc_from_micro(u128::from(usdc_r))
        ),
        (0, eth_r) => format!(" <span class=\"muted\">({} ETH reserved)</span>", eth(eth_r)),
        (usdc_r, eth_r) => format!(
            " <span class=\"muted\">({} USDC, {} ETH reserved)</span>",
            usdc_from_micro(u128::from(usdc_r)),
            eth(eth_r)
        ),
    };
    Ok(html::html(format!(
        "{}{}{}{}{}{}<span class=\"wallet\"><span class=\"k\">{ACCOUNT} wallet</span><b class=\"num\">{} USDC</b> · <b class=\"num\">{} ETH</b>{reserved}</span>",
        cell("best bid", price(bid), "bid"),
        cell("best ask", price(ask), "ask"),
        cell("spread", spread, ""),
        cell("mid", mid, ""),
        cell("last trade", price(u(m.last_trade_price_ticks)), ""),
        cell("sequence", thousands(m.sequence), ""),
        usdc_from_micro(u128::from(w.usdc_available_micro)),
        eth(w.eth_available_lots),
    )))
}

pub async fn book(app: &App) -> anyhow::Result<Html> {
    book_depth(app, DEPTH).await
}

pub async fn book_depth(app: &App, depth: u32) -> anyhow::Result<Html> {
    let b = app
        .engine
        .clone()
        .get_order_book(GetOrderBookRequest {
            depth: depth.clamp(1, 20),
        })
        .await?
        .into_inner();
    let max = b
        .bids
        .iter()
        .chain(b.asks.iter())
        .map(|l| u(l.quantity_lots))
        .max()
        .unwrap_or(1)
        .max(1);
    let row = |class: &str, price: i64, qty: i64, count: u32| {
        let width = 100 * u(qty) / max;
        format!(
            "<tr class=\"{class}\"><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{count}</td><td class=\"bar {class}\"><span style=\"width:{width}%\"></span></td></tr>",
            usdc(u(price)),
            eth(u(qty))
        )
    };
    // A fixed number of slots a side: the ladder keeps its height whatever the book holds.
    let slots = depth.clamp(1, 20) as usize;
    let empty = "<tr class=\"empty\"><td class=\"num\">–</td><td></td><td></td><td></td></tr>";
    let mut out = String::from(
        "<table class=\"ladder\"><thead><tr><th>price USDC</th><th>ETH</th><th>orders</th><th></th></tr></thead><tbody>",
    );
    for _ in b.asks.len()..slots {
        out.push_str(empty);
    }
    for l in b.asks.iter().rev() {
        out.push_str(&row("ask", l.price_ticks, l.quantity_lots, l.order_count));
    }
    let spread = match (b.bids.first(), b.asks.first()) {
        (Some(bid), Some(ask)) => format!(
            "spread {} · mid {}",
            usdc(u(ask.price_ticks).saturating_sub(u(bid.price_ticks))),
            mcp_server::units::mid(u(bid.price_ticks), u(ask.price_ticks))
        ),
        (None, None) => "the book is empty: reset it, or place an order through the tools or the agent".to_string(),
        _ => "one side of the book is empty".to_string(),
    };
    out.push_str(&format!("<tr class=\"spread\"><td colspan=\"4\">{spread}</td></tr>"));
    for l in &b.bids {
        out.push_str(&row("bid", l.price_ticks, l.quantity_lots, l.order_count));
    }
    for _ in b.bids.len()..slots {
        out.push_str(empty);
    }
    out.push_str("</tbody></table>");
    Ok(html::html(out))
}

pub async fn trades(app: &App) -> anyhow::Result<Html> {
    trades_limit(app, 10).await
}

pub async fn trades_limit(app: &App, limit: u32) -> anyhow::Result<Html> {
    let t = app
        .engine
        .clone()
        .list_trades(ListTradesRequest {
            account_id: String::new(),
            limit: limit.clamp(1, 50),
        })
        .await?
        .into_inner();
    let mut out = String::from(
        "<table><thead><tr><th>seq</th><th>price</th><th>ETH</th><th class=\"l\">taker</th><th class=\"l\">maker</th></tr></thead><tbody>",
    );
    if t.trades.is_empty() {
        out.push_str("<tr class=\"empty\"><td colspan=\"5\" class=\"l muted\">no trades yet</td></tr>");
    }
    for tr in &t.trades {
        let side = side_name(tr.taker_side);
        let class = if side == "buy" { "bid" } else { "ask" };
        out.push_str(&format!(
            "<tr><td class=\"num muted\">{}</td><td class=\"num {class}\">{}</td><td class=\"num\">{}</td><td class=\"l\">{} <span class=\"{class}\">{side}</span></td><td class=\"l\">{}</td></tr>",
            tr.sequence,
            usdc(u(tr.price_ticks)),
            eth(u(tr.quantity_lots)),
            esc(&tr.taker_account),
            esc(&tr.maker_account)
        ));
    }
    // The tape keeps the height of a full page of trades.
    for _ in t.trades.len().max(1)..limit.clamp(1, 50) as usize {
        out.push_str("<tr class=\"empty\"><td class=\"num\">–</td><td></td><td></td><td></td><td></td></tr>");
    }
    out.push_str("</tbody></table>");
    Ok(html::html(out))
}

pub async fn accounts(app: &App) -> anyhow::Result<Html> {
    let mut engine = app.engine.clone();
    let mut out = String::from("<dl class=\"kv\">");
    for account in [ACCOUNT, MAKER] {
        let b = engine
            .get_balances(GetBalancesRequest {
                account_id: account.into(),
            })
            .await?
            .into_inner();
        out.push_str(&format!(
            "<dt>{account}</dt><dd class=\"num\">{} USDC <span class=\"muted\">({} reserved)</span><br>{} ETH <span class=\"muted\">({} reserved)</span></dd>",
            usdc_from_micro(u128::from(b.usdc_available_micro)),
            usdc_from_micro(u128::from(b.usdc_reserved_micro)),
            eth(b.eth_available_lots),
            eth(b.eth_reserved_lots)
        ));
    }
    out.push_str("</dl>");
    let s = engine
        .get_statement(GetStatementRequest {
            account_id: ACCOUNT.into(),
        })
        .await?
        .into_inner();
    let inventory = if s.inventory_lots > 0 {
        let avg = mcp_server::units::average_price(u128::from(s.inventory_cost_micro), s.inventory_lots)
            .map(usdc)
            .unwrap_or_else(|| "?".into());
        format!("{} ETH at {avg} average", eth(s.inventory_lots))
    } else {
        "none bought here".to_string()
    };
    out.push_str(&format!(
        r#"<p class="muted small note">statement for <b>{ACCOUNT}</b></p><dl class="kv">
<dt>trades</dt><dd class="num">{}</dd>
<dt>bought / sold</dt><dd class="num">{} / {} ETH</dd>
<dt>USDC paid / received</dt><dd class="num">{} / {}</dd>
<dt>inventory</dt><dd class="num">{inventory}</dd>
<dt>realised P&amp;L</dt><dd class="num">{} USDC</dd>
<dt>deposited</dt><dd class="num">{} USDC, {} ETH</dd>
</dl>"#,
        s.trades,
        eth(s.bought_lots),
        eth(s.sold_lots),
        usdc_from_micro(u128::from(s.usdc_paid_micro)),
        usdc_from_micro(u128::from(s.usdc_received_micro)),
        signed_usdc_from_micro(i128::from(s.realised_pnl_micro)),
        usdc_from_micro(u128::from(s.deposits_usdc_micro)),
        eth(s.deposits_eth_lots),
    ));
    Ok(html::html(out))
}

pub async fn stats(app: &App) -> anyhow::Result<Html> {
    let s = app.engine.clone().get_stats(GetStatsRequest {}).await?.into_inner();
    let up = app.started.elapsed().as_secs();
    Ok(html::html(format!(
        r#"<dl class="kv">
<dt>commands applied <span class="muted">(reads included)</span></dt><dd class="num">{}</dd>
<dt>matcher batches</dt><dd class="num">{} (largest {})</dd>
<dt>events published</dt><dd class="num">{}</dd>
<dt>sequence</dt><dd class="num">{}</dd>
<dt>command queue</dt><dd class="num">{} of {} free</dd>
<dt>orders retained</dt><dd class="num">{}</dd>
<dt>trades retained</dt><dd class="num">{}</dd>
<dt>up</dt><dd class="num">{}m {:02}s</dd>
</dl>"#,
        thousands(s.commands),
        thousands(s.batches),
        s.max_batch,
        thousands(s.events),
        thousands(s.sequence),
        s.queue_free,
        s.queue_capacity,
        thousands(s.orders_retained),
        thousands(s.trades_retained),
        up / 60,
        up % 60
    )))
}

pub fn events(app: &App) -> Html {
    let events = app.events.lock().expect("events lock");
    if events.is_empty() {
        return html::html(
            "<p class=\"muted small\">No events since this page opened its stream. Any order, cancel or fill appears here with its sequence number.</p>".into(),
        );
    }
    let mut out = String::from("<ul class=\"list\">");
    for e in events.iter().rev().take(SHOWN_EVENTS) {
        let class = match e.kind {
            "trade" => "bid",
            "rejected" => "ask",
            _ => "muted",
        };
        out.push_str(&format!(
            "<li><span class=\"seq num\">{}</span><span class=\"k {class}\">{}</span><span>{}</span></li>",
            e.seq,
            e.kind,
            esc(&e.text)
        ));
    }
    out.push_str("</ul>");
    html::html(out)
}

/// Follows the engine's event stream for the event panel; reconnects if the stream lags out.
pub async fn watch_events(app: Arc<App>) {
    loop {
        let mut engine = app.engine.clone();
        if let Ok(stream) = engine
            .subscribe(SubscribeRequest {
                account_id: String::new(),
            })
            .await
        {
            let mut stream = stream.into_inner();
            while let Ok(Some(ev)) = stream.message().await {
                if let Some(row) = describe(&ev) {
                    let mut events = app.events.lock().expect("events lock");
                    if events.len() >= KEPT_EVENTS {
                        events.pop_front();
                    }
                    events.push_back(row);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn describe(ev: &EngineEvent) -> Option<EventRow> {
    let (kind, text) = match ev.event.as_ref()? {
        Event::Accepted(o) => (
            "accepted",
            format!(
                "{} {} ETH at {} for {}",
                side_name(o.side),
                eth(u(o.quantity_lots)),
                usdc(u(o.price_ticks)),
                o.account_id
            ),
        ),
        Event::Traded(t) => (
            "trade",
            format!(
                "{} ETH at {}: {} {} from {}",
                eth(u(t.quantity_lots)),
                usdc(u(t.price_ticks)),
                t.taker_account,
                if t.taker_side == Side::Buy as i32 {
                    "bought"
                } else {
                    "sold to"
                },
                t.maker_account
            ),
        ),
        Event::Cancelled(c) => (
            "cancelled",
            format!("order {} of {}: {}", c.order_id, c.account_id, c.reason),
        ),
        Event::Rejected(r) => (
            "rejected",
            format!("order {} of {}: {}", r.order_id, r.account_id, r.reason),
        ),
        Event::Deposited(t) => (
            "deposit",
            format!(
                "{} USDC and {} ETH to {}",
                usdc_from_micro(u128::from(t.usdc_micro)),
                eth(t.eth_lots),
                t.account_id
            ),
        ),
        Event::Withdrawn(t) => (
            "withdrawal",
            format!(
                "{} USDC and {} ETH from {}",
                usdc_from_micro(u128::from(t.usdc_micro)),
                eth(t.eth_lots),
                t.account_id
            ),
        ),
    };
    Some(EventRow {
        seq: ev.sequence,
        kind,
        text,
    })
}

/// Cancels every open order of every account this process funded and rests the demo levels again.
pub async fn reset(app: &App) -> anyhow::Result<Html> {
    let accounts: Vec<String> = app.accounts.lock().expect("accounts lock").iter().cloned().collect();
    let mut engine = app.engine.clone();
    let mut cancelled = 0;
    for account in accounts {
        let r = engine
            .cancel_all_orders(CancelAllOrdersRequest { account_id: account })
            .await?;
        cancelled += r.into_inner().orders.len();
    }
    let n = app.reseeds.fetch_add(1, Ordering::Relaxed) + 1;
    let book = crate::demo::seed_book();
    let mut placed = 0;
    for (side, levels) in [(Side::Buy, &book.bids), (Side::Sell, &book.asks)] {
        for [price, qty] in levels {
            placed += 1;
            engine
                .place_order(PlaceOrderRequest {
                    account_id: MAKER.into(),
                    client_order_id: format!("reseed-{n}-{placed}"),
                    side: side as i32,
                    price_ticks: i64::try_from(parse_price(price).map_err(anyhow::Error::msg)?)?,
                    quantity_lots: i64::try_from(parse_qty(qty).map_err(anyhow::Error::msg)?)?,
                    tif: TimeInForce::Gtc as i32,
                })
                .await?;
        }
    }
    Ok(html::html_trigger(
        format!("cancelled {cancelled} open orders, rested {placed} maker levels again"),
        "engine",
    ))
}

/// N connections each placing M GTC orders inside the spread, then the invariants.
pub async fn load(app: &App, form: &HashMap<String, String>) -> anyhow::Result<Html> {
    let number = |key: &str, default: u64, max: u64| -> u64 {
        form.get(key)
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(default)
            .clamp(1, max)
    };
    let clients = number("clients", 8, 32);
    let per = number("per", 1_000, 20_000);
    let mut engine = app.engine.clone();

    // Prices inside the current spread, so the load trades with itself and not with the demo book.
    let m = engine.get_market(GetMarketRequest {}).await?.into_inner();
    let (bid, ask) = (u(m.best_bid_ticks), u(m.best_ask_ticks));
    let (lo, hi) = if bid > 0 && ask > bid + 2 {
        (bid + 1, ask - 1)
    } else {
        (299_901, 300_099)
    };
    let width = (hi - lo + 1).min(50);

    let names: Vec<String> = (0..clients)
        .map(|t| format!("load-{}{t}", if t % 2 == 0 { "b" } else { "s" }))
        .collect();
    let fresh: Vec<String> = {
        let mut known = app.accounts.lock().expect("accounts lock");
        names.iter().filter(|n| known.insert((*n).clone())).cloned().collect()
    };
    for account in fresh {
        engine
            .deposit(DepositRequest {
                account_id: account,
                usdc_micro: 1_000_000_000_000_000,
                eth_lots: 10_000_000_000,
            })
            .await?;
    }
    let before = engine.get_stats(GetStatsRequest {}).await?.into_inner();
    let url = format!("http://{}", app.engine_addr);
    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for (t, account) in names.iter().enumerate() {
        let (url, account) = (url.clone(), account.clone());
        let t = t as u64;
        tasks.push(tokio::spawn(async move {
            let mut c = EngineClient::connect(url).await?;
            let mut lat = Vec::with_capacity(per as usize);
            let mut filled = 0u64;
            for i in 0..per {
                let t1 = Instant::now();
                let r = c
                    .place_order(PlaceOrderRequest {
                        account_id: account.clone(),
                        client_order_id: format!("load-{t}-{i}-{}", t0.elapsed().as_nanos()),
                        side: if t.is_multiple_of(2) { Side::Buy } else { Side::Sell } as i32,
                        price_ticks: i64::try_from(lo + (t + i) % width)?,
                        quantity_lots: 100,
                        tif: TimeInForce::Gtc as i32,
                    })
                    .await?;
                lat.push(t1.elapsed().as_micros() as u64);
                if r.into_inner().order.is_some_and(|o| o.remaining_lots < o.quantity_lots) {
                    filled += 1;
                }
            }
            anyhow::Ok((lat, filled))
        }));
    }
    let mut lat = Vec::new();
    let mut filled = 0;
    for task in tasks {
        let (l, f) = task.await??;
        lat.extend(l);
        filled += f;
    }
    let elapsed = t0.elapsed();
    lat.sort_unstable();
    let pct = |p: f64| lat[((lat.len() - 1) as f64 * p).round() as usize];
    let after = engine.get_stats(GetStatsRequest {}).await?.into_inner();

    for account in &names {
        engine
            .cancel_all_orders(CancelAllOrdersRequest {
                account_id: account.clone(),
            })
            .await?;
    }
    let m = engine.get_market(GetMarketRequest {}).await?.into_inner();
    let (bid, ask) = (u(m.best_bid_ticks), u(m.best_ask_ticks));
    let not_crossed = bid == 0 || ask == 0 || bid < ask;
    let (mut bal_usdc, mut bal_eth, mut dep_usdc, mut dep_eth) = (0u128, 0u128, 0i128, 0i128);
    let accounts: Vec<String> = app.accounts.lock().expect("accounts lock").iter().cloned().collect();
    for account in &accounts {
        let b = engine
            .get_balances(GetBalancesRequest {
                account_id: account.clone(),
            })
            .await?
            .into_inner();
        let s = engine
            .get_statement(GetStatementRequest {
                account_id: account.clone(),
            })
            .await?
            .into_inner();
        bal_usdc += u128::from(b.usdc_available_micro) + u128::from(b.usdc_reserved_micro);
        bal_eth += u128::from(b.eth_available_lots) + u128::from(b.eth_reserved_lots);
        dep_usdc += i128::from(s.deposits_usdc_micro) - i128::from(s.withdrawals_usdc_micro);
        dep_eth += i128::from(s.deposits_eth_lots) - i128::from(s.withdrawals_eth_lots);
    }
    let usdc_ok = i128::try_from(bal_usdc).is_ok_and(|b| b == dep_usdc);
    let eth_ok = i128::try_from(bal_eth).is_ok_and(|b| b == dep_eth);
    let check = |ok: bool, what: &str| {
        if ok {
            chip("ok", &format!("{what}: holds"))
        } else {
            chip("bad", &format!("{what}: VIOLATED"))
        }
    };
    let total = clients * per;
    Ok(html::html_trigger(
        format!(
            r#"<p><b>{} orders</b> from {clients} connections in <b>{:.0} ms</b>: <b>{} orders/s</b>; round trip p50 {} µs, p99 {} µs; {} of them filled at least partly.</p>
<p class="muted">engine: +{} commands in +{} batches (largest batch so far {}), +{} events, +{} trades</p>
<p>{} {} {}</p>"#,
            thousands(total),
            elapsed.as_secs_f64() * 1_000.0,
            thousands((total as f64 / elapsed.as_secs_f64()) as u64),
            pct(0.50),
            pct(0.99),
            thousands(filled),
            thousands(after.commands - before.commands),
            thousands(after.batches - before.batches),
            after.max_batch,
            thousands(after.events - before.events),
            thousands(after.trades_retained.saturating_sub(before.trades_retained)),
            check(not_crossed, "book not crossed"),
            check(usdc_ok, &format!("USDC conserved across {} accounts", accounts.len())),
            check(eth_ok, "ETH conserved"),
        ),
        "engine",
    ))
}
