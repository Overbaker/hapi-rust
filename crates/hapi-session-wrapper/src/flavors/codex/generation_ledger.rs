use super::value::{field, text};
use serde_json::{json, Value};
use std::collections::HashMap;
#[derive(Default)]
pub struct GenerationLedger {
    turn: Option<String>,
    start: i64,
    baseline: Option<i64>,
    total: Option<i64>,
    windows: HashMap<String, (i64, Option<i64>)>,
    waits: HashMap<u64, (i64, Option<i64>)>,
    next: u64,
}
impl GenerationLedger {
    pub fn start(&mut self, id: &str, at: i64) {
        if id.is_empty() || self.turn.as_deref() == Some(id) {
            return;
        }
        self.reset();
        self.turn = Some(id.into());
        self.start = at;
        self.baseline = self.total;
    }
    pub fn reset(&mut self) {
        self.turn = None;
        self.baseline = None;
        self.windows.clear();
        self.waits.clear();
    }
    pub fn usage(&mut self, n: i64) {
        if (0..=9_007_199_254_740_991).contains(&n) {
            self.total = Some(n);
        }
    }
    pub fn begin_tool(&mut self, id: &str, at: i64) {
        self.windows.entry(id.into()).or_insert((at, None));
    }
    pub fn end_tool(&mut self, id: &str, at: i64, duration: Option<i64>) {
        if let Some(d) = duration.filter(|d| *d > 0) {
            self.windows.insert(id.into(), (at - d, Some(at)));
        } else {
            self.windows
                .entry(id.into())
                .or_insert((at, None))
                .1
                .get_or_insert(at);
        }
    }
    pub fn begin_wait(&mut self, at: i64) -> u64 {
        self.next += 1;
        self.waits.insert(self.next, (at, None));
        self.next
    }
    pub fn end_wait(&mut self, id: u64, at: i64) {
        if let Some(w) = self.waits.get_mut(&id) {
            w.1.get_or_insert(at);
        }
    }
    pub fn observe(&mut self, e: &Value, at: i64) {
        let kind = text(e, "type").unwrap_or("");
        if let Some(id) = text(e, "call_id") {
            match kind {
                "exec_command_begin"
                | "mcp_tool_call_begin"
                | "patch_apply_begin"
                | "codex_tool_call_begin" => self.begin_tool(
                    id,
                    e["started_at_ms"].as_i64().filter(|n| *n > 0).unwrap_or(at),
                ),
                "exec_command_end"
                | "mcp_tool_call_end"
                | "patch_apply_end"
                | "codex_tool_call_end" => self.end_tool(
                    id,
                    e["completed_at_ms"]
                        .as_i64()
                        .filter(|n| *n > 0)
                        .unwrap_or(at),
                    e["duration_ms"].as_i64(),
                ),
                _ => {}
            }
        }
        if kind == "token_count" {
            if let Some(n) = field(
                field(&e["info"], "total totalTokenUsage total_token_usage"),
                "outputTokens output_tokens",
            )
            .as_i64()
            {
                self.usage(n);
            }
        }
    }
    pub fn settle(&mut self, id: &str, at: i64, completed: bool) -> Option<Value> {
        if self.turn.as_deref() != Some(id) {
            return None;
        }
        let mut windows: Vec<_> = self
            .windows
            .values()
            .chain(self.waits.values())
            .copied()
            .collect();
        windows.sort_by_key(|v| v.0);
        let mut until = self.start;
        let mut ms = at - self.start;
        for (start, end) in windows {
            let from = start.max(until);
            let end = end.unwrap_or(at).min(at);
            if end > from {
                ms -= end - from;
                until = end;
            }
        }
        let baseline = self.baseline;
        self.reset();
        let tokens = self.total? - baseline?;
        (completed && tokens > 0 && ms > 0)
            .then(|| json!({"generationMs":ms,"generationOutputTokens":tokens}))
    }
}
