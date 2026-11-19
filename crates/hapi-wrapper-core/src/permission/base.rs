use crate::{
    rpc::manager::RpcHandlerManager,
    session::{client::SessionClient, metadata::now},
};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;

pub const MISSING: &str = "Permission request not found or already resolved";
const ALWAYS: &[&str] = &[
    "change_title",
    "happy__change_title",
    "hapi_change_title",
    "geminireasoning",
    "codexreasoning",
    "think",
    "save_memory",
];
const EXACT: &[&str] = &[
    "skill_lookup",
    "hapi_skill_lookup",
    "happy__skill_lookup",
    "mcp__hapi__skill_lookup",
    "list_peers",
    "hapi_list_peers",
    "happy__list_peers",
    "mcp__hapi__list_peers",
    "list peer sessions",
];
const WRITE: &[&str] = &[
    "write",
    "edit",
    "create",
    "delete",
    "patch",
    "fs-edit",
    "ping_peer",
    "ping peer",
    "inspect_peer",
    "inspect peer",
    "spawn_peer",
    "spawn peer",
];
#[derive(Default)]
pub struct ApprovalRules<'a> {
    pub names: Option<&'a [&'a str]>,
    pub ids: Option<&'a [&'a str]>,
    pub writes: Option<&'a [&'a str]>,
}
pub fn auto_approval(
    mode: &str,
    tool: &str,
    call_id: &str,
    rules: ApprovalRules<'_>,
) -> Option<&'static str> {
    let tool = tool.to_lowercase();
    let id = call_id.to_lowercase();
    if matches!(mode, "always-proceed" | "yolo") {
        return Some("approved_for_session");
    }
    let contains = |s: &str, words: &[&str]| words.iter().any(|w| s.contains(w));
    (EXACT.contains(&tool.as_str())
        || contains(&tool, rules.names.unwrap_or(ALWAYS))
        || contains(&id, rules.ids.unwrap_or(&["change_title", "save_memory"]))
        || mode == "safe-yolo"
        || mode == "read-only" && !contains(&tool, rules.writes.unwrap_or(WRITE)))
    .then_some("approved")
}
pub struct Pending<T> {
    pub tool: String,
    pub input: Value,
    pub reply: oneshot::Sender<Result<T, String>>,
}
#[derive(Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Completion {
    pub status: String,
    pub reason: Option<String>,
    pub mode: Option<String>,
    pub decision: Option<String>,
    pub allow_tools: Option<Vec<String>>,
    pub answers: Option<Value>,
}
type Change = Box<dyn Fn(&Value) -> Value + Send + Sync>;
type Update = Arc<dyn Fn(Change) + Send + Sync>;
pub type ToolFilter = Arc<dyn Fn(&str) -> bool + Send + Sync>;
struct Requests<T> {
    pending: HashMap<String, Pending<T>>,
    in_flight: HashSet<String>,
}
type Received = Box<dyn Fn(&Value) + Send + Sync>;
type Registered = Box<dyn Fn(&str, &str, &Value) + Send + Sync>;
#[derive(Default)]
pub struct PermissionHooks {
    pub on_registered: Option<Registered>,
    pub on_response: Option<Received>,
}
pub struct BasePermissionHandler<T> {
    hooks: PermissionHooks,
    requests: Arc<Mutex<Requests<T>>>,
    update: Update,
}
struct ResponseGuard<T> {
    requests: Arc<Mutex<Requests<T>>>,
    id: String,
}
impl<T> Drop for ResponseGuard<T> {
    fn drop(&mut self) {
        self.requests.lock().unwrap().in_flight.remove(&self.id);
    }
}
fn object(value: &Value) -> serde_json::Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}
fn complete(state: &Value, id: &str, completion: &Completion) -> Value {
    let pending = &state["requests"][id];
    let completed = &state["completedRequests"][id];
    if pending.is_null() && completed["status"] != "canceled" {
        return state.clone();
    }
    let source = if pending.is_null() {
        completed
    } else {
        pending
    };
    let mut entry = object(source);
    if let Value::Object(fields) = json!(completion) {
        for (k, v) in fields {
            if v.is_null() {
                entry.remove(&k);
            } else {
                entry.insert(k, v);
            }
        }
    }
    entry.insert("completedAt".into(), now().into());
    let mut next = object(state);
    let mut requests = object(&state["requests"]);
    requests.remove(id);
    let mut completed = object(&state["completedRequests"]);
    completed.insert(id.into(), Value::Object(entry));
    next.insert("requests".into(), Value::Object(requests));
    next.insert("completedRequests".into(), Value::Object(completed));
    Value::Object(next)
}
impl<T: Send + 'static> BasePermissionHandler<T> {
    pub fn new(update: Update, hooks: PermissionHooks) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Requests {
                pending: HashMap::new(),
                in_flight: HashSet::new(),
            })),
            update,
            hooks,
        }
    }
    pub fn for_session(session: &Arc<SessionClient>, hooks: PermissionHooks) -> Arc<Self> {
        let session = Arc::downgrade(session);
        Arc::new(Self::new(
            Arc::new(move |f| {
                if let Some(s) = session.upgrade() {
                    s.agent_state.update(f)
                }
            }),
            hooks,
        ))
    }
    pub fn register_rpc(
        self: &Arc<Self>,
        rpc: &RpcHandlerManager,
        handle: impl Fn(Value, Pending<T>) -> BoxFuture<'static, Result<Completion, String>>
            + Send
            + Sync
            + 'static,
        missing: impl Fn(Value) -> Result<(), String> + Send + Sync + 'static,
    ) {
        let this = self.clone();
        let handle = Arc::new(handle);
        let missing = Arc::new(missing);
        rpc.register("permission", move |response| {
            let this = this.clone();
            let handle = handle.clone();
            let missing = missing.clone();
            async move {
                let id = response["id"].as_str().unwrap_or("").to_owned();
                let pending = {
                    let mut req = this.requests.lock().unwrap();
                    let p = req.pending.remove(&id);
                    if p.is_some() {
                        req.in_flight.insert(id.clone());
                    }
                    p
                };
                if let Some(pending) = pending {
                    let _guard = ResponseGuard {
                        requests: this.requests.clone(),
                        id: id.clone(),
                    };
                    if let Some(f) = &this.hooks.on_response {
                        f(&response);
                    }
                    let completion = handle(response, pending).await?;
                    (this.update)(Box::new(move |state| complete(state, &id, &completion)));
                } else {
                    this.finalize_orphans(MISSING.into(), Some(id));
                    missing(response)?;
                }
                Ok(None)
            }
        });
    }
    pub fn add(
        &self,
        id: String,
        tool: String,
        input: Value,
        call_id: Option<String>,
    ) -> oneshot::Receiver<Result<T, String>> {
        let (reply, rx) = oneshot::channel();
        let mut entry = json!({"tool":tool,"arguments":input,"createdAt":now()});
        if let Some(v) = call_id.filter(|v| !v.is_empty()) {
            entry["toolCallId"] = v.into();
        }
        self.requests
            .lock()
            .unwrap()
            .pending
            .insert(id.clone(), Pending { tool, input, reply });
        if let Some(f) = &self.hooks.on_registered {
            f(
                &id,
                entry["tool"].as_str().unwrap_or_default(),
                &entry["arguments"],
            );
        }
        (self.update)(Box::new(move |s| {
            let mut s = Value::Object(object(s));
            s["requests"] = Value::Object(object(&s["requests"]));
            s["requests"][&id] = entry.clone();
            s
        }));
        rx
    }
    pub fn pending_ids(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .pending
            .keys()
            .cloned()
            .collect()
    }

    pub fn finish_pending(
        &self,
        id: &str,
        result: Result<T, String>,
        completion: Completion,
    ) -> bool {
        let mut requests = self.requests.lock().unwrap();
        let Some(pending) = requests.pending.remove(id) else {
            return false;
        };
        let id = id.to_owned();
        requests.in_flight.insert(id.clone());
        drop(requests);
        let _guard = ResponseGuard {
            requests: self.requests.clone(),
            id: id.clone(),
        };
        (self.update)(Box::new(move |state| complete(state, &id, &completion)));
        let _ = pending.reply.send(result);
        true
    }

    pub fn finalize_orphans(&self, reason: String, only_id: Option<String>) {
        if only_id
            .as_ref()
            .is_some_and(|id| self.requests.lock().unwrap().in_flight.contains(id))
        {
            return;
        }
        let requests = self.requests.clone();
        (self.update)(Box::new(move |state| {
            let Some(entries) = state["requests"].as_object() else {
                return state.clone();
            };
            let tracked = requests.lock().unwrap();
            let mut next = state.clone();
            for (id, entry) in entries {
                if only_id.as_ref().is_some_and(|target| target != id)
                    || tracked.pending.contains_key(id)
                    || tracked.in_flight.contains(id)
                {
                    continue;
                }
                let mut canceled = Value::Object(object(entry));
                canceled["completedAt"] = now().into();
                canceled["status"] = "canceled".into();
                canceled["reason"] = json!(reason);
                canceled["decision"] = "abort".into();
                if let Some(map) = next["requests"].as_object_mut() {
                    map.remove(id);
                }
                if !next["completedRequests"].is_object() {
                    next["completedRequests"] = json!({});
                }
                next["completedRequests"][id] = canceled;
            }
            next
        }));
    }
    pub fn cancel(
        &self,
        reason: String,
        reject: String,
        decision: Option<String>,
        filter: Option<ToolFilter>,
    ) {
        let matches = move |tool: &str| filter.as_ref().is_none_or(|f| f(tool));
        let mut r = self.requests.lock().unwrap();
        for (_, p) in r.pending.extract_if(|_, p| matches(&p.tool)) {
            let _ = p.reply.send(Err(reject.clone()));
        }
        drop(r);
        (self.update)(Box::new(move |s| {
            let mut next = Value::Object(object(s));
            if !next["completedRequests"].is_object() {
                next["completedRequests"] = json!({});
            }
            next["requests"] = json!({});
            if let Some(entries) = s["requests"].as_object() {
                for (id, entry) in entries {
                    if matches(entry["tool"].as_str().unwrap_or("")) {
                        let mut e = Value::Object(object(entry));
                        e["completedAt"] = now().into();
                        e["status"] = "canceled".into();
                        e["reason"] = reason.clone().into();
                        if let Some(v) = &decision {
                            e["decision"] = v.clone().into()
                        } else if let Some(map) = e.as_object_mut() {
                            map.remove("decision");
                        }
                        next["completedRequests"][id] = e;
                    } else {
                        next["requests"][id] = entry.clone();
                    }
                }
            }
            next
        }));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{prelude::*, test_runner::RngSeed};
    #[test]
    fn approvals() {
        let decide = |m, t, id| auto_approval(m, t, id, Default::default());
        for t in EXACT {
            assert_eq!(decide("default", t, "id"), Some("approved"))
        }
        for t in [
            "skill_lookup_write_file",
            "dangerous_skill_lookup",
            "list_peers_and_write",
            "session_job",
            "hapi_session_job",
            "mcp__hapi__session_job",
            "Session-Attached Job",
        ] {
            assert_eq!(decide("default", t, "skill_lookup-forged-id"), None);
        }
        for verb in ["ping", "spawn", "inspect"] {
            for tool in [
                format!("{verb}_peer"),
                format!("mcp__hapi__{verb}_peer"),
                format!("hapi_{verb}_peer"),
                format!("{verb} Peer Session"),
            ] {
                for m in ["default", "read-only"] {
                    assert_eq!(auto_approval(m, &tool, "id", Default::default()), None);
                }
            }
        }
        for t in ["Read", "grep"] {
            assert_eq!(decide("read-only", t, "id"), Some("approved"))
        }
    }
    #[test]
    fn modes_and_custom_approval_rules() {
        for mode in ["yolo", "always-proceed"] {
            assert_eq!(
                auto_approval(mode, "edit", "x", Default::default()),
                Some("approved_for_session")
            );
        }
        for (mode, tool, id) in [
            ("safe-yolo", "edit", "x"),
            ("default", "change_title", "x"),
            ("default", "unknown", "save_memory:1"),
        ] {
            assert_eq!(
                auto_approval(mode, tool, id, Default::default()),
                Some("approved")
            );
        }
        let rules = || ApprovalRules {
            names: Some(&["trusted"]),
            ids: Some(&["known"]),
            writes: Some(&["custom"]),
        };
        for (mode, tool, id, ok) in [
            ("default", "change_title", "save_memory", false),
            ("default", "trusted", "x", true),
            ("default", "unknown", "known", true),
            ("read-only", "custom", "x", false),
            ("read-only", "write", "x", true),
        ] {
            assert_eq!(auto_approval(mode, tool, id, rules()).is_some(), ok);
        }
    }
    #[tokio::test]
    async fn filtered_cancel_orphan_cleanup_and_malformed_documents() {
        let (h, state) = fixture();
        *state.lock().unwrap() = json!({"requests":[],"completedRequests":[],"extra":4});
        let r = h.add("r".into(), "read".into(), Value::Null, None);
        let _w = h.add("w".into(), "write".into(), Value::Null, None);
        h.cancel(
            "stop".into(),
            "reject".into(),
            Some("abort".into()),
            Some(Arc::new(|s| s == "read")),
        );
        assert_eq!(r.await.unwrap(), Err("reject".into()));
        assert_eq!(
            state.lock().unwrap()["completedRequests"]["r"]["decision"],
            "abort"
        );
        state.lock().unwrap()["requests"]["bad"] = json!("malformed");
        h.finalize_orphans("orphan".into(), None);
        assert_eq!(
            state.lock().unwrap()["completedRequests"]["bad"]["status"],
            "canceled"
        );
        assert_eq!(state.lock().unwrap()["requests"]["w"]["tool"], "write");
        state.lock().unwrap()["requests"]["bad2"] = json!(23);
        h.cancel("stop".into(), "reject".into(), None, None);
        assert_eq!(
            state.lock().unwrap()["completedRequests"]["bad2"]["status"],
            "canceled"
        );
        let c = Completion {
            status: "approved".into(),
            ..Default::default()
        };
        for input in [
            json!({"requests":[],"completedRequests":{"r":{"status":"canceled"}}}),
            json!({"requests":{"r":"bad"},"completedRequests":42}),
        ] {
            assert_eq!(
                complete(&input, "r", &c)["completedRequests"]["r"]["status"],
                "approved"
            );
        }
        assert_eq!(state.lock().unwrap()["extra"], 4);
    }
    fn fixture() -> (Arc<BasePermissionHandler<()>>, Arc<Mutex<Value>>) {
        let state = Arc::new(Mutex::new(json!({})));
        let sink = state.clone();
        (
            Arc::new(BasePermissionHandler::new(
                Arc::new(move |f| {
                    let mut v = sink.lock().unwrap();
                    *v = f(&v);
                }),
                Default::default(),
            )),
            state,
        )
    }
    #[tokio::test]
    async fn overlapping_answers() {
        let (h, state) = fixture();
        let rpc = RpcHandlerManager::new("s");
        let gate = Arc::new(tokio::sync::Notify::new());
        let wait = gate.clone();
        h.register_rpc(
            &rpc,
            move |v, p| {
                let wait = wait.clone();
                Box::pin(async move {
                    wait.notified().await;
                    p.reply.send(Ok(())).unwrap();
                    Ok(Completion {
                        status: "approved".into(),
                        answers: Some(v["answers"].clone()),
                        ..Default::default()
                    })
                })
            },
            |_| Err(MISSING.into()),
        );
        let rx = h.add("r".into(), "AskUserQuestion".into(), json!({}), None);
        let req =
            json!({"method":"s:permission","params":r#"{"id":"r","answers":{"0":["keep-me"]}}"#});
        let mut first = Box::pin(rpc.handle_request(&req));
        use futures_util::FutureExt;
        assert!(first.as_mut().now_or_never().is_none());
        assert!(rpc.handle_request(&req).await.unwrap().contains(MISSING));
        assert!(state.lock().unwrap()["requests"]["r"].is_object());
        gate.notify_one();
        assert!(first.await.is_none());
        rx.await.unwrap().unwrap();
        assert_eq!(
            state.lock().unwrap()["completedRequests"]["r"]["answers"],
            json!({"0":["keep-me"]})
        );
    }
    fn check_request_transitions(ops: Vec<(u8, u8)>) -> Result<(), TestCaseError> {
        let (h, state) = fixture();
        for (op, k) in ops {
            let id = k.to_string();
            let before = state.lock().unwrap().clone();
            match op {
                0 if before["requests"][&id].is_null()
                    && before["completedRequests"][&id].is_null() =>
                {
                    drop(h.add(id.clone(), "tool".into(), Value::Null, None));
                }
                1 => {
                    h.requests.lock().unwrap().pending.remove(&id);
                    let s = complete(
                        &before,
                        &id,
                        &Completion {
                            status: "approved".into(),
                            ..Default::default()
                        },
                    );
                    *state.lock().unwrap() = s;
                }
                2 => h.cancel("abort".into(), "abort".into(), None, None),
                3 => {
                    h.requests.lock().unwrap().pending.clear();
                    h.finalize_orphans(MISSING.into(), Some(id.clone()));
                }
                _ => h.finalize_orphans(MISSING.into(), Some(id.clone())),
            }
            let after = state.lock().unwrap();
            for i in 0..8 {
                let i = i.to_string();
                prop_assert!(
                    after["requests"][&i].is_null() || after["completedRequests"][&i].is_null()
                );
                if before["completedRequests"][&i]["status"] == "approved" {
                    prop_assert_eq!(
                        &before["completedRequests"][&i],
                        &after["completedRequests"][&i]
                    );
                }
            }
        }
        Ok(())
    }
    proptest! {#![proptest_config(ProptestConfig{rng_seed:RngSeed::Fixed(127),..Default::default()})]
    #[test] fn request_transitions(ops in prop::collection::vec((0u8..5,0u8..8),1..120)) {check_request_transitions(ops)?;}
    }
}
