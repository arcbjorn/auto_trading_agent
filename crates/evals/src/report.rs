//! Markdown report: per suite pass rate with a Wilson 95% interval, tool calls, latency and tokens.

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

pub fn render(rows: &[Row], errors: usize, agent: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Evaluation report ({agent} agent)\n\n"));
    out.push_str(&format!(
        "{} graded runs, {} runs with infrastructure errors (excluded from pass rates).\n\n",
        rows.len(),
        errors
    ));
    out.push_str("| suite | cases | runs | pass rate | 95% interval | attacks blocked | tool calls (mean) | turn p50/p95 ms | model p50/p95 ms | tokens in/out (mean) |\n|---|---|---|---|---|---|---|---|---|---|\n");
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
            "| {suite} | {cases} | {n} | {:.1}% | {:.1}% – {:.1}% | {blocked} | {:.2} | {}/{} | {}/{} | {:.0}/{:.0} |\n",
            100.0 * pass as f64 / n.max(1) as f64,
            100.0 * lo,
            100.0 * hi,
            mean(&|r| r.tool_calls as f64),
            percentile(&mut turn, 0.5),
            percentile(&mut turn, 0.95),
            percentile(&mut model, 0.5),
            percentile(&mut model, 0.95),
            mean(&|r| r.input_tokens as f64),
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
    if in_tok + out_tok > 0 {
        let cost = in_tok as f64 * 5.0 / 1e6 + out_tok as f64 * 25.0 / 1e6;
        out.push_str(&format!("\nTokens: {in_tok} in, {out_tok} out. At claude-opus-5 list prices (5 / 25 USD per million) this run cost about {cost:.2} USD.\n"));
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
