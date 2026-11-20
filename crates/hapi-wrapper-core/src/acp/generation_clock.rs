//! Event-fed generation clock for one ACP turn (TS
//! `cli/src/agent/generationClock.ts`). Times are epoch milliseconds.
use super::types::AgentMessage;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// A span not spent generating: a tool run or a wait for the user.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IdleWindow {
    pub start: u64,
    pub end: Option<u64>,
}

/// The round minus the idle spans, clipped to the round and merged.
pub fn generation_time_ms(
    sent_at: Option<u64>,
    answered_at: Option<u64>,
    windows: &[IdleWindow],
) -> Option<f64> {
    let (sent, answered) = (sent_at?, answered_at?);
    if answered <= sent {
        return None;
    }
    let mut clipped: Vec<(u64, u64)> = windows
        .iter()
        .map(|w| (w.start.max(sent), w.end.unwrap_or(answered).min(answered)))
        .filter(|(s, e)| e > s)
        .collect();
    clipped.sort_by_key(|w| w.0);
    let (mut excluded, mut covered) = (0, sent);
    for (start, end) in clipped {
        let from = start.max(covered);
        if end > from {
            excluded += end - from;
        }
        covered = covered.max(end);
    }
    Some((answered - sent - excluded) as f64)
}

#[derive(Default)]
struct State {
    windows: Vec<IdleWindow>,
    tools: HashMap<String, usize>,
    saw_output: bool,
}

type Now = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Clone)]
pub struct GenerationClock {
    state: Arc<Mutex<State>>,
    now: Now,
}

impl Default for GenerationClock {
    fn default() -> Self {
        Self::with_clock(Arc::new(crate::session::metadata::now))
    }
}

impl GenerationClock {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_clock(now: Now) -> Self {
        Self {
            state: Arc::default(),
            now,
        }
    }
    pub fn observe(&self, message: &AgentMessage) {
        if matches!(
            message,
            AgentMessage::Usage(_) | AgentMessage::TurnComplete { .. }
        ) {
            return;
        }
        let mut s = self.state.lock().unwrap();
        s.saw_output = true;
        let (Some(id), Some(status)) = (message.tool_id(), message.status()) else {
            return;
        };
        let now = (self.now)();
        let index = match s.tools.get(id) {
            Some(&i) => i,
            None => {
                s.windows.push(IdleWindow {
                    start: now,
                    end: None,
                });
                let i = s.windows.len() - 1;
                s.tools.insert(id.to_owned(), i);
                i
            }
        };
        let window = &mut s.windows[index];
        if matches!(status, "completed" | "failed") && window.end.is_none() {
            window.end = Some(now);
        }
    }
    /// Opens a wait-for-the-user span; the callback closes it.
    pub fn begin_user_wait(&self) -> Box<dyn FnOnce() + Send> {
        let index = {
            let mut s = self.state.lock().unwrap();
            s.windows.push(IdleWindow {
                start: (self.now)(),
                end: None,
            });
            s.windows.len() - 1
        };
        let this = self.clone();
        Box::new(move || {
            let now = (this.now)();
            let mut s = this.state.lock().unwrap();
            let window = &mut s.windows[index];
            if window.end.is_none() {
                window.end = Some(now);
            }
        })
    }
    /// `None` when the turn produced no output the clock saw.
    pub fn generation_ms(&self, sent_at: Option<u64>, answered_at: Option<u64>) -> Option<f64> {
        let s = self.state.lock().unwrap();
        if !s.saw_output {
            return None;
        }
        generation_time_ms(sent_at, answered_at, &s.windows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::types::ToolCall;
    use serde_json::Value;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn w(start: u64, end: Option<u64>) -> IdleWindow {
        IdleWindow { start, end }
    }

    #[test]
    fn merges_and_clips_windows() {
        assert_eq!(generation_time_ms(None, Some(5), &[]), None);
        assert_eq!(generation_time_ms(Some(5), Some(5), &[]), None);
        assert_eq!(generation_time_ms(Some(0), Some(100), &[]), Some(100.0));
        let windows = [
            w(10, Some(30)),
            w(20, Some(40)),
            w(90, None),
            w(200, Some(300)),
        ];
        assert_eq!(
            generation_time_ms(Some(0), Some(100), &windows),
            Some(100.0 - 30.0 - 10.0)
        );
    }

    fn call(id: &str, status: &str) -> AgentMessage {
        AgentMessage::ToolCall(ToolCall {
            id: id.into(),
            name: "Bash".into(),
            input: Value::Null,
            status: status.into(),
            title: None,
            kind: None,
            progress: None,
            native_name: None,
            native_args: None,
        })
    }

    #[test]
    fn tool_and_user_waits_are_subtracted() {
        let t = Arc::new(AtomicU64::new(0));
        let tc = t.clone();
        let clock = GenerationClock::with_clock(Arc::new(move || tc.load(Ordering::SeqCst)));
        assert_eq!(clock.generation_ms(Some(0), Some(100)), None);
        t.store(10, Ordering::SeqCst);
        clock.observe(&AgentMessage::text("hi"));
        clock.observe(&call("a", "in_progress"));
        t.store(30, Ordering::SeqCst);
        clock.observe(&AgentMessage::ToolResult {
            id: "a".into(),
            output: Value::Null,
            status: "completed".into(),
        });
        t.store(50, Ordering::SeqCst);
        let end = clock.begin_user_wait();
        t.store(70, Ordering::SeqCst);
        end();
        assert_eq!(clock.generation_ms(Some(0), Some(100)), Some(60.0));
    }
}
