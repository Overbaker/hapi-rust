use super::value::*;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

fn scope(p: &Value) -> Value {
    let mut out = json!({});
    let usage = field(p, "tokenUsage token_usage info");
    for (key, name, keys) in [
        ("thread_id", "thread", "threadId thread_id"),
        ("turn_id", "turn", "turnId turn_id"),
    ] {
        let nested = format!("{keys} id");
        for x in [
            field(p, keys),
            field(&p[name], &nested),
            field(usage, keys),
            field(&usage[name], &nested),
            field(&p["item"], keys),
            field(&p["item"][name], &nested),
        ] {
            if !x.is_null() {
                if let Some(s) = x.as_str().filter(|s| !s.is_empty()) {
                    out[key] = json!(s);
                }
                break;
            }
        }
    }
    out
}
fn plan(p: &Value) -> Vec<Value> {
    let list = field(p, "plan update items steps");
    let list = list
        .as_array()
        .or_else(|| field(list, "plan items steps").as_array());
    let steps: Vec<_> = list
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let step = v
                .as_str()
                .or_else(|| text(v, "step content text title description"))?;
            let status = match normalize(text(v, "status state").unwrap_or("")).as_str() {
                "completed" | "complete" | "done" => "completed",
                "inprogress" | "active" | "running" => "in_progress",
                _ => "pending",
            };
            Some(json!({"step":step,"status":status}))
        })
        .collect();
    if steps.is_empty() {
        vec![]
    } else {
        vec![json!({"type":"plan_update","plan":steps})]
    }
}
fn assistant_text(text: &str, phase: &Value) -> String {
    if phase != "commentary" && phase != "partial_answer" {
        return text.into();
    }
    let end = text.trim_end();
    let start = end.rfind('\n').map_or(0, |i| i + 1);
    let line = &end[start..];
    for (i, marker) in line.match_indices("AGENT_NOTIFY_SUMMARY ") {
        let prefix = &line[..i];
        if !prefix.trim().is_empty() && prefix.ends_with(char::is_whitespace) {
            continue;
        }
        if serde_json::from_str::<Value>(line[i + marker.len()..].trim())
            .is_ok_and(|v| v.is_object())
        {
            return end[..start + i].trim_end().into();
        }
    }
    text.into()
}
fn item_text(i: &Value) -> Option<String> {
    if let Some(s) = text(i, "text message").or_else(|| text(i, "content")) {
        return Some(s.into());
    }
    let s: String = i["content"]
        .as_array()?
        .iter()
        .filter_map(|v| text(v, "text message content"))
        .collect();
    (!s.is_empty()).then_some(s)
}
fn tool_name(i: &Value, keys: &str) -> Option<&'static str> {
    Some(match normalize(text(i, keys)?).as_str() {
        "spawnagent" | "spawn" => "spawn_agent",
        "sendinput" => "send_input",
        "sendmessage" => "send_message",
        "resumeagent" | "resume" => "resume_agent",
        "followuptask" | "assigntask" => "followup_task",
        "waitagent" | "wait" => "wait_agent",
        "closeagent" | "close" => "close_agent",
        "interruptagent" | "interrupt" => "interrupt_agent",
        "listagents" => "list_agents",
        _ => return None,
    })
}
fn agent_status(s: &Value) -> Value {
    let status = normalize(text(s, "status state").unwrap_or(""));
    let msg = [
        "message",
        "output",
        "result",
        "finalMessage",
        "final_message",
    ]
    .into_iter()
    .find_map(|k| text(s, k));
    if matches!(status.as_str(), "completed" | "complete" | "done")
        || s["done"] == true
        || s["completed"] == true
    {
        return msg.map_or_else(
            || merge(s.clone(), &json!({"status":"completed"})),
            |m| json!({"completed":m}),
        );
    }
    match (status.as_str(), msg) {
        ("failed" | "error", Some(m)) => json!({"failed":m}),
        ("cancelled" | "canceled", Some(m)) => json!({"canceled":m}),
        _ => s.clone(),
    }
}
fn collab(i: &Value, name: &str, start: bool) -> Value {
    let targets = strings(field(i, "receiverThreadIds receiver_thread_ids targets"));
    let first = targets.first();
    let mut out = json!({});
    if start {
        if let Some(value) = text(i, "prompt message") {
            out["message"] = json!(value);
        }
        if let Some(value) = text(i, "agentType agent_type") {
            out["agent_type"] = json!(value);
        }
        if let Some(value) = field(i, "forkContext fork_context").as_bool() {
            out["fork_context"] = json!(value);
        }
        if let Some(value) = text(i, "model") {
            out["model"] = json!(value);
        }
        if let Some(value) = text(i, "reasoningEffort reasoning_effort") {
            out["reasoning_effort"] = json!(value);
        }
        if let Some(value) = text(i, "senderThreadId sender_thread_id") {
            out["sender_thread_id"] = json!(value);
        }
        if !targets.is_empty() {
            out["targets"] = json!(targets);
            if matches!(name, "close_agent" | "send_input" | "resume_agent") {
                out["target"] = json!(first);
            }
        }
    } else {
        let states = object(field(i, "agentsStates agents_states"));
        let status = text(i, "status");
        match name {
            "spawn_agent" => {
                if let Some(t) = first {
                    out["agent_id"] = json!(t);
                    out["agentId"] = json!(t);
                }
            }
            "wait_agent" => {
                out["status"] = json!({});
                for (k, v) in states.as_object().unwrap() {
                    out["status"][k] = agent_status(v);
                }
            }
            "close_agent" => {
                if let Some(s) = first.and_then(|k| states.get(*k)).or_else(|| {
                    if first.is_none() {
                        states.as_object()?.values().next()
                    } else {
                        None
                    }
                }) {
                    out["previous_status"] = agent_status(s);
                }
            }
            _ => {
                if !targets.is_empty() {
                    out["targets"] = json!(targets);
                }
            }
        }
        if !matches!(name, "wait_agent" | "close_agent") {
            if let Some(s) = status {
                out["status"] = json!(s);
            }
        }
        if let Some(e) = text(i, "error message") {
            out["error"] = json!(e);
            out["message"] = json!(e);
        }
        match name {
            "wait_agent" => {
                out["timed_out"] = json!(matches!(status, Some("timedOut" | "timed_out")))
            }
            "close_agent" => {
                if let Some(t) = first {
                    out["agent_id"] = json!(t);
                }
            }
            _ => out["agentsStates"] = states,
        }
    }
    out
}
#[derive(Default)]
struct Deltas {
    buf: HashMap<String, String>,
    last: HashMap<String, String>,
}
impl Deltas {
    fn append(&mut self, id: &str, delta: &str) -> bool {
        if self.last.get(id).is_some_and(|s| s == delta) {
            return false;
        }
        self.last.insert(id.into(), delta.into());
        self.buf.entry(id.into()).or_default().push_str(delta);
        true
    }
    fn take(&mut self, id: &str) -> Option<String> {
        self.last.remove(id);
        self.buf.remove(id)
    }
}
#[derive(Default)]
pub struct Converter {
    messages: Deltas,
    reasoning: Deltas,
    commands: Deltas,
    completed: HashSet<(String, String)>,
    breaks: HashSet<(String, String)>,
    compacting: HashSet<(String, String)>,
    command_meta: HashMap<String, Value>,
    patch_meta: HashMap<String, Value>,
    raw_ids: HashSet<String>,
    raw_names: HashMap<String, String>,
}
impl Converter {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn notification(&mut self, method: &str, p: &Value) -> Vec<Value> {
        let sc = scope(p);
        if method.starts_with("codex/event/") {
            return self.wrapped(&p["msg"]);
        }
        let turn = &p["turn"];
        let thread = &p["thread"];
        let id = text(p, "itemId item_id id").or_else(|| text(&p["item"], "id itemId item_id"));
        let mut event = match method {
            "item/started" | "item/completed" => {
                return self.item(method == "item/started", p, id, &sc)
            }
            "turn/plan/updated" => {
                return plan(p).into_iter().map(|e| merge(sc.clone(), &e)).collect()
            }
            "account/rateLimits/updated" => {
                if p.is_object() {
                    record_quota_update();
                }
                return vec![];
            }
            "thread/compacted" => return compact(&sc, false),
            "thread/goal/updated" => {
                let Some(t) = field_or(
                    p,
                    "threadId thread_id",
                    field(&p["goal"], "threadId thread_id"),
                )
                .as_str()
                .filter(|s| !s.is_empty()) else {
                    return vec![];
                };
                if !p["goal"].is_object() {
                    return vec![];
                }
                let mut e = json!({"type":"thread_goal_updated","thread_id":t});
                if let Some(value) = text(p, "turnId turn_id") {
                    e["turn_id"] = json!(value);
                }
                e["goal"] = p["goal"].clone();
                return vec![e];
            }
            "thread/goal/cleared" => {
                return text(&sc, "thread_id")
                    .map(|t| json!({"type":"thread_goal_cleared","thread_id":t}))
                    .into_iter()
                    .collect()
            }
            "thread/started" | "thread/resumed" => {
                return text(
                    if thread.is_object() { thread } else { p },
                    "threadId thread_id id",
                )
                .map(|id| json!({"type":"thread_started","thread_id":id}))
                .into_iter()
                .collect()
            }
            "thread/status/changed" => {
                let t = if thread.is_object() { thread } else { p };
                let status = field_or(p, "status", &t["status"]);
                if field_or(status, "type", field(p, "statusType status_type")).as_str()
                    != Some("systemError")
                {
                    return vec![];
                }
                let mut e = json!({"type":"task_failed"});
                if let Some(id) = text(t, "threadId thread_id id") {
                    e["thread_id"] = json!(id);
                }
                e["terminal_source"] = json!("thread_status");
                e["error"] = json!(field_or(status, "message error", field(p, "message error"))
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Codex thread entered systemError"));
                e
            }
            "turn/started" | "turn/completed" => {
                let t = if turn.is_object() { turn } else { p };
                let status = field_or(p, "status", &t["status"])
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase();
                let kind = if method == "turn/started" {
                    "task_started"
                } else {
                    match status.as_str() {
                        "interrupted" | "cancelled" | "canceled" => "turn_aborted",
                        "failed" | "error" => "task_failed",
                        _ => "task_complete",
                    }
                };
                let mut e = json!({"type":kind});
                if let Some(id) = text(t, "turnId turn_id id") {
                    e["turn_id"] = json!(id);
                }
                if kind == "task_failed" {
                    e["terminal_source"] = json!("turn_completed");
                    let err = field_or(p, "error", &t["error"]);
                    if let Some(info) = text(p, "codexErrorInfo codex_error_info")
                        .or_else(|| text(err, "codexErrorInfo codex_error_info"))
                    {
                        e["codex_error_info"] = json!(info);
                    }
                    if let Some(msg) =
                        text(p, "error message reason").or_else(|| text(err, "message"))
                    {
                        e["error"] = json!(msg);
                    }
                }
                e
            }
            "turn/diff/updated" => {
                let Some(s) = text(p, "diff unified_diff unifiedDiff") else {
                    return vec![];
                };
                json!({"type":"turn_diff","unified_diff":s})
            }
            "thread/tokenUsage/updated" => {
                let info = field_or(p, "tokenUsage token_usage", p);
                json!({"type":"token_count","usageSchema":"hapi.usage.v1","inputTokenSemantics":"includes-cache","info":if info.is_object()||info.is_array(){info.clone()}else{json!({})}})
            }
            "model/safetyBuffering/updated" => {
                let Some(show) = field(p, "showBufferingUi show_buffering_ui").as_bool() else {
                    return vec![];
                };
                if show && text(p, "model").is_none() {
                    return vec![];
                }
                let mut e = json!({"type":"model_safety_buffering"});
                if let Some(value) = text(p, "model") {
                    e["model"] = json!(value);
                }
                merge(
                    e,
                    &json!({"use_cases":strings(field(p,"useCases use_cases")),"reasons":strings(&p["reasons"]),"show_buffering_ui":show,"faster_model":text(p,"fasterModel faster_model")}),
                )
            }
            "model/rerouted" => {
                let (Some(from), Some(to), Some(reason)) = (
                    text(p, "fromModel from_model"),
                    text(p, "toModel to_model"),
                    text(p, "reason"),
                ) else {
                    return vec![];
                };
                json!({"type":"model_rerouted","from_model":from,"to_model":to,"reason":reason})
            }
            "model/verification" => {
                json!({"type":"model_verification","verifications":strings(&p["verifications"])})
            }
            "error" => {
                let Some(e) = error_event(p, false) else {
                    return vec![];
                };
                e
            }
            "rawResponseItem/completed" => {
                let Some(e) = self.raw(&p["item"]) else {
                    return vec![];
                };
                e
            }
            "item/agentMessage/delta" => {
                let (Some(id), Some(d)) = (id, text(p, "delta text message")) else {
                    return vec![];
                };
                if !self.messages.append(id, d) {
                    return vec![];
                }
                json!({"type":"agent_message_delta"})
            }
            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                let Some(d) = text(p, "delta text message") else {
                    return vec![];
                };
                if !self.reasoning.append(id.unwrap_or("reasoning"), d) {
                    return vec![];
                }
                json!({"type":"agent_reasoning_delta","delta":d})
            }
            "item/reasoning/summaryPartAdded" => {
                let index = field(p, "summaryIndex summary_index");
                if index.is_number()
                    && !self
                        .breaks
                        .insert((id.unwrap_or("reasoning").into(), index.to_string()))
                {
                    return vec![];
                }
                json!({"type":"agent_reasoning_section_break"})
            }
            "item/commandExecution/outputDelta" => {
                if let (Some(id), Some(d)) = (id, text(p, "delta text output stdout")) {
                    self.commands.append(id, d);
                }
                return vec![];
            }
            _ => {
                debug_unhandled(method, p);
                return vec![];
            }
        };
        event = merge(sc, &event);
        vec![event]
    }
    fn raw(&mut self, i: &Value) -> Option<Value> {
        let id = text(i, "call_id callId")?;
        match normalize(text(i, "type")?).as_str() {
            "functioncall" => {
                let name = tool_name(i, "name")?;
                let mut input = field_or(i, "arguments", &json!({})).clone();
                if let Some(s) = input.as_str() {
                    if let Ok(v) = serde_json::from_str(s) {
                        input = v;
                    }
                }
                let v2 = match name {
                    "send_message" | "followup_task" | "interrupt_agent" | "list_agents" => true,
                    "spawn_agent" => text(&input, "task_name taskName").is_some(),
                    "wait_agent" => !input["targets"].is_array(),
                    _ => false,
                };
                if i["namespace"] == "multi_agent_v1" || !v2 || !self.raw_ids.insert(id.into()) {
                    return None;
                }
                self.raw_names.insert(id.into(), name.into());
                Some(json!({"type":"codex_tool_call_begin","call_id":id,"name":name,"input":input}))
            }
            "functioncalloutput" => {
                let name = self.raw_names.remove(id)?;
                let mut e = json!({"type":"codex_tool_call_end","call_id":id,"name":name});
                if let Some(v) = i.get("output") {
                    e["output"] = v.clone();
                }
                e["is_error"] = json!(false);
                Some(e)
            }
            _ => None,
        }
    }
    fn wrapped(&mut self, m: &Value) -> Vec<Value> {
        let Some(kind) = text(m, "type") else {
            return vec![];
        };
        let sc = scope(m);
        let events = match kind {
            "item_started" | "item_completed" => {
                let p = merge(
                    sc,
                    &json!({"item":m["item"],"itemId":field(m,"item_id itemId")}),
                );
                return self.notification(
                    if kind == "item_started" {
                        "item/started"
                    } else {
                        "item/completed"
                    },
                    &p,
                );
            }
            "task_started" | "task_complete" | "turn_aborted" | "task_failed" => {
                if kind != "task_started" && sc["turn_id"].is_null() {
                    return vec![];
                }
                let mut e = json!({"type":kind});
                if kind == "task_failed" {
                    if let Some(msg) =
                        text(m, "error message").or_else(|| text(&m["error"], "message"))
                    {
                        e["error"] = json!(msg);
                    }
                }
                vec![e]
            }
            "agent_message_delta"
            | "agent_message_content_delta"
            | "reasoning_content_delta"
            | "agent_reasoning_section_break"
            | "exec_command_output_delta" => {
                let (method, id) = match kind {
                    "reasoning_content_delta" => {
                        ("item/reasoning/summaryTextDelta", Some("reasoning"))
                    }
                    "agent_reasoning_section_break" => {
                        ("item/reasoning/summaryPartAdded", Some("reasoning"))
                    }
                    "exec_command_output_delta" => ("item/commandExecution/outputDelta", None),
                    _ => ("item/agentMessage/delta", Some("agent-message")),
                };
                let mut p = merge(sc, m);
                p["itemId"] = json!(text(m, "call_id callId item_id itemId id").or(id));
                return self.notification(method, &p);
            }
            "error" => error_event(m, true).into_iter().collect(),
            "plan_update" => plan(m),
            "context_compacted" => return compact(&sc, false),
            "agent_reasoning_delta"
            | "agent_reasoning"
            | "agent_message"
            | "mcp_startup_update"
            | "mcp_startup_complete"
            | "skills_update_available"
            | "stream_error"
            | "warning"
            | "terminal_interaction"
            | "user_message" => vec![],
            _ => vec![m.clone()],
        };
        events.into_iter().map(|e| merge(sc.clone(), &e)).collect()
    }
    fn item(&mut self, start: bool, p: &Value, id: Option<&str>, sc: &Value) -> Vec<Value> {
        let i = p.get("item").filter(|v| v.is_object()).unwrap_or(p);
        let Some(id) = id.or_else(|| text(i, "id itemId item_id")) else {
            return vec![];
        };
        let kind = normalize(text(i, "type itemType kind").unwrap_or(""));
        let mut e = match kind.as_str() {
            "contextcompaction" => {
                if !start {
                    return compact(sc, true);
                }
                if !self
                    .compacting
                    .insert((text(sc, "thread_id").unwrap_or("").into(), id.into()))
                {
                    return vec![];
                }
                json!({"type":"compaction_started","compaction_id":id})
            }
            "agentmessage" | "reasoning" => {
                let key = (kind.clone(), id.into());
                if start || self.completed.contains(&key) {
                    return vec![];
                }
                let reasoning = kind == "reasoning";
                let mut t = item_text(i);
                if reasoning && t.is_none() {
                    let summary = strings(field(i, "summary_text summaryText"));
                    if !summary.is_empty() {
                        t = Some(summary.join("\n"));
                    }
                }
                let buffered = if reasoning {
                    self.reasoning.take(id)
                } else {
                    self.messages.take(id)
                };
                let Some(s) = t.or(buffered) else {
                    return vec![];
                };
                self.completed.insert(key);
                if reasoning {
                    json!({"type":"agent_reasoning","text":s})
                } else {
                    let msg = assistant_text(&s, &i["phase"]);
                    if msg.is_empty() {
                        return vec![];
                    }
                    json!({"type":"agent_message","message":msg})
                }
            }
            "plan" => {
                let Some(plan) = text(i, "text").filter(|s| !s.trim().is_empty() && !start) else {
                    return vec![];
                };
                json!({"type":"proposed_plan","plan":plan})
            }
            "commandexecution" => {
                let mut meta = if start {
                    json!({})
                } else {
                    self.command_meta.remove(id).unwrap_or(json!({}))
                };
                let cmd = field(i, "command cmd args");
                let cmd = cmd.as_str().map(str::to_string).or_else(|| {
                    cmd.as_array().map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                });
                if let Some(cmd) = cmd.filter(|s| !s.is_empty()) {
                    meta["command"] = json!(cmd);
                }
                if let Some(value) = text(i, "cwd workingDirectory working_directory") {
                    meta["cwd"] = json!(value);
                }
                if start {
                    if let Some(value) = field(i, "autoApproved auto_approved").as_bool() {
                        meta["auto_approved"] = json!(value);
                    }
                    if let Some(value) = i["commandActions"]
                        .as_array()
                        .or_else(|| i["command_actions"].as_array())
                    {
                        meta["command_actions"] = json!(value);
                    }
                    if let Some(value) = text(i, "source") {
                        meta["command_source"] = json!(value);
                    }
                    if let Some(value) = field(i, "startedAtMs started_at_ms").as_number() {
                        meta["started_at_ms"] = json!(value);
                    }
                    self.command_meta.insert(id.into(), meta.clone());
                } else {
                    let buf = self.commands.take(id);
                    if let Some(out) =
                        text(i, "aggregatedOutput aggregated_output output result stdout")
                            .or(buf.as_deref())
                    {
                        meta["output"] = json!(out);
                    }
                    if let Some(value) = text(i, "stderr") {
                        meta["stderr"] = json!(value);
                    }
                    if let Some(value) = text(i, "error") {
                        meta["error"] = json!(value);
                    }
                    if let Some(value) = field(i, "exitCode exit_code exitcode").as_number() {
                        meta["exit_code"] = json!(value);
                    }
                    if let Some(value) = field(i, "durationMs duration_ms").as_number() {
                        meta["duration_ms"] = json!(value);
                    }
                    if let Some(value) = field(i, "completedAtMs completed_at_ms").as_number() {
                        meta["completed_at_ms"] = json!(value);
                    }
                    if let Some(value) = text(i, "status") {
                        meta["status"] = json!(value);
                    }
                }
                merge(
                    json!({"type":if start{"exec_command_begin"}else{"exec_command_end"},"call_id":id}),
                    &meta,
                )
            }
            "mcptoolcall" => {
                let server = text(i, "server serverName server_name");
                let tool = text(i, "tool toolName tool_name name");
                let mut e = json!({"type":if start{"mcp_tool_call_begin"}else{"mcp_tool_call_end"},"call_id":id,"server":server,"tool":tool});
                if start {
                    if let Some(value) = field(i, "startedAtMs started_at_ms").as_number() {
                        e["started_at_ms"] = json!(value);
                    }
                    let input = field(i, "arguments input");
                    e["invocation"] = json!({"server":server,"tool":tool,"arguments":if input.is_null(){json!({})}else{input.clone()} });
                } else {
                    if truthy(&i["error"]) {
                        e["result"] = json!({"Err":i["error"]});
                    } else if let Some(r) = i.get("result") {
                        e["result"] = r.clone();
                    }
                    if let Some(value) = field(i, "durationMs duration_ms").as_number() {
                        e["duration_ms"] = json!(value);
                    }
                    if let Some(value) = field(i, "completedAtMs completed_at_ms").as_number() {
                        e["completed_at_ms"] = json!(value);
                    }
                }
                e
            }
            "imagegeneration" => {
                let Some(path) = text(
                    i,
                    "savedPath saved_path path filePath file_path outputPath output_path",
                )
                .filter(|_| !start) else {
                    return vec![];
                };
                let name = text(i, "fileName file_name filename name")
                    .or_else(|| path.rsplit(['/', '\\']).find(|s| !s.is_empty()))
                    .unwrap_or("generated-image.png");
                let mut e = json!({"type":"generated_image","image_id":id,"saved_path":path,"file_name":name});
                if let Some(value) = text(i, "mimeType mime_type mediaType media_type") {
                    e["mime_type"] = json!(value);
                }
                e
            }
            "imageview" => {
                let Some(raw) = text(i, "path").filter(|_| !start) else {
                    return vec![];
                };
                let Some(path) = view_path(raw) else {
                    return vec![];
                };
                json!({"type":"viewed_image","call_id":id,"path":path})
            }
            "collabagenttoolcall" => {
                if self.raw_ids.contains(id) {
                    return vec![];
                }
                let Some(name) = tool_name(i, "tool name").filter(|n| {
                    matches!(
                        *n,
                        "spawn_agent"
                            | "send_input"
                            | "resume_agent"
                            | "wait_agent"
                            | "close_agent"
                    )
                }) else {
                    return vec![];
                };
                let mut e = json!({"type":if start{"codex_tool_call_begin"}else{"codex_tool_call_end"},"call_id":id,"name":name});
                if start {
                    if let Some(value) = field(i, "startedAtMs started_at_ms").as_number() {
                        e["started_at_ms"] = json!(value);
                    }
                    e["input"] = collab(i, name, true);
                } else {
                    if let Some(value) = field(i, "completedAtMs completed_at_ms").as_number() {
                        e["completed_at_ms"] = json!(value);
                    }
                    e["output"] = collab(i, name, false);
                    e["is_error"] = json!(matches!(text(i, "status"), Some("failed" | "error")));
                }
                e
            }
            "filechange" => {
                let mut meta = if start {
                    json!({})
                } else {
                    self.patch_meta.remove(id).unwrap_or(json!({}))
                };
                if start {
                    if let Some(value) = Some(field(i, "changes change diff"))
                        .filter(|v| v.is_object() || v.is_array())
                    {
                        meta["changes"] = json!(value);
                    }
                    if let Some(value) = field(i, "autoApproved auto_approved").as_bool() {
                        meta["auto_approved"] = json!(value);
                    }
                    if let Some(value) = field(i, "startedAtMs started_at_ms").as_number() {
                        meta["started_at_ms"] = json!(value);
                    }
                    self.patch_meta.insert(id.into(), meta.clone());
                } else {
                    if let Some(value) = text(i, "stdout output") {
                        meta["stdout"] = json!(value);
                    }
                    if let Some(value) = text(i, "stderr") {
                        meta["stderr"] = json!(value);
                    }
                    if let Some(value) = field(i, "completedAtMs completed_at_ms").as_number() {
                        meta["completed_at_ms"] = json!(value);
                    }
                    meta["success"] = json!(field_or(
                        i,
                        "success ok applied",
                        &json!(i["status"] == "completed")
                    )
                    .as_bool()
                    .unwrap_or(false));
                }
                merge(
                    json!({"type":if start{"patch_apply_begin"}else{"patch_apply_end"},"call_id":id}),
                    &meta,
                )
            }
            _ => {
                debug_unhandled(
                    if start {
                        "item/started"
                    } else {
                        "item/completed"
                    },
                    p,
                );
                return vec![];
            }
        };
        e = merge(sc.clone(), &e);
        vec![e]
    }
}
fn error_event(p: &Value, wrapped: bool) -> Option<Value> {
    let err = &p["error"];
    let retry = field_or(
        p,
        "will_retry willRetry",
        field(err, "will_retry willRetry"),
    )
    .as_bool();
    if retry == Some(true) {
        return None;
    }
    let msg = text(p, if wrapped { "message reason" } else { "message" })
        .or_else(|| text(err, "message"))?;
    let mut e = json!({"type":"task_failed"});
    if !wrapped {
        e["terminal_source"] = json!("error");
    }
    if let Some(b) = retry {
        e["retryable"] = json!(b);
    }
    if let Some(s) = text(p, "codexErrorInfo codex_error_info")
        .or_else(|| text(err, "codexErrorInfo codex_error_info"))
    {
        e["codex_error_info"] = json!(s);
    }
    e["error"] = json!(msg);
    Some(e)
}
fn compact(sc: &Value, await_turn: bool) -> Vec<Value> {
    if sc["thread_id"].is_null() {
        return vec![];
    }
    let mut e = merge(json!({"type":"thread_compacted"}), sc);
    if await_turn {
        e["await_turn_completion"] = json!(true);
    }
    vec![e, merge(sc.clone(), &json!({"type":"context_compacted"}))]
}
fn view_path(raw: &str) -> Option<String> {
    let Some(url) = raw.strip_prefix("file://") else {
        return Some(raw.into());
    };
    let (host, path) = url.split_once('/')?;
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let path = path.split(['?', '#']).next()?;
    let mut bytes = vec![b'/'];
    let mut chars = path.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let pair = [chars.next()?, chars.next()?];
            let byte = u8::from_str_radix(std::str::from_utf8(&pair).ok()?, 16).ok()?;
            if byte == b'/' {
                return None;
            }
            bytes.push(byte);
        } else {
            bytes.push(b)
        }
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
#[test]
fn typescript_event_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vectors");
    let out = std::process::Command::new("bun")
        .args(["test", "src/flavors/codex/event_vectors.test.ts"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CODEX_EVENT_VECTORS", &path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cases: Vec<Vec<Value>> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(cases.len() > 30);
    for case in cases {
        let mut converter = Converter::default();
        for call in case {
            if call["reset"] == true {
                converter.reset();
                continue;
            }
            let result = converter.notification(call["method"].as_str().unwrap(), &call["params"]);
            let hashes: Vec<_> = result
                .iter()
                .map(|v| super::projection::hash(&hapi_wrapper_core::jsjson::stringify(v)))
                .collect();
            assert_eq!(
                json!(hashes),
                call["hashes"],
                "event encoding: {} {}",
                call["method"],
                json!(result)
            );
            assert_eq!(
                json!(result),
                call["result"],
                "{} {}",
                call["method"],
                call["params"]
            );
        }
    }
}

fn record_quota_update() {
    let configured = std::env::var("HAPI_HOME").ok().filter(|s| !s.is_empty());
    if configured.is_none() && std::env::var("VITEST").as_deref() == Ok("true") {
        return;
    }
    let Some(home) = std::env::home_dir() else {
        return;
    };
    let dir = configured
        .map(|s| {
            if let Some(path) = s.strip_prefix('~') {
                std::path::PathBuf::from(format!("{}{path}", home.display()))
            } else {
                s.into()
            }
        })
        .unwrap_or_else(|| home.join(".hapi"))
        .join("agent-quotas-live");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let temp = dir.join(format!("codex.json.{}.tmp", uuid::Uuid::new_v4()));
    if std::fs::write(
        &temp,
        json!({"updatedAt":hapi_wrapper_core::session::metadata::now()}).to_string(),
    )
    .and_then(|_| std::fs::rename(&temp, dir.join("codex.json")))
    .is_err()
    {
        let _ = std::fs::remove_file(temp);
    }
}

fn sanitized(v: &Value, depth: usize) -> Value {
    match v {
        Value::String(s) if s.encode_utf16().count() > 512 => {
            let prefix = String::from_utf16_lossy(&s.encode_utf16().take(512).collect::<Vec<_>>());
            json!(format!(
                "{prefix}... [truncated {} chars for logs]",
                s.encode_utf16().count() - 512
            ))
        }
        Value::Array(a) => {
            let mut rows = a
                .iter()
                .take(20)
                .map(|v| sanitized(v, depth + 1))
                .collect::<Vec<_>>();
            if a.len() > 20 {
                rows.push(json!(format!(
                    "... [truncated {} array items for logs]",
                    a.len() - 20
                )));
            }
            json!(rows)
        }
        Value::Object(_) if depth >= 8 => json!("[truncated nested object for logs]"),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(key, v)| (key.clone(), sanitized(v, depth + 1)))
                .collect(),
        ),
        _ => v.clone(),
    }
}
fn debug_unhandled(method: &str, p: &Value) {
    if std::env::var_os("DEBUG").is_some_and(|v| !v.is_empty()) {
        eprintln!(
            "[AppServerEventConverter] Unhandled notification {}",
            json!({"method":method,"params":sanitized(p,1)})
        );
    }
}
#[cfg(test)]
#[test]
fn bounds_unhandled_notification_logs() {
    let v = json!({"item":{"savedPath":"/tmp/image.png","result":"a".repeat(4096)}});
    let logged = sanitized(&v, 1);
    assert!(logged["item"]["result"]
        .as_str()
        .unwrap()
        .ends_with("[truncated 3584 chars for logs]"));
    assert_eq!(logged["item"]["savedPath"], v["item"]["savedPath"]);
}
