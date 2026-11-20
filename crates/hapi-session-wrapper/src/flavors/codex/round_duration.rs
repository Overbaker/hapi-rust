use serde_json::{json, Value};
#[derive(Default)]
pub struct RoundDuration {
    active: Option<(String, i64)>,
}
impl RoundDuration {
    pub fn start(&mut self, turn: &str, at: i64) {
        if !turn.is_empty() && self.active.as_ref().is_none_or(|(id, _)| id != turn) {
            self.active = Some((turn.into(), at));
        }
    }
    pub fn finish(&mut self, turn: &str, at: i64, completed: bool) -> Option<Value> {
        let (id, start) = self.active.as_ref()?;
        if id != turn {
            return None;
        }
        let duration = at - *start;
        self.active = None;
        (completed && duration >= 0).then(|| json!({"type":"turn_duration","flavor":"codex","turn_id":turn,"durationMs":duration}))
    }
}
