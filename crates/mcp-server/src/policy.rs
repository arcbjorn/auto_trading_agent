//! Deterministic risk rules, applied before any order reaches the engine. These live in the MCP
//! server because that is the boundary every model path crosses, including desktop hosts that
//! never touch the chat service. They are code and configuration: no prompt can change them.

use crate::units::{eth, usdc_from_micro};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct PolicyConfig {
    /// Largest single order, in lots (default 10 ETH).
    pub max_order_lots: u64,
    /// Largest single order value, in micro-USDC (default 50,000 USDC).
    pub max_order_notional_micro: u128,
    /// Fat-finger collar: reject limit prices further than this many basis points from mid.
    pub collar_bps: u64,
    /// Maximum live orders per account.
    pub max_open_orders: usize,
    /// Actions (place or cancel) per account per rolling minute.
    pub actions_per_minute: u32,
    /// Total order value an account may submit through this process (default 200,000 USDC).
    pub session_notional_cap_micro: u128,
    /// Kill switch: every action is rejected while set.
    pub halted: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            max_order_lots: 10 * 10_000,
            max_order_notional_micro: 50_000 * 1_000_000,
            collar_bps: 1_000,
            max_open_orders: 20,
            actions_per_minute: 10,
            session_notional_cap_micro: 200_000 * 1_000_000,
            halted: false,
        }
    }
}

impl PolicyConfig {
    /// Reads overrides from the environment: POLICY_MAX_ORDER_ETH, POLICY_MAX_ORDER_USDC,
    /// POLICY_COLLAR_BPS, POLICY_MAX_OPEN_ORDERS, POLICY_ACTIONS_PER_MINUTE, POLICY_SESSION_CAP_USDC,
    /// TRADING_HALTED.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        let get = |k: &str| std::env::var(k).ok();
        if let Some(v) = get("POLICY_MAX_ORDER_ETH").and_then(|s| s.parse::<u64>().ok()) {
            cfg.max_order_lots = v * 10_000;
        }
        if let Some(v) = get("POLICY_MAX_ORDER_USDC").and_then(|s| s.parse::<u128>().ok()) {
            cfg.max_order_notional_micro = v * 1_000_000;
        }
        if let Some(v) = get("POLICY_COLLAR_BPS").and_then(|s| s.parse().ok()) {
            cfg.collar_bps = v;
        }
        if let Some(v) = get("POLICY_MAX_OPEN_ORDERS").and_then(|s| s.parse().ok()) {
            cfg.max_open_orders = v;
        }
        if let Some(v) = get("POLICY_ACTIONS_PER_MINUTE").and_then(|s| s.parse().ok()) {
            cfg.actions_per_minute = v;
        }
        if let Some(v) = get("POLICY_SESSION_CAP_USDC").and_then(|s| s.parse::<u128>().ok()) {
            cfg.session_notional_cap_micro = v * 1_000_000;
        }
        cfg.halted = matches!(get("TRADING_HALTED").as_deref(), Some("1") | Some("true") | Some("yes"));
        cfg
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub code: &'static str,
    pub message: String,
    pub hint: String,
}

impl Rejection {
    fn new(code: &'static str, message: String, hint: &str) -> Self {
        Self {
            code,
            message,
            hint: hint.into(),
        }
    }
}

#[derive(Default)]
struct AccountState {
    actions: VecDeque<Instant>,
    notional_used_micro: u128,
}

pub struct Policy {
    cfg: PolicyConfig,
    accounts: Mutex<HashMap<String, AccountState>>,
}

impl Policy {
    pub fn new(cfg: PolicyConfig) -> Self {
        Self {
            cfg,
            accounts: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &PolicyConfig {
        &self.cfg
    }

    fn check_common(&self, account: &str, now: Instant) -> Result<(), Rejection> {
        if self.cfg.halted {
            return Err(Rejection::new(
                "HALTED",
                "trading is halted by the operator".into(),
                "do not retry; tell the user",
            ));
        }
        let mut accounts = self.accounts.lock().expect("policy lock");
        let state = accounts.entry(account.to_string()).or_default();
        while state
            .actions
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
        {
            state.actions.pop_front();
        }
        if u32::try_from(state.actions.len()).unwrap_or(u32::MAX) >= self.cfg.actions_per_minute {
            return Err(Rejection::new(
                "RATE_LIMIT",
                format!("more than {} actions in the last minute", self.cfg.actions_per_minute),
                "wait a minute before the next order or cancel",
            ));
        }
        state.actions.push_back(now);
        Ok(())
    }

    /// Checks a new order and, when it passes, charges its value against the session cap.
    /// `reference_ticks` is the price the collar is measured from (see [`reference_price`]);
    /// `open_orders` is the account's live order count.
    pub fn check_place(
        &self,
        account: &str,
        price_ticks: u64,
        qty_lots: u64,
        reference_ticks: Option<u64>,
        open_orders: usize,
    ) -> Result<(), Rejection> {
        self.check_place_at(
            account,
            price_ticks,
            qty_lots,
            reference_ticks,
            open_orders,
            Instant::now(),
        )
    }

    pub fn check_place_at(
        &self,
        account: &str,
        price_ticks: u64,
        qty_lots: u64,
        reference_ticks: Option<u64>,
        open_orders: usize,
        now: Instant,
    ) -> Result<(), Rejection> {
        self.check_common(account, now)?;
        if qty_lots > self.cfg.max_order_lots {
            return Err(Rejection::new(
                "MAX_ORDER_SIZE",
                format!(
                    "orders are capped at {} ETH; {} ETH requested",
                    eth(self.cfg.max_order_lots),
                    eth(qty_lots)
                ),
                "split the order or reduce the quantity",
            ));
        }
        let notional = price_ticks as u128 * qty_lots as u128;
        if notional > self.cfg.max_order_notional_micro {
            return Err(Rejection::new(
                "MAX_ORDER_VALUE",
                format!(
                    "order value {} USDC exceeds the per-order cap of {} USDC",
                    usdc_from_micro(notional),
                    usdc_from_micro(self.cfg.max_order_notional_micro)
                ),
                "reduce the quantity or the price",
            ));
        }
        if let Some(reference) = reference_ticks {
            let deviation_bps = u128::from(price_ticks.abs_diff(reference)) * 10_000 / u128::from(reference.max(1));
            if deviation_bps > u128::from(self.cfg.collar_bps) {
                return Err(Rejection::new(
                    "PRICE_COLLAR",
                    format!(
                        "limit price is {deviation_bps} bps away from the reference price {}; the collar is {} bps",
                        crate::units::usdc(reference),
                        self.cfg.collar_bps
                    ),
                    "use a price closer to the current market, or ask the user to confirm the intended price",
                ));
            }
        }
        if open_orders >= self.cfg.max_open_orders {
            return Err(Rejection::new(
                "MAX_OPEN_ORDERS",
                format!(
                    "the account already has {open_orders} open orders (limit {})",
                    self.cfg.max_open_orders
                ),
                "cancel an open order first",
            ));
        }
        let mut accounts = self.accounts.lock().expect("policy lock");
        let state = accounts.entry(account.to_string()).or_default();
        if notional
            > self
                .cfg
                .session_notional_cap_micro
                .saturating_sub(state.notional_used_micro)
        {
            return Err(Rejection::new(
                "SESSION_CAP",
                format!(
                    "this session has submitted {} USDC of orders; the cap is {} USDC",
                    usdc_from_micro(state.notional_used_micro),
                    usdc_from_micro(self.cfg.session_notional_cap_micro)
                ),
                "no further orders can be placed in this session",
            ));
        }
        state.notional_used_micro += notional;
        Ok(())
    }

    /// Gives back session-cap room charged by `check_place` for an order the engine never accepted.
    pub fn release(&self, account: &str, price_ticks: u64, qty_lots: u64) {
        let mut accounts = self.accounts.lock().expect("policy lock");
        if let Some(state) = accounts.get_mut(account) {
            state.notional_used_micro = state
                .notional_used_micro
                .saturating_sub(price_ticks as u128 * qty_lots as u128);
        }
    }

    pub fn check_cancel(&self, account: &str) -> Result<(), Rejection> {
        self.check_common(account, Instant::now())
    }
}

/// The price the fat-finger collar is measured from: the midpoint when both sides of the book
/// exist, else the last trade, else the one side that is quoted. `None` only for an empty market
/// with no trade yet, where no reference exists and the collar cannot apply.
pub fn reference_price(best_bid: Option<u64>, best_ask: Option<u64>, last_trade: Option<u64>) -> Option<u64> {
    match (best_bid, best_ask) {
        (Some(b), Some(a)) => Some(b / 2 + a / 2 + (b % 2 + a % 2) / 2),
        _ => last_trade.or(best_bid).or(best_ask),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::new(PolicyConfig {
            actions_per_minute: 3,
            ..PolicyConfig::default()
        })
    }

    #[test]
    fn wide_values_do_not_wrap_the_collar_or_session_cap() {
        let p = Policy::new(PolicyConfig {
            max_order_lots: u64::MAX,
            max_order_notional_micro: u128::MAX,
            session_notional_cap_micro: u128::MAX,
            ..PolicyConfig::default()
        });
        assert_eq!(
            p.check_place("a", u64::MAX, 1, Some(1), 0).unwrap_err().code,
            "PRICE_COLLAR"
        );
        assert!(p.check_place("b", u64::MAX, u64::MAX, None, 0).is_ok());
        assert_eq!(
            p.check_place("b", u64::MAX, u64::MAX, None, 0).unwrap_err().code,
            "SESSION_CAP"
        );
        assert_eq!(reference_price(Some(u64::MAX), Some(u64::MAX), None), Some(u64::MAX));
    }

    #[test]
    fn accepts_a_normal_order() {
        assert_eq!(policy().check_place("a", 300_000, 5_000, Some(300_050), 0), Ok(()));
    }

    #[test]
    fn rejects_size_value_collar_and_open_orders() {
        let p = policy();
        assert_eq!(
            p.check_place("a", 300_000, 200_000, Some(300_000), 0).unwrap_err().code,
            "MAX_ORDER_SIZE"
        );
        assert_eq!(
            p.check_place("b", 6_000_000, 100_000, Some(6_000_000), 0)
                .unwrap_err()
                .code,
            "MAX_ORDER_VALUE"
        );
        assert_eq!(
            p.check_place("c", 400_000, 1_000, Some(300_000), 0).unwrap_err().code,
            "PRICE_COLLAR"
        );
        assert_eq!(
            p.check_place("d", 300_000, 1_000, Some(300_000), 20).unwrap_err().code,
            "MAX_OPEN_ORDERS"
        );
        assert_eq!(p.check_place("e", 300_000, 1_000, None, 0), Ok(())); // no reference: collar not applicable
    }

    #[test]
    fn collar_reference_falls_back_from_mid_to_last_trade_to_one_side() {
        assert_eq!(reference_price(Some(299_900), Some(300_100), Some(1)), Some(300_000));
        assert_eq!(reference_price(Some(299_900), None, Some(300_500)), Some(300_500));
        assert_eq!(reference_price(None, Some(300_100), None), Some(300_100));
        assert_eq!(reference_price(Some(299_900), None, None), Some(299_900));
        assert_eq!(reference_price(None, None, None), None);
    }

    #[test]
    fn release_gives_back_session_cap_room() {
        let p = Policy::new(PolicyConfig {
            session_notional_cap_micro: 10_000 * 1_000_000,
            actions_per_minute: 100,
            ..PolicyConfig::default()
        });
        assert_eq!(p.check_place("a", 300_000, 20_000, None, 0), Ok(())); // 6,000 USDC
        assert_eq!(
            p.check_place("a", 300_000, 20_000, None, 0).unwrap_err().code,
            "SESSION_CAP"
        );
        p.release("a", 300_000, 20_000);
        assert_eq!(p.check_place("a", 300_000, 20_000, None, 0), Ok(()));
        p.release("nobody", 1, 1); // unknown account: no-op
    }

    #[test]
    fn rate_limits_per_account_and_halts() {
        let p = policy();
        let t0 = Instant::now();
        for _ in 0..3 {
            assert_eq!(p.check_place_at("a", 300_000, 100, None, 0, t0), Ok(()));
        }
        assert_eq!(
            p.check_place_at("a", 300_000, 100, None, 0, t0).unwrap_err().code,
            "RATE_LIMIT"
        );
        assert_eq!(p.check_place_at("b", 300_000, 100, None, 0, t0), Ok(())); // other account unaffected
        assert_eq!(
            p.check_place_at("a", 300_000, 100, None, 0, t0 + Duration::from_secs(61)),
            Ok(())
        );
        let halted = Policy::new(PolicyConfig {
            halted: true,
            ..PolicyConfig::default()
        });
        assert_eq!(halted.check_cancel("a").unwrap_err().code, "HALTED");
    }

    #[test]
    fn session_cap_accumulates() {
        let p = Policy::new(PolicyConfig {
            session_notional_cap_micro: 10_000 * 1_000_000,
            actions_per_minute: 100,
            ..PolicyConfig::default()
        });
        assert_eq!(p.check_place("a", 300_000, 20_000, None, 0), Ok(())); // 6,000 USDC
        assert_eq!(
            p.check_place("a", 300_000, 20_000, None, 0).unwrap_err().code,
            "SESSION_CAP"
        );
    }
}
