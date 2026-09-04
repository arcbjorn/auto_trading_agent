//! Counters and one histogram for the service, rendered in the Prometheus text format at
//! `GET /metrics`: turns by outcome, tool calls by tool and outcome, confirmations, gate
//! rejections, unsupported figures, model latency, live sessions.

use crate::agent::TurnResult;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

const LATENCY_BUCKETS_MS: [u64; 7] = [500, 1_000, 2_000, 5_000, 10_000, 20_000, 60_000];

#[derive(Debug)]
pub struct Metrics {
    started: Instant,
    turns: Mutex<BTreeMap<&'static str, u64>>,
    tool_calls: Mutex<BTreeMap<(String, &'static str), u64>>,
    flags: Mutex<BTreeMap<String, u64>>,
    /// Model latency per turn: bucket counts (cumulative on render), sum and count.
    latency: Mutex<([u64; 8], u64, u64)>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            turns: Mutex::new(BTreeMap::new()),
            tool_calls: Mutex::new(BTreeMap::new()),
            flags: Mutex::new(BTreeMap::new()),
            latency: Mutex::new(([0; 8], 0, 0)),
        }
    }
}

impl Metrics {
    /// A turn that did not run: rate limited, session full, bad request, model failure.
    pub fn turn_refused(&self, outcome: &'static str) {
        *self.turns.lock().expect("metrics").entry(outcome).or_default() += 1;
    }

    pub fn turn(&self, t: &TurnResult) {
        *self.turns.lock().expect("metrics").entry("ok").or_default() += 1;
        {
            let mut calls = self.tool_calls.lock().expect("metrics");
            for c in &t.tool_calls {
                let outcome = if c.intercepted {
                    "held"
                } else if c.is_error {
                    "error"
                } else {
                    "ok"
                };
                *calls.entry((c.name.clone(), outcome)).or_default() += 1;
            }
        }
        {
            // Flags are counted by their family: "confirmation_requested:no_intent:x" -> "confirmation_requested".
            let mut flags = self.flags.lock().expect("metrics");
            for f in &t.flags {
                let family = f.split(':').next().unwrap_or(f).to_string();
                *flags.entry(family).or_default() += 1;
            }
        }
        let mut l = self.latency.lock().expect("metrics");
        let ms = t.model_latency_ms;
        let idx = LATENCY_BUCKETS_MS
            .iter()
            .position(|b| ms <= *b)
            .unwrap_or(LATENCY_BUCKETS_MS.len());
        l.0[idx] += 1;
        l.1 += ms;
        l.2 += 1;
    }

    pub fn render(&self, sessions_active: usize) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "# TYPE agent_uptime_seconds gauge\nagent_uptime_seconds {}\n# TYPE agent_sessions_active gauge\nagent_sessions_active {sessions_active}\n",
            self.started.elapsed().as_secs()
        ));
        out.push_str("# TYPE agent_turns_total counter\n");
        for (outcome, n) in self.turns.lock().expect("metrics").iter() {
            out.push_str(&format!("agent_turns_total{{outcome=\"{outcome}\"}} {n}\n"));
        }
        out.push_str("# TYPE agent_tool_calls_total counter\n");
        for ((tool, outcome), n) in self.tool_calls.lock().expect("metrics").iter() {
            out.push_str(&format!(
                "agent_tool_calls_total{{tool=\"{tool}\",outcome=\"{outcome}\"}} {n}\n"
            ));
        }
        out.push_str("# TYPE agent_flags_total counter\n");
        for (flag, n) in self.flags.lock().expect("metrics").iter() {
            out.push_str(&format!("agent_flags_total{{flag=\"{flag}\"}} {n}\n"));
        }
        let (buckets, sum, count) = *self.latency.lock().expect("metrics");
        out.push_str("# TYPE agent_model_latency_seconds histogram\n");
        let mut cumulative = 0;
        for (i, bound) in LATENCY_BUCKETS_MS.iter().enumerate() {
            cumulative += buckets[i];
            out.push_str(&format!(
                "agent_model_latency_seconds_bucket{{le=\"{}\"}} {cumulative}\n",
                *bound as f64 / 1000.0
            ));
        }
        out.push_str(&format!("agent_model_latency_seconds_bucket{{le=\"+Inf\"}} {count}\n"));
        out.push_str(&format!("agent_model_latency_seconds_sum {}\n", sum as f64 / 1000.0));
        out.push_str(&format!("agent_model_latency_seconds_count {count}\n"));
        out
    }
}
