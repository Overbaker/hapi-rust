use super::{async_questions, event_converter::Converter, plan, value::*};
use hapi_wrapper_core::jsjson::stringify;
use indexmap::IndexMap;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

pub(super) fn hash(bytes: &str) -> String {
    Sha256::digest(bytes.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn input_text(input: &Value) -> String {
    input
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            if let Some(s) = text(p, "text") {
                return Some(s.to_owned());
            }
            Some(match p["type"].as_str()? {
                "mention" => format!("@\"{}\"", p["path"].as_str().unwrap_or("undefined")),
                "skill" => format!("${}", p["name"].as_str().unwrap_or("undefined")),
                "localImage" => format!("[Image: {}]", p["path"].as_str().unwrap_or("undefined")),
                "image" => "[Image]".into(),
                _ => return None,
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn requested_title(item: &Value) -> Option<String> {
    if item["type"] != "mcpToolCall" || item["server"] != "hapi" || item["tool"] != "change_title" {
        return None;
    }
    let title = text(&item["arguments"], "title")?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!title.is_empty()).then_some(title)
}
fn successful_title(item: &Value, pending: Option<String>) -> Option<String> {
    let res = &item["result"];
    if item["type"] != "mcpToolCall"
        || item.get("status").is_some_and(|s| s != "completed")
        || !item["error"].is_null()
        || res.is_null()
        || res.get("Err").is_some()
        || res["isError"] == true
        || res["Ok"]["isError"] == true
    {
        return None;
    }
    requested_title(item).or(pending)
}
pub trait Host {
    fn current_metadata(&self) -> Value;
    fn agent(&mut self, id: &str, body: Value);
    fn user(&mut self, id: &str, body: &str);
    fn status(&mut self, body: Value);
    fn metadata(&mut self, update: Box<dyn Fn(&Value) -> Value + Send + Sync>);
    fn committed(&mut self, id: &str, content: &Value) -> Result<Option<Vec<Value>>, String>;
    fn observed(&mut self, thread: &str, item: &Value);
    fn image(&mut self, path: &str, id: &str, name: Option<&str>) -> Option<Value>;
    fn compaction(&mut self, thread: &str, turn: &str, item: &str, history: bool) -> Option<Value>;
    fn generation_event(&mut self, event: &Value, at: i64);
    fn turn_clock(&mut self, turn: &str, at: i64, completion: Option<bool>) -> Option<Value>;
}
pub struct Projection {
    pub thread: String,
    parent: Option<String>,
    converter: Converter,
    emitted: HashSet<String>,
    turns: IndexMap<String, String>,
    models: HashMap<String, String>,
    pending: HashMap<String, String>,
    titles: HashSet<String>,
    revision: Arc<AtomicU64>,
}
impl Projection {
    pub fn new(thread: String, parent: Option<String>, metadata: &Value) -> Self {
        let turns = if parent.is_none() {
            serde_json::from_value(metadata["conversationHistoryTurns"].clone()).unwrap_or_default()
        } else {
            IndexMap::new()
        };
        Self {
            thread,
            parent,
            turns,
            converter: Converter::default(),
            emitted: HashSet::new(),
            models: HashMap::new(),
            pending: HashMap::new(),
            titles: HashSet::new(),
            revision: Arc::new(AtomicU64::new(0)),
        }
    }
    pub fn reset(&mut self) {
        self.converter.reset();
        self.emitted.clear();
    }
    pub fn turn_for(&self, id: &str) -> Option<&str> {
        self.turns.get(id).map(String::as_str)
    }
    fn send(&mut self, host: &mut impl Host, key: &str, mut body: Value, turn: Option<&str>) {
        if !self.emitted.insert(key.into()) {
            return;
        }
        let id = format!("codex:{}:{key}", self.thread);
        if let Some(t) = turn {
            body["turn_id"] = json!(t);
        }
        body["id"] = json!(id);
        if let Some(parent) = self
            .parent
            .as_ref()
            .filter(|_| !text(&body, "type").unwrap_or("").starts_with("agent-run-"))
        {
            body = json!({"type":"agent-run-trace","agentId":self.thread,"cardId":format!("codex-agent:{}",self.thread),"message":body,"id":id,"scope":{"role":"child","threadId":self.thread,"parentThreadId":parent},"scope_role":"child"});
        }
        host.agent(&id, body);
    }
    fn rename(&self, host: &mut impl Host, title: String, revision: u64, history: bool) {
        let current = self.revision.clone();
        host.metadata(Box::new(move |m| {
            if current.load(Ordering::Relaxed) != revision
                || m["titleLocked"] == true
                || (history
                    && [m["name"].as_str(), m["summary"]["text"].as_str()]
                        .into_iter()
                        .flatten()
                        .any(|s| !s.trim().is_empty()))
            {
                return m.clone();
            }
            merge(m.clone(), &json!({"name":title}))
        }));
    }
    pub fn notification(
        &mut self,
        host: &mut impl Host,
        method: &str,
        p: &Value,
        model: Option<&str>,
        at: i64,
    ) -> Result<(), String> {
        let item = &p["item"];
        if self.parent.is_none() && p["threadId"] == self.thread {
            if let Some(id) = text(item, "id") {
                let key = format!("{}:{id}", text(p, "turnId").unwrap_or("thread"));
                if !self.titles.contains(&key) {
                    if method == "item/started" {
                        if let Some(t) = requested_title(item) {
                            self.pending.insert(key, t);
                        }
                    } else if method == "item/completed" {
                        if let Some(t) = successful_title(item, self.pending.remove(&key)) {
                            self.titles.insert(key);
                            let r = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
                            self.rename(host, t, r, false);
                        }
                    }
                }
            }
        }
        self.project(host, method, p, model, false, at)?;
        if self.parent.is_none()
            && p["threadId"] == self.thread
            && matches!(method, "turn/started" | "turn/completed")
        {
            if let Some(turn) = text(&p["turn"], "id") {
                let complete =
                    (method == "turn/completed").then_some(p["turn"]["status"] == "completed");
                if let Some(body) = host.turn_clock(turn, at, complete) {
                    self.send(host, &format!("duration:{turn}"), body, None);
                }
            }
        }
        Ok(())
    }
    pub fn title_revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }
    pub fn history(&mut self, host: &mut impl Host, thread: &Value) -> Result<(), String> {
        self.history_at(host, thread, self.title_revision())
    }
    pub(super) fn history_at(
        &mut self,
        host: &mut impl Host,
        thread: &Value,
        revision: u64,
    ) -> Result<(), String> {
        let mut latest = None;
        for turn in thread["turns"].as_array().into_iter().flatten() {
            for item in turn["items"].as_array().into_iter().flatten() {
                let p = json!({"threadId":self.thread,"turnId":turn["id"],"item":item});
                let key = format!(
                    "{}:{}",
                    text(turn, "id").unwrap_or("thread"),
                    text(item, "id").unwrap_or("undefined")
                );
                if self.parent.is_none() && item["id"].is_string() && !self.titles.contains(&key) {
                    if let Some(title) = requested_title(item) {
                        self.pending.insert(key.clone(), title);
                    }
                }
                self.project(host, "item/started", &p, None, true, 0)?;
                if turn["status"] != "inProgress"
                    || item["status"] == "completed"
                    || item["type"] == "userMessage"
                    || !async_questions::questions(&self.thread, item).is_empty()
                {
                    if self.parent.is_none() {
                        if let Some(title) =
                            successful_title(item, None).filter(|_| item["id"].is_string())
                        {
                            latest = Some(title);
                            self.titles.insert(key.clone());
                        }
                        self.pending.remove(&key);
                    }
                    self.project(host, "item/completed", &p, None, true, 0)?;
                }
            }
        }
        let metadata = host.current_metadata();
        let has_title = [&metadata["name"], &metadata["summary"]["text"]]
            .iter()
            .any(|s| s.as_str().is_some_and(|s| !s.trim().is_empty()));
        if let Some(title) = latest.filter(|_| self.title_revision() == revision && !has_title) {
            self.rename(host, title, revision, true);
        }
        Ok(())
    }
    fn project(
        &mut self,
        host: &mut impl Host,
        method: &str,
        p: &Value,
        model: Option<&str>,
        history: bool,
        at: i64,
    ) -> Result<(), String> {
        if method.starts_with("codex/event/") {
            return Ok(());
        }
        let item = &p["item"];
        let turn = text(p, "turnId").or_else(|| text(&p["turn"], "id"));
        let item_id = text(item, "id").or_else(|| text(p, "itemId"));
        let questions = async_questions::questions(&self.thread, item);
        if !questions.is_empty() {
            if method == "item/completed" {
                for q in questions {
                    self.send(host, q["id"].as_str().unwrap(), json!({"type":"tool-call","name":"request_user_input","callId":q["id"],"input":q["input"]}), turn);
                }
                host.observed(&self.thread, item);
            }
            return Ok(());
        }
        if method == "item/completed" {
            host.observed(&self.thread, item);
        }
        if let Some(t) = turn {
            if let Some(m) = model.filter(|_| method == "turn/started") {
                self.models.entry(t.into()).or_insert(m.into());
            }
            if let Some(m) = text(p, "toModel").filter(|_| method == "model/rerouted") {
                self.models.insert(t.into(), m.into());
            }
        }
        if self.parent.is_none()
            && matches!(method, "item/started" | "item/completed")
            && item["type"] == "userMessage"
        {
            let id = text(item, "clientId clientUserMessageId")
                .map(str::to_string)
                .or_else(|| item_id.map(|id| format!("codex:{}:user:{id}", self.thread)));
            if let Some(id) = id {
                for member in host
                    .committed(&id, &item["content"])?
                    .unwrap_or_else(|| vec![json!({"id":id,"input":item["content"]})])
                {
                    let id = member["id"].as_str().ok_or("Invalid queue member")?;
                    let boundary = turn.is_some_and(|t| {
                        self.turns
                            .iter()
                            .find(|(_, v)| *v == t)
                            .is_none_or(|(first, _)| first == id)
                    });
                    if let Some(t) = turn {
                        self.turns.insert(id.into(), t.into());
                    }
                    let mut s = input_text(&member["input"]);
                    let replies = async_questions::replies(&s);
                    if !replies.is_empty() {
                        s = replies
                            .iter()
                            .map(|r| {
                                [text(r, "question"), text(r, "answer")]
                                    .into_iter()
                                    .flatten()
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                    }
                    if !s.is_empty() {
                        host.user(id, &s);
                    }
                    let turns = json!(self.turns);
                    let id = id.to_owned();
                    host.metadata(Box::new(move |m| {
                        let mut m = m.clone();
                        m["conversationHistoryTurns"] = turns.clone();
                        if boundary {
                            m["conversationHistoryPoints"][&id] = json!(true);
                        }
                        m
                    }));
                }
            }
        }
        if self.parent.is_some() && matches!(method, "turn/started" | "turn/completed") {
            self.send(host, &format!("lifecycle:{}:{method}", turn.unwrap_or("undefined")), json!({"type":"agent-run-update","agentId":self.thread,"cardId":format!("codex-agent:{}",self.thread),"status":if method=="turn/started"{"running"}else if p["turn"]["status"]=="completed"{"completed"}else{"failed"}}), turn);
        }
        for e in self.converter.notification(method, p) {
            if !history && self.parent.is_none() {
                host.generation_event(&e, at);
            }
            let call = text(&e, "call_id");
            let ty = text(&e, "type").unwrap_or("");
            let part = item_id
                .or(call)
                .map(str::to_owned)
                .unwrap_or_else(|| hash(&stringify(&e)));
            let key = format!("{}:{part}:{ty}", turn.unwrap_or("thread"));
            let mut body = match ty {
                "agent_message" => json!({"type":"message","message":e["message"]}),
                "agent_reasoning" => json!({"type":"reasoning","message":e["text"]}),
                "exec_command_begin" => {
                    json!({"type":"tool-call","name":"CodexBash","callId":call,"input":e})
                }
                "exec_command_end" => {
                    let failed = matches!(
                        text(&e, "status"),
                        Some("failed" | "cancelled" | "canceled" | "declined" | "interrupted")
                    ) || e["exit_code"].as_f64().is_some_and(|n| n != 0.)
                        || text(&e, "error").is_some();
                    let mut output = e.clone();
                    copy_fields(&mut output, &e, &[("stdout", "output")]);
                    json!({"type":"tool-call-result","callId":call,"output":output,"is_error":failed})
                }
                "patch_apply_begin" => {
                    let mut input = json!({});
                    copy_fields(
                        &mut input,
                        &e,
                        &[("changes", "changes"), ("auto_approved", "auto_approved")],
                    );
                    json!({"type":"tool-call","name":"CodexPatch","callId":call,"input":input})
                }
                "patch_apply_end" => {
                    let mut out = json!({});
                    copy_fields(
                        &mut out,
                        &e,
                        &[
                            ("stdout", "stdout"),
                            ("stderr", "stderr"),
                            ("success", "success"),
                        ],
                    );
                    json!({"type":"tool-call-result","callId":call,"output":out})
                }
                "mcp_tool_call_begin" => {
                    let i = &e["invocation"];
                    json!({"type":"tool-call","name":format!("mcp__{}__{}",text(i,"server").unwrap_or("null"),text(i,"tool").unwrap_or("null")),"callId":call,"input":i["arguments"]})
                }
                "mcp_tool_call_end" => {
                    let r = &e["result"];
                    let out = r.get("Ok").or_else(|| r.get("Err")).unwrap_or(r);
                    let mut b = json!({"type":"tool-call-result","callId":call,"is_error":r.get("Err").is_some()});
                    if e.get("result").is_some() {
                        b["output"] = out.clone();
                    }
                    b
                }
                "codex_tool_call_begin" => {
                    json!({"type":"tool-call","name":e["name"],"callId":call,"input":field(&e,"input arguments")})
                }
                "codex_tool_call_end" => {
                    json!({"type":"tool-call-result","callId":call,"output":e["output"],"is_error":e["is_error"]})
                }
                "compaction_started" => {
                    if history {
                        continue;
                    }
                    if self.parent.is_none() {
                        if self.emitted.insert(key) {
                            host.status(
                                json!({"type":"message","message":"📦 Compaction started"}),
                            );
                        }
                        continue;
                    }
                    json!({"type":"compaction_started","statusText":"📦 Compaction started"})
                }
                "context_compacted"
                | "token_count"
                | "thread_goal_updated"
                | "thread_goal_cleared" => {
                    if ty == "context_compacted" {
                        if let (Some(t), Some(i)) = (turn, item_id) {
                            if let Some(info) = host.compaction(&self.thread, t, i, history) {
                                let mut b = merge(json!({"type":"compact-summary"}), &info);
                                b["summary"] = json!(text(&info, "summary").unwrap_or(""));
                                b["compactionId"] = json!(format!("codex:{}:{key}", self.thread));
                                self.send(host, &format!("{key}:summary"), b, turn);
                                continue;
                            }
                        }
                    }
                    let mut b = e.clone();
                    if ty == "token_count" {
                        if let Some(m) = turn.and_then(|t| self.models.get(t)) {
                            b["model"] = json!(m);
                        }
                    }
                    merge(
                        b,
                        &json!({"flavor":"codex","scope":{"role":"parent","threadId":self.thread},"scope_role":"parent","thread_id":self.thread}),
                    )
                }
                "proposed_plan" => {
                    let (Some(t), Some(i)) = (turn, item_id) else {
                        continue;
                    };
                    let id = plan::proposal_id(&self.thread, t, i);
                    self.send(host, &key, json!({"type":"tool-call","name":"ExitPlanMode","callId":id,"input":{"plan":e["plan"]}}), turn);
                    self.send(
                        host,
                        &format!("{key}:result"),
                        json!({"type":"tool-call-result","callId":id,"output":null}),
                        turn,
                    );
                    continue;
                }
                "plan_update" => {
                    let id = turn.map_or_else(
                        || "codex-plan-state".to_string(),
                        |t| format!("codex-plan-state:{t}"),
                    );
                    let input = json!({"plan":e["plan"],"source":"codex"});
                    self.send(
                        host,
                        &key,
                        json!({"type":"tool-call","name":"update_plan","callId":id,"input":input}),
                        turn,
                    );
                    self.send(host, &format!("{key}:result"), json!({"type":"tool-call-result","callId":id,"output":merge(input,&json!({"status":"updated"}))}), turn);
                    continue;
                }
                "generated_image" | "viewed_image" => {
                    let Some(path) = text(&e, "saved_path path") else {
                        continue;
                    };
                    let Some(i) = host.image(
                        path,
                        &hash(&format!("{}:{key}", self.thread)),
                        text(&e, "file_name"),
                    ) else {
                        continue;
                    };
                    let mut b = json!({"type":"generated-image","imageId":i["id"],"fileName":i["fileName"],"mimeType":i["mimeType"]});
                    if ty == "viewed_image" {
                        b["source"] = json!({"ingress":"tool_result","flavor":"codex","toolName":"view_image","toolCallId":call});
                    }
                    b
                }
                "task_failed" => {
                    json!({"type":"message","message":format!("Codex error: {}",text(&e,"error message").unwrap_or("Turn failed"))})
                }
                _ => continue,
            };
            if body["type"] == "tool-call-result"
                && ty == "codex_tool_call_end"
                && e.get("output").is_none()
            {
                body.as_object_mut().unwrap().remove("output");
            }
            self.send(host, &key, body, turn);
        }
        if item["type"] == "collabAgentToolCall" {
            for (agent, state) in item["agentsStates"].as_object().into_iter().flatten() {
                let mut b = json!({"type":"agent-run-update","agentId":agent,"cardId":format!("codex-agent:{agent}"),"status":match text(state,"status"){Some("completed")=>"completed",Some("errored")=>"failed",_=>"running"},"input":item,"scope":{"role":"child","threadId":agent,"parentThreadId":self.thread},"scope_role":"child","thread_id":agent});
                copy_fields(&mut b, state, &[("summary", "message")]);
                self.send(
                    host,
                    &format!(
                        "agent:{agent}:{}:{method}:{}",
                        item_id.unwrap_or("undefined"),
                        stringify(state)
                    ),
                    b,
                    turn,
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Sink {
        events: Vec<Value>,
        metadata: Value,
    }
    impl Host for Sink {
        fn current_metadata(&self) -> Value {
            self.metadata.clone()
        }
        fn agent(&mut self, id: &str, body: Value) {
            self.events.push(json!({"body":body,"id":id}));
        }
        fn user(&mut self, id: &str, message: &str) {
            self.events.push(json!({"message":message,"id":id}));
        }
        fn status(&mut self, body: Value) {
            self.events.push(json!({"body":body}));
        }
        fn metadata(&mut self, f: Box<dyn Fn(&Value) -> Value + Send + Sync>) {
            self.metadata = f(&self.metadata);
        }
        fn committed(&mut self, _: &str, _: &Value) -> Result<Option<Vec<Value>>, String> {
            Ok(None)
        }
        fn observed(&mut self, _: &str, _: &Value) {}
        fn image(&mut self, _: &str, _: &str, _: Option<&str>) -> Option<Value> {
            None
        }
        fn compaction(&mut self, _: &str, _: &str, _: &str, _: bool) -> Option<Value> {
            None
        }
        fn generation_event(&mut self, _: &Value, _: i64) {}
        fn turn_clock(&mut self, _: &str, _: i64, _: Option<bool>) -> Option<Value> {
            None
        }
    }
    #[test]
    fn typescript_projection_ids_and_replay() {
        let out = std::process::Command::new("bun")
            .arg("src/flavors/codex/projection_vectors.ts")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let cases: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
        for case in cases {
            let mut sink = Sink {
                metadata: json!({}),
                ..Default::default()
            };
            let mut projection = Projection::new(
                "🧪".into(),
                text(&case, "parent").map(String::from),
                &sink.metadata,
            );
            for op in case["operations"].as_array().unwrap() {
                match op[0].as_str().unwrap() {
                    "history" => projection.history(&mut sink, &op[1]).unwrap(),
                    "reset" => projection.reset(),
                    method => projection
                        .notification(&mut sink, method, &op[1], None, 0)
                        .unwrap(),
                }
            }
            assert_eq!(sink.events.len(), case["events"].as_array().unwrap().len());
            for (actual, expected) in sink.events.iter().zip(case["events"].as_array().unwrap()) {
                assert_eq!(actual, expected);
            }
            assert_eq!(sink.metadata, case["metadata"]);
        }
    }
}
