//! Exact conversion between the human decimals the model sees and the integers the engine uses.
//!
//! Prices: 2 decimals (tick 0.01 USDC). Quantities: 4 decimals (lot 0.0001 ETH). Parsing never
//! goes through a float; formatting never drops or invents digits.

pub const PRICE_DECIMALS: u32 = 2;
pub const QTY_DECIMALS: u32 = 4;
/// One tick times one lot is 0.01 * 0.0001 USDC = 1 micro-USDC.
pub const NOTIONAL_DECIMALS: u32 = 6;

fn pow10(d: u32) -> u64 {
    10u64.pow(d)
}

/// Parses a positive decimal with at most `decimals` fractional digits into fixed-point units.
/// Accepts thousands separators ("3,000.50"), a leading "+", and surrounding whitespace.
pub fn parse_fixed(input: &str, decimals: u32, what: &str, step: &str) -> Result<u64, String> {
    let cleaned: String = input.trim().chars().filter(|c| !matches!(c, ',' | '_' | ' ')).collect();
    let cleaned = cleaned.strip_prefix('+').unwrap_or(&cleaned);
    if cleaned.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if cleaned.starts_with('-') {
        return Err(format!("{what} must be positive, got {input:?}"));
    }
    let (int_part, frac_part) = match cleaned.split_once('.') {
        Some((i, f)) => (i, f),
        None => (cleaned, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(format!("{what} {input:?} is not a number"));
    }
    if !int_part.chars().all(|c| c.is_ascii_digit()) || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("{what} {input:?} is not a number"));
    }
    if frac_part.len() as u32 > decimals {
        return Err(format!(
            "{what} {input:?} has more than {decimals} decimals; it must be a multiple of {step}. Round it and retry"
        ));
    }
    let scale = pow10(decimals);
    let int_value: u64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| format!("{what} {input:?} is too large"))?
    };
    let mut frac_value: u64 = if frac_part.is_empty() {
        0
    } else {
        frac_part
            .parse()
            .map_err(|_| format!("{what} {input:?} is not a number"))?
    };
    frac_value *= pow10(decimals - frac_part.len() as u32);
    let value = int_value
        .checked_mul(scale)
        .and_then(|v| v.checked_add(frac_value))
        .ok_or_else(|| format!("{what} {input:?} is too large"))?;
    if value == 0 {
        return Err(format!("{what} must be positive, got {input:?}"));
    }
    Ok(value)
}

/// Formats fixed-point units with exactly `decimals` fractional digits.
pub fn format_fixed(value: u64, decimals: u32) -> String {
    let scale = pow10(decimals);
    format!("{}.{:0width$}", value / scale, value % scale, width = decimals as usize)
}

pub fn parse_price(s: &str) -> Result<u64, String> {
    parse_fixed(s, PRICE_DECIMALS, "price_usdc", "0.01")
}

pub fn parse_qty(s: &str) -> Result<u64, String> {
    parse_fixed(s, QTY_DECIMALS, "quantity_eth", "0.0001")
}

pub fn usdc(ticks: u64) -> String {
    format_fixed(ticks, PRICE_DECIMALS)
}

pub fn eth(lots: u64) -> String {
    format_fixed(lots, QTY_DECIMALS)
}

/// Notional in micro-USDC (ticks * lots) formatted as USDC with 2 decimals, rounded half up.
pub fn usdc_from_micro(micro: u128) -> String {
    let cents = (micro + 5_000) / 10_000;
    format_fixed(cents.min(u64::MAX as u128) as u64, PRICE_DECIMALS)
}

/// A signed amount in micro-USDC as USDC with 2 decimals, rounded half up away from zero.
pub fn signed_usdc_from_micro(micro: i128) -> String {
    let text = usdc_from_micro(micro.unsigned_abs());
    if micro < 0 && text != "0.00" {
        format!("-{text}")
    } else {
        text
    }
}

/// Midpoint of two prices in ticks, shown with 3 decimals only when the sum is odd.
pub fn mid(bid: u64, ask: u64) -> String {
    let sum = bid + ask;
    if sum % 2 == 0 {
        usdc(sum / 2)
    } else {
        format_fixed(sum * 5, PRICE_DECIMALS + 1)
    }
}

/// Average price in ticks of a set of fills, rounded half up.
pub fn average_price(notional_micro: u128, lots: u64) -> Option<u64> {
    if lots == 0 {
        return None;
    }
    Some(((notional_micro + lots as u128 / 2) / lots as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prices_and_quantities_exactly() {
        assert_eq!(parse_price("3000.50"), Ok(300_050));
        assert_eq!(parse_price("3000"), Ok(300_000));
        assert_eq!(parse_price("3,000.5"), Ok(300_050));
        assert_eq!(parse_price(" +3000.5 "), Ok(300_050));
        assert_eq!(parse_price(".5"), Ok(50));
        assert_eq!(parse_qty("0.25"), Ok(2_500));
        assert_eq!(parse_qty("1"), Ok(10_000));
        assert_eq!(parse_qty("0.0001"), Ok(1));
    }

    #[test]
    fn rejects_bad_inputs_with_actionable_messages() {
        assert!(parse_price("3000.123").unwrap_err().contains("multiple of 0.01"));
        assert!(parse_qty("0.00001").unwrap_err().contains("multiple of 0.0001"));
        assert!(parse_price("-5").unwrap_err().contains("positive"));
        assert!(parse_price("0").unwrap_err().contains("positive"));
        assert!(parse_price("abc").unwrap_err().contains("not a number"));
        assert!(parse_price("").unwrap_err().contains("empty"));
        assert!(parse_price("99999999999999999999").unwrap_err().contains("too large"));
        assert!(parse_price("3000..5").unwrap_err().contains("not a number"));
    }

    #[test]
    fn formats_with_fixed_decimals() {
        assert_eq!(usdc(300_050), "3000.50");
        assert_eq!(usdc(300_200), "3002.00");
        assert_eq!(usdc(5), "0.05");
        assert_eq!(eth(2_500), "0.2500");
        assert_eq!(eth(1), "0.0001");
        assert_eq!(mid(300_050, 300_100), "3000.75");
        assert_eq!(mid(300_050, 300_051), "3000.505");
        assert_eq!(usdc_from_micro(3_601_900_000), "3601.90");
        assert_eq!(signed_usdc_from_micro(-600_000), "-0.60");
        assert_eq!(signed_usdc_from_micro(1_250_000), "1.25");
        assert_eq!(signed_usdc_from_micro(-1), "0.00");
        assert_eq!(average_price(3_601_900_000, 12_000), Some(300_158)); // 3001.58, rounded from 3001.5833
        assert_eq!(average_price(0, 0), None);
    }
}
