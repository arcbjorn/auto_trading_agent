//! Counters for the MCP server, rendered in the Prometheus text format at `GET /metrics` on the
//! HTTP transport, next to the engine's own counters fetched over gRPC on each scrape. No metrics
//! crate: a few maps behind a mutex and a formatter are all this needs.

use clob_proto::v1 as pb;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug)]
pub struct Metrics {
    started: Instant,
    /// (tool, outcome) -> count, outcome one of ok, rejected, error.
    tool_calls: Mutex<BTreeMap<(String, &'static str), u64>>,
    /// Policy rejection code -> count.
    rejections: Mutex<BTreeMap<String, u64>>,
    /// Requests over HTTP by outcome.
    http: Mutex<BTreeMap<&'static str, u64>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            tool_calls: Mutex::new(BTreeMap::new()),
            rejections: Mutex::new(BTreeMap::new()),
            http: Mutex::new(BTreeMap::new()),
        }
    }
}

impl Metrics {
    pub fn tool_call(&self, tool: &str, outcome: &'static str) {
        *self
            .tool_calls
            .lock()
            .expect("metrics")
            .entry((tool.to_string(), outcome))
            .or_default() += 1;
    }

    pub fn rejection(&self, code: &str) {
        *self
            .rejections
            .lock()
            .expect("metrics")
            .entry(code.to_string())
            .or_default() += 1;
    }

    pub fn http(&self, outcome: &'static str) {
        *self.http.lock().expect("metrics").entry(outcome).or_default() += 1;
    }

    /// The exposition: this server's counters and, when the engine answered, its counters too.
    pub fn render(&self, engine: Option<&pb::EngineStats>) -> String {
        let mut out = String::new();
        out.push_str("# TYPE mcp_uptime_seconds gauge\n");
        out.push_str(&format!("mcp_uptime_seconds {}\n", self.started.elapsed().as_secs()));
        out.push_str("# TYPE mcp_tool_calls_total counter\n");
        for ((tool, outcome), n) in self.tool_calls.lock().expect("metrics").iter() {
            out.push_str(&format!(
                "mcp_tool_calls_total{{tool=\"{tool}\",outcome=\"{outcome}\"}} {n}\n"
            ));
        }
        out.push_str("# TYPE mcp_policy_rejections_total counter\n");
        for (code, n) in self.rejections.lock().expect("metrics").iter() {
            out.push_str(&format!("mcp_policy_rejections_total{{code=\"{code}\"}} {n}\n"));
        }
        out.push_str("# TYPE mcp_http_requests_total counter\n");
        for (outcome, n) in self.http.lock().expect("metrics").iter() {
            out.push_str(&format!("mcp_http_requests_total{{outcome=\"{outcome}\"}} {n}\n"));
        }
        if let Some(e) = engine {
            for (name, kind, value) in [
                ("engine_commands_total", "counter", e.commands),
                ("engine_batches_total", "counter", e.batches),
                ("engine_max_batch", "gauge", e.max_batch),
                ("engine_events_total", "counter", e.events),
                ("engine_orders_retained", "gauge", e.orders_retained),
                ("engine_trades_retained", "gauge", e.trades_retained),
                ("engine_sequence", "gauge", e.sequence),
                ("engine_queue_free", "gauge", e.queue_free as u64),
                ("engine_queue_capacity", "gauge", e.queue_capacity as u64),
            ] {
                out.push_str(&format!("# TYPE {name} {kind}\n{name} {value}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counters_in_the_exposition_format() {
        let m = Metrics::default();
        m.tool_call("place_limit_order", "ok");
        m.tool_call("place_limit_order", "rejected");
        m.rejection("MAX_ORDER_SIZE");
        let text = m.render(Some(&pb::EngineStats {
            commands: 7,
            queue_capacity: 4096,
            ..Default::default()
        }));
        assert!(text.contains("mcp_tool_calls_total{tool=\"place_limit_order\",outcome=\"ok\"} 1"));
        assert!(text.contains("mcp_policy_rejections_total{code=\"MAX_ORDER_SIZE\"} 1"));
        assert!(text.contains("engine_commands_total 7"));
        assert!(text.contains("engine_queue_capacity 4096"));
    }
}
