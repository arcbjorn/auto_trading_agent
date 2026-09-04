//! Markdown report: per suite pass rate with a Wilson 95% interval, tool calls, latency, tokens
//! (uncached, served from the prompt cache, and generated) and the cost at list prices.

use crate::harness::Row;
use std::collections::BTreeMap;
use std::path::Path;

/// Wilson score interval for a binomial proportion, 95%.
pub fn wilson(pass: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 0.0);
    }
    let (p, z) = (pass as f64 / n as f64, 1.96_f64);
    let nf = n as f64;
    let d = 1.0 + z * z / nf;
    let c = (p + z * z / (2.0 * nf)) / d;
    let h = z * ((p * (1.0 - p) + z * z / (4.0 * nf)) / nf).sqrt() / d;
    ((c - h).max(0.0), (c + h).min(1.0))
}

fn percentile(values: &mut [u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[((values.len() - 1) as f64 * p).round() as usize]
}

/// List prices per million tokens by model id prefix, as of September 2026:
/// (uncached input, cache read, cache write, output). Claude cache reads are 0.1x and writes 1.25x
/// the input price; DeepSeek prices are the peak-hour rates (off-peak is half) and its cache
/// writes cost nothing beyond the uncached input.
pub fn list_prices(model: &str) -> Option<(f64, f64, f64, f64)> {
    const PRICES: [(&str, f64, f64, f64, f64); 10] = [
        ("claude-fable", 10.0, 1.0, 12.5, 50.0),
        ("claude-mythos", 10.0, 1.0, 12.5, 50.0),
        ("claude-opus-5", 5.0, 0.5, 6.25, 25.0),
        ("claude-opus-4", 5.0, 0.5, 6.25, 25.0),
        ("claude-sonnet-5", 2.0, 0.2, 2.5, 10.0),
        ("claude-sonnet-4-6", 3.0, 0.3, 3.75, 15.0),
        ("claude-sonnet-4", 3.0, 0.3, 3.75, 15.0),
        ("claude-haiku-4", 1.0, 0.1, 1.25, 5.0),
        ("deepseek-v4-pro", 1.32, 0.044, 1.32, 3.96),
        ("deepseek-v4-flash", 0.44, 0.014, 0.44, 1.32),
    ];
    PRICES
        .iter()
        .find(|(prefix, ..)| model.starts_with(prefix))
        .map(|(_, i, r, w, o)| (*i, *r, *w, *o))
}

/// Cost of one run at its model's list prices; `None` for an unknown model.
pub fn estimated_cost_usd(model: &str, input: u64, cache_read: u64, cache_write: u64, output: u64) -> Option<f64> {
    let (i, r, w, o) = list_prices(model)?;
    Some((input as f64 * i + cache_read as f64 * r + cache_write as f64 * w + output as f64 * o) / 1e6)
}

pub fn render(rows: &[Row], errors: usize, agent: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Evaluation report ({agent} agent)\n\n"));
    if let Some(kind) = rows.iter().map(|r| r.perturbation.as_str()).find(|p| !p.is_empty()) {
        out.push_str(&format!(
            "Every turn was perturbed before sending (`--perturb {kind}`): {}.\n\n",
            match kind {
                "casing" => "all upper case, all lower case, or alternating words",
                "noise" => "filler before and after, doubled spaces, a lost full stop",
                "typos" => "two adjacent letters swapped in ordinary words; numbers and intent words untouched",
                _ => "typos, then noise, then casing",
            }
        ));
    }
    out.push_str(&format!(
        "{} graded runs, {} runs with infrastructure errors (excluded from pass rates).\n\n",
        rows.len(),
        errors
    ));
    let without_intent = rows.iter().filter(|r| !r.authorises_write).count();
    let unauthorised = rows.iter().filter(|r| !r.authorises_write && r.mutated).count();
    out.push_str(&format!(
        "Unauthorised mutations: {unauthorised} in {without_intent} runs whose request asked for no order or cancel.\n\n"
    ));
    out.push_str("| suite | cases | runs | pass rate | 95% interval | attacks blocked | tool calls (mean) | turn p50/p95 ms | model p50/p95 ms | tokens in/cached/out (mean) |\n|---|---|---|---|---|---|---|---|---|---|\n");
    let mut by_suite: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for r in rows {
        by_suite.entry(r.suite.as_str()).or_default().push(r);
    }
    for (suite, rs) in &by_suite {
        let n = rs.len();
        let pass = rs.iter().filter(|r| r.pass).count();
        let (lo, hi) = wilson(pass, n);
        let cases = rs
            .iter()
            .map(|r| r.case.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let attacks: Vec<&&Row> = rs.iter().filter(|r| r.attack).collect();
        let blocked = if attacks.is_empty() {
            "n/a".to_string()
        } else {
            format!("{}/{}", attacks.iter().filter(|r| r.pass).count(), attacks.len())
        };
        let mean = |f: &dyn Fn(&Row) -> f64| rs.iter().map(|r| f(r)).sum::<f64>() / n.max(1) as f64;
        let mut turn: Vec<u64> = rs.iter().map(|r| r.latency_ms).collect();
        let mut model: Vec<u64> = rs.iter().map(|r| r.model_latency_ms).collect();
        out.push_str(&format!(
            "| {suite} | {cases} | {n} | {:.1}% | {:.1}% – {:.1}% | {blocked} | {:.2} | {}/{} | {}/{} | {:.0}/{:.0}/{:.0} |\n",
            100.0 * pass as f64 / n.max(1) as f64,
            100.0 * lo,
            100.0 * hi,
            mean(&|r| r.tool_calls as f64),
            percentile(&mut turn, 0.5),
            percentile(&mut turn, 0.95),
            percentile(&mut model, 0.5),
            percentile(&mut model, 0.95),
            mean(&|r| r.input_tokens as f64),
            mean(&|r| r.cache_read_tokens as f64),
            mean(&|r| r.output_tokens as f64),
        ));
    }
    let mut rtt: Vec<u64> = rows.iter().map(|r| r.grpc_rtt_us_p50).filter(|v| *v > 0).collect();
    if !rtt.is_empty() {
        out.push_str(&format!(
            "\nEngine gRPC round trip while seeding: p50 {} us, p95 {} us (in-process server, loopback).\n",
            percentile(&mut rtt, 0.5),
            percentile(&mut rtt, 0.95)
        ));
    }
    let in_tok: u64 = rows.iter().map(|r| r.input_tokens).sum();
    let out_tok: u64 = rows.iter().map(|r| r.output_tokens).sum();
    let cached: u64 = rows.iter().map(|r| r.cache_read_tokens).sum();
    let written: u64 = rows.iter().map(|r| r.cache_creation_tokens).sum();
    if in_tok + out_tok + cached + written > 0 {
        let prompt = in_tok + cached + written;
        let models: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.model.as_str()).collect();
        let cost: Option<f64> = rows
            .iter()
            .map(|r| {
                estimated_cost_usd(
                    &r.model,
                    r.input_tokens,
                    r.cache_read_tokens,
                    r.cache_creation_tokens,
                    r.output_tokens,
                )
            })
            .sum();
        out.push_str(&format!(
            "\nTokens: {in_tok} uncached in, {cached} read from cache, {written} written to cache, {out_tok} out; cache hit rate {:.0}% of prompt tokens. ",
            if prompt > 0 { 100.0 * cached as f64 / prompt as f64 } else { 0.0 }
        ));
        match cost {
            Some(cost) => out.push_str(&format!(
                "At list prices for {} (Claude: cache reads 0.1x and writes 1.25x the input price; DeepSeek: peak-hour rates) this run cost about {cost:.2} USD.\n",
                models.iter().copied().collect::<Vec<_>>().join(", ")
            )),
            None => out.push_str("No list price is known for every model in this run, so no cost is shown.\n"),
        }
    }
    let failures: Vec<&Row> = rows.iter().filter(|r| !r.pass).collect();
    if !failures.is_empty() {
        out.push_str("\n## Failures\n\n| suite | case | rep | failed fields | flags | reply | notes |\n|---|---|---|---|---|---|---|\n");
        for r in failures {
            let failed: Vec<&str> = r
                .fields
                .iter()
                .filter(|(_, ok)| !**ok)
                .map(|(k, _)| k.as_str())
                .collect();
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                r.suite,
                r.case,
                r.rep,
                failed.join(", "),
                r.flags.join(", "),
                r.reply.replace('|', "/").chars().take(120).collect::<String>(),
                r.notes.replace('|', "/")
            ));
        }
    }
    let flagged: Vec<&Row> = rows.iter().filter(|r| !r.flags.is_empty()).collect();
    if !flagged.is_empty() {
        out.push_str("\n## Verifier flags\n\n| suite | case | rep | flags |\n|---|---|---|---|\n");
        for r in flagged {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                r.suite,
                r.case,
                r.rep,
                r.flags.join(", ")
            ));
        }
    }
    out
}

pub fn regenerate(out_dir: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(out_dir)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if let Some(agent) = name.strip_prefix("results-").and_then(|n| n.strip_suffix(".jsonl")) {
            let rows: Vec<Row> = std::fs::read_to_string(&path)?
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(serde_json::from_str)
                .collect::<Result<_, _>>()?;
            let errors = std::fs::read_to_string(out_dir.join(format!("errors-{agent}.jsonl")))
                .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
                .unwrap_or(0);
            let md = render(&rows, errors, agent);
            std::fs::write(out_dir.join(format!("report-{agent}.md")), &md)?;
            println!("{md}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_follow_the_model_and_the_cache_discounts() {
        assert_eq!(list_prices("claude-opus-5"), Some((5.0, 0.5, 6.25, 25.0)));
        assert_eq!(list_prices("claude-sonnet-5"), Some((2.0, 0.2, 2.5, 10.0)));
        assert_eq!(list_prices("deepseek-v4-flash"), Some((0.44, 0.014, 0.44, 1.32)));
        assert_eq!(list_prices("gpt-x"), None);
        // 1M of each on V4 Flash at peak: 0.44 + 0.014 + 0.44 + 1.32.
        let d = estimated_cost_usd("deepseek-v4-flash", 1_000_000, 1_000_000, 1_000_000, 1_000_000).unwrap();
        assert!((d - 2.214).abs() < 1e-9, "{d}");
        // 1M uncached in, 1M cached, 1M written, 1M out on Sonnet 5: 2 + 0.2 + 2.5 + 10.
        let c = estimated_cost_usd("claude-sonnet-5", 1_000_000, 1_000_000, 1_000_000, 1_000_000).unwrap();
        assert!((c - 14.7).abs() < 1e-9, "{c}");
        assert_eq!(estimated_cost_usd("oracle", 0, 0, 0, 0), None);
    }
}
