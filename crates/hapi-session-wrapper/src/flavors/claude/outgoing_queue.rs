use serde_json::Value;
use std::{collections::VecDeque, time::Duration};
use tokio::{
    sync::{mpsc, oneshot},
    time::{sleep_until, Instant},
};
struct Item {
    row: Value,
    ids: Vec<String>,
    deadline: Instant,
}
enum Action {
    Add(Value),
    Release(String),
    Flush(oneshot::Sender<()>),
}
#[derive(Clone)]
pub struct OutgoingQueue(mpsc::UnboundedSender<Action>);
impl OutgoingQueue {
    pub fn new(mut send: impl FnMut(Value) + Send + 'static) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut queue = VecDeque::<Item>::new();
            loop {
                while queue.front().is_some_and(|i| i.deadline <= Instant::now()) {
                    let item = queue.pop_front().unwrap();
                    if visible(&item.row) {
                        send(item.row);
                    }
                }
                let action = if let Some(item) = queue.front() {
                    tokio::select! {_=sleep_until(item.deadline)=>continue,a=rx.recv()=>a}
                } else {
                    rx.recv().await
                };
                match action {
                    None => break,
                    Some(Action::Flush(done)) => {
                        for item in queue.drain(..) {
                            if visible(&item.row) {
                                send(item.row);
                            }
                        }
                        let _ = done.send(());
                    }
                    Some(Action::Release(id)) => release(&mut queue, &id),
                    Some(Action::Add(row)) => {
                        let mut ids = Vec::new();
                        for b in row["message"]["content"].as_array().into_iter().flatten() {
                            if row["type"] == "user" && b["type"] == "tool_result" {
                                if let Some(id) = b["tool_use_id"].as_str() {
                                    release(&mut queue, id);
                                }
                            }
                            if row["type"] == "assistant"
                                && row["isSidechain"] != true
                                && b["type"] == "tool_use"
                            {
                                if let Some(id) = b["id"].as_str().filter(|s| !s.is_empty()) {
                                    ids.push(id.into());
                                }
                            }
                        }
                        let deadline = Instant::now()
                            + Duration::from_millis(if ids.is_empty() { 0 } else { 250 });
                        queue.push_back(Item { row, ids, deadline });
                    }
                }
            }
        });
        Self(tx)
    }
    pub fn enqueue(&self, row: Value) {
        let _ = self.0.send(Action::Add(row));
    }
    pub fn release_tool_call(&self, id: &str) {
        let _ = self.0.send(Action::Release(id.into()));
    }
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.send(Action::Flush(tx));
        let _ = rx.await;
    }
}
fn release(queue: &mut VecDeque<Item>, id: &str) {
    for item in queue.iter_mut().filter(|i| i.ids.iter().any(|v| v == id)) {
        item.deadline = Instant::now();
    }
}
fn visible(v: &Value) -> bool {
    v["isMeta"] != true
        && v["isCompactSummary"] != true
        && match v["type"].as_str() {
            Some("tool_progress" | "rate_limit_event") => false,
            Some("system") => matches!(
                v["subtype"].as_str(),
                Some(
                    "api_error"
                        | "turn_duration"
                        | "compact_boundary"
                        | "microcompact_boundary"
                        | "away_summary"
                        | "background_tasks_changed"
                )
            ),
            _ => true,
        }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test(start_paused = true)]
    async fn ordered_timeout_permission_result_flush() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let q = OutgoingQueue::new(move |v| tx.send(v).unwrap());
        for release in ["time", "permission", "result", "flush"] {
            q.enqueue(
                json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"t"}]}}),
            );
            q.enqueue(json!({"type":"user"}));
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(249)).await;
            assert!(rx.try_recv().is_err());
            match release{"permission"=>q.release_tool_call("t"),"result"=>q.enqueue(json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t"}]}})),"flush"=>q.flush().await,_=>tokio::time::advance(Duration::from_millis(1)).await}
            tokio::task::yield_now().await;
            assert_eq!(rx.try_recv().unwrap()["type"], "assistant");
            assert_eq!(rx.try_recv().unwrap()["type"], "user");
            if release == "result" {
                assert!(rx.try_recv().is_ok());
            }
        }
    }
    #[test]
    fn filtering() {
        for v in [
            json!({"type":"system","subtype":"init"}),
            json!({"type":"system","subtype":"task_started"}),
            json!({"type":"user","isMeta":true}),
            json!({"type":"assistant","isCompactSummary":true}),
        ] {
            assert!(!visible(&v));
        }
        for subtype in ["turn_duration", "background_tasks_changed"] {
            assert!(visible(
                &json!({"type":"system","subtype":subtype,"tasks":[]})
            ));
        }
    }
}
