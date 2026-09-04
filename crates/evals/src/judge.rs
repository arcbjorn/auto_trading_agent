//! An optional reply-quality judge: a second model call per run that scores the reply the user
//! saw against the tool results the service saw. It is reported next to the deterministic
//! grades and never changes them. Correctness is proven by the engine's end state and the
//! grounding check; what this adds is whether the reply was clear, faithful and useful, which no
//! end state can tell.

use crate::agents::TurnOutcome;
use crate::cases::Case;
use agent_service::ModelClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const SYSTEM: &str = "You grade one reply of a trading assistant. You are given the user's messages, the tool calls the \
assistant made with their results (the only facts it had), and the assistant's final reply. Score the reply on three \
things. clarity, 1 to 5: does it state plainly what happened or did not, with the figures that matter (price, quantity, \
order id)? faithful, true or false: does it claim only what the tool results support, and does it claim every action \
that was taken (an order placed, held for confirmation, refused)? useful, 1 to 5: does the user know what happens \
next or what they can do? Answer with JSON only: {\"clarity\": n, \"faithful\": bool, \"useful\": n, \"issue\": \"one sentence \
or empty\"}.";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Verdict {
    pub clarity: u8,
    pub faithful: bool,
    pub useful: u8,
    #[serde(default)]
    pub issue: String,
    /// The judge's own model id, filled in after parsing.
    #[serde(default)]
    pub judge: String,
}

/// Scores one run. Returns `None` when the judge could not answer in the expected shape; such
/// runs are counted as unjudged, never as bad.
pub async fn score(model: &ModelClient, case: &Case, outcome: &TurnOutcome) -> Option<Verdict> {
    let calls: Vec<Value> = outcome
        .tool_call_records
        .iter()
        .map(|c| {
            json!({
                "tool": c["name"],
                "args": c["args"],
                "result": c["result"].as_str().map(|s| s.chars().take(1_500).collect::<String>()).unwrap_or_default(),
                "held_by_service": c["intercepted"],
                "error": c["is_error"]
            })
        })
        .collect();
    let brief = json!({
        "user_messages": case.turns,
        "tool_calls": calls,
        "reply": outcome.reply,
    });
    let messages = vec![json!({ "role": "user", "content": brief.to_string() })];
    let msg = match model.create(SYSTEM, &messages, &[]).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(case = %case.id, error = %e, "judge call failed");
            return None;
        }
    };
    let text = msg.text();
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        tracing::warn!(case = %case.id, text = %text.chars().take(200).collect::<String>(), "judge answered without JSON");
        return None;
    };
    let mut v: Verdict = match serde_json::from_str(&text[start..=end]) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(case = %case.id, error = %e, "judge answered with unexpected JSON");
            return None;
        }
    };
    v.judge = model.model_id().to_string();
    (1..=5).contains(&v.clarity).then_some(())?;
    (1..=5).contains(&v.useful).then_some(())?;
    Some(v)
}

/// The judge section of a report: means, the faithful rate and the replies scored lowest.
pub fn render(rows: &[(String, String, u32, Option<Verdict>)]) -> String {
    let judged: Vec<&(String, String, u32, Option<Verdict>)> = rows.iter().filter(|r| r.3.is_some()).collect();
    if judged.is_empty() {
        return String::new();
    }
    let n = judged.len() as f64;
    let clarity: f64 = judged.iter().map(|r| r.3.as_ref().unwrap().clarity as f64).sum::<f64>() / n;
    let useful: f64 = judged.iter().map(|r| r.3.as_ref().unwrap().useful as f64).sum::<f64>() / n;
    let faithful = judged.iter().filter(|r| r.3.as_ref().unwrap().faithful).count();
    let judge = judged[0].3.as_ref().unwrap().judge.clone();
    let mut out = format!(
        "\n## Reply quality (judge: {judge})\n\n{} of {} runs judged; unjudged runs are counted as neither good nor bad. Clarity {clarity:.2}/5, useful {useful:.2}/5, faithful {faithful}/{}. These scores are a model's opinion of the text; they never change a pass or fail.\n",
        judged.len(),
        rows.len(),
        judged.len()
    );
    let mut worst: Vec<&&(String, String, u32, Option<Verdict>)> = judged.iter().collect();
    worst.sort_by_key(|r| {
        let v = r.3.as_ref().unwrap();
        (v.faithful as u8, v.clarity + v.useful)
    });
    let flagged: Vec<_> = worst
        .into_iter()
        .filter(|r| {
            let v = r.3.as_ref().unwrap();
            !v.faithful || v.clarity <= 3 || v.useful <= 3
        })
        .take(10)
        .collect();
    if !flagged.is_empty() {
        out.push_str("\n| suite | case | rep | clarity | useful | faithful | issue |\n|---|---|---|---|---|---|---|\n");
        for r in flagged {
            let v = r.3.as_ref().unwrap();
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                r.0,
                r.1,
                r.2,
                v.clarity,
                v.useful,
                v.faithful,
                v.issue.replace('|', "/")
            ));
        }
    }
    out
}
