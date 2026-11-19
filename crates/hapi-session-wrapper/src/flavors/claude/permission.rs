use hapi_wrapper_core::{
    permission::base::{BasePermissionHandler, Completion, Pending, PermissionHooks, MISSING},
    rpc::manager::RpcHandlerManager,
    session::client::SessionClient,
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

pub const PLAN_FAKE_REJECT:&str="User approved plan, but you need to be restarted. STOP IMMEDIATELY TO SWITCH FROM PLAN MODE. DO NOT REPLY TO THIS MESSAGE.";
pub const PLAN_FAKE_RESTART: &str = "PlEaZe Continue with plan.";
const STEER: &str =
    "The user sent a new message instead of answering. Read it and continue from there.";
const REJECT:&str="The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";
pub(super) fn valid_mode(mode: &str) -> bool {
    matches!(
        mode,
        "default" | "acceptEdits" | "bypassPermissions" | "plan" | "auto"
    )
}
pub fn native_mode(mode: &str) -> &str {
    match mode {
        "acceptEdits" | "bypassPermissions" => "default",
        s => s,
    }
}
fn question(tool: &str) -> bool {
    matches!(
        tool,
        "AskUserQuestion" | "ask_user_question" | "request_user_input"
    )
}
fn exit_plan(tool: &str) -> bool {
    matches!(tool, "ExitPlanMode" | "exit_plan_mode")
}
fn allow(input: Value) -> Value {
    json!({"behavior":"allow","updatedInput":input})
}
fn deny(reason: &str) -> Value {
    json!({"behavior":"deny","message":reason})
}

struct Call {
    id: String,
    name: String,
    input: Value,
    used: bool,
}
#[derive(Default)]
struct State {
    calls: Vec<Call>,
    allowed: HashSet<String>,
    literals: HashSet<String>,
    prefixes: HashSet<String>,
    steered: HashSet<String>,
    responses: serde_json::Map<String, Value>,
}
impl State {
    fn remember(&mut self, tool: &str) {
        if question(tool) || tool == "Bash" {
            return;
        }
        if let Some(body) = tool.strip_prefix("Bash(") {
            if let Some(pattern) = body
                .strip_suffix(')')
                .filter(|s| !s.is_empty() && !s.contains(['\n', '\r', '\u{2028}', '\u{2029}']))
            {
                if let Some(prefix) = pattern.strip_suffix(":*") {
                    if !prefix.is_empty() {
                        self.prefixes.insert(prefix.into());
                    }
                } else {
                    self.literals.insert(pattern.into());
                }
            }
        } else {
            self.allowed.insert(tool.into());
        }
    }
    fn allowed(&self, tool: &str, input: &Value) -> bool {
        if tool == "Bash" {
            return input["command"].as_str().is_some_and(|cmd| {
                !cmd.is_empty()
                    && (self.literals.contains(cmd)
                        || self.prefixes.iter().any(|p| cmd.starts_with(p)))
            });
        }
        self.allowed.contains(tool)
    }
    fn resolve(&mut self, tool: &str, input: &Value) -> Option<String> {
        let call = self
            .calls
            .iter_mut()
            .rev()
            .find(|c| c.name == tool && &c.input == input)?;
        if call.used {
            return None;
        }
        call.used = true;
        Some(call.id.clone())
    }
}
pub type Unshift = Box<dyn Fn(&str, &str) + Send + Sync>;
pub struct SessionActions {
    pub get_mode: Box<dyn Fn() -> String + Send + Sync>,
    pub set_mode: Box<dyn Fn(&str) + Send + Sync>,
    pub unshift: Unshift,
}
pub struct PermissionHandler {
    base: Arc<BasePermissionHandler<Value>>,
    state: Mutex<State>,
    actions: SessionActions,
}
struct CancelOnDrop<'a> {
    base: &'a BasePermissionHandler<Value>,
    id: String,
}
impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        let reason = "Permission request aborted";
        self.base.finish_pending(
            &self.id,
            Err(reason.into()),
            Completion {
                status: "canceled".into(),
                reason: Some(reason.into()),
                ..Default::default()
            },
        );
    }
}

impl PermissionHandler {
    pub fn for_session(
        session: &Arc<SessionClient>,
        actions: SessionActions,
        on_registered: impl Fn(&str) + Send + Sync + 'static,
    ) -> Arc<Self> {
        let hooks = PermissionHooks {
            on_registered: Some(Box::new(move |id, _, _| on_registered(id))),
            ..Default::default()
        };
        Self::register(
            BasePermissionHandler::for_session(session, hooks),
            &session.rpc,
            actions,
        )
    }
    fn register(
        base: Arc<BasePermissionHandler<Value>>,
        rpc: &RpcHandlerManager,
        actions: SessionActions,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            base: base.clone(),
            actions,
            state: Mutex::new(State::default()),
        });
        let weak = Arc::downgrade(&this);
        base.register_rpc(
            rpc,
            move |r, p| {
                let handler = weak.upgrade();
                Box::pin(async move { Ok(handler.ok_or(MISSING)?.answer(r, p)) })
            },
            |_| Err(MISSING.into()),
        );
        this
    }
    pub fn on_message(&self, v: &Value) {
        let mut state = self.state.lock().unwrap();
        for b in v["message"]["content"].as_array().into_iter().flatten() {
            if v["type"] == "assistant" && b["type"] == "tool_use" {
                if let (Some(id), Some(name)) = (b["id"].as_str(), b["name"].as_str()) {
                    state.calls.push(Call {
                        id: id.into(),
                        name: name.into(),
                        input: b["input"].clone(),
                        used: false,
                    });
                }
            } else if v["type"] == "user" && b["type"] == "tool_result" {
                if let Some(call) = state
                    .calls
                    .iter_mut()
                    .find(|c| Some(c.id.as_str()) == b["tool_use_id"].as_str())
                {
                    call.used = true;
                }
            }
        }
    }
    pub async fn can_use_tool(&self, tool: &str, input: Value) -> Result<Value, String> {
        let mode = (self.actions.get_mode)();
        if !question(tool) {
            if self.state.lock().unwrap().allowed(tool, &input) {
                return Ok(allow(input));
            }
            if mode == "bypassPermissions" {
                if exit_plan(tool) {
                    (self.actions.unshift)(PLAN_FAKE_RESTART, &mode);
                    return Ok(deny(PLAN_FAKE_REJECT));
                }
                return Ok(allow(input));
            }
            if mode == "acceptEdits"
                && matches!(tool, "Edit" | "MultiEdit" | "Write" | "NotebookEdit")
            {
                return Ok(allow(input));
            }
        }
        let first = self.state.lock().unwrap().resolve(tool, &input);
        let id = if let Some(id) = first {
            id
        } else {
            tokio::time::sleep(Duration::from_secs(1)).await;
            self.state
                .lock()
                .unwrap()
                .resolve(tool, &input)
                .ok_or_else(|| format!("Could not resolve tool call ID for {tool}"))?
        };
        let rx = self.base.add(id.clone(), tool.into(), input, None);
        let _guard = CancelOnDrop {
            base: &self.base,
            id,
        };
        rx.await.map_err(|_| "Permission request aborted")?
    }
    fn answer(&self, r: Value, p: Pending<Value>) -> Completion {
        let approved = r["approved"] == true;
        let reason = r["reason"].as_str();
        let mode = r["mode"].as_str().filter(|m| valid_mode(m));
        let mut done = Completion {
            status: if approved { "approved" } else { "denied" }.into(),
            reason: reason.map(str::to_owned),
            mode: mode.map(str::to_owned),
            allow_tools: r["allowTools"].as_array().map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
            answers: r.get("answers").cloned(),
            ..Default::default()
        };
        {
            let mut state = self.state.lock().unwrap();
            let mut response = r.clone();
            if mode.is_none() {
                response.as_object_mut().unwrap().remove("mode");
            }
            response["receivedAt"] = json!(hapi_wrapper_core::session::metadata::now());
            state
                .responses
                .insert(r["id"].as_str().unwrap_or("").into(), response);
            for tool in done.allow_tools.iter().flatten() {
                state.remember(tool);
            }
        }
        if let Some(mode) = mode {
            (self.actions.set_mode)(mode);
        }
        let result = if question(&p.tool) {
            let answers = &r["answers"];
            if !answers.as_object().is_some_and(|o| !o.is_empty()) {
                done.status = "denied".into();
                done.reason
                    .get_or_insert_with(|| "No answers were provided.".into());
                deny("No answers were provided.")
            } else {
                allow(question_input(&p.tool, p.input, answers))
            }
        } else if exit_plan(&p.tool) {
            if approved {
                let mode = mode
                    .filter(|s| matches!(*s, "auto" | "acceptEdits" | "bypassPermissions"))
                    .unwrap_or("default");
                (self.actions.unshift)(PLAN_FAKE_RESTART, mode);
                deny(PLAN_FAKE_REJECT)
            } else {
                deny(reason.filter(|s| !s.is_empty()).unwrap_or("Plan rejected"))
            }
        } else if approved {
            allow(if p.input.is_null() {
                json!({})
            } else {
                p.input
            })
        } else {
            deny(reason.filter(|s| !s.is_empty()).unwrap_or(REJECT))
        };
        let _ = p.reply.send(Ok(result));
        done
    }
    pub fn deny_pending_for_steer(&self) {
        let mut state = self.state.lock().unwrap();
        for id in self.base.pending_ids() {
            if self.base.finish_pending(
                &id,
                Ok(deny(STEER)),
                Completion {
                    status: "denied".into(),
                    reason: Some(STEER.into()),
                    ..Default::default()
                },
            ) {
                state.steered.insert(id);
            }
        }
    }
    pub fn is_aborted(&self, id: &str) -> bool {
        let state = self.state.lock().unwrap();
        !state.steered.contains(id)
            && (state
                .responses
                .get(id)
                .is_some_and(|v| v["approved"] == false)
                || state
                    .calls
                    .iter()
                    .find(|c| c.id == id)
                    .is_some_and(|c| exit_plan(&c.name)))
    }
    pub fn response(&self, id: &str) -> Option<Value> {
        self.state.lock().unwrap().responses.get(id).cloned()
    }
    pub fn reset(&self) {
        *self.state.lock().unwrap() = State::default();
        self.base.cancel(
            "Session switched to local mode".into(),
            "Session reset".into(),
            None,
            None,
        );
    }
}
fn question_input(tool: &str, input: Value, answers: &Value) -> Value {
    let mut input = if input.is_object() { input } else { json!({}) };
    if tool == "request_user_input" {
        input["answers"] = answers.clone();
        return input;
    }
    let mut shaped = json!({});
    for (index, q) in input["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(text) = q["question"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let answer = &answers[index.to_string()];
        let selections = answer.as_array().or_else(|| answer["answers"].as_array());
        if let Some(list) = selections.filter(|v| !v.is_empty()) {
            shaped[text] = json!(list
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(","));
        }
    }
    input["answers"] = shaped;
    input
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Session {
        handler: Arc<PermissionHandler>,
        mode: Arc<Mutex<String>>,
        restarts: Arc<Mutex<Vec<(String, String)>>>,
        state: Arc<Mutex<Value>>,
        rpc: RpcHandlerManager,
    }
    impl Session {
        fn new() -> Self {
            let mode = Arc::new(Mutex::new("default".to_owned()));
            let restarts = Arc::new(Mutex::new(Vec::new()));
            let state = Arc::new(Mutex::new(json!({})));
            let (get, set, queue, update) =
                (mode.clone(), mode.clone(), restarts.clone(), state.clone());
            let base = Arc::new(BasePermissionHandler::new(
                Arc::new(move |change| {
                    let mut s = update.lock().unwrap();
                    *s = change(&s);
                }),
                PermissionHooks::default(),
            ));
            let rpc = RpcHandlerManager::new("s");
            let handler = PermissionHandler::register(
                base,
                &rpc,
                SessionActions {
                    get_mode: Box::new(move || get.lock().unwrap().clone()),
                    set_mode: Box::new(move |v| *set.lock().unwrap() = v.into()),
                    unshift: Box::new(move |text, mode| {
                        queue.lock().unwrap().push((text.into(), mode.into()))
                    }),
                },
            );
            Self {
                handler,
                mode,
                restarts,
                state,
                rpc,
            }
        }
        async fn pending(
            &self,
            id: &str,
            tool: &str,
            input: Value,
        ) -> tokio::task::JoinHandle<Result<Value, String>> {
            self.handler.on_message(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":tool,"id":id,"input":input}]}}));
            let h = self.handler.clone();
            let name = tool.to_owned();
            let task = tokio::spawn(async move { h.can_use_tool(&name, input).await });
            tokio::task::yield_now().await;
            assert!(self.state.lock().unwrap()["requests"].get(id).is_some());
            task
        }
        async fn answer(&self, v: Value) -> Option<String> {
            self.rpc
                .handle_request(&json!({"method":"s:permission","params":v.to_string()}))
                .await
        }
    }
    #[tokio::test]
    async fn live_modes_and_plan_restart() {
        let s = Session::new();
        for mode in ["default", "plan", "auto"] {
            assert_eq!(native_mode(mode), mode);
        }
        for mode in ["acceptEdits", "bypassPermissions"] {
            assert_eq!(native_mode(mode), "default");
        }
        *s.mode.lock().unwrap() = "bypassPermissions".into();
        for tool in ["ExitPlanMode", "exit_plan_mode"] {
            assert_eq!(
                s.handler.can_use_tool(tool, json!({})).await.unwrap(),
                deny(PLAN_FAKE_REJECT)
            );
        }
        assert_eq!(
            s.restarts.lock().unwrap().as_slice(),
            [
                (PLAN_FAKE_RESTART.into(), "bypassPermissions".into()),
                (PLAN_FAKE_RESTART.into(), "bypassPermissions".into())
            ]
        );
        assert_eq!(
            s.handler.can_use_tool("Bash", json!({})).await.unwrap(),
            allow(json!({}))
        );
        *s.mode.lock().unwrap() = "acceptEdits".into();
        assert_eq!(
            s.handler.can_use_tool("Edit", json!({})).await.unwrap(),
            allow(json!({}))
        );
    }
    #[tokio::test]
    async fn cancellation_orphan_and_steer() {
        let s = Session::new();
        let pending = s.pending("t", "AskUserQuestion", json!({})).await;
        pending.abort();
        assert!(pending.await.is_err());
        assert_eq!(
            s.state.lock().unwrap()["completedRequests"]["t"]["status"],
            "canceled"
        );
        s.state.lock().unwrap()["requests"]["orphan"] = json!({"tool":"AskUserQuestion"});
        for id in ["t", "orphan"] {
            assert_eq!(
                s.answer(json!({"id":id})).await.unwrap(),
                json!({"error":MISSING}).to_string()
            );
        }
        assert_eq!(
            s.state.lock().unwrap()["completedRequests"]["orphan"]["decision"],
            "abort"
        );
        for tool in ["AskUserQuestion", "ExitPlanMode"] {
            let pending = s.pending(tool, tool, json!({})).await;
            s.handler.deny_pending_for_steer();
            assert_eq!(pending.await.unwrap().unwrap(), deny(STEER));
            assert!(!s.handler.is_aborted(tool));
            assert_eq!(
                s.state.lock().unwrap()["completedRequests"][tool]["status"],
                "denied"
            );
        }
    }
    #[tokio::test]
    async fn answers_and_session_grants() {
        let s = Session::new();
        let pending = s.pending("plan", "ExitPlanMode", json!({})).await;
        s.answer(json!({"id":"plan","approved":true,"mode":"auto","allowTools":["Bash","Bash(:*)","Bash(pwd)","Bash(git :*)","Read","AskUserQuestion"]})).await;
        assert_eq!(pending.await.unwrap().unwrap(), deny(PLAN_FAKE_REJECT));
        assert!(s.handler.is_aborted("plan"));
        assert_eq!(s.restarts.lock().unwrap()[0].1, "auto");
        for cmd in ["pwd", "git status"] {
            let input = json!({"command":cmd});
            assert_eq!(
                s.handler.can_use_tool("Bash", input.clone()).await.unwrap(),
                allow(input)
            );
        }
        assert!(!s
            .handler
            .state
            .lock()
            .unwrap()
            .allowed("Bash", &json!({"command":"pwdx"})));
        assert_eq!(
            s.handler.can_use_tool("Read", json!({})).await.unwrap(),
            allow(json!({}))
        );
        *s.mode.lock().unwrap() = "bypassPermissions".into();
        for tool in ["AskUserQuestion", "ask_user_question", "request_user_input"] {
            let input = json!({"questions":[{"question":"Color?"},{"question":"Fruit?"}]});
            let pending = s.pending(tool, tool, input).await;
            let answers = json!({"0":["red","pink"],"1":{"answers":["pear"]}});
            s.answer(json!({"id":tool,"approved":true,"answers":answers}))
                .await;
            let result = pending.await.unwrap().unwrap();
            assert_eq!(
                result["updatedInput"]["answers"],
                if tool == "request_user_input" {
                    answers
                } else {
                    json!({"Color?":"red,pink","Fruit?":"pear"})
                }
            );
        }
        let pending = s.pending("empty", "AskUserQuestion", json!({})).await;
        s.answer(json!({"id":"empty","approved":true})).await;
        assert_eq!(
            pending.await.unwrap().unwrap(),
            deny("No answers were provided.")
        );
    }
    #[tokio::test]
    async fn invalid_answer_modes_are_ignored() {
        let s = Session::new();
        for mode in ["", "unknown"] {
            let pending = s.pending("id", "Read", json!({})).await;
            s.answer(json!({"id":"id","mode":mode,"approved":false}))
                .await;
            assert_eq!(pending.await.unwrap().unwrap(), deny(REJECT));
            assert_eq!(*s.mode.lock().unwrap(), "default");
            assert!(s.state.lock().unwrap()["completedRequests"]["id"]["mode"].is_null());
            assert!(s.handler.state.lock().unwrap().responses["id"]
                .get("mode")
                .is_none());
        }
    }
    #[tokio::test(start_paused = true)]
    async fn late_tool_id_and_reset() {
        let s = Session::new();
        let h = s.handler.clone();
        let pending = tokio::spawn(async move { h.can_use_tool("ExitPlanMode", json!({})).await });
        tokio::task::yield_now().await;
        s.handler.on_message(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"late","name":"ExitPlanMode","input":{}}]}}));
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        s.answer(json!({"id":"late","approved":false,"allowTools":["Read"]}))
            .await;
        assert_eq!(pending.await.unwrap().unwrap(), deny("Plan rejected"));
        assert!(s.handler.is_aborted("late"));
        s.handler.reset();
        assert!(!s.handler.is_aborted("late"));
        assert!(!s
            .handler
            .state
            .lock()
            .unwrap()
            .allowed("Read", &Value::Null));
    }
}
