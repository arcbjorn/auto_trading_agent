//! Conversation-level guardrails. Everything here is deterministic code: which action tools a turn
//! may execute at once, a confirmation step for everything else, and a verifier that checks what
//! happened against what the user asked for.
//!
//! The tool *list* sent to the model never changes within a session (a changing list defeats
//! prompt caching and, on the newest models, invalidates the thinking blocks bound to the
//! conversation prefix). What changes per turn is *permission*, decided from the user's own words.
//! An action the words justify executes at once; any other action, and any order that is large,
//! at a price the user never stated, or framed as a demo, is turned into a confirmation request:
//! the model must relay a summary and the user must say so in a turn of their own. Nothing reaches
//! the MCP server without either the words or the confirmation.

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
/// A confirmation in the user's own turn, in the languages a Latin-script tokenizer can tell apart.
const CONFIRM_WORDS: [&str; 20] = [
    "confirm",
    "confirmed",
    "confirmo",
    "confirme",
    "yes",
    "yep",
    "yeah",
    "go ahead",
    "do it",
    "proceed",
    "approve",
    "approved",
    "ok",
    "okay",
    "sí",
    "si",
    "oui",
    "ja",
    "da",
    "sim",
];
const ASSET_WORDS: [&str; 3] = ["eth", "ether", "ethereum"];
const BUY_WORDS: [&str; 8] = [
    "buy", "bid", "long", "purchase", "acquire", "grab", "pick up", "load up",
];
const SELL_WORDS: [&str; 7] = ["sell", "ask", "short", "offer", "dump", "unload", "liquidate"];

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '.' || c == ','))
        .filter(|w| !w.is_empty())
}

fn has_word(text: &str, word: &str) -> bool {
    let lower = text.to_lowercase();
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

/// Every word the gate recognises, one word per entry (multi-word phrases split), so an
/// evaluation that perturbs prompts can leave them alone and measure the model rather than this
/// vocabulary.
pub fn intent_vocabulary() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = TRADE_VERBS
        .iter()
        .chain(CANCEL_VERBS.iter())
        .chain(FRAMING_WORDS.iter())
        .chain(CONFIRM_WORDS.iter())
        .chain(ASSET_WORDS.iter())
        .chain(BUY_WORDS.iter())
        .chain(SELL_WORDS.iter())
        .flat_map(|w| w.split(' '))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// A trade verb, or the shape of an order: the asset plus at least two numbers ("0.5 ETH @ 3000").
pub fn mentions_trade_intent(text: &str) -> bool {
    TRADE_VERBS.iter().any(|w| has_word(text, w))
        || (ASSET_WORDS.iter().any(|w| has_word(text, w)) && numbers(text).len() >= 2)
}

/// Whether the user's own words name the side of the order the model is placing.
pub fn side_quoted(text: &str, side: &str) -> bool {
    let words: &[&str] = match side {
        "buy" => &BUY_WORDS,
        "sell" => &SELL_WORDS,
        _ => return false,
    };
    words.iter().any(|w| has_word(text, w))
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

/// Which action tools this turn may execute without a confirmation step. Read-only tools are
/// always permitted; the others are permitted when the user's own words carry the intent, or when
/// the user confirms an action of that kind that is pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Permissions {
    pub trade: bool,
    pub cancel: bool,
}

impl Permissions {
    /// `pending_tool` is the action tool awaiting confirmation, if any. With the gate disabled
    /// everything is permitted and the verifier is the only check.
    pub fn for_turn(user_text: &str, pending_tool: Option<&str>, gate_enabled: bool) -> Self {
        if !gate_enabled {
            return Self {
                trade: true,
                cancel: true,
            };
        }
        let confirms = pending_tool.is_some() && mentions_confirmation(user_text);
        Self {
            trade: mentions_trade_intent(user_text) || (confirms && pending_tool == Some("place_limit_order")),
            cancel: mentions_cancel_intent(user_text) || (confirms && pending_tool != Some("place_limit_order")),
        }
    }

    pub fn allows(&self, tool: &str) -> bool {
        match tool {
            "place_limit_order" => self.trade,
            "cancel_order" | "cancel_all_orders" => self.cancel,
            _ => true,
        }
    }

    /// The note for a turn in which the user confirmed a pending action: it names the action and
    /// the token, so even a small model completes the flow instead of asking again.
    pub fn note_confirming(tool: &str, summary: &str, token: &str) -> String {
        format!(
            "[service] The user confirmed the pending action ({summary}). Call {tool} now with the same arguments plus confirmation_token \"{token}\"; do not ask again."
        )
    }

    /// The short note appended after the user's message so the model knows what this turn allows
    /// without the tool list changing. Enforcement does not depend on the model reading it.
    pub fn note(&self) -> String {
        let how = |b: bool| if b { "allowed" } else { "confirmation required" };
        format!(
            "[service] This turn: place orders = {}; cancel orders = {}. \
             An action that requires confirmation returns needs_confirmation: relay the summary, ask the user, and never assume approval.",
            how(self.trade),
            how(self.cancel)
        )
    }
}

#[derive(Debug, Clone)]
pub struct PendingConfirmation {
    pub token: String,
    pub tool: String,
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
    /// Decides whether an action tool call executes now, is turned into a confirmation request, or
    /// completes a pending confirmation. `permitted` is the turn's permission for this tool.
    #[allow(clippy::too_many_arguments)]
    pub fn intercept(
        &self,
        pending: &mut Option<PendingConfirmation>,
        tool: &str,
        args: &Value,
        session_id: &str,
        turn: u32,
        user_text: &str,
        permitted: bool,
    ) -> Intercept {
        if !ACTION_TOOLS.contains(&tool) {
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
        if let Some(token) = token {
            return match pending.take() {
                Some(p)
                    if p.token == token
                        && p.tool == tool
                        && p.created.elapsed() <= self.ttl
                        && same_action(tool, &p.args, &args) =>
                {
                    Intercept::Proceed(args)
                }
                Some(p) => {
                    *pending = Some(p);
                    Intercept::Reply(json!({
                        "rejected": true,
                        "code": "CONFIRMATION_MISMATCH",
                        "message": "the confirmation token does not match the pending action (different tool or parameters, expired, or unknown)",
                        "hint": "ask the user to confirm the exact action again"
                    }))
                }
                None => Intercept::Reply(json!({
                    "rejected": true,
                    "code": "NO_PENDING_CONFIRMATION",
                    "message": "there is no action awaiting confirmation",
                    "hint": "call the tool without a confirmation_token; if it needs confirmation you will be told"
                })),
            };
        }
        let framed = mentions_framing(user_text);
        let (reason, summary) = match tool {
            "place_limit_order" => {
                let side = args["side"].as_str().unwrap_or("").to_string();
                let (Ok(price), Ok(qty)) = (
                    parse_price(&text_of(&args["price_usdc"])),
                    parse_qty(&text_of(&args["quantity_eth"])),
                ) else {
                    return Intercept::Proceed(args); // let the server produce the validation message
                };
                let price_quoted = numbers(user_text).iter().any(|n| parse_price(n).ok() == Some(price));
                let reason = if !permitted {
                    "The user's message did not clearly ask to trade."
                } else if qty >= self.threshold_lots {
                    "This order is large."
                } else if self.confirm_unpriced && !price_quoted {
                    "The user did not state this price; the model chose it."
                } else if self.confirm_unpriced && !side_quoted(user_text, &side) {
                    "The user did not state the side; the model chose it."
                } else if framed {
                    "The request was framed as a demo, test or hypothetical; a real order needs an explicit confirmation."
                } else {
                    return Intercept::Proceed(args);
                };
                let notional = usdc_from_micro(price as u128 * qty as u128);
                (
                    reason,
                    format!(
                        "{side} {} ETH at {} USDC (up to {notional} USDC)",
                        eth(qty),
                        mcp_server::units::usdc(price)
                    ),
                )
            }
            _ => {
                let reason = if !permitted {
                    "The user's message did not clearly ask to cancel."
                } else if framed {
                    "The request was framed as a demo or test."
                } else {
                    return Intercept::Proceed(args);
                };
                let summary = if tool == "cancel_order" {
                    format!("cancel order {}", text_of(&args["order_id"]))
                } else {
                    "cancel every open order of the account".to_string()
                };
                (reason, summary)
            }
        };
        let token = format!(
            "cfm-{session_id}-{turn}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        *pending = Some(PendingConfirmation {
            token: token.clone(),
            tool: tool.to_string(),
            args: args.clone(),
            summary: summary.clone(),
            created: Instant::now(),
        });
        Intercept::Reply(json!({
            "needs_confirmation": true,
            "confirmation_token": token,
            "summary": summary,
            "instruction": format!("{reason} Tell the user the summary and ask them to confirm. When they confirm, call {tool} again with the same arguments plus this confirmation_token.")
        }))
    }
}

fn text_of(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

fn same_action(tool: &str, a: &Value, b: &Value) -> bool {
    let keys: &[&str] = match tool {
        "place_limit_order" => &["side", "price_usdc", "quantity_eth"],
        "cancel_order" => &["order_id"],
        _ => &[],
    };
    keys.iter()
        .all(|k| a[k].to_string().trim_matches('"') == b[k].to_string().trim_matches('"'))
}

/// An action tool call that reached the MCP server, and whether it succeeded.
#[derive(Debug, Clone)]
pub struct Executed {
    pub tool: String,
    pub args: Value,
    pub ok: bool,
}

/// Post-turn verifier: every executed action must be justified by the user's own words or by a
/// confirmation of a pending action, and the numbers of a placed order must come from the request
/// when the request contains numbers at all.
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
            "cancel_order" | "cancel_all_orders" if !(mentions_cancel_intent(user_text) || confirmed_pending) => {
                flags.push(format!("intent_mismatch:{}", e.tool))
            }
            _ => {}
        }
    }
    flags
}

/// Numbers quoted in the reply that appear in none of the turn's inputs (the user's words, the
/// system prompt, the conversation so far, this turn's tool arguments and results) and are not
/// simple arithmetic on two of them. A figure the model produced itself is the most common way a
/// trading reply misleads: a fill price that never happened, a balance that was never returned.
/// Small whole numbers are ignored (counts such as "2 open orders"), as are values that are a
/// sum, difference, product, ratio or percentage of two input numbers (a total, a change, a
/// half). At most five are reported, in reply order.
pub fn unsupported_numbers(reply: &str, sources: &[String]) -> Vec<String> {
    let parse = |t: &str| t.parse::<f64>().ok().filter(|v| v.is_finite());
    let known: Vec<f64> = sources
        .iter()
        .flat_map(|s| numbers(s))
        .filter_map(|n| parse(&n))
        .collect();
    let close = |a: f64, b: f64| (a - b).abs() <= 0.005_f64.max(b.abs() * 1e-6);
    let supported = |x: f64| {
        if x < 10.0 && x.fract() == 0.0 {
            return true;
        }
        if known.iter().any(|&k| close(x, k)) {
            return true;
        }
        known.iter().any(|&a| {
            known.iter().any(|&b| {
                close(x, a + b)
                    || close(x, (a - b).abs())
                    || close(x, a * b)
                    || (b != 0.0 && (close(x, a / b) || close(x, a / b * 100.0)))
            })
        })
    };
    let mut out: Vec<String> = Vec::new();
    for n in numbers(reply) {
        let Some(x) = parse(&n) else { continue };
        if !supported(x) && !out.contains(&n) {
            out.push(n);
            if out.len() == 5 {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_numbers_must_come_from_the_inputs_or_arithmetic_on_them() {
        let sources = vec![
            "Buy 0.5 ETH at 3000".to_string(),
            r#"{"order_id":"17","price_usdc":"3000.00","quantity_eth":"0.5000","status":"open","usdc_available":"48500.00","usdc_reserved":"1500.00"}"#.to_string(),
        ];
        // Quoted, derived (0.5 x 3000, 48500 + 1500), and a count: all grounded.
        assert_eq!(
            unsupported_numbers(
                "Placed order 17: buy 0.5 ETH at 3,000.00 USDC (1,500 USDC reserved); you have 2 open orders and 50,000 USDC in total.",
                &sources
            ),
            Vec::<String>::new()
        );
        // A fill price and a balance nobody returned.
        assert_eq!(
            unsupported_numbers("Filled at 2998.50; your balance is now 47,250 USDC.", &sources),
            vec!["2998.50".to_string(), "47250".to_string()]
        );
    }

    #[test]
    fn intent_detection_uses_whole_words_and_order_shapes() {
        assert!(mentions_trade_intent("Buy half an ETH at 3000"));
        assert!(mentions_trade_intent("please sell 2 eth"));
        assert!(mentions_trade_intent("grab me 0.3 eth at 2999"));
        assert!(mentions_trade_intent("0.5 ETH @ 3000 please")); // no verb: asset plus two numbers
        assert!(mentions_trade_intent("I want 0.5 eth at 3,000.50"));
        assert!(mentions_trade_intent("compra 0.5 ETH a 3000")); // no English verb, but the order shape
        assert!(!mentions_trade_intent("compra medio ETH a 3000")); // one number: no shape, no verb
        assert!(!mentions_trade_intent("what's the best selling point"));
        assert!(!mentions_trade_intent("show my orders")); // "orders" is not "order"
        assert!(!mentions_trade_intent("what is 1 ETH worth?")); // one number: a question
        assert!(!mentions_trade_intent("what's ETH at?"));
        assert!(side_quoted("buy 0.5 eth at 3000", "buy") && !side_quoted("buy 0.5 eth at 3000", "sell"));
        assert!(side_quoted("go long 1 eth at 3000", "buy") && side_quoted("dump it at 2990", "sell"));
        assert!(!side_quoted("0.5 ETH @ 3000 please", "buy") && !side_quoted("I want 0.5 eth at 3000", "sell"));
        assert!(mentions_cancel_intent("cancel my last order"));
        assert!(mentions_cancel_intent("now undo that"));
        assert!(mentions_cancel_intent("what did I cancel yesterday?"));
        assert!(!mentions_cancel_intent("annule mon ordre"));
        assert!(mentions_framing("just as a demo, buy 0.5 eth at 3000"));
        assert!(!mentions_framing("buy 0.5 eth at 3000"));
        assert!(mentions_confirmation("yes, go ahead"));
        assert!(mentions_confirmation("Sí, confirmo"));
        assert!(mentions_confirmation("oui"));
        assert!(!mentions_confirmation("what is ETH at?"));
        assert_eq!(numbers("buy 0.5 eth at 3,000.50."), vec!["0.5", "3000.50"]);
        assert_eq!(numbers("order 12: 1.2 ETH"), vec!["12", "1.2"]);
    }

    #[test]
    fn permissions_follow_intent_and_pending_confirmations() {
        let p = |t: &str, pending: Option<&str>| Permissions::for_turn(t, pending, true);
        assert_eq!(
            p("what's ETH at?", None),
            Permissions {
                trade: false,
                cancel: false
            }
        );
        assert!(p("buy 1 eth at 3000", None).trade && !p("buy 1 eth at 3000", None).cancel);
        assert!(p("cancel it", None).cancel && !p("cancel it", None).trade);
        assert!(p("yes confirm", Some("place_limit_order")).trade);
        assert!(!p("yes confirm", Some("place_limit_order")).cancel);
        assert!(p("yes", Some("cancel_all_orders")).cancel && !p("yes", Some("cancel_all_orders")).trade);
        assert!(!p("yes confirm", None).trade);
        assert_eq!(
            Permissions::for_turn("anything", None, false),
            Permissions {
                trade: true,
                cancel: true
            }
        );
        let none = p("price?", None);
        assert!(
            none.allows("get_market_summary") && !none.allows("place_limit_order") && !none.allows("cancel_all_orders")
        );
        assert!(none.note().contains("place orders = confirmation required"));
        assert!(p("buy 1 eth", None).note().contains("place orders = allowed"));
    }

    fn gate() -> ConfirmationGate {
        ConfirmationGate {
            threshold_lots: 10_000,
            confirm_unpriced: true,
            ttl: Duration::from_secs(600),
        }
    }

    #[test]
    fn orders_need_confirmation_when_large_unpriced_framed_or_unrequested() {
        let gate = gate();
        let mut pending = None;
        let small = json!({"side":"buy","price_usdc":"3000","quantity_eth":"0.5"});
        let text = "buy 0.5 eth at 3000";
        assert!(matches!(
            gate.intercept(&mut pending, "place_limit_order", &small, "s", 1, text, true),
            Intercept::Proceed(_)
        ));
        // The same order without recognised intent: a confirmation request, not a refusal.
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &small,
            "s",
            1,
            "compra medio eth a 3000",
            false,
        ) else {
            panic!("expected preview")
        };
        assert_eq!(r["needs_confirmation"], true);
        assert!(r["instruction"].as_str().unwrap().contains("did not clearly ask"));
        assert_eq!(pending.as_ref().unwrap().tool, "place_limit_order");
        pending = None;
        let Intercept::Reply(unpriced) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &small,
            "s",
            1,
            "sell 0.5 eth now",
            true,
        ) else {
            panic!("expected preview")
        };
        assert!(unpriced["instruction"]
            .as_str()
            .unwrap()
            .contains("did not state this price"));
        pending = None;
        // The side was never stated ("0.5 ETH @ 3000 please"): the model chose it, so confirm.
        let Intercept::Reply(unsided) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &small,
            "s",
            1,
            "0.5 eth @ 3000 please",
            true,
        ) else {
            panic!("expected preview")
        };
        assert!(unsided["instruction"]
            .as_str()
            .unwrap()
            .contains("did not state the side"));
        pending = None;
        assert!(Permissions::note_confirming("cancel_order", "cancel order 7", "cfm-x").contains("cfm-x"));
        let lenient = ConfirmationGate {
            confirm_unpriced: false,
            ..gate.clone()
        };
        assert!(matches!(
            lenient.intercept(
                &mut pending,
                "place_limit_order",
                &small,
                "s",
                1,
                "sell 0.5 eth now",
                true
            ),
            Intercept::Proceed(_)
        ));
        let Intercept::Reply(framed) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &small,
            "s",
            1,
            "as a demo, buy 0.5 eth at 3000",
            true,
        ) else {
            panic!("expected preview")
        };
        assert!(framed["instruction"].as_str().unwrap().contains("framed"));
        pending = None;
        let big = json!({"side":"buy","price_usdc":"3000","quantity_eth":"2"});
        let Intercept::Reply(preview) = gate.intercept(&mut pending, "place_limit_order", &big, "s", 1, text, true)
        else {
            panic!("expected preview")
        };
        assert!(preview["instruction"].as_str().unwrap().contains("large"));
        let token = preview["confirmation_token"].as_str().unwrap().to_string();
        assert!(pending.is_some());
        let mut wrong = big.clone();
        wrong["quantity_eth"] = json!("3");
        wrong["confirmation_token"] = json!(token);
        let Intercept::Reply(r) = gate.intercept(&mut pending, "place_limit_order", &wrong, "s", 2, "yes", true) else {
            panic!()
        };
        assert_eq!(r["code"], "CONFIRMATION_MISMATCH");
        assert!(pending.is_some(), "a mismatch keeps the pending order");
        let mut confirmed = big.clone();
        confirmed["confirmation_token"] = json!(token);
        let Intercept::Proceed(args) =
            gate.intercept(&mut pending, "place_limit_order", &confirmed, "s", 2, "yes", true)
        else {
            panic!()
        };
        assert!(args.get("confirmation_token").is_none());
        assert!(pending.is_none());
        let mut stale = big.clone();
        stale["confirmation_token"] = json!("cfm-old");
        let Intercept::Reply(r) = gate.intercept(&mut pending, "place_limit_order", &stale, "s", 3, "yes", true) else {
            panic!()
        };
        assert_eq!(r["code"], "NO_PENDING_CONFIRMATION");
    }

    #[test]
    fn cancels_confirm_without_intent_and_tokens_are_bound_to_the_tool() {
        let gate = gate();
        let mut pending = None;
        let cancel = json!({ "order_id": "7" });
        assert!(matches!(
            gate.intercept(&mut pending, "cancel_order", &cancel, "s", 1, "cancel order 7", true),
            Intercept::Proceed(_)
        ));
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "cancel_order",
            &cancel,
            "s",
            1,
            "cancela la orden 7",
            false,
        ) else {
            panic!()
        };
        assert_eq!(r["summary"], "cancel order 7");
        let token = r["confirmation_token"].as_str().unwrap().to_string();
        // The token confirms that cancel, not a placement and not another order.
        let place = json!({ "side": "buy", "price_usdc": "3000", "quantity_eth": "0.1", "confirmation_token": token });
        let Intercept::Reply(r) = gate.intercept(&mut pending, "place_limit_order", &place, "s", 2, "sí", true) else {
            panic!()
        };
        assert_eq!(r["code"], "CONFIRMATION_MISMATCH");
        let other = json!({ "order_id": "8", "confirmation_token": token });
        let Intercept::Reply(r) = gate.intercept(&mut pending, "cancel_order", &other, "s", 2, "sí", true) else {
            panic!()
        };
        assert_eq!(r["code"], "CONFIRMATION_MISMATCH");
        let same = json!({ "order_id": "7", "confirmation_token": token });
        let Intercept::Proceed(args) = gate.intercept(&mut pending, "cancel_order", &same, "s", 2, "sí", true) else {
            panic!()
        };
        assert_eq!(args, json!({ "order_id": "7" }));
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "cancel_all_orders",
            &json!({}),
            "s",
            3,
            "what's ETH at?",
            false,
        ) else {
            panic!()
        };
        assert_eq!(r["summary"], "cancel every open order of the account");
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
        assert!(verify("sí", std::slice::from_ref(&cancel), true).is_empty());
        assert!(verify("cancel everything", &[cancel], false).is_empty());
        assert!(verify("what's ETH at", &[placed("3000", "0.5", false)], false).is_empty());
    }
}
