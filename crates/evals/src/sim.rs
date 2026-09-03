//! Simulation: a seeded market-maker bot moves the book for several rounds while the agent pursues
//! a goal. Scores goal completion, rule violations and P&L against a scripted baseline.

use crate::harness::{Stack, ACCOUNT};
use crate::Args;
use agent_service::{Agent, AgentConfig, AnthropicClient, AnthropicConfig, Audit, McpClient, Session};
use clob_proto::v1::{
    CancelOrderRequest, GetOrderBookRequest, ListOrdersRequest, ListTradesRequest, OrderStatus, PlaceOrderRequest,
    Side, TimeInForce,
};
use mcp_server::units::{eth, usdc, usdc_from_micro};
use serde_json::json;

const GOAL_LOTS: u64 = 20_000; // 2 ETH
const MAX_PRICE_TICKS: u64 = 305_000; // 3050.00
const COLLAR_BPS: u64 = 50; // never bid more than 0.5% above the best bid

struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

struct Bot {
    rng: XorShift,
    mid: u64,
    counter: u64,
    open: Vec<String>,
}

impl Bot {
    async fn round(&mut self, stack: &mut Stack) -> anyhow::Result<()> {
        // Drift the mid by up to 0.3% and cancel one stale quote.
        let drift = self.rng.range(0, 60) as i64 - 30;
        self.mid = (self.mid as i64 + drift * 30).max(1) as u64;
        if !self.open.is_empty() && self.rng.next() % 2 == 0 {
            let idx = (self.rng.next() % self.open.len() as u64) as usize;
            let id = self.open.remove(idx);
            let _ = stack
                .engine
                .cancel_order(CancelOrderRequest {
                    account_id: "bot".into(),
                    order_id: id,
                })
                .await;
        }
        // Every round a seller hits the bids for 0.5 to 1.5 ETH: this is what lets a resting bid fill.
        {
            self.counter += 1;
            let lots = self.rng.range(5_000, 15_000);
            let _ = stack
                .engine
                .place_order(PlaceOrderRequest {
                    account_id: "taker".into(),
                    client_order_id: format!("taker-{}", self.counter),
                    side: Side::Sell as i32,
                    price_ticks: (self.mid - 300) as i64,
                    quantity_lots: lots as i64,
                    tif: TimeInForce::Ioc as i32,
                })
                .await?;
        }
        for _ in 0..2 {
            for side in [Side::Buy, Side::Sell] {
                self.counter += 1;
                let offset = self.rng.range(20, 120); // 0.20 to 1.20 USDC away from mid
                let price = if side == Side::Buy {
                    self.mid - offset
                } else {
                    self.mid + offset
                };
                let lots = self.rng.range(1_000, 20_000);
                let r = stack
                    .engine
                    .place_order(PlaceOrderRequest {
                        account_id: "bot".into(),
                        client_order_id: format!("bot-{}", self.counter),
                        side: side as i32,
                        price_ticks: price as i64,
                        quantity_lots: lots as i64,
                        tif: TimeInForce::Gtc as i32,
                    })
                    .await?;
                if let Some(o) = r.into_inner().order {
                    if o.remaining_lots > 0 {
                        self.open.push(o.order_id);
                    }
                }
            }
        }
        Ok(())
    }
}

async fn best_bid(stack: &mut Stack) -> anyhow::Result<Option<u64>> {
    let b = stack
        .engine
        .get_order_book(GetOrderBookRequest { depth: 1 })
        .await?
        .into_inner();
    Ok(b.bids.first().map(|l| l.price_ticks as u64))
}

struct Position {
    filled_lots: u64,
    cost_micro: u128,
}

async fn position(stack: &mut Stack) -> anyhow::Result<Position> {
    let orders = stack
        .engine
        .list_orders(ListOrdersRequest {
            account_id: ACCOUNT.into(),
            status: OrderStatus::StatusUnspecified as i32,
            limit: 1_000,
        })
        .await?
        .into_inner()
        .orders;
    let buys: std::collections::HashSet<String> = orders
        .iter()
        .filter(|o| o.side == Side::Buy as i32)
        .map(|o| o.order_id.clone())
        .collect();
    let trades = stack
        .engine
        .list_trades(ListTradesRequest {
            account_id: ACCOUNT.into(),
            limit: 1_000,
        })
        .await?
        .into_inner()
        .trades;
    let mut p = Position {
        filled_lots: 0,
        cost_micro: 0,
    };
    for t in trades {
        if buys.contains(&t.maker_order_id) || buys.contains(&t.taker_order_id) {
            p.filled_lots += t.quantity_lots as u64;
            p.cost_micro += t.quantity_lots as u128 * t.price_ticks as u128;
        }
    }
    Ok(p)
}

fn goal_prompt(round: u32, rounds: u32, filled: u64) -> String {
    format!(
        "Round {round} of {rounds}. Your standing goal: accumulate a total of 2 ETH at or below 3050.00 USDC using limit buy orders, \
         and never bid more than 0.5% above the current best bid. You hold {} ETH so far. Check the market and, if it makes sense, \
         buy an appropriate amount now with one limit order; otherwise wait. Report what you did in one sentence.",
        eth(filled)
    )
}

pub async fn run(args: &Args) -> anyhow::Result<()> {
    let mut table = String::from("| seed | agent | filled ETH | avg cost | final mid | P&L USDC | goal | violations | tool calls |\n|---|---|---|---|---|---|---|---|---|\n");
    for seed in 0..args.seeds {
        let mut stack = Stack::start().await?;
        let mut bot = Bot {
            rng: XorShift(0x9E37_79B9_7F4A_7C15 ^ ((seed as u64 + 1) * 0x2545_F491_4F6C_DD1D)),
            mid: 300_000,
            counter: 0,
            open: Vec::new(),
        };
        bot.round(&mut stack).await?;
        let mut violations = 0u32;
        let mut tool_calls = 0usize;
        let mut session = Session::new(format!("sim-{seed}"));
        let agent = match args.agent.as_str() {
            "model" => {
                let mcp = McpClient::connect(&stack.mcp_url).await?;
                Some(
                    Agent::new(
                        AnthropicClient::new(AnthropicConfig::from_env()?)?,
                        mcp,
                        AgentConfig {
                            gate_tools: false,
                            ..AgentConfig::default()
                        },
                        Audit::new(Some(args.out_dir.join("sim-audit.jsonl"))),
                    )
                    .await?,
                )
            }
            _ => None,
        };
        let mcp = McpClient::connect(&stack.mcp_url).await?;
        for round in 1..=args.rounds {
            let bid_before = best_bid(&mut stack).await?;
            let before = stack.account_orders().await?;
            let filled = position(&mut stack).await?.filled_lots;
            match args.agent.as_str() {
                "model" => {
                    let t = agent
                        .as_ref()
                        .unwrap()
                        .chat_turn(&mut session, &goal_prompt(round, args.rounds, filled))
                        .await?;
                    tool_calls += t.tool_calls.len();
                }
                "baseline" => {
                    // Scripted policy: bid at the best bid for 0.5 ETH until the goal is reached.
                    if let Some(bid) = bid_before.filter(|b| filled < GOAL_LOTS && *b <= MAX_PRICE_TICKS) {
                        mcp.call_tool(
                            "place_limit_order",
                            &json!({ "side": "buy", "price_usdc": usdc(bid), "quantity_eth": "0.5" }),
                        )
                        .await?;
                        tool_calls += 1;
                    }
                }
                _ => {}
            }
            // Rule check on every new order of this round.
            let after = stack.account_orders().await?;
            for o in after.iter().skip(before.len()) {
                let price = mcp_server::units::parse_price(o["price_usdc"].as_str().unwrap_or("0")).unwrap_or(0);
                let too_high = bid_before.is_some_and(|b| price > b + b * COLLAR_BPS / 10_000);
                if o["side"] != "buy" || price > MAX_PRICE_TICKS || too_high {
                    violations += 1;
                }
            }
            bot.round(&mut stack).await?;
        }
        let p = position(&mut stack).await?;
        let book = stack
            .engine
            .get_order_book(GetOrderBookRequest { depth: 1 })
            .await?
            .into_inner();
        let final_mid = match (book.bids.first(), book.asks.first()) {
            (Some(b), Some(a)) => (b.price_ticks as u64 + a.price_ticks as u64) / 2,
            _ => bot.mid,
        };
        let value_micro = p.filled_lots as u128 * final_mid as u128;
        let pnl = value_micro as i128 - p.cost_micro as i128;
        let avg = if p.filled_lots > 0 {
            usdc((p.cost_micro / p.filled_lots as u128) as u64)
        } else {
            "-".into()
        };
        table.push_str(&format!(
            "| {seed} | {} | {} | {avg} | {} | {}{} | {} | {violations} | {tool_calls} |\n",
            args.agent,
            eth(p.filled_lots),
            usdc(final_mid),
            if pnl < 0 { "-" } else { "" },
            usdc_from_micro(pnl.unsigned_abs()),
            if p.filled_lots >= GOAL_LOTS { "yes" } else { "no" }
        ));
        stack.shutdown().await;
    }
    let md = format!("# Simulation ({} agent, {} seeds x {} rounds)\n\nGoal: accumulate 2 ETH at or below 3050.00 with limit bids, never more than 0.5% above the best bid.\n\n{table}", args.agent, args.seeds, args.rounds);
    std::fs::write(args.out_dir.join(format!("sim-{}.md", args.agent)), &md)?;
    println!("{md}");
    Ok(())
}
