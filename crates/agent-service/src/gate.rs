//! Conversation-level guardrails. Everything here is deterministic code: which action tools a turn
//! may execute, a confirmation step for large orders, and a verifier that checks what happened
//! against what the user asked for.
//!
//! The tool *list* sent to the model never changes within a session (a changing list defeats
//! prompt caching and, on the newest models, invalidates the thinking blocks bound to the
//! conversation prefix). Permission is decided per turn from the user's own words and enforced
//! when the model calls a tool: a call outside the permission is answered with an error result and
//! never reaches the MCP server.

use mcp_server::units::{eth, parse_price, parse_qty, usdc_from_micro};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const ACTION_TOOLS: [&str; 3] = ["place_limit_order", "cancel_order", "cancel_all_orders"];
const TRADE_VERBS: [&str; 20] = [
    "buy",
    "sell",
    "purchase",
    "acquire",
    "bid",
    "offer",
    "long",
    "short",
    "place",
    "submit",
    "order",
    "execute",
    "go long",
    "go short",
    "grab",
    "dump",
    "unload",
    "liquidate",
    "pick up",
    "load up",
];
const CANCEL_VERBS: [&str; 9] = [
    "cancel", "remove", "withdraw", "pull", "kill", "close", "undo", "revert", "unwind",
];
/// Words that frame a request as not meant for real: an order placed under them needs a
/// confirmation turn, so a "demo" never rests in the book without the user saying so twice.
const FRAMING_WORDS: [&str; 8] = [
    "demo",
    "demonstration",
    "hypothetical",
    "hypothetically",
    "pretend",
    "simulate",
    "example",
    "test",
];
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
const ASSET_WORDS: [&str; 3] = ["eth", "ether", "ethereum"];

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '.' || c == ','))
        .filter(|w| !w.is_empty())
}

fn has_word(text: &str, word: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    words(&lower).any(|w| w.trim_matches(|c| c == '.' || c == ',') == word)
        || (word.contains(' ') && lower.contains(word))
}

/// Numbers in the text, normalised: "3,000.50" -> "3000.50", "3000." -> "3000".
pub fn numbers(text: &str) -> Vec<String> {
    words(text)
        .map(|w| w.trim_matches(|c| c == '.' || c == ',').replace(',', ""))
        .filter(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_digit() || c == '.') && w.matches('.').count() <= 1)
        .collect()
}

/// A trade verb, or the shape of an order: the asset plus at least two numbers ("0.5 ETH @ 3000").
pub fn mentions_trade_intent(text: &str) -> bool {
    TRADE_VERBS.iter().any(|w| has_word(text, w))
        || (ASSET_WORDS.iter().any(|w| has_word(text, w)) && numbers(text).len() >= 2)
}

pub fn mentions_cancel_intent(text: &str) -> bool {
    CANCEL_VERBS.iter().any(|w| has_word(text, w))
}

pub fn mentions_framing(text: &str) -> bool {
    FRAMING_WORDS.iter().any(|w| has_word(text, w))
}

pub fn mentions_confirmation(text: &str) -> bool {
    CONFIRM_WORDS.iter().any(|w| has_word(text, w))
}

/// What this turn may execute. Read-only tools are always permitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Permissions {
    pub trade: bool,
    pub cancel: bool,
}

impl Permissions {
    /// Decides from the user's own words (or a confirmation of a pending order). With the gate
    /// disabled everything is permitted and the verifier is the only check.
    pub fn for_turn(user_text: &str, pending_confirmation: bool, gate_enabled: bool) -> Self {
        if !gate_enabled {
            return Self {
                trade: true,
                cancel: true,
            };
        }
        Self {
            trade: mentions_trade_intent(user_text) || (pending_confirmation && mentions_confirmation(user_text)),
            cancel: mentions_cancel_intent(user_text),
        }
    }

    pub fn allows(&self, tool: &str) -> bool {
        match tool {
            "place_limit_order" => self.trade,
            "cancel_order" | "cancel_all_orders" => self.cancel,
            _ => true,
        }
    }

    /// The short note appended to the user's message so the model knows what this turn allows,
    /// without the tool list changing. Enforcement does not depend on the model reading it.
    pub fn note(&self) -> String {
        let yes_no = |b: bool| if b { "yes" } else { "no" };
        format!(
            "[service] This turn permits: place orders = {}, cancel orders = {}. \
             Calls outside that are refused; if the user's message does not ask for an action, do not attempt one.",
            yes_no(self.trade),
            yes_no(self.cancel)
        )
    }
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
    /// An order whose limit price does not appear in the user's message (the model chose it, as
    /// for "sell now") needs a confirmation turn whatever its size.
    pub confirm_unpriced: bool,
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
        user_text: &str,
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
        let (Ok(price), Ok(qty)) = (
            parse_price(&text_of(&args["price_usdc"])),
            parse_qty(&text_of(&args["quantity_eth"])),
        ) else {
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
        let price_quoted = numbers(user_text).iter().any(|n| parse_price(n).ok() == Some(price));
        let reason = if qty >= self.threshold_lots {
            "This order is large."
        } else if self.confirm_unpriced && !price_quoted {
            "The user did not state this price; the model chose it."
        } else if mentions_framing(user_text) {
            "The request was framed as a demo, test or hypothetical; a real order needs an explicit confirmation."
        } else {
            return Intercept::Proceed(args);
        };
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
            "instruction": format!("{reason} Tell the user the summary and ask them to confirm. When they confirm, call place_limit_order again with the same arguments plus this confirmation_token.")
        }))
    }
}

fn text_of(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

fn same_order(a: &Value, b: &Value) -> bool {
    ["side", "price_usdc", "quantity_eth"]
        .iter()
        .all(|k| a[k].to_string().trim_matches('"') == b[k].to_string().trim_matches('"'))
}

/// An action tool call that reached the MCP server, and whether it succeeded.
#[derive(Debug, Clone)]
pub struct Executed {
    pub tool: String,
    pub args: Value,
    pub ok: bool,
}

/// Post-turn verifier: every executed action must be justified by the user's own words, and the
/// numbers of a placed order must come from the request when the request contains numbers at all.
pub fn verify(user_text: &str, executed: &[Executed], confirmed_pending: bool) -> Vec<String> {
    let mut flags = Vec::new();
    let mentioned = numbers(user_text);
    for e in executed.iter().filter(|e| e.ok) {
        match e.tool.as_str() {
            "place_limit_order" => {
                if !(mentions_trade_intent(user_text) || confirmed_pending) {
                    flags.push("intent_mismatch:place_limit_order".to_string());
                }
                if !confirmed_pending && !mentioned.is_empty() {
                    let price = parse_price(&text_of(&e.args["price_usdc"])).ok();
                    let qty = parse_qty(&text_of(&e.args["quantity_eth"])).ok();
                    let price_seen = mentioned.iter().any(|n| parse_price(n).ok() == price);
                    let qty_seen = mentioned.iter().any(|n| parse_qty(n).ok() == qty);
                    if !price_seen && !qty_seen {
                        flags.push("params_not_in_request:place_limit_order".to_string());
                    }
                }
            }
            "cancel_order" | "cancel_all_orders" if !mentions_cancel_intent(user_text) => {
                flags.push(format!("intent_mismatch:{}", e.tool))
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
    fn intent_detection_uses_whole_words_and_order_shapes() {
        assert!(mentions_trade_intent("Buy half an ETH at 3000"));
        assert!(mentions_trade_intent("please sell 2 eth"));
        assert!(mentions_trade_intent("grab me 0.3 eth at 2999"));
        assert!(mentions_trade_intent("0.5 ETH @ 3000 please")); // no verb: asset plus two numbers
        assert!(mentions_trade_intent("I want 0.5 eth at 3,000.50"));
        assert!(!mentions_trade_intent("what's the best selling point"));
        assert!(!mentions_trade_intent("show my orders")); // "orders" is not "order"
        assert!(!mentions_trade_intent("what is 1 ETH worth?")); // one number: a question
        assert!(!mentions_trade_intent("what's ETH at?"));
        assert!(mentions_cancel_intent("cancel my last order"));
        assert!(mentions_cancel_intent("now undo that"));
        assert!(mentions_framing("just as a demo, buy 0.5 eth at 3000"));
        assert!(!mentions_framing("buy 0.5 eth at 3000"));
        assert!(mentions_cancel_intent("what did I cancel yesterday?"));
        assert!(mentions_confirmation("yes, go ahead"));
        assert!(!mentions_confirmation("what is ETH at?"));
        assert_eq!(numbers("buy 0.5 eth at 3,000.50."), vec!["0.5", "3000.50"]);
        assert_eq!(numbers("order 12: 1.2 ETH"), vec!["12", "1.2"]);
    }

    #[test]
    fn permissions_follow_intent_and_pending_confirmations() {
        let p = |t: &str, pending: bool| Permissions::for_turn(t, pending, true);
        assert_eq!(
            p("what's ETH at?", false),
            Permissions {
                trade: false,
                cancel: false
            }
        );
        assert!(p("buy 1 eth at 3000", false).trade && !p("buy 1 eth at 3000", false).cancel);
        assert!(p("cancel it", false).cancel && !p("cancel it", false).trade);
        assert!(p("yes confirm", true).trade);
        assert!(!p("yes confirm", false).trade);
        assert_eq!(
            Permissions::for_turn("anything", false, false),
            Permissions {
                trade: true,
                cancel: true
            }
        );
        let none = p("price?", false);
        assert!(
            none.allows("get_market_summary") && !none.allows("place_limit_order") && !none.allows("cancel_all_orders")
        );
        assert!(none.note().contains("place orders = no"));
    }

    #[test]
    fn large_orders_need_a_matching_token() {
        let gate = ConfirmationGate {
            threshold_lots: 10_000,
            confirm_unpriced: true,
            ttl: Duration::from_secs(600),
        };
        let mut pending = None;
        let small = json!({"side":"buy","price_usdc":"3000","quantity_eth":"0.5"});
        let text = "buy 0.5 eth at 3000";
        assert!(matches!(
            gate.intercept(&mut pending, "place_limit_order", &small, "s", 1, text),
            Intercept::Proceed(_)
        ));
        // A small order at a price the user never mentioned: the model chose it, so confirm.
        let Intercept::Reply(unpriced) =
            gate.intercept(&mut pending, "place_limit_order", &small, "s", 1, "sell 0.5 eth now")
        else {
            panic!("expected preview")
        };
        assert_eq!(unpriced["needs_confirmation"], true);
        assert!(unpriced["instruction"]
            .as_str()
            .unwrap()
            .contains("did not state this price"));
        pending = None;
        let lenient = ConfirmationGate {
            confirm_unpriced: false,
            ..gate.clone()
        };
        assert!(matches!(
            lenient.intercept(&mut pending, "place_limit_order", &small, "s", 1, "sell 0.5 eth now"),
            Intercept::Proceed(_)
        ));
        let Intercept::Reply(framed) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &small,
            "s",
            1,
            "as a demo, buy 0.5 eth at 3000",
        ) else {
            panic!("expected preview")
        };
        assert!(framed["instruction"].as_str().unwrap().contains("framed"));
        pending = None;
        let big = json!({"side":"buy","price_usdc":"3000","quantity_eth":"2"});
        let Intercept::Reply(preview) = gate.intercept(&mut pending, "place_limit_order", &big, "s", 1, text) else {
            panic!("expected preview")
        };
        assert!(preview["instruction"].as_str().unwrap().contains("large"));
        assert_eq!(preview["needs_confirmation"], true);
        let token = preview["confirmation_token"].as_str().unwrap().to_string();
        assert!(pending.is_some());
        let mut wrong = big.clone();
        wrong["quantity_eth"] = json!("3");
        wrong["confirmation_token"] = json!(token);
        let Intercept::Reply(r) = gate.intercept(&mut pending, "place_limit_order", &wrong, "s", 2, "yes") else {
            panic!()
        };
        assert_eq!(r["code"], "CONFIRMATION_MISMATCH");
        assert!(pending.is_some(), "a mismatch keeps the pending order");
        let mut confirmed = big.clone();
        confirmed["confirmation_token"] = json!(token);
        let Intercept::Proceed(args) = gate.intercept(&mut pending, "place_limit_order", &confirmed, "s", 2, "yes")
        else {
            panic!()
        };
        assert!(args.get("confirmation_token").is_none());
        assert!(pending.is_none());
        let mut stale = big.clone();
        stale["confirmation_token"] = json!("cfm-old");
        let Intercept::Reply(r) = gate.intercept(&mut pending, "place_limit_order", &stale, "s", 3, "yes") else {
            panic!()
        };
        assert_eq!(r["code"], "NO_PENDING_CONFIRMATION");
    }

    fn placed(price: &str, qty: &str, ok: bool) -> Executed {
        Executed {
            tool: "place_limit_order".into(),
            args: json!({ "side": "buy", "price_usdc": price, "quantity_eth": qty }),
            ok,
        }
    }

    #[test]
    fn verifier_flags_unrequested_actions_and_invented_numbers() {
        assert!(verify("show my orders", &[placed("3000", "0.5", true)], false)
            .contains(&"intent_mismatch:place_limit_order".to_string()));
        assert!(verify("buy 0.5 eth at 3000", &[placed("3000", "0.5", true)], false).is_empty());
        assert!(verify("buy half an eth at 3,000", &[placed("3000.00", "0.5", true)], false).is_empty());
        assert!(verify("buy some eth", &[placed("3000", "0.5", true)], false).is_empty()); // no numbers to check
        assert_eq!(
            verify("buy 0.5 eth at 3000", &[placed("2000", "5", true)], false),
            vec!["params_not_in_request:place_limit_order"]
        );
        assert!(verify("yes", &[placed("3000", "2", true)], true).is_empty());
        let cancel = Executed {
            tool: "cancel_all_orders".into(),
            args: json!({}),
            ok: true,
        };
        assert_eq!(
            verify("what's ETH at", std::slice::from_ref(&cancel), false),
            vec!["intent_mismatch:cancel_all_orders"]
        );
        assert!(verify("cancel everything", &[cancel], false).is_empty());
        assert!(verify("what's ETH at", &[placed("3000", "0.5", false)], false).is_empty());
    }
}
