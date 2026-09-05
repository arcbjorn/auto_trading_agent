//! Experimental: give the agent a goal on the live book. The brief asks for a supervised
//! natural-language service and an evaluation that includes a simulation; letting the model act
//! on a goal against the live book goes beyond it, and is marked as such on the page and in the
//! docs. The simulation's market-maker bot and taker trade on this process's engine while the
//! agent, alone with the goal, decides each round; the cockpit's book, trades and wallet move as
//! it happens, and every action lands in the audit log.
//!
//! This is `evals sim` re-based onto the shared engine: the same bot, the same goal, the same
//! rule check, but the account starts from whatever it holds, so P&L is the wallet's change over
//! the run marked at the final mid.

use super::App;
use super::evals::{Job, Output, new_id, register};
use super::html::{self, chip, esc};
use crate::harness::{self, ACCOUNT, MAKER};
use crate::sim::{self, Bot, COLLAR_BPS, GOAL_LOTS, MAX_PRICE_TICKS, UNLIMITED_ETH_LOTS, UNLIMITED_USDC_MICRO};
use agent_service::{Agent, AgentConfig, McpClient, ModelClient, NoteChannel, Session};
use bytes::Bytes;
use clob_proto::v1::engine_client::EngineClient;
use clob_proto::v1::{
    DepositRequest, GetBalancesRequest, GetMarketRequest, GetOrderBookRequest, PlaceOrderRequest, Side, TimeInForce,
};
use http_body_util::Full;
use hyper::Response;
use mcp_server::units::{eth, parse_price, parse_qty, signed_usdc_from_micro, usdc};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tonic::transport::Channel;

type Html = Response<Full<Bytes>>;

pub struct RoundRow {
    pub round: u32,
    /// What the agent did, in its own words for the model and in ours for the baseline.
    pub action: String,
    pub tool_calls: usize,
    pub held_eth: String,
    pub avg_cost: String,
    pub mid: String,
    pub violations: u32,
}

pub struct Summary {
    pub filled_eth: String,
    pub avg_cost: String,
    pub final_mid: String,
    pub pnl: String,
    pub goal: bool,
    pub violations: u32,
    pub tool_calls: usize,
    pub bot_quotes_cancelled: usize,
}

#[derive(Default)]
pub struct Live {
    pub rounds: Vec<RoundRow>,
    pub summary: Option<Summary>,
}

pub fn panel(app: &App) -> String {
    let (model_option, baseline_selected) = match &app.model {
        Ok(_) => (
            "<option value=\"model\">the model, on its own (experimental)</option>",
            "",
        ),
        Err(_) => (
            "<option value=\"model\" disabled>the model (needs a key)</option>",
            " selected",
        ),
    };
    format!(
        r##"<div class="panel"><h3>Give the agent a goal <span class="chip warn" style="margin-left:.4rem">experimental</span><span class="right muted">autonomous rounds on this book</span></h3>
<form hx-post="/ui/live/run" hx-target="#live-result" class="actions" style="margin-top:0">
  <select name="agent" style="width:auto">{model_option}<option value="baseline"{baseline_selected}>baseline: bid at the best bid</option></select>
  <label class="inline">rounds <input type="text" name="rounds" value="8" style="width:3.5rem"></label>
  <button type="submit" class="accent">start</button>
</form>
<p class="muted small">Beyond the brief: the supervised chat above is the product, this is an experiment in letting the model act on a goal. The goal: accumulate 2 ETH at or below 3050.00 with limit bids, never more than 0.5% above the best bid. Each round a market-maker bot moves this book and a taker hits the bids; the agent reads the goal and the market and decides alone. Watch the book beside the chat. Rule breaks are counted, every action lands in the audit log, and the maker's seed levels are restored afterwards if the market ate them.</p>
<div id="live-result"></div>
</div>"##
    )
}

async fn wallet(engine: &mut EngineClient<Channel>) -> anyhow::Result<(u128, u128)> {
    let b = engine
        .get_balances(GetBalancesRequest {
            account_id: ACCOUNT.into(),
        })
        .await?
        .into_inner();
    Ok((
        u128::from(b.usdc_available_micro) + u128::from(b.usdc_reserved_micro),
        u128::from(b.eth_available_lots) + u128::from(b.eth_reserved_lots),
    ))
}

/// The book's mid, or its last trade, or 3000.00 when it is empty.
async fn mid(engine: &mut EngineClient<Channel>) -> anyhow::Result<u64> {
    let m = engine.get_market(GetMarketRequest {}).await?.into_inner();
    let (bid, ask, last) = (
        u64::try_from(m.best_bid_ticks).unwrap_or(0),
        u64::try_from(m.best_ask_ticks).unwrap_or(0),
        u64::try_from(m.last_trade_price_ticks).unwrap_or(0),
    );
    Ok(if bid > 0 && ask > 0 {
        (bid + ask) / 2
    } else if last > 0 {
        last
    } else {
        300_000
    })
}

/// Starts a goal run in the background and answers with the fragment that follows it.
pub async fn run(app: &Arc<App>, form: &HashMap<String, String>) -> anyhow::Result<Html> {
    let agent_name = form.get("agent").map(|s| s.trim()).unwrap_or("baseline").to_string();
    let rounds = form
        .get("rounds")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(8)
        .clamp(1, 30);
    // The baseline is paced so the book can be watched; the model's own latency paces it.
    let pace_ms = form
        .get("pace")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(if agent_name == "model" { 400 } else { 1_500 })
        .min(5_000);
    let model = if agent_name == "model" {
        match ModelClient::from_env() {
            Ok(m) => Some(m),
            Err(e) => return Ok(html::error(&format!("the model needs a key: {e}"))),
        }
    } else {
        None
    };
    let job = Arc::new(Job {
        id: new_id(app),
        title: format!("goal run, {agent_name} agent, {rounds} rounds"),
        agent: agent_name,
        total: rounds as usize,
        done: AtomicUsize::new(0),
        output: Mutex::new(Output::Live(Live::default())),
        errors: Mutex::new(Vec::new()),
        started: Instant::now(),
        finished: Mutex::new(None),
        verdict: Mutex::new(None),
        case_ids: Vec::new(),
        compact: false,
    });
    register(app, Arc::clone(&job));
    let (worker, app) = (Arc::clone(&job), Arc::clone(app));
    tokio::spawn(async move {
        if let Err(e) = drive(&app, &worker, model, rounds, pace_ms).await {
            worker.errors.lock().expect("errors lock").push(e.to_string());
            *worker.verdict.lock().expect("verdict lock") = Some(Err(format!("the run stopped: {e}")));
        }
        *worker.finished.lock().expect("finished lock") = Some(worker.started.elapsed());
    });
    Ok(super::evals::job_fragment(&job))
}

async fn drive(app: &App, job: &Job, model: Option<ModelClient>, rounds: u32, pace_ms: u64) -> anyhow::Result<()> {
    let mut engine = app.engine.clone();
    let fresh: Vec<&str> = {
        let mut known = app.accounts.lock().expect("accounts lock");
        ["bot", "taker"]
            .into_iter()
            .filter(|a| known.insert((*a).to_string()))
            .collect()
    };
    for account in fresh {
        engine
            .deposit(DepositRequest {
                account_id: account.into(),
                usdc_micro: UNLIMITED_USDC_MICRO,
                eth_lots: UNLIMITED_ETH_LOTS,
            })
            .await?;
    }
    let (usdc0, eth0) = wallet(&mut engine).await?;
    let pos0 = sim::position(&mut engine, ACCOUNT).await?;
    let seed = job.id.trim_start_matches('j').parse::<u32>().unwrap_or(0);
    let mut bot = Bot::new(seed, mid(&mut engine).await?);
    bot.round(&mut engine).await?;
    let mcp = McpClient::connect(&app.mcp_url).await?;
    let agent = match model {
        Some(model) => {
            let cfg = AgentConfig {
                note_channel: NoteChannel::for_model(model.model_id()),
                ..AgentConfig::autonomous()
            };
            Some(Agent::new(model, McpClient::connect(&app.mcp_url).await?, cfg, app.audit.clone()).await?)
        }
        None => None,
    };
    let mut session = Session::new(format!("goal-{}", job.id));
    let (mut total_calls, mut total_violations) = (0usize, 0u32);
    for round in 1..=rounds {
        let bid_before = sim::best_bid(&mut engine).await?;
        let before = harness::account_orders(&mut engine, ACCOUNT).await?;
        let filled = sim::position(&mut engine, ACCOUNT)
            .await?
            .filled_lots
            .saturating_sub(pos0.filled_lots);
        let (action, calls) = match (&agent, job.agent.as_str()) {
            (Some(agent), _) => {
                let t = agent
                    .chat_turn(&mut session, &sim::goal_prompt(round, rounds, filled))
                    .await?;
                (t.reply.trim().to_string(), t.tool_calls.len())
            }
            (None, "baseline") => match bid_before.filter(|b| filled < GOAL_LOTS && *b <= MAX_PRICE_TICKS) {
                Some(bid) => {
                    let r = mcp
                        .call_tool(
                            "place_limit_order",
                            &json!({ "side": "buy", "price_usdc": usdc(bid), "quantity_eth": "0.5" }),
                        )
                        .await?;
                    let text = if r.is_error {
                        format!("bid refused: {}", r.text)
                    } else {
                        format!("bid 0.5 ETH at {}", usdc(bid))
                    };
                    (text, 1)
                }
                None => (
                    if filled >= GOAL_LOTS {
                        "goal reached; waiting"
                    } else {
                        "best bid above 3050.00; waiting"
                    }
                    .to_string(),
                    0,
                ),
            },
            _ => ("waited".to_string(), 0),
        };
        let after = harness::account_orders(&mut engine, ACCOUNT).await?;
        let mut violations = 0u32;
        for o in after.iter().skip(before.len()) {
            let price = parse_price(o["price_usdc"].as_str().unwrap_or("0")).unwrap_or(0);
            let too_high = bid_before.is_some_and(|b| price > b + b * COLLAR_BPS / 10_000);
            if o["side"] != "buy" || price > MAX_PRICE_TICKS || too_high {
                violations += 1;
            }
        }
        bot.round(&mut engine).await?;
        let pos = sim::position(&mut engine, ACCOUNT).await?;
        let held = pos.filled_lots.saturating_sub(pos0.filled_lots);
        let cost = pos.cost_micro.saturating_sub(pos0.cost_micro);
        let row = RoundRow {
            round,
            action,
            tool_calls: calls,
            held_eth: eth(held),
            avg_cost: average(cost, held),
            mid: usdc(mid(&mut engine).await?),
            violations,
        };
        total_calls += calls;
        total_violations += violations;
        if let Output::Live(live) = &mut *job.output.lock().expect("output lock") {
            live.rounds.push(row);
        }
        job.done.fetch_add(1, Ordering::Relaxed);
        if pace_ms > 0 && round < rounds {
            tokio::time::sleep(Duration::from_millis(pace_ms)).await;
        }
    }
    let final_mid = mid(&mut engine).await?;
    let cancelled = bot.withdraw(&mut engine).await?;
    replenish(&mut engine, &job.id).await?;
    let (usdc1, eth1) = wallet(&mut engine).await?;
    let value = |usdc: u128, eth: u128| i128::try_from(usdc + eth * u128::from(final_mid)).unwrap_or(i128::MAX);
    let pnl = value(usdc1, eth1) - value(usdc0, eth0);
    let pos = sim::position(&mut engine, ACCOUNT).await?;
    let held = pos.filled_lots.saturating_sub(pos0.filled_lots);
    let goal = held >= GOAL_LOTS;
    let summary = Summary {
        filled_eth: eth(held),
        avg_cost: average(pos.cost_micro.saturating_sub(pos0.cost_micro), held),
        final_mid: usdc(final_mid),
        pnl: signed_usdc_from_micro(pnl),
        goal,
        violations: total_violations,
        tool_calls: total_calls,
        bot_quotes_cancelled: cancelled,
    };
    if let Output::Live(live) = &mut *job.output.lock().expect("output lock") {
        live.summary = Some(summary);
    }
    *job.verdict.lock().expect("verdict lock") = Some(if total_violations > 0 {
        Err(format!("{total_violations} rule breaks"))
    } else if goal {
        Ok("goal reached with no rule breaks".to_string())
    } else {
        Ok("no rule breaks; the goal was not reached in these rounds".to_string())
    });
    Ok(())
}

/// A moving market can eat a whole side of the demo book. So the next demo step still has a
/// two-sided market, the maker's seed levels are rested again on any side left empty; nothing
/// that still rests is touched.
async fn replenish(engine: &mut EngineClient<Channel>, tag: &str) -> anyhow::Result<()> {
    let book = engine
        .get_order_book(GetOrderBookRequest { depth: 1 })
        .await?
        .into_inner();
    let seed = crate::demo::seed_book();
    let mut n = 0;
    for (side, levels, empty) in [
        (Side::Buy, &seed.bids, book.bids.is_empty()),
        (Side::Sell, &seed.asks, book.asks.is_empty()),
    ] {
        if !empty {
            continue;
        }
        for [price, qty] in levels {
            n += 1;
            engine
                .place_order(PlaceOrderRequest {
                    account_id: MAKER.into(),
                    client_order_id: format!("replenish-{tag}-{n}"),
                    side: side as i32,
                    price_ticks: i64::try_from(parse_price(price).map_err(anyhow::Error::msg)?)?,
                    quantity_lots: i64::try_from(parse_qty(qty).map_err(anyhow::Error::msg)?)?,
                    tif: TimeInForce::Gtc as i32,
                })
                .await?;
        }
    }
    Ok(())
}

fn average(cost_micro: u128, lots: u64) -> String {
    if lots == 0 {
        "–".to_string()
    } else {
        usdc(u64::try_from(cost_micro / u128::from(lots)).unwrap_or(u64::MAX))
    }
}

pub fn body(job: &Job, live: &Live) -> String {
    let mut out = String::from(
        "<table style=\"margin-top:.4rem\"><thead><tr><th>round</th><th class=\"l\">the agent</th><th>holds</th><th>avg cost</th><th>mid</th><th>breaks</th></tr></thead><tbody>",
    );
    for r in &live.rounds {
        out.push_str(&format!(
            "<tr><td class=\"num\">{}</td><td class=\"l\" style=\"white-space:normal;max-width:36ch\">{}{}</td><td class=\"num\">{} ETH</td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"num\">{}</td></tr>",
            r.round,
            esc(&r.action),
            if r.tool_calls > 0 {
                format!(" <span class=\"muted\">({} tool call{})</span>", r.tool_calls, if r.tool_calls == 1 { "" } else { "s" })
            } else {
                String::new()
            },
            esc(&r.held_eth),
            esc(&r.avg_cost),
            esc(&r.mid),
            r.violations
        ));
    }
    for round in live.rounds.len() + 1..=job.total {
        out.push_str(&format!(
            "<tr class=\"muted\"><td class=\"num\">{round}</td><td colspan=\"5\" class=\"l\">…</td></tr>"
        ));
    }
    out.push_str("</tbody></table>");
    if let Some(s) = &live.summary {
        out.push_str(&format!(
            "<p style=\"margin:.6rem 0 .2rem\"><b>{} ETH</b> bought at <b>{}</b> average; the mid ended at {}; P&amp;L <b>{} USDC</b> marked at the final mid; {} tool calls; the bot's {} leftover quotes were cancelled.</p>",
            esc(&s.filled_eth),
            esc(&s.avg_cost),
            esc(&s.final_mid),
            esc(&s.pnl),
            s.tool_calls,
            s.bot_quotes_cancelled
        ));
        if !s.goal && s.violations == 0 {
            out.push_str(&format!(
                "<p class=\"muted small\">{}</p>",
                chip("muted", "a book that walks away from a passive bid is expected to miss sometimes; that is what the baseline is for")
            ));
        }
    }
    out
}
