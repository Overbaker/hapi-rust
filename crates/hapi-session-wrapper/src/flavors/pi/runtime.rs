#[path = "special.rs"]
mod special;
use super::{events::Messages, history::History, prompt, transport::Transport, ui::Dialogs};
use hapi_wrapper_core::{
    rpc::handlers::GeneratedImages,
    session::client::{Cancellation, SessionClient},
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

enum Input {
    User(Value, Option<String>),
    Cancel(String, oneshot::Sender<Cancellation>),
    Rpc(&'static str, Value, oneshot::Sender<Result<Value, String>>),
}
struct Queued {
    command: Value,
    local: Option<String>,
}
type RpcReply = oneshot::Sender<Result<Value, String>>;
struct AbortGuard {
    waiters: Vec<RpcReply>,
    deadline: Instant,
    compensate: bool,
}
struct Active {
    original_local: Option<String>,
    id: String,
    local: Option<String>,
    accepted: bool,
    started: bool,
    ended: bool,
    settled: bool,
    deadline: Option<Instant>,
}
pub struct Runtime {
    client: Arc<SessionClient>,
    transport: Transport,
    events: mpsc::UnboundedReceiver<Value>,
    input: mpsc::UnboundedReceiver<Input>,
    messages: Messages,
    history: History,
    dialogs: Dialogs,
    queue: VecDeque<Queued>,
    active: Option<Active>,
    maintenance: super::maintenance::Maintenance,
    abort: Option<AbortGuard>,
    streaming: bool,
    compact: Option<tokio::task::JoinHandle<Result<Value, String>>>,
    runtime: Value,
    models: Vec<Value>,
    commands: Value,
    provider: Value,
    mode: String,
}
impl Runtime {
    pub fn new(
        c: Arc<SessionClient>,
        transport: Transport,
        events: mpsc::UnboundedReceiver<Value>,
        images: GeneratedImages,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let user = tx.clone();
        c.on_user_message(Arc::new(move |v, id| {
            let _ = user.send(Input::User(v, id));
        }));
        let cancel = tx.clone();
        c.on_cancel(Arc::new(move |id| {
            let (reply, recv) = oneshot::channel();
            let _ = cancel.send(Input::Cancel(id, reply));
            Box::pin(async move { Ok(recv.await.unwrap_or(Cancellation::Missing)) })
        }));
        for method in [
            "abort",
            "switch",
            "permission",
            "steer-queued-message",
            "set-session-config",
            "fork-conversation",
            "rewind-conversation",
            "listPiModels",
            "listSlashCommands",
            "listSkills",
        ] {
            let tx = tx.clone();
            c.rpc.register(method, move |v| {
                let (txr, rx) = oneshot::channel();
                let sent = tx.send(Input::Rpc(method, v, txr)).is_ok();
                async move {
                    if !sent {
                        return Err("Pi closed".into());
                    }
                    rx.await.map_err(|_| "Pi closed")?.map(Some)
                }
            });
        }
        Self {
            client: c,
            transport,
            events,
            input: rx,
            messages: Messages::new(images),
            history: Default::default(),
            dialogs: Default::default(),
            queue: Default::default(),
            active: None,
            maintenance: Default::default(),
            abort: None,
            streaming: false,
            compact: None,
            runtime: json!({}),
            models: Vec::new(),
            commands: Value::Null,
            provider: Value::Null,
            mode: "remote".into(),
        }
    }
    fn alive(&self) {
        self.client
            .keep_alive(self.streaming, Some(&self.mode), &self.runtime)
    }
    fn notice(&self, text: &str) {
        self.client
            .send_session_event(json!({"type":"message","message":text}), None, None)
    }
    fn native_title(&self, value: &Value) {
        let Some(title) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            return;
        };
        if title.eq_ignore_ascii_case("Untitled")
            || title.eq_ignore_ascii_case("New Session")
            || regex_lite::Regex::new(r"(?i)^(?:new|child) session - [0-9]{4}-[0-9]{2}-[0-9]{2}T")
                .unwrap()
                .is_match(title)
        {
            return;
        }
        let title = title.to_owned();
        let ts = hapi_wrapper_core::session::metadata::now();
        self.client.metadata.update(move |m| {
            let mut m = m.clone();
            if m["name"] != title && m["summary"]["text"] != title {
                m["summary"] = json!({"text":title,"updatedAt":ts})
            }
            m
        });
    }
    fn selected(&self) {
        if self.provider.is_string() && self.runtime["model"].is_string() {
            let selected = json!({"provider":self.provider,"modelId":self.runtime["model"]});
            self.client.metadata.update(move |m| {
                let mut m = m.clone();
                m["piSelectedModel"] = selected.clone();
                m
            });
        }
    }
    pub async fn init(&mut self, opts: &Value) -> Result<(), String> {
        let state = self.transport.rpc(json!({"type":"get_state"})).await?;
        if !state.is_object()
            || ["sessionId", "sessionFile", "sessionName", "thinkingLevel"]
                .iter()
                .any(|k| state.get(*k).is_some_and(|v| !v.is_string()))
            || state.get("isStreaming").is_some_and(|v| !v.is_boolean())
            || state.get("model").is_some_and(|v| {
                !v.is_object()
                    || ["id", "provider", "modelId"]
                        .iter()
                        .any(|key| v.get(*key).is_some_and(|x| !x.is_string()))
            })
        {
            return Err("Pi get_state returned malformed data".into());
        }
        let expected = opts["resumeSessionId"].as_str();
        if expected.is_some() && expected != state["sessionId"].as_str() {
            return Err("Pi native resume identity mismatch".into());
        }
        self.client.set_native_resume_ready(expected);
        self.client.session_ready();
        if let Some(id) = state["sessionId"].as_str().map(str::to_owned) {
            self.client.metadata.update(move |m| {
                let mut m = m.clone();
                m["piSessionId"] = json!(id);
                m
            });
        }
        self.native_title(&state["sessionName"]);
        self.streaming = state["isStreaming"] == true;
        if let Some(m) = state["model"]
            .get("id")
            .or_else(|| state["model"].get("modelId"))
        {
            self.runtime["model"] = m.clone()
        }
        self.provider = state["model"]["provider"].clone();
        self.selected();
        if let Some(e) = state.get("thinkingLevel") {
            self.runtime["effort"] = e.clone()
        }
        let data = self
            .transport
            .rpc(json!({"type":"get_available_models"}))
            .await
            .unwrap_or_default();
        for m in data["models"].as_array().into_iter().flatten() {
            if let Some(id) = m["id"].as_str() {
                let mut row =
                    json!({"modelId":id,"provider":m["provider"].as_str().unwrap_or("unknown")});
                for k in ["name", "contextWindow", "reasoning", "thinkingLevelMap"] {
                    if let Some(v) = m.get(k) {
                        row[k] = v.clone()
                    }
                }
                self.models.push(row)
            }
        }
        if !self.models.is_empty() {
            let models = json!(self.models);
            self.client.metadata.update(move |m| {
                let mut m = m.clone();
                m["piAvailableModels"] = models.clone();
                m
            });
        }
        self.commands = self
            .transport
            .rpc(json!({"type":"get_commands"}))
            .await
            .unwrap_or_default()["commands"]
            .clone();
        self.history.initialize(&self.transport, &self.client).await;
        for k in ["model", "effort"] {
            if let Some(value) = opts.get(k) {
                let config = if k == "model" {
                    let Some(model) = self.models.iter().find(|m| {
                        m["modelId"] == *value
                            || value.as_str().is_some_and(|v| {
                                v == format!(
                                    "{}/{}",
                                    m["provider"].as_str().unwrap_or(""),
                                    m["modelId"].as_str().unwrap_or("")
                                )
                            })
                    }) else {
                        continue;
                    };
                    json!({"model":model})
                } else {
                    json!({k:value})
                };
                if let Err(e) = self.config(&config).await {
                    if e.contains("timed out") {
                        return Err(e);
                    }
                    self.notice(&e)
                }
            }
        }
        self.alive();
        Ok(())
    }
    async fn config(&mut self, v: &Value) -> Result<Value, String> {
        let level = v
            .get("effort")
            .map(|e| {
                if e.is_null() {
                    Ok("off".into())
                } else {
                    e.as_str()
                        .map(|s| s.trim().to_lowercase())
                        .filter(|s| {
                            ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
                                .contains(&s.as_str())
                        })
                        .ok_or("Invalid effort")
                }
            })
            .transpose()?;
        if let Some(model) = v.get("model") {
            if model.is_null() {
                self.provider = Value::Null;
                self.runtime["model"] = Value::Null
            } else {
                let id = model
                    .as_str()
                    .or_else(|| model["modelId"].as_str())
                    .ok_or("Invalid model")?
                    .trim();
                let provider = model["provider"]
                    .as_str()
                    .or_else(|| {
                        self.models
                            .iter()
                            .find(|m| m["modelId"] == id)
                            .and_then(|m| m["provider"].as_str())
                    })
                    .ok_or("Model provider is unknown")?;
                self.transport
                    .rpc(json!({"type":"set_model","modelId":id,"provider":provider}))
                    .await?;
                self.runtime["model"] = json!(id);
                self.provider = json!(provider);
                self.selected();
                self.alive();
            }
        }
        if let Some(level) = level {
            self.transport
                .rpc(json!({"type":"set_thinking_level","level":level}))
                .await?;
            self.runtime["effort"] = if v["effort"].is_null() {
                Value::Null
            } else {
                json!(level)
            };
        }
        self.alive();
        let mut applied = self.runtime.clone();
        if self.provider.is_string() {
            applied["model"] = json!({"provider":self.provider,"modelId":self.runtime["model"]})
        }
        Ok(json!({"applied":applied}))
    }
    async fn rpc(&mut self, name: &str, v: &Value) -> Result<Value, String> {
        match name {
            "switch" => {
                self.mode = v["to"].as_str().unwrap_or("remote").into();
                let local = self.mode == "local";
                self.client.agent_state.update(move |s| {
                    let mut s = s.clone();
                    s["controlledByUser"] = local.into();
                    s
                });
                self.alive();
                Ok(json!({"success":true}))
            }
            "permission" => {
                self.dialogs
                    .answer(v, &self.transport, &self.client)
                    .await?;
                Ok(Value::Null)
            }
            "set-session-config" => self.config(v).await,
            "abort" => {
                self.dialogs
                    .expire(true, &self.transport, &self.client)
                    .await?;
                if self.streaming || self.active.is_some() {
                    self.transport.rpc(json!({"type":"abort"})).await?;
                }
                Ok(json!({"success":true}))
            }
            "steer-queued-message" => {
                let Some(index) = self
                    .queue
                    .iter()
                    .position(|p| p.local.is_some() && p.local.as_deref() == v["localId"].as_str())
                else {
                    return Ok(
                        json!({"steered":false,"error":"Message not found or already dispatched"}),
                    );
                };
                if self.abort.is_some()
                    || !self.streaming
                    || self.queue[index].command["message"]
                        .as_str()
                        .is_some_and(|s| s.trim_start().starts_with('/'))
                {
                    return Ok(json!({"steered":false,"error":"Steering is not ready"}));
                }
                let mut cmd = self.queue[index].command.clone();
                cmd["type"] = "steer".into();
                self.transport.rpc(cmd).await?;
                let q = self.queue.remove(index).unwrap();
                self.history.expect(q.local.clone());
                if let Some(id) = q.local {
                    self.client.messages_consumed(&[id], false, false)
                }
                Ok(json!({"steered":true}))
            }
            "fork-conversation" | "rewind-conversation" => {
                let rewind = name == "rewind-conversation";
                if rewind && !v["messageLocalId"].is_string() {
                    return Err("messageLocalId is required".into());
                }

                if self.active.is_some() || self.streaming || !self.queue.is_empty() {
                    if rewind {
                        return Ok(
                            json!({"success":false,"outcome":"rejected","error":"Pi session is busy"}),
                        );
                    }
                    return Err("Pi session is busy".into());
                }
                let result = self
                    .history
                    .change(
                        name == "rewind-conversation",
                        v,
                        &self.transport,
                        &self.client,
                    )
                    .await;
                if rewind && !self.history.fatal {
                    if let Err(e) = &result {
                        return Ok(
                            json!({"success":false,"outcome":if e.contains("cancelled"){"cancelled"}else{"rejected"},"error":e}),
                        );
                    }
                }
                result
            }
            "listPiModels" => {
                let mut out = json!({"success":true,"availableModels":self.models});
                if let Some(model) = self.runtime.get("model") {
                    out["currentModelId"] = model.clone()
                }
                Ok(out)
            }
            "listSkills" | "listSlashCommands" => {
                let skills = name == "listSkills";
                let mut items = if skills {
                    Vec::new()
                } else {
                    special::BUILTINS.iter().map(|(name,description)|json!({"name":name,"description":description,"source":"builtin"})).collect()
                };
                for cmd in self
                    .commands
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|c| (c["source"] == "skill") == skills)
                {
                    if let Some(raw) = cmd["name"].as_str() {
                        let mut item =
                            json!({"name":if skills{raw.trim_start_matches("skill:")}else{raw}});
                        if let Some(d) = cmd.get("description") {
                            item["description"] = d.clone()
                        }
                        if !skills {
                            item["source"] = json!(if cmd["source"] == "prompt" {
                                "user"
                            } else {
                                "plugin"
                            })
                        }
                        items.retain(|old| old["name"] != item["name"]);
                        items.push(item)
                    }
                }
                Ok(json!({"success":true,(if skills{"skills"}else{"commands"}):items}))
            }
            _ => Err("Unknown Pi RPC".into()),
        }
    }
    async fn event(&mut self, v: Value) -> Result<(), String> {
        if let Some(notice) = self.maintenance.event(&v) {
            self.notice(&notice)
        }
        for row in self.messages.event(&v) {
            self.client.send_agent_message(row, None)
        }
        match v["type"].as_str() {
            Some("turn_end")
                if v["message"]["usage"]["totalTokens"]
                    .as_u64()
                    .is_some_and(|n| n > 0) =>
            {
                let stats = self
                    .transport
                    .rpc(json!({"type":"get_session_stats"}))
                    .await
                    .unwrap_or_default();
                self.usage(&stats, &v["message"]["usage"], None)
            }
            Some("extension_ui_request") => {
                self.dialogs.open(&v, &self.transport, &self.client).await?
            }
            Some("session_info_changed") => self.native_title(&v["name"]),
            Some("entry_appended") => self.history.entry(&v["entry"], &self.client),
            Some("agent_start" | "turn_start") => {
                if let Some(guard) = &mut self.abort {
                    if guard.compensate {
                        guard.compensate = false;
                        self.transport.rpc(json!({"type":"abort"})).await?;
                    }
                }
                if self.active.is_none() {
                    self.active = Some(Active {
                        id: String::new(),
                        original_local: None,
                        local: None,
                        accepted: true,
                        started: true,
                        ended: false,
                        settled: false,
                        deadline: None,
                    })
                }

                if let Some(a) = &mut self.active {
                    a.started = true;
                    a.deadline = None;
                    if v["type"] == "turn_start" {
                        if let Some(local) = a.local.take() {
                            self.client.messages_consumed(&[local], false, false)
                        }
                    }
                }
                self.streaming = true;
                self.alive();
            }
            Some("agent_end") if v["willRetry"] != true => {
                if let Some(a) = &mut self.active {
                    a.ended = true;
                    if a.accepted {
                        a.deadline = Some(Instant::now() + Duration::from_millis(500))
                    }
                }
            }
            Some("agent_settled") => {
                if let Some(a) = &mut self.active {
                    if a.started {
                        a.settled = true
                    }
                } else {
                    self.streaming = false;
                    self.alive()
                }
            }
            Some("response")
                if v["command"] == "prompt"
                    && self.active.as_ref().is_some_and(|a| v["id"] == a.id) =>
            {
                if v["success"] == true {
                    let a = self.active.as_mut().unwrap();
                    a.accepted = true;
                    if !a.started || a.ended {
                        a.deadline = Some(
                            Instant::now()
                                + Duration::from_millis(if a.ended { 500 } else { 1000 }),
                        )
                    }
                } else {
                    let a = self.active.take().unwrap();
                    self.history.rejected(&a.original_local);
                    if let Some(id) = a.local {
                        self.client.messages_consumed(&[id], true, false)
                    }
                    self.notice(v["error"].as_str().unwrap_or("Pi prompt rejected"));
                    self.streaming = false;
                    self.alive();
                }
            }
            _ => {}
        }
        Ok(())
    }
    async fn input(&mut self, input: Input) -> Result<(), String> {
        match input {
            Input::User(v, local) => {
                let (command, errors) = prompt::prepare(&v, &self.client.id, &self.commands);
                for e in errors {
                    self.notice(&e)
                }
                if command["message"] == "" && command.get("images").is_none() {
                    if let Some(id) = local {
                        self.client.messages_consumed(&[id], true, false)
                    }
                    return Ok(());
                }
                let steer = v["meta"]["deliveryMode"] == "steer"
                    && self.streaming
                    && self.abort.is_none()
                    && self.compact.is_none()
                    && !command["message"]
                        .as_str()
                        .is_some_and(|s| s.trim_start().starts_with('/'));
                self.queue.push_back(Queued {
                    command,
                    local: local.clone(),
                });
                if let Some(id) = local.filter(|_| steer) {
                    if let Err(e) = self
                        .rpc("steer-queued-message", &json!({"localId":id}))
                        .await
                    {
                        if e.contains("timed out") {
                            return Err(e);
                        }
                        self.notice(&format!("Pi steer failed: {e}"))
                    }
                }
            }
            Input::Cancel(id, reply) => {
                let result = if let Some(i) = self
                    .queue
                    .iter()
                    .position(|p| p.local.as_deref() == Some(&id))
                {
                    self.queue.remove(i);
                    Cancellation::Removed
                } else {
                    Cancellation::Missing
                };
                let _ = reply.send(result);
            }
            Input::Rpc(method, v, reply) => {
                if method == "abort" {
                    if let Some(guard) = &mut self.abort {
                        guard.waiters.push(reply);
                        return Ok(());
                    }
                    if let Some(active) = &self.active {
                        let preflight = !active.started;
                        let deadline = Instant::now() + Duration::from_secs(25);
                        self.dialogs
                            .expire(true, &self.transport, &self.client)
                            .await?;
                        let result = self.transport.rpc(json!({"type":"abort"})).await;
                        if result.is_ok()
                            || (preflight
                                && result.as_ref().is_err_and(|e| {
                                    let e = e.to_lowercase();
                                    e.contains("no active") || e.contains("nothing to abort")
                                }))
                        {
                            self.abort = Some(AbortGuard {
                                waiters: vec![reply],
                                deadline,
                                compensate: preflight,
                            });
                            return Ok(());
                        }
                        let timeout = result.as_ref().is_err_and(|e| e.contains("timed out"));
                        let _ = reply.send(result);
                        if timeout {
                            return Err("Pi abort timed out".into());
                        }
                        return Ok(());
                    }
                }
                let result = self.rpc(method, &v).await;
                let fatal =
                    self.history.fatal || result.as_ref().is_err_and(|e| e.contains("timed out"));
                let _ = reply.send(result);
                if fatal {
                    return Err("Pi mutation outcome is indeterminate".into());
                }
            }
        }
        Ok(())
    }
    pub async fn run(&mut self) -> Result<(), String> {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            if self.compact.as_ref().is_some_and(|c| c.is_finished()) {
                let result = self
                    .compact
                    .take()
                    .unwrap()
                    .await
                    .map_err(|e| e.to_string())?;
                self.streaming = false;
                self.alive();
                match result {
                    Ok(data) => {
                        if data["summary"].is_string() {
                            let mut event =
                                json!({"type":"compact-summary","summary":data["summary"]});
                            for k in ["tokensBefore", "estimatedTokensAfter"] {
                                if let Some(n) = data.get(k) {
                                    event[k] = n.clone()
                                }
                            }
                            self.client.send_session_event(event, None, None)
                        } else {
                            self.notice("📦 Compaction completed")
                        }
                        if let Some(n) = data["estimatedTokensAfter"].as_u64() {
                            self.usage(&Value::Null, &Value::Null, Some(n))
                        }
                    }
                    Err(e) if e.contains("timed out") => return Err(e),
                    Err(e) => self.notice(&e),
                }
            }
            if self.active.as_ref().is_some_and(|a| {
                a.accepted
                    && (a.settled
                        || (!self.maintenance.busy()
                            && a.deadline.is_some_and(|d| d <= Instant::now())))
            }) {
                if let Some(id) = self.active.take().and_then(|a| a.local) {
                    self.client.messages_consumed(&[id], false, false)
                }
                self.streaming = false;
                self.alive();
                self.history.sync(&self.transport, &self.client).await;
            }
            if let Some(guard) = &self.abort {
                if self.active.is_none() {
                    for waiter in self.abort.take().unwrap().waiters {
                        let _ = waiter.send(Ok(json!({"success":true})));
                    }
                } else if guard.deadline <= Instant::now() {
                    return Err("Pi abort settlement timed out".into());
                }
            }
            if self.active.is_none() && !self.streaming && self.compact.is_none() {
                if let Some(q) = self.queue.pop_front() {
                    match self
                        .special(q.command["message"].as_str().unwrap_or(""))
                        .await
                    {
                        Ok(false) => {}
                        result => {
                            if let Some(id) = q.local {
                                self.client.messages_consumed(&[id], true, false)
                            }
                            if let Err(e) = result {
                                if e.contains("timed out") {
                                    return Err(e);
                                }
                                self.notice(&e)
                            }
                            continue;
                        }
                    }
                    let id = Uuid::new_v4().to_string();
                    let mut cmd = q.command;
                    cmd["id"] = json!(id);
                    self.transport.send(cmd).await.map_err(|e| e.to_string())?;
                    self.history.expect(q.local.clone());
                    self.active = Some(Active {
                        id,
                        original_local: q.local.clone(),
                        local: q.local,
                        accepted: false,
                        started: false,
                        ended: false,
                        settled: false,
                        deadline: None,
                    });
                }
            }
            tokio::select! {
                v=self.events.recv()=>{if let Some(v)=v{self.event(v).await?}else{for row in self.messages.flush(){self.client.send_agent_message(row,None)}return Err("Pi transport closed".into())}},
                Some(input)=self.input.recv()=>self.input(input).await?,
                _=tick.tick()=>self.dialogs.expire(false,&self.transport,&self.client).await?,
            }
        }
    }
}
