//! Scenario files. One JSON file per case under `evals/cases/<suite>/`.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub seed_book: SeedBook,
    /// Actions by the account under test before the turn, e.g. an order to cancel later.
    #[serde(default)]
    pub setup: Vec<SetupAction>,
    /// The user's messages, in order. Most cases have one; confirmation flows have two.
    pub turns: Vec<String>,
    pub expect: Expect,
    /// Adversarial case: success means nothing happened.
    #[serde(default)]
    pub attack: bool,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SeedBook {
    /// `[price_usdc, quantity_eth]` pairs, resting under a market-maker account.
    #[serde(default)]
    pub bids: Vec<[String; 2]>,
    #[serde(default)]
    pub asks: Vec<[String; 2]>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetupAction {
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Expect {
    /// Every order of the account after the turns, in any order, with final statuses.
    #[serde(default)]
    pub orders: Vec<ExpectedOrder>,
    /// Number of the account's trades after the turns.
    #[serde(default)]
    pub trades: Option<usize>,
    /// The account's orders must be exactly as they were after setup.
    #[serde(default)]
    pub no_action: bool,
    #[serde(default)]
    pub reply_mentions: Vec<String>,
    #[serde(default)]
    pub reply_asks_question: bool,
    #[serde(default)]
    pub tool_calls_max: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedOrder {
    pub side: String,
    pub price: String,
    pub qty: String,
    pub status: String,
}

pub fn load(dir: &Path, suite: &str) -> anyhow::Result<Vec<(String, Case)>> {
    let mut out = Vec::new();
    let mut suites: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    suites.sort_by_key(|e| e.file_name());
    for entry in suites {
        let name = entry.file_name().to_string_lossy().to_string();
        if suite != "all" && name != suite {
            continue;
        }
        let mut files: Vec<_> = std::fs::read_dir(entry.path())?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        for file in files {
            let text = std::fs::read_to_string(&file)?;
            let case: Case = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
            anyhow::ensure!(!case.turns.is_empty(), "{}: turns must not be empty", file.display());
            out.push((name.clone(), case));
        }
    }
    Ok(out)
}
