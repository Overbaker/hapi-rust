use serde_json::Value;
use std::time::{Duration, Instant};
#[derive(Default)]
pub struct Maintenance {
    compact: bool,
    retry: Option<String>,
    summary: bool,
    next_compact: Option<Instant>,
}
impl Maintenance {
    pub fn busy(&self) -> bool {
        self.compact
            || self.summary
            || self.retry.is_some()
            || self.next_compact.is_some_and(|t| t > Instant::now())
    }
    pub fn event(&mut self, v: &Value) -> Option<String> {
        let attempt = &v["attempt"];
        let max = &v["maxAttempts"];
        let error = v["errorMessage"].as_str().unwrap_or("");
        match v["type"].as_str()? {
            "compaction_start" | "auto_compaction_start" if !self.compact => {
                self.compact = true;
                self.next_compact = None;
                Some("📦 Compaction started".into())
            }
            "compaction_end" | "auto_compaction_end" if self.compact => {
                self.compact = false;
                if v["willRetry"] == true {
                    self.next_compact = Some(Instant::now() + Duration::from_secs(1))
                }
                Some(if v["aborted"] == true {
                    "📦 Compaction canceled".into()
                } else if !error.is_empty() {
                    format!("📦 Compaction failed: {error}")
                } else if v["willRetry"] == true {
                    "📦 Compaction will retry".into()
                } else {
                    "📦 Compaction completed".into()
                })
            }
            "auto_retry_start" => {
                let key = format!("{attempt}:{error}");
                if self.retry.as_ref() == Some(&key) {
                    return None;
                }
                self.retry = Some(key);
                Some(format!(
                    "↻ Retrying after error (attempt {attempt}/{max}): {error}"
                ))
            }
            "auto_retry_end" if self.retry.take().is_some() => Some(if v["success"] == true {
                format!("↻ Retry succeeded (attempt {attempt})")
            } else {
                let detail = v["finalError"]
                    .as_str()
                    .map(|s| format!(": {s}"))
                    .unwrap_or_default();
                format!("↻ Retry failed (attempt {attempt}){detail}")
            }),
            "summarization_retry_scheduled" if !self.summary => {
                self.summary = true;
                Some(format!(
                    "📝 Summary retry scheduled (attempt {attempt}/{max}): {error}"
                ))
            }
            "summarization_retry_attempt_start" if self.summary => Some(format!(
                "📝 Summary retry started ({})",
                v["source"].as_str().unwrap_or("")
            )),
            "summarization_retry_finished" if self.summary => {
                self.summary = false;
                Some("📝 Summary retry completed".into())
            }
            "agent_start" | "turn_start" => {
                self.next_compact = None;
                None
            }
            _ => None,
        }
    }
}
