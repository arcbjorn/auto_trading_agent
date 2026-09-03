//! Conversation-level guardrails. Everything here is deterministic code: which tools a turn may
//! use, a confirmation step for large orders, and a verifier that checks what happened against
//! what the user asked for.

use mcp_server::units::{eth, parse_price, parse_qty, usdc_from_micro};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const ACTION_TOOLS: [&str; 2] = ["place_limit_order", "cancel_order"];
const TRADE_VERBS: [&str; 14] = [
    "buy", "sell", "purchase", "acquire", "bid", "offer", "long", "short", "place", "submit", "order", "execute",
    "go long", "go short",
];
const CANCEL_VERBS: [&str; 6] = ["cancel", "remove", "withdraw", "pull", "kill", "close"];
const CONFIRM_WORDS: [&str; 10] = [
    "confirm",
    "confirmed",
    "yes",
    "go ahead",
    "do it",
    "proceed",
    "approve",
    "approved",
    "ok",
    "okay",
];

fn has_word(text: &str, word: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric() && c != ' ')
        .flat_map(|s| s.split_whitespace())
        .any(|w| w == word)
        || (word.contains(' ') && lower.contains(word))
}

pub fn mentions_trade_intent(text: &str) -> bool {
    TRADE_VERBS.iter().any(|w| has_word(text, w))
}

pub fn mentions_cancel_intent(text: &str) -> bool {
    CANCEL_VERBS.iter().any(|w| has_word(text, w))
}

pub fn mentions_confirmation(text: &str) -> bool {
    CONFIRM_WORDS.iter().any(|w| has_word(text, w))
}

/// Which tools the model is offered on this turn. Read-only tools are always offered; action
/// tools only when the user's own words carry the matching intent (or a confirmation is pending).
pub fn offered_tools(all: &[Value], user_text: &str, pending_confirmation: bool, gate_enabled: bool) -> Vec<Value> {
    let trade =
        !gate_enabled || mentions_trade_intent(user_text) || (pending_confirmation && mentions_confirmation(user_text));
    let cancel = !gate_enabled || mentions_cancel_intent(user_text);
    all.iter()
        .filter(|t| match t["name"].as_str().unwrap_or("") {
            "place_limit_order" => trade,
            "cancel_order" => cancel,
            _ => true,
        })
        .cloned()
        .map(|mut t| {
            if t["name"] == "place_limit_order" {
                t["input_schema"]["properties"]["confirmation_token"] = json!({
                    "type": "string",
                    "description": "Only after the user confirmed an order that needed confirmation: the token from the needs_confirmation result."
                });
            }
            t
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct PendingConfirmation {
    pub token: String,
    pub args: Value,
    pub summary: String,
    pub created: Instant,
}

#[derive(Debug, Clone)]
pub struct ConfirmationGate {
    /// Orders at or above this many lots need an explicit confirmation turn.
    pub threshold_lots: u64,
    pub ttl: Duration,
}

pub enum Intercept {
    /// Forward the (possibly cleaned) arguments to the MCP server.
    Proceed(Value),
    /// Do not forward; return this as the tool result instead.
    Reply(Value),
}

impl ConfirmationGate {
    pub fn intercept(
        &self,
        pending: &mut Option<PendingConfirmation>,
        tool: &str,
        args: &Value,
        session_id: &str,
        turn: u32,
    ) -> Intercept {
        if tool != "place_limit_order" {
            return Intercept::Proceed(args.clone());
        }
        let mut args = args.clone();
        let token = args
            .get("confirmation_token")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(obj) = args.as_object_mut() {
            obj.remove("confirmation_token");
        }
        let side = args["side"].as_str().unwrap_or("").to_string();
        let price_text = args["price_usdc"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| args["price_usdc"].to_string());
        let qty_text = args["quantity_eth"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| args["quantity_eth"].to_string());
        let (Ok(price), Ok(qty)) = (parse_price(&price_text), parse_qty(&qty_text)) else {
            return Intercept::Proceed(args); // let the server produce the validation message
        };
        if let Some(token) = token {
            return match pending.take() {
                Some(p) if p.token == token && p.created.elapsed() <= self.ttl && same_order(&p.args, &args) => {
                    Intercept::Proceed(args)
                }
                Some(p) => {
                    *pending = Some(p);
                    Intercept::Reply(json!({
                        "rejected": true,
                        "code": "CONFIRMATION_MISMATCH",
                        "message": "the confirmation token does not match a pending order (different parameters, expired, or unknown)",
                        "hint": "ask the user to confirm the exact order again"
                    }))
                }
                None => Intercept::Reply(json!({
                    "rejected": true,
                    "code": "NO_PENDING_CONFIRMATION",
                    "message": "there is no order awaiting confirmation",
                    "hint": "place the order without a confirmation_token; if it needs confirmation you will be told"
                })),
            };
        }
        if qty < self.threshold_lots {
            return Intercept::Proceed(args);
        }
        let token = format!(
            "cfm-{session_id}-{turn}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let notional = usdc_from_micro(price as u128 * qty as u128);
        let summary = format!(
            "{side} {} ETH at {} USDC (up to {notional} USDC)",
            eth(qty),
            mcp_server::units::usdc(price)
        );
        *pending = Some(PendingConfirmation {
            token: token.clone(),
            args: args.clone(),
            summary: summary.clone(),
            created: Instant::now(),
        });
        Intercept::Reply(json!({
            "needs_confirmation": true,
            "confirmation_token": token,
            "summary": summary,
            "instruction": "This order is large. Tell the user the summary and ask them to confirm. When they confirm, call place_limit_order again with the same arguments plus this confirmation_token."
        }))
    }
}

fn same_order(a: &Value, b: &Value) -> bool {
    ["side", "price_usdc", "quantity_eth"]
        .iter()
        .all(|k| a[k].to_string().trim_matches('"') == b[k].to_string().trim_matches('"'))
}

/// Post-turn verifier: every executed action must be justified by the user's own words.
pub fn verify(user_text: &str, executed: &[(String, bool)], confirmed_pending: bool) -> Vec<String> {
    let mut flags = Vec::new();
    for (tool, succeeded) in executed {
        if !succeeded {
            continue;
        }
        match tool.as_str() {
            "place_limit_order" if !(mentions_trade_intent(user_text) || confirmed_pending) => {
                flags.push("intent_mismatch:place_limit_order".to_string())
            }
            "cancel_order" if !mentions_cancel_intent(user_text) => {
                flags.push("intent_mismatch:cancel_order".to_string())
            }
            _ => {}
        }
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_detection_uses_whole_words() {
        assert!(mentions_trade_intent("Buy half an ETH at 3000"));
        assert!(mentions_trade_intent("please sell 2 eth"));
        assert!(!mentions_trade_intent("what's the best selling point"));
        assert!(!mentions_trade_intent("show my orders")); // "orders" is not "order"
        assert!(mentions_cancel_intent("cancel my last order"));
        assert!(mentions_cancel_intent("what did I cancel yesterday?"));
        assert!(mentions_confirmation("yes, go ahead"));
        assert!(!mentions_confirmation("what is ETH at?"));
    }

    #[test]
    fn gate_offers_action_tools_only_with_intent() {
        let all = vec![
            json!({"name":"get_market_summary","input_schema":{"type":"object","properties":{}}}),
            json!({"name":"place_limit_order","input_schema":{"type":"object","properties":{}}}),
            json!({"name":"cancel_order","input_schema":{"type":"object","properties":{}}}),
        ];
        let names = |v: Vec<Value>| {
            v.iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(offered_tools(&all, "what's ETH at?", false, true)),
            vec!["get_market_summary"]
        );
        assert_eq!(
            names(offered_tools(&all, "buy 1 eth at 3000", false, true)),
            vec!["get_market_summary", "place_limit_order"]
        );
        assert_eq!(
            names(offered_tools(&all, "cancel it", false, true)),
            vec!["get_market_summary", "cancel_order"]
        );
        assert_eq!(
            names(offered_tools(&all, "yes confirm", true, true)),
            vec!["get_market_summary", "place_limit_order"]
        );
        assert_eq!(
            names(offered_tools(&all, "yes confirm", false, true)),
            vec!["get_market_summary"]
        );
        assert_eq!(offered_tools(&all, "anything", false, false).len(), 3);
        let place = offered_tools(&all, "buy", false, true)
            .into_iter()
            .find(|t| t["name"] == "place_limit_order")
            .unwrap();
        assert!(place["input_schema"]["properties"]["confirmation_token"].is_object());
    }

    #[test]
    fn large_orders_need_a_matching_token() {
        let gate = ConfirmationGate {
            threshold_lots: 10_000,
            ttl: Duration::from_secs(600),
        };
        let mut pending = None;
        let small = json!({"side":"buy","price_usdc":"3000","quantity_eth":"0.5"});
        assert!(matches!(
            gate.intercept(&mut pending, "place_limit_order", &small, "s", 1),
            Intercept::Proceed(_)
        ));
        let big = json!({"side":"buy","price_usdc":"3000","quantity_eth":"2"});
        let Intercept::Reply(preview) = gate.intercept(&mut pending, "place_limit_order", &big, "s", 1) else {
            panic!("expected preview")
        };
        assert_eq!(preview["needs_confirmation"], true);
        let token = preview["confirmation_token"].as_str().unwrap().to_string();
        assert!(pending.is_some());
        let mut wrong = big.clone();
        wrong["quantity_eth"] = json!("3");
        wrong["confirmation_token"] = json!(token);
        let Intercept::Reply(r) = gate.intercept(&mut pending, "place_limit_order", &wrong, "s", 2) else {
            panic!()
        };
        assert_eq!(r["code"], "CONFIRMATION_MISMATCH");
        assert!(pending.is_some(), "a mismatch keeps the pending order");
        let mut confirmed = big.clone();
        confirmed["confirmation_token"] = json!(token);
        let Intercept::Proceed(args) = gate.intercept(&mut pending, "place_limit_order", &confirmed, "s", 2) else {
            panic!()
        };
        assert!(args.get("confirmation_token").is_none());
        assert!(pending.is_none());
        let mut stale = big.clone();
        stale["confirmation_token"] = json!("cfm-old");
        let Intercept::Reply(r) = gate.intercept(&mut pending, "place_limit_order", &stale, "s", 3) else {
            panic!()
        };
        assert_eq!(r["code"], "NO_PENDING_CONFIRMATION");
    }

    #[test]
    fn verifier_flags_unrequested_actions() {
        assert!(verify("show my orders", &[("place_limit_order".into(), true)], false)
            .contains(&"intent_mismatch:place_limit_order".to_string()));
        assert!(verify("buy 1 eth", &[("place_limit_order".into(), true)], false).is_empty());
        assert!(verify("yes", &[("place_limit_order".into(), true)], true).is_empty());
        assert!(verify("what's ETH at", &[("cancel_order".into(), true)], false).len() == 1);
        assert!(verify("what's ETH at", &[("cancel_order".into(), false)], false).is_empty());
    }
}
