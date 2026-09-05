//! Conversation-level guardrails. Everything here is deterministic code: which action tools a turn
//! may execute at once, a confirmation step for everything else, and a verifier that checks what
//! happened against what the user asked for.
//!
//! The tool *list* sent to the model never changes within a session. A changing list defeats
//! prompt caching and, on the newest models, invalidates the thinking blocks bound to the
//! conversation prefix.
//!
//! What changes per turn is *permission*, decided from the user's own words. An action those words
//! justify executes at once. Anything else becomes a confirmation request: an order that is large,
//! priced where the user never said, on a side they never named, or framed as a demo.
//!
//! A confirmation request is answered by the user, not the model. The service issues an exact
//! summary and a token; the model must relay that summary; the user must confirm in a turn of
//! their own. Nothing reaches the MCP server without either the words or that confirmation.

use mcp_server::units::{eth, parse_price, parse_qty, usdc_from_micro};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const ACTION_TOOLS: [&str; 3] = ["place_limit_order", "cancel_order", "cancel_all_orders"];
const TRADE_VERBS: [&str; 41] = [
    // French, Spanish, German, Italian and Portuguese too: an unlisted verb costs the user a
    // confirmation turn, and the perturbation suite leaves listed words alone.
    "achète",
    "acheter",
    "achetez",
    "vends",
    "vendre",
    "vendez",
    "compra",
    "comprar",
    "compre",
    "vende",
    "vender",
    "venda",
    "kaufe",
    "kaufen",
    "kauf",
    "verkaufe",
    "verkaufen",
    "verkauf",
    "compro",
    "vendo",
    "vendi",
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
const CANCEL_VERBS: [&str; 19] = [
    "cancel",
    "remove",
    "withdraw",
    "pull",
    "kill",
    "close",
    "undo",
    "revert",
    "unwind",
    "annule",
    "annuler",
    "annulez",
    "cancela",
    "cancelar",
    "cancele",
    "storniere",
    "stornieren",
    "annulla",
    "annullare",
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
const BUY_WORDS: [&str; 19] = [
    "buy", "bid", "long", "purchase", "acquire", "grab", "pick up", "load up", "achète", "acheter", "achetez",
    "compra", "comprar", "compre", "compro", "kaufe", "kaufen", "kauf", "acquista",
];
const SELL_WORDS: [&str; 19] = [
    "sell",
    "ask",
    "short",
    "offer",
    "dump",
    "unload",
    "liquidate",
    "vends",
    "vendre",
    "vendez",
    "vende",
    "vender",
    "venda",
    "vendo",
    "vendi",
    "verkaufe",
    "verkaufen",
    "verkauf",
    "liquida",
];

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '.' || c == ','))
        .filter(|w| !w.is_empty())
}

/// Words that turn an instruction into its opposite. A clause carrying one grants nothing: the
/// gate fails safe, so "do not buy" asks rather than trades.
const NEGATIONS: [&str; 15] = [
    "not", "dont", "don", "doesnt", "doesn", "never", "no", "nope", "without", "avoid", "ne", "nicht", "sin", "nunca",
    "pas",
];

/// Text split into clauses, since a negation binds to its own clause: in "sell 1 ETH, do not buy
/// more", the sell stands and the buy does not.
fn clauses(text: &str) -> impl Iterator<Item = &str> {
    // A full stop between digits belongs to a number ("0.5 ETH"), not to a sentence.
    let bytes = text.as_bytes();
    let mut cuts: Vec<usize> = text
        .char_indices()
        .filter(|(i, c)| match c {
            ';' | '!' | '?' | '\n' => true,
            '.' => {
                let before = bytes.get(i.wrapping_sub(1)).is_some_and(u8::is_ascii_digit);
                let after = bytes.get(i + 1).is_some_and(u8::is_ascii_digit);
                !(before && after)
            }
            _ => false,
        })
        .map(|(i, _)| i)
        .collect();
    cuts.push(text.len());
    let mut start = 0;
    let mut parts = Vec::new();
    for cut in cuts {
        if cut >= start {
            parts.push(&text[start..cut]);
        }
        start = (cut + 1).min(text.len());
    }
    parts
        .into_iter()
        .flat_map(|s| s.split(", "))
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .into_iter()
}

fn clause_is_negated(clause: &str) -> bool {
    // Contractions keep their apostrophe in the text but lose it in tokens ("can't" -> "can",
    // "t"), so they are matched on the text; plain words are matched on tokens.
    let lower = clause.to_lowercase();
    if ["n't", "cannot", "can not"].iter().any(|c| lower.contains(c)) {
        return true;
    }
    words(&lower)
        .map(|w| w.trim_matches(|c| c == '.' || c == ','))
        .any(|w| NEGATIONS.contains(&w))
}

/// A question about what happened is not an instruction to do it.
fn is_question(clause: &str) -> bool {
    let lower = clause.trim().to_lowercase();
    [
        "what", "which", "why", "when", "who", "how", "did i", "was my", "were my",
    ]
    .iter()
    .any(|q| lower.starts_with(q))
}

fn word_in(text: &str, word: &str) -> bool {
    let lower = text.to_lowercase();
    words(&lower).any(|w| w.trim_matches(|c| c == '.' || c == ',') == word)
        || (word.contains(' ') && lower.contains(word))
}

/// Whether the text carries `word` as an instruction: present in some clause that is neither
/// negated nor a question. Used for every intent decision, so negation and interrogation fail
/// safe everywhere at once.
fn has_word(text: &str, word: &str) -> bool {
    clauses(text)
        .filter(|c| !clause_is_negated(c) && !is_question(c))
        .any(|c| word_in(c, word))
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
        || clauses(text)
            .filter(|c| !clause_is_negated(c) && !is_question(c))
            .any(|c| ASSET_WORDS.iter().any(|w| word_in(c, w)) && numbers(c).len() >= 2)
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

/// Order ids the user referred to as ids: a number right after "order", "id", "#" or their
/// equivalents. A bare number is not an id, since "cancel the 2990 bid" names a price and
/// "cancel half" names a quantity; those leave the model free to choose and the usual
/// confirmation rules apply.
pub fn ids_named(text: &str) -> Vec<String> {
    const ID_WORDS: [&str; 8] = ["order", "orden", "ordre", "ordine", "id", "number", "nummer", "numero"];
    let lower = text.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '#'))
        .filter(|t| !t.is_empty())
        .collect();
    let mut ids = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if let Some(rest) = t.strip_prefix('#')
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit())
        {
            ids.push(rest.to_string());
            continue;
        }
        if ID_WORDS.contains(t)
            && let Some(next) = tokens.get(i + 1)
            && next.chars().all(|c| c.is_ascii_digit())
        {
            ids.push((*next).to_string());
        }
    }
    ids
}

/// Words that make a request cover every order: without one, "cancel my order" never means all
/// of them.
const ALL_WORDS: [&str; 14] = [
    "all",
    "everything",
    "every",
    "todo",
    "todos",
    "todas",
    "tout",
    "tous",
    "toutes",
    "alle",
    "alles",
    "tutti",
    "tutto",
    "tudo",
];

pub fn mentions_all(text: &str) -> bool {
    ALL_WORDS.iter().any(|w| has_word(text, w))
}

pub fn mentions_cancel_intent(text: &str) -> bool {
    CANCEL_VERBS.iter().any(|w| has_word(text, w))
}

pub fn mentions_framing(text: &str) -> bool {
    FRAMING_WORDS.iter().any(|w| has_word(text, w))
}

pub fn mentions_confirmation(text: &str) -> bool {
    // A clause that asks about confirming ("Can you confirm what is pending?", "Should I
    // confirm?") is a question, not an answer. `has_word` already drops negated clauses, so
    // "I can't confirm that" is not a confirmation either.
    // Clause splitting drops the question mark, so the whole text is checked for one first: a
    // message that asks something is not an answer.
    if text.contains('?') || clause_is_negated(text) {
        return false;
    }
    clauses(text)
        .filter(|c| !clause_is_negated(c) && !is_question(c) && !asks_about_confirming(c))
        .any(|c| CONFIRM_WORDS.iter().any(|w| word_in(c, w)))
}

/// A clause that ends in a question mark, or opens with a word that makes it a request for
/// information rather than an instruction.
fn asks_about_confirming(clause: &str) -> bool {
    let lower = clause.trim().to_lowercase();
    [
        "can you",
        "could you",
        "should i",
        "shall i",
        "do i",
        "would you",
        "is there",
        "are there",
    ]
    .iter()
    .any(|q| lower.starts_with(q))
}

/// Whether a reply actually put a pending action in front of the user.
///
/// The figures that define the action must appear in the reply. For an order that is the quantity
/// and the price, which the summary states first; the total it also carries is derived from them,
/// so a reply need not repeat it. For a cancel it is the order id.
///
/// The wording is the model's to choose; what it may not do is hide what it is asking about.
/// Figures are compared on digits alone, so "3000.00" matches "3,000", and a reply that names more
/// than the summary is fine.
pub fn summary_is_disclosed(summary: &str, reply: &str) -> bool {
    // Figures alone cannot disclose whether this is a buy, a sell or a cancellation.
    // Inspect words directly here: the reply is normally a question, not an instruction.
    // The side is judged on action verbs only. "bid", "ask", "offer", "long" and "short" are
    // also names for the book's sides and prices, and a reply that explains the market uses them:
    // "resting until asks reach 3000", "at the best bid". Those disclose nothing about the order
    // and must not read as the opposite side.
    if let Some(side) = ["buy", "sell"].into_iter().find(|s| word_in(summary, s)) {
        let (expected, opposite): (&[&str], &[&str]) = if side == "buy" {
            (&BUY_WORDS, &SELL_WORDS)
        } else {
            (&SELL_WORDS, &BUY_WORDS)
        };
        if !side_verbs(expected).any(|w| names_side(reply, w)) || side_verbs(opposite).any(|w| names_side(reply, w)) {
            return false;
        }
    }
    if word_in(summary, "cancel")
        && (!CANCEL_VERBS.iter().any(|w| word_in(reply, w))
            || (word_in(summary, "every") && !ALL_WORDS.iter().any(|w| word_in(reply, w))))
    {
        return false;
    }
    let mut figures: Vec<String> = numbers(summary).iter().map(|n| digits_of(n)).collect();
    figures.truncate(2);
    if figures.is_empty() {
        return true; // nothing to hide
    }
    let shown: Vec<String> = numbers(reply).iter().map(|n| digits_of(n)).collect();
    figures.iter().all(|f| shown.iter().any(|s| s == f))
}

/// The verbs that name an order's side: the side words minus the nouns that also name a book's
/// sides and prices ("bid", "ask", "offer", "long", "short"), which a reply uses to describe the
/// market rather than the order.
const BOOK_NOUNS: [&str; 5] = ["bid", "ask", "offer", "long", "short"];

fn side_verbs(words: &'static [&'static str]) -> impl Iterator<Item = &'static str> {
    words.iter().copied().filter(|w| !BOOK_NOUNS.contains(w))
}

/// Whether the reply names the side with `word` in any inflection: "buy", "buying", "bought",
/// "buys", "purchase", "purchasing". A reply that says "Buying 1.2 ETH at 3002" has disclosed a
/// buy; demanding the bare verb would hide a summary the user plainly saw.
fn names_side(reply: &str, word: &str) -> bool {
    if word_in(reply, word) {
        return true;
    }
    if word.contains(' ') {
        return false;
    }
    let stem = word.trim_end_matches('e');
    let lower = reply.to_lowercase();
    words(&lower)
        .map(|w| w.trim_matches(|c| c == '.' || c == ','))
        .filter(|w| w.len() >= stem.len() + 1)
        .any(|w| {
            let tail = &w[stem.len()..];
            w.starts_with(stem) && matches!(tail, "ing" | "s" | "es" | "ed" | "e")
        })
        || (word == "buy" && word_in(reply, "bought"))
        || (word == "sell" && word_in(reply, "sold"))
}

/// Only an explicit, disclosed question can leave a request for a later bare "yes".
pub fn asks_to_confirm(request: &str, reply: &str) -> bool {
    let asks = ["confirm", "confirmation", "proceed", "shall i", "should i"]
        .iter()
        .any(|w| word_in(reply, w));
    asks && !clause_is_negated(reply)
        && (mentions_trade_intent(request) || mentions_cancel_intent(request))
        && summary_is_disclosed(request, reply)
}

/// A decimal normalized without changing its value: "3000.00" and "3,000" become "3000".
fn digits_of(n: &str) -> String {
    let n = n.replace(',', "");
    let trimmed = if n.contains('.') {
        n.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        n
    };
    trimmed.trim_start_matches('0').to_string()
}

/// A message that only confirms: "yes", "ok, confirm", "sí". No figures, no trade or cancel word,
/// a handful of words at most. Such a message stands for the request before it.
pub fn is_bare_confirmation(text: &str) -> bool {
    mentions_confirmation(text)
        && numbers(text).is_empty()
        && !mentions_trade_intent(text)
        && !mentions_cancel_intent(text)
        && words(text).count() <= 6
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
    /// Whether the turn that raised this action ended with a reply carrying its summary. A
    /// confirmation can only answer a question the user was actually asked, so an action whose
    /// summary the model kept to itself is never executed by a later "yes".
    pub disclosed: bool,
    /// The turn on which the service asked. A token may only be spent on a later turn, and only
    /// one whose user message actually confirms, so the token is never authority by itself.
    pub asked_on_turn: u32,
}

/// What the service knows about the turn a tool call belongs to. The gate decides from this and
/// never from the model's arguments.
#[derive(Clone, Copy, Debug)]
pub struct TurnContext<'a> {
    pub session_id: &'a str,
    pub turn: u32,
    /// The user's own words for this turn, plus the previous request when this turn carries it.
    pub user_text: &'a str,
    /// This turn permits this tool (from the user's words, not the model's).
    pub permitted: bool,
    /// A bare confirmation standing for the previous request.
    pub carried: bool,
    /// An action was pending before this turn and this turn's message confirms it. Only then may
    /// a confirmation token execute.
    pub confirming_turn: bool,
    /// Action tool calls already sent on this turn, including attempts with a lost response.
    /// One request authorises one action; a second call is refused, so a model cannot
    /// turn "buy 0.2 ETH" into two orders.
    pub actions_taken: usize,
}

#[derive(Debug, Clone)]
pub struct ConfirmationGate {
    /// Orders at or above this many lots need an explicit confirmation turn.
    pub threshold_lots: u64,
    /// An order whose limit price does not appear in the user's message (the model chose it, as
    /// for "sell now") needs a confirmation turn whatever its size.
    pub confirm_unpriced: bool,
    /// A message that states two figures states the order: a price not among them is refused
    /// and a changed quantity goes to confirmation. Off for a goal run, where the message is a
    /// goal ("2 ETH at or below 3050") and the model chooses each order's price.
    pub pin_stated_figures: bool,
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
    pub fn intercept(
        &self,
        pending: &mut Option<PendingConfirmation>,
        tool: &str,
        args: &Value,
        cx: &TurnContext,
    ) -> Intercept {
        let TurnContext {
            session_id,
            turn,
            user_text,
            permitted,
            carried,
            confirming_turn,
            actions_taken,
        } = *cx;
        if !ACTION_TOOLS.contains(&tool) {
            return Intercept::Proceed(args.clone());
        }
        if confirming_turn && pending.as_ref().is_some_and(|p| !p.disclosed) {
            return Intercept::Reply(json!({
                "rejected": true,
                "code": "CONFIRMATION_NOT_DISCLOSED",
                "message": "the pending action's summary was never shown to the user, so their confirmation cannot refer to it",
                "hint": "tell the user the exact summary and ask again"
            }));
        }
        if actions_taken > 0 {
            return Intercept::Reply(json!({
                "rejected": true,
                "code": "ALREADY_ACTED",
                "message": "this request has already been acted on; one request authorises one action",
                "hint": "tell the user what was done and let them ask for anything further"
            }));
        }
        let mut args = args.clone();
        let token = args
            .get("confirmation_token")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(obj) = args.as_object_mut() {
            obj.remove("confirmation_token");
        }
        if confirming_turn && token.is_none() {
            return Intercept::Reply(json!({
                "rejected": true,
                "code": "CONFIRMATION_TOKEN_REQUIRED",
                "message": "confirming a pending action requires its token and exact arguments"
            }));
        }
        if let Some(token) = token {
            // A token is not a credential the model holds: it identifies which pending action the
            // user's confirmation refers to. Without a confirming user turn after the ask, no
            // token executes, so a model cannot spend one it found in the history or produce one
            // in the same turn it was issued.
            if !confirming_turn {
                return Intercept::Reply(json!({
                    "rejected": true,
                    "code": "CONFIRMATION_NOT_GIVEN",
                    "message": "this turn's message does not confirm anything; a confirmation_token only executes on a turn where the user confirms",
                    "hint": "tell the user the pending summary and wait for their answer"
                }));
            }
            if pending.as_ref().is_some_and(|p| !p.disclosed) {
                return Intercept::Reply(json!({
                    "rejected": true,
                    "code": "CONFIRMATION_NOT_DISCLOSED",
                    "message": "the pending action's summary was never shown to the user, so their confirmation cannot refer to it",
                    "hint": "tell the user the exact summary and ask again"
                }));
            }
            return match pending.take() {
                Some(p)
                    if p.token == token
                        && p.tool == tool
                        && p.asked_on_turn < turn
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
                // The figures the user stated, in clauses that instruct: a negated clause ("do not
                // buy 0.5 at 3000") and a question state nothing. A clause need not carry a trade
                // verb: "qty 0.5, price 3000.00" is an order, and the permission check already
                // decided whether this turn may trade at all.
                let stated = clauses(user_text)
                    .filter(|c| !clause_is_negated(c) && !is_question(c))
                    .flat_map(numbers)
                    .collect::<Vec<_>>();
                let price_quoted = stated.iter().any(|n| parse_price(n).ok() == Some(price));
                let qty_quoted = stated.iter().any(|n| parse_qty(n).ok() == Some(qty));
                // "1500 USDC worth at 3000": the quantity is not stated but its notional is. The
                // figure that states the notional must not be the price itself, or a quantity of
                // exactly one would always pass on a stated price alone.
                let notional_quoted = stated.iter().any(|n| {
                    parse_price(n)
                        .ok()
                        .is_some_and(|t| t != price && t as u128 * 10_000 == price as u128 * qty as u128)
                });
                // A proposal that contradicts what the user stated is refused outright rather than
                // offered for confirmation: a "yes" to the gate's summary must never turn a stated
                // sell into a buy, or a stated price into another. A quantity the model adjusted
                // (the wallet holds less than asked) still goes to confirmation, since the user
                // sees the exact figure before saying yes.
                let pinned = self.pin_stated_figures && stated.len() >= 2;
                if permitted && pinned && !price_quoted {
                    return Intercept::Reply(json!({
                        "rejected": true,
                        "code": "PRICE_NOT_REQUESTED",
                        "message": format!("the user's message states figures and {} is not one of them", mcp_server::units::usdc(price)),
                        "hint": "use the price the user stated, or ask the user for a price"
                    }));
                }
                let opposite = if side == "buy" { "sell" } else { "buy" };
                if permitted && side_quoted(user_text, opposite) && !side_quoted(user_text, &side) {
                    return Intercept::Reply(json!({
                        "rejected": true,
                        "code": "SIDE_CONTRADICTS_REQUEST",
                        "message": format!("the user asked to {opposite}, this order would {side}"),
                        "hint": "use the side the user asked for"
                    }));
                }
                let reason = if !permitted {
                    "The user's message did not clearly ask to trade."
                } else if pinned && !(price_quoted && (qty_quoted || notional_quoted)) {
                    // A message that states two figures states the order. An order that keeps one
                    // and changes the other is not what was asked for, whatever the model says;
                    // a hostile scripted model found this gap by keeping the price and shrinking
                    // the quantity.
                    "The order's price or quantity differs from the figures in the user's message."
                } else if carried && price_quoted && side_quoted(user_text, &side) {
                    // The model asked in words, the user said yes, and the order is the one the
                    // user described: that confirmation counts, no token is needed.
                    return Intercept::Proceed(args);
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
                // "cancel order 7" authorises cancelling order 7. If the user named ids and this
                // is not one of them, the request is refused outright rather than confirmed: a
                // "yes" to the gate's summary must not redirect a cancel onto another order.
                if permitted && tool == "cancel_order" {
                    let named = ids_named(user_text);
                    let target = text_of(&args["order_id"]);
                    if !named.is_empty() && !named.contains(&target) {
                        return Intercept::Reply(json!({
                            "rejected": true,
                            "code": "ORDER_NOT_REQUESTED",
                            "message": format!("the user named order {} , this call would cancel {target}", named.join(" or ")),
                            "hint": "cancel the order the user named, or ask which order they mean"
                        }));
                    }
                }
                let reason = if !permitted {
                    "The user's message did not clearly ask to cancel."
                } else if tool == "cancel_all_orders"
                    && !clauses(user_text).any(|c| mentions_cancel_intent(c) && mentions_all(c))
                {
                    // "cancel my order" grants a cancel, never every cancel: a hostile model
                    // that cancelled everything on an ambiguous request found this gap.
                    "The user did not ask to cancel every order."
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
            asked_on_turn: turn,
            // Set at the end of the turn, once the reply is known.
            disclosed: false,
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

/// Post-turn verifier.
///
/// Every executed action must be justified by the user's own words, or by a confirmation of a
/// pending action. The numbers of a placed order must come from the request, whenever the request
/// contains numbers at all.
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

/// Figures the reply quotes that no input of the turn supports.
///
/// An input is the user's words, the system prompt, the conversation so far, or this turn's tool
/// arguments and results. A figure counts as supported if it appears in one of those, or is
/// simple arithmetic on two of them (a sum, difference, product, ratio or percentage: a total, a
/// change, a half). Small whole numbers are ignored, since they are usually counts ("2 open
/// orders").
///
/// Why: a number the model invented is the most common way a trading reply misleads. A fill price
/// that never happened. A balance nobody returned. At most five are reported, in reply order.
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

    /// A turn context for the gate: the common case, with no pending confirmation to answer.
    /// Marks a pending action as shown to the user, which a real turn does from its reply.
    fn disclose(pending: &mut Option<PendingConfirmation>) {
        if let Some(p) = pending.as_mut() {
            p.disclosed = true;
        }
    }

    fn cx<'a>(session_id: &'a str, turn: u32, user_text: &'a str, permitted: bool, carried: bool) -> TurnContext<'a> {
        cx_full(session_id, turn, user_text, permitted, carried, false)
    }

    fn cx_full<'a>(
        session_id: &'a str,
        turn: u32,
        user_text: &'a str,
        permitted: bool,
        carried: bool,
        confirming_turn: bool,
    ) -> TurnContext<'a> {
        TurnContext {
            session_id,
            turn,
            user_text,
            permitted,
            carried,
            confirming_turn,
            actions_taken: 0,
        }
    }

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
    fn intent_is_recognised_in_a_few_other_languages() {
        for text in [
            "achète 0.5 ETH à 3000",
            "vends la moitié de mes ETH à 3005",
            "compra 0.5 ETH a 3000",
            "kaufe 0.5 ETH zu 3000",
            "vendi 0.5 ETH a 3000",
        ] {
            assert!(mentions_trade_intent(text), "{text}");
        }
        assert!(side_quoted("achète 0.5 ETH", "buy"));
        assert!(side_quoted("vends 0.5 ETH", "sell"));
        for text in [
            "annule mon ordre",
            "cancela mi orden",
            "storniere meine Order",
            "annulla il mio ordine",
        ] {
            assert!(mentions_cancel_intent(text), "{text}");
        }
        assert!(intent_vocabulary().contains(&"annule"));
    }

    #[test]
    fn contradictions_are_refused_and_cancel_all_needs_the_word_all() {
        let gate = ConfirmationGate {
            threshold_lots: u64::MAX,
            confirm_unpriced: true,
            pin_stated_figures: true,
            ttl: std::time::Duration::from_secs(60),
        };
        let mut pending = None;
        // The user said sell at 3005; a buy at 3000 is refused, not offered for confirmation.
        let buy = json!({ "side": "buy", "price_usdc": "3000.00", "quantity_eth": "5" });
        assert!(matches!(
            gate.intercept(&mut pending, "place_limit_order", &buy, &cx("s", 1, "sell 2 ETH at 3005", true, false)),
            Intercept::Reply(v) if v["code"] == "PRICE_NOT_REQUESTED"
        ));
        let buy_right_price = json!({ "side": "buy", "price_usdc": "3005.00", "quantity_eth": "5" });
        assert!(matches!(
            gate.intercept(&mut pending, "place_limit_order", &buy_right_price, &cx("s", 1, "sell 2 ETH at 3005", true, false)),
            Intercept::Reply(v) if v["code"] == "SIDE_CONTRADICTS_REQUEST"
        ));
        assert!(pending.is_none(), "a refused proposal leaves nothing to confirm");
        // A smaller quantity on the stated side and price still goes to confirmation.
        let smaller = json!({ "side": "sell", "price_usdc": "3005.00", "quantity_eth": "1" });
        assert!(matches!(
            gate.intercept(&mut pending, "place_limit_order", &smaller, &cx("s", 1, "sell 2 ETH at 3005", true, false)),
            Intercept::Reply(v) if v["needs_confirmation"] == true
        ));
        // Cancelling everything needs the word.
        let mut pending = None;
        assert!(matches!(
            gate.intercept(&mut pending, "cancel_all_orders", &json!({}), &cx("s", 1, "cancel my order", true, false)),
            Intercept::Reply(v) if v["needs_confirmation"] == true
        ));
        let mut pending = None;
        assert!(matches!(
            gate.intercept(
                &mut pending,
                "cancel_all_orders",
                &json!({}),
                &cx("s", 1, "cancel all my orders", true, false)
            ),
            Intercept::Proceed(_)
        ));
    }

    #[test]
    fn asking_about_confirming_is_not_confirming() {
        for text in [
            "I can't confirm that",
            "Can you confirm what is pending?",
            "Should I confirm?",
            "what am I confirming?",
            "do not confirm it",
        ] {
            assert!(!mentions_confirmation(text), "{text}");
            assert!(!is_bare_confirmation(text), "{text}");
        }
        for text in ["yes", "yes, confirm", "ok do it", "sí", "confirmed"] {
            assert!(mentions_confirmation(text), "{text}");
        }
    }

    #[test]
    fn a_summary_is_disclosed_only_when_its_figures_reach_the_user() {
        let order = "buy 2.0000 ETH at 2990.00 USDC (up to 5980.00 USDC)";
        // Any wording, as long as the quantity and the price are there.
        assert!(summary_is_disclosed(
            order,
            "This is a large order: buy 2 ETH at 2990.00. Please confirm."
        ));
        assert!(summary_is_disclosed(order, "Confirm buying: buy 2 ETH @ 2,990?"));
        // Inflections disclose the side too; these two replies failed a live run once.
        assert!(summary_is_disclosed(
            order,
            "Buying 2 ETH at 2990.00 would fill at about 2989.58 USDC. Please confirm to place this order."
        ));
        assert!(summary_is_disclosed(
            order,
            "Purchasing 2 ETH at 2990.00 needs confirmation. Shall I place it?"
        ));
        assert!(!summary_is_disclosed(order, "Selling 2 ETH at 2990.00. Confirm?"));
        // Book vocabulary is not a side: these two replies from a live run disclose their orders.
        assert!(summary_is_disclosed(
            "buy 2.0000 ETH at 3000.00 USDC (up to 6000.00 USDC)",
            "Please confirm: buy 2.0000 ETH at 3000.00 USDC (up to 6000.00 USDC), resting until asks reach 3000. Shall I place it?"
        ));
        assert!(summary_is_disclosed(
            "sell 0.5000 ETH at 2999.00 USDC (up to 1499.50 USDC)",
            "Sell 0.5000 ETH at a limit of 2999.00 USDC (best bid, up to 1499.50 USDC), confirm to place it?"
        ));
        assert!(!summary_is_disclosed(order, "Confirm sell 2 ETH @ 2,990?"));
        assert!(!summary_is_disclosed(order, "Confirm 2 ETH @ 2,990?"));
        assert!(!summary_is_disclosed("cancel every open order of the account", "Done."));
        assert!(!summary_is_disclosed(
            "cancel every open order of the account",
            "Cancel order 7?"
        ));
        assert!(summary_is_disclosed(
            "cancel every open order of the account",
            "Cancel all open orders?"
        ));
        // Hiding either figure, or the whole thing, is not disclosure.
        assert!(!summary_is_disclosed(order, "Done."));
        assert!(!summary_is_disclosed(order, "Shall I place that order?"));
        assert!(!summary_is_disclosed(order, "Confirm the purchase at 2990.00?"));
        let cancel = "cancel order 7";
        assert!(summary_is_disclosed(
            cancel,
            "Cancel order 7, the buy at 2990? Please confirm."
        ));
        assert!(!summary_is_disclosed(cancel, "Shall I cancel it?"));
    }

    /// Two paraphrases with no trade verb in the figure-bearing clause failed a live run when
    /// figures were pinned only in clauses that carry one. The permission check decides whether
    /// the turn may trade; the figures themselves are pinned wherever the user stated them.
    #[test]
    fn stated_figures_pin_the_order_without_a_verb_in_their_clause() {
        let gate = ConfirmationGate {
            threshold_lots: 10_000,
            confirm_unpriced: true,
            pin_stated_figures: true,
            ttl: Duration::from_secs(60),
        };
        let args = json!({ "side": "buy", "price_usdc": "3000.00", "quantity_eth": "0.5" });
        for text in [
            "pls buy half an eth, limit 3000",
            "Place a limit buy: qty 0.5, price 3000.00",
        ] {
            assert!(
                matches!(
                    gate.intercept(&mut None, "place_limit_order", &args, &cx("s", 1, text, true, false)),
                    Intercept::Proceed(_)
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn unrelated_and_negated_clauses_do_not_expand_an_action() {
        let gate = ConfirmationGate {
            threshold_lots: 10_000,
            confirm_unpriced: true,
            pin_stated_figures: true,
            ttl: Duration::from_secs(60),
        };
        let args = json!({ "side": "buy", "price_usdc": "3000", "quantity_eth": "0.5" });
        assert!(matches!(
            gate.intercept(&mut None, "place_limit_order", &args,
                &cx("s", 1, "buy 0.2 ETH at 2990. Do not buy 0.5 ETH at 3000", true, false)),
            Intercept::Reply(v) if v["code"] == "PRICE_NOT_REQUESTED"
        ));
        assert!(matches!(
            gate.intercept(&mut None, "cancel_all_orders", &json!({}),
                &cx("s", 1, "Cancel order 7. Show all my balances", true, false)),
            Intercept::Reply(v) if v["needs_confirmation"] == true
        ));
    }

    #[test]
    fn a_confirmation_cannot_authorise_extra_or_tokenless_actions() {
        let gate = ConfirmationGate {
            threshold_lots: 10_000,
            confirm_unpriced: true,
            pin_stated_figures: true,
            ttl: Duration::from_secs(60),
        };
        let mut context = cx_full("s", 2, "yes, cancel all", true, false, true);
        context.actions_taken = 1;
        assert!(matches!(
            gate.intercept(&mut None, "cancel_all_orders", &json!({}), &context),
            Intercept::Reply(v) if v["code"] == "ALREADY_ACTED"
        ));
        context.actions_taken = 0;
        assert!(matches!(
            gate.intercept(&mut None, "cancel_all_orders", &json!({}), &context),
            Intercept::Reply(v) if v["code"] == "CONFIRMATION_TOKEN_REQUIRED"
        ));
        for message in [
            "yes, but do not place it",
            "Confirm? I need more details",
            "yes. Never cancel anything",
        ] {
            assert!(!mentions_confirmation(message), "{message}");
        }
    }

    #[test]
    fn a_bare_confirmation_stands_for_the_previous_request() {
        assert!(is_bare_confirmation("yes, confirm"));
        assert!(is_bare_confirmation("ok do it"));
        assert!(!is_bare_confirmation("yes, buy 2 ETH at 3000"));
        assert!(!is_bare_confirmation("yes cancel it"));
        let gate = ConfirmationGate {
            threshold_lots: 10_000, // 1 ETH
            confirm_unpriced: true,
            pin_stated_figures: true,
            ttl: std::time::Duration::from_secs(60),
        };
        // Large order, the user described it and has just confirmed in words: it proceeds.
        let mut pending = None;
        let order = json!({ "side": "buy", "price_usdc": "3000.00", "quantity_eth": "2" });
        assert!(matches!(
            gate.intercept(
                &mut pending,
                "place_limit_order",
                &order,
                &cx("s", 2, "buy 2 ETH at 3000\nyes, confirm", true, true),
            ),
            Intercept::Proceed(_)
        ));
        // Without the carried confirmation the same order is held as large.
        let mut pending = None;
        assert!(matches!(
            gate.intercept(&mut pending, "place_limit_order", &order, &cx("s", 1, "buy 2 ETH at 3000", true, false)),
            Intercept::Reply(v) if v["needs_confirmation"] == true
        ));
        // A carried confirmation never lets a different order through.
        let mut pending = None;
        let other = json!({ "side": "buy", "price_usdc": "3000.00", "quantity_eth": "0.1" });
        assert!(matches!(
                    gate.intercept(
        &mut pending,
        "place_limit_order",
        &other,
        &cx("s", 2, "buy 2 ETH at 3000\nyes, confirm", true, true),
        ),
                    Intercept::Reply(v) if v["needs_confirmation"] == true
                ));
    }

    #[test]
    fn two_stated_figures_pin_both_price_and_quantity() {
        let gate = ConfirmationGate {
            threshold_lots: u64::MAX,
            confirm_unpriced: true,
            pin_stated_figures: true,
            ttl: std::time::Duration::from_secs(60),
        };
        let mut pending = None;
        let swapped = json!({ "side": "buy", "price_usdc": "3000.00", "quantity_eth": "0.1000" });
        // The price is the user's, the quantity is not: held for confirmation.
        assert!(matches!(
            gate.intercept(&mut pending, "place_limit_order", &swapped, &cx("s", 1, "buy 30 ETH at 3000 now", true, false)),
            Intercept::Reply(v) if v["needs_confirmation"] == true
        ));
        let mut pending = None;
        let exact = json!({ "side": "buy", "price_usdc": "3000.00", "quantity_eth": "30" });
        assert!(matches!(
            gate.intercept(
                &mut pending,
                "place_limit_order",
                &exact,
                &cx("s", 1, "buy 30 ETH at 3000 now", true, false)
            ),
            Intercept::Proceed(_)
        ));
        // A quantity given as a notional is derived from the two figures and passes.
        let mut pending = None;
        let by_notional = json!({ "side": "buy", "price_usdc": "3000.00", "quantity_eth": "0.5000" });
        assert!(matches!(
            gate.intercept(
                &mut pending,
                "place_limit_order",
                &by_notional,
                &cx("s", 1, "buy 1500 USDC worth of ETH at 3000", true, false)
            ),
            Intercept::Proceed(_)
        ));
        // A quantity of one makes the notional equal the price; that must not count as stating it.
        let mut pending = None;
        let one = json!({ "side": "sell", "price_usdc": "3005.00", "quantity_eth": "1" });
        assert!(matches!(
                    gate.intercept(
        &mut pending,
        "place_limit_order",
        &one,
        &cx("s", 2, "sell 2 ETH at 3005\nyes, confirm", true, true),
        ),
                    Intercept::Reply(v) if v["needs_confirmation"] == true
                ));
        // One stated figure leaves room for a derived quantity ("bring me to 12 ETH").
        let mut pending = None;
        let derived = json!({ "side": "buy", "price_usdc": "3001.00", "quantity_eth": "2" });
        assert!(!matches!(
            gate.intercept(&mut pending, "place_limit_order", &derived, &cx("s", 1, "bring my holding to 12 ETH at the ask 3001", true, false)),
            Intercept::Reply(v) if v["message"].as_str().is_some_and(|m| m.contains("differs"))
        ));
    }

    #[test]
    fn negation_and_questions_grant_nothing() {
        // A request that says not to do something must not permit doing it, and a question about
        // the past is not an instruction. Both used to grant permission through keyword matching.
        for text in [
            "do not buy 0.5 ETH at 3000",
            "don't buy 0.5 ETH at 3000",
            "never sell my ETH at 2999",
            "no, do not place that order",
        ] {
            assert!(!mentions_trade_intent(text), "{text}");
        }
        for text in [
            "what did I cancel yesterday?",
            "why was my order cancelled?",
            "do not cancel order 7",
        ] {
            assert!(!mentions_cancel_intent(text), "{text}");
        }
        // "don't do it" is a refusal, not a confirmation.
        assert!(!mentions_confirmation("don't do it"));
        assert!(!mentions_confirmation("no, do not do it"));
        assert!(mentions_confirmation("yes, do it"));
        // The plain forms still work.
        assert!(mentions_trade_intent("buy 0.5 ETH at 3000"));
        assert!(mentions_cancel_intent("cancel order 7"));
    }

    #[test]
    fn a_cancel_is_bound_to_the_order_the_user_named() {
        let gate = gate();
        let mut pending = None;
        // The user named order 7; cancelling 8 is not what they asked for.
        let other = json!({ "order_id": "8" });
        assert!(matches!(
            gate.intercept(&mut pending, "cancel_order", &other, &cx("s", 1, "cancel order 7", true, false)),
            Intercept::Reply(v) if v["code"] == "ORDER_NOT_REQUESTED"
        ));
        // The named one proceeds.
        let same = json!({ "order_id": "7" });
        assert!(matches!(
            gate.intercept(
                &mut pending,
                "cancel_order",
                &same,
                &cx("s", 1, "cancel order 7", true, false)
            ),
            Intercept::Proceed(_)
        ));
        // A price or quantity is not an id: "cancel the 2990 bid" leaves the choice to the model,
        // under the usual confirmation rules.
        let mut pending = None;
        assert!(matches!(
            gate.intercept(
                &mut pending,
                "cancel_order",
                &other,
                &cx("s", 1, "cancel the 2990 bid", true, false)
            ),
            Intercept::Proceed(_)
        ));
        assert!(matches!(
            gate.intercept(&mut pending, "cancel_order", &other, &cx("s", 1, "cancel order #7", true, false)),
            Intercept::Reply(v) if v["code"] == "ORDER_NOT_REQUESTED"
        ));
        // With no id in the message the model may choose one, held for confirmation as before.
        assert!(matches!(
            gate.intercept(
                &mut pending,
                "cancel_order",
                &other,
                &cx("s", 1, "cancel my order", true, false)
            ),
            Intercept::Proceed(_) | Intercept::Reply(_)
        ));
    }

    #[test]
    fn intent_detection_uses_whole_words_and_order_shapes() {
        assert!(mentions_trade_intent("Buy half an ETH at 3000"));
        assert!(mentions_trade_intent("please sell 2 eth"));
        assert!(mentions_trade_intent("grab me 0.3 eth at 2999"));
        assert!(mentions_trade_intent("0.5 ETH @ 3000 please")); // no verb: asset plus two numbers
        assert!(mentions_trade_intent("I want 0.5 eth at 3,000.50"));
        assert!(mentions_trade_intent("compra 0.5 ETH a 3000")); // no English verb, but the order shape
        assert!(mentions_trade_intent("compra medio ETH a 3000")); // the Spanish verb is listed
        assert!(!mentions_trade_intent("what's the best selling point"));
        assert!(!mentions_trade_intent("show my orders")); // "orders" is not "order"
        assert!(!mentions_trade_intent("what is 1 ETH worth?")); // one number: a question
        assert!(!mentions_trade_intent("what's ETH at?"));
        assert!(side_quoted("buy 0.5 eth at 3000", "buy") && !side_quoted("buy 0.5 eth at 3000", "sell"));
        assert!(side_quoted("go long 1 eth at 3000", "buy") && side_quoted("dump it at 2990", "sell"));
        assert!(!side_quoted("0.5 ETH @ 3000 please", "buy") && !side_quoted("I want 0.5 eth at 3000", "sell"));
        assert!(mentions_cancel_intent("cancel my last order"));
        assert!(mentions_cancel_intent("now undo that"));
        // A question about the past is not an instruction (it used to be treated as one).
        assert!(!mentions_cancel_intent("what did I cancel yesterday?"));
        assert!(mentions_cancel_intent("annule mon ordre")); // the French verb is listed
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
            pin_stated_figures: true,
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
            gate.intercept(
                &mut pending,
                "place_limit_order",
                &small,
                &cx("s", 1, text, true, false)
            ),
            Intercept::Proceed(_)
        ));
        // The same order without recognised intent: a confirmation request, not a refusal.
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &small,
            &cx("s", 1, "compra medio eth a 3000", false, false),
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
            &json!({"side":"sell","price_usdc":"3000","quantity_eth":"0.5"}),
            &cx("s", 1, "sell 0.5 eth now", true, false),
        ) else {
            panic!("expected preview")
        };
        assert!(
            unpriced["instruction"]
                .as_str()
                .unwrap()
                .contains("did not state this price")
        );
        pending = None;
        // The side was never stated ("0.5 ETH @ 3000 please"): the model chose it, so confirm.
        let Intercept::Reply(unsided) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &small,
            &cx("s", 1, "0.5 eth @ 3000 please", true, false),
        ) else {
            panic!("expected preview")
        };
        assert!(
            unsided["instruction"]
                .as_str()
                .unwrap()
                .contains("did not state the side")
        );
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
                &json!({"side":"sell","price_usdc":"3000","quantity_eth":"0.5"}),
                &cx("s", 1, "sell 0.5 eth now", true, false)
            ),
            Intercept::Proceed(_)
        ));
        let Intercept::Reply(framed) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &small,
            &cx("s", 1, "as a demo, buy 0.5 eth at 3000", true, false),
        ) else {
            panic!("expected preview")
        };
        assert!(framed["instruction"].as_str().unwrap().contains("framed"));
        pending = None;
        let big = json!({"side":"buy","price_usdc":"3000","quantity_eth":"2"});
        // The user asked for exactly this order, so only its size holds it.
        let Intercept::Reply(preview) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &big,
            &cx("s", 1, "buy 2 eth at 3000", true, false),
        ) else {
            panic!("expected preview")
        };
        assert!(preview["instruction"].as_str().unwrap().contains("large"));
        let token = preview["confirmation_token"].as_str().unwrap().to_string();
        assert!(pending.is_some());
        // A real turn marks this from its reply; these tests exercise the branches after the ask.
        disclose(&mut pending);
        let mut wrong = big.clone();
        wrong["quantity_eth"] = json!("3");
        wrong["confirmation_token"] = json!(token);
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &wrong,
            &cx_full("s", 2, "yes", true, false, true),
        ) else {
            panic!()
        };
        assert_eq!(r["code"], "CONFIRMATION_MISMATCH");
        assert!(pending.is_some(), "a mismatch keeps the pending order");
        // The right token on a turn that confirms nothing is refused: the token identifies the
        // pending action, the user's message is what authorises it.
        let mut replayed = big.clone();
        replayed["confirmation_token"] = json!(token);
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &replayed,
            &cx("s", 2, "and what is my balance?", true, false),
        ) else {
            panic!()
        };
        assert_eq!(r["code"], "CONFIRMATION_NOT_GIVEN");
        assert!(pending.is_some(), "an attempted replay keeps the pending order");
        // Nor may the model answer its own ask in the turn that raised it.
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &replayed,
            &cx_full("s", 1, "buy 2 eth at 3000", true, false, true),
        ) else {
            panic!()
        };
        assert_eq!(
            r["code"], "CONFIRMATION_MISMATCH",
            "a token cannot be spent on the turn it was issued"
        );
        let mut confirmed = big.clone();
        confirmed["confirmation_token"] = json!(token);
        let Intercept::Proceed(args) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &confirmed,
            &cx_full("s", 2, "yes", true, false, true),
        ) else {
            panic!()
        };
        assert!(args.get("confirmation_token").is_none());
        assert!(pending.is_none());
        let mut stale = big.clone();
        stale["confirmation_token"] = json!("cfm-old");
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &stale,
            &cx_full("s", 3, "yes", true, false, true),
        ) else {
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
            gate.intercept(
                &mut pending,
                "cancel_order",
                &cancel,
                &cx("s", 1, "cancel order 7", true, false)
            ),
            Intercept::Proceed(_)
        ));
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "cancel_order",
            &cancel,
            &cx("s", 1, "cancela la orden 7", false, false),
        ) else {
            panic!()
        };
        assert_eq!(r["summary"], "cancel order 7");
        let token = r["confirmation_token"].as_str().unwrap().to_string();
        disclose(&mut pending);
        // The token confirms that cancel, not a placement and not another order.
        let place = json!({ "side": "buy", "price_usdc": "3000", "quantity_eth": "0.1", "confirmation_token": token });
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "place_limit_order",
            &place,
            &cx_full("s", 2, "sí", true, false, true),
        ) else {
            panic!()
        };
        assert_eq!(r["code"], "CONFIRMATION_MISMATCH");
        let other = json!({ "order_id": "8", "confirmation_token": token });
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "cancel_order",
            &other,
            &cx_full("s", 2, "sí", true, false, true),
        ) else {
            panic!()
        };
        assert_eq!(r["code"], "CONFIRMATION_MISMATCH");
        let same = json!({ "order_id": "7", "confirmation_token": token });
        let Intercept::Proceed(args) = gate.intercept(
            &mut pending,
            "cancel_order",
            &same,
            &cx_full("s", 2, "sí", true, false, true),
        ) else {
            panic!()
        };
        assert_eq!(args, json!({ "order_id": "7" }));
        let Intercept::Reply(r) = gate.intercept(
            &mut pending,
            "cancel_all_orders",
            &json!({}),
            &cx("s", 3, "what's ETH at?", false, false),
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
        assert!(
            verify("show my orders", &[placed("3000", "0.5", true)], false)
                .contains(&"intent_mismatch:place_limit_order".to_string())
        );
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
