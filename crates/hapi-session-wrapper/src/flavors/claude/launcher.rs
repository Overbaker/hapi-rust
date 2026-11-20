use super::{
    outgoing_queue::OutgoingQueue,
    permission::{PermissionHandler, SessionActions, PLAN_FAKE_REJECT},
    remote::{Launch, Pickup, Remote, Target},
    sdk_to_log::Converter,
    special_commands,
    tool_result_images::ToolResultImages,
};
use hapi_wrapper_core::{
    queue::{Batch, MessageQueue2, Placement, Reservation},
    session::{client::SessionClient, metadata::now},
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, Notify},
    task::JoinSet,
    time::Instant,
};

pub(super) fn mode_key(mode: &Value, effort: bool) -> String {
    let mut key = json!({"agentEnforcedMode":mode["permissionMode"].as_str().filter(|s|matches!(*s,"plan"|"auto"))});
    for k in [
        "model",
        "fallbackModel",
        "customSystemPrompt",
        "appendSystemPrompt",
        "allowedTools",
        "disallowedTools",
    ] {
        if !mode[k].is_null() {
            key[k] = mode[k].clone();
        }
    }
    if effort && !mode["effort"].is_null() {
        key["effort"] = mode["effort"].clone();
    }
    key.to_string()
}

pub(super) enum Command {
    Abort(oneshot::Sender<Result<(), String>>),
    Mode(String),
}
pub(super) struct Session {
    pub client: Arc<SessionClient>,
    pub queue: Mutex<MessageQueue2<Value>>,
    pub changed: Notify,
    pub mode: Mutex<Value>,
    pub id: Mutex<Option<String>>,
    pub skills: Mutex<HashSet<String>>,
    thinking: AtomicBool,
    target: Mutex<Option<Arc<Target>>>,
    tx: mpsc::UnboundedSender<Command>,
}
impl Session {
    pub fn new(
        client: Arc<SessionClient>,
        mode: Value,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<Command>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut queue = MessageQueue2::new(|m| mode_key(m, true));
        let c = client.clone();
        queue.on_batch_consumed = Some(Box::new(move |ids| c.messages_consumed(ids, false, false)));
        (
            Arc::new(Self {
                client,
                queue: Mutex::new(queue),
                mode: Mutex::new(mode),
                id: Mutex::default(),
                skills: Mutex::default(),
                thinking: false.into(),
                changed: Notify::new(),
                target: Mutex::default(),
                tx,
            }),
            rx,
        )
    }
    pub fn enqueue(&self, text: String, mode: Value, id: Option<String>, place: Placement) {
        if self
            .queue
            .lock()
            .unwrap()
            .enqueue(place, text, mode, id, None)
            .is_ok()
        {
            self.changed.notify_one();
        }
    }
    pub fn expand(&self, text: &str, attachments: &str) -> String {
        if let Some(s) = text.trim_start().strip_prefix('$') {
            let name = s.split_whitespace().next().unwrap_or("");
            if !name.is_empty() && s.starts_with(name) && self.skills.lock().unwrap().contains(name)
            {
                return if attachments.is_empty() {
                    format!("/{s}")
                } else {
                    format!("/{s}\n\n{attachments}")
                };
            }
        }
        text.into()
    }
    pub fn set_mode(&self, mode: &str) {
        let mut m = self.mode.lock().unwrap();
        if m["permissionMode"] != mode {
            m["permissionMode"] = json!(mode);
            let _ = self.tx.send(Command::Mode(mode.into()));
        }
    }
    pub fn keep_alive(&self) {
        let m = self.mode.lock().unwrap();
        self.client.keep_alive(
            self.thinking.load(Ordering::Relaxed),
            Some("remote"),
            &json!({"permissionMode":m["permissionMode"],"model":m["model"],"effort":m["effort"]}),
        );
    }
    fn thinking(&self, b: bool) {
        if self.thinking.swap(b, Ordering::Relaxed) != b {
            self.keep_alive();
        }
    }
    pub fn found(&self, id: String) {
        *self.id.lock().unwrap() = Some(id.clone());
        self.client.metadata.update(move |m| {
            let mut next = m.clone();
            let mut history = Vec::new();
            for s in m["claudeSessionIdHistory"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(m.get("claudeSessionId"))
                .filter_map(Value::as_str)
                .map(str::trim)
            {
                if !s.is_empty() && s != id && !history.contains(&s) {
                    history.push(s);
                }
            }
            if !history.is_empty() || m.get("claudeSessionIdHistory").is_some() {
                next["claudeSessionIdHistory"] = json!(history[history.len().saturating_sub(32)..]);
            }
            next["claudeSessionId"] = json!(id);
            next
        });
    }
    fn event(&self, v: Value) {
        self.client.send_session_event(v, None, None);
    }
    pub fn notice(&self, s: &str) {
        self.event(json!({"type":"message","message":s}));
    }
    fn target(&self, target: Option<Arc<Target>>) {
        let active = target.is_some();
        let mut old = self.target.lock().unwrap();
        if old.is_some() != active {
            self.client.agent_state.update(move |s| {
                let mut s = s.clone();
                s["steeringActive"] = json!(active);
                s
            });
        }
        *old = target;
    }
    fn reject_mode(&self, mode: &str, reason: &str) {
        self.set_mode(mode);
        self.keep_alive();
        self.event(json!({"type":"permission-mode-changed","mode":mode}));
        self.notice(reason);
    }
    fn indeterminate(&self, r: &Reservation<Value>, ids: &[String]) {
        if self.queue.lock().unwrap().mark_indeterminate(r) {
            self.client.steer_indeterminate(ids);
        }
    }
    async fn restore(&self, r: &Reservation<Value>, ids: &[String]) {
        let ambiguous = self.queue.lock().unwrap().is_ambiguous(r);
        if !ambiguous && !self.client.steer_state(ids, "queued").await {
            self.indeterminate(r, ids);
            return;
        }
        let mut q = self.queue.lock().unwrap();
        if q.is_dispatching(r) && q.restore(r) {
            self.changed.notify_one();
        } else {
            self.client.steer_indeterminate(ids);
        }
    }
    async fn steer(
        self: Arc<Self>,
        id: String,
        permissions: Arc<PermissionHandler>,
        out: OutgoingQueue,
    ) -> Value {
        let fail = |s: &str| json!({"steered":false,"error":s});
        if id.is_empty() {
            return fail("Missing localId");
        }
        let Some(target) = self.target.lock().unwrap().clone() else {
            return fail("No active steerable turn");
        };
        let r = {
            let mut q = self.queue.lock().unwrap();
            let Some(r) = q.take_by_local_id(&id) else {
                return fail("Message not in queue");
            };
            if r.item().isolated || special_commands::parse(&r.item().message).is_some() {
                q.restore(&r);
                return fail("Control commands cannot be steered");
            }
            if r.item().hash != target.hash {
                q.restore(&r);
                return fail("Queued message mode differs from the active turn");
            }
            if !q.begin_dispatch(&r) {
                return fail("Steer cancelled");
            }
            r
        };
        let ids = [id.clone()];
        if !self.client.steer_state(&ids, "dispatching").await {
            self.indeterminate(&r, &ids);
            return fail("Steer state is indeterminate");
        }
        if !self.queue.lock().unwrap().is_dispatching(&r) {
            self.client.steer_indeterminate(&ids);
            return fail("Steer cancelled");
        }
        let text = self.expand(&r.item().message, "");
        self.client.echoes.lock().unwrap().note(&text, &ids);
        let Some((delivery, settle)) = target.steer(&text) else {
            self.client.echoes.lock().unwrap().discard_id(&id);
            self.restore(&r, &ids).await;
            return fail("Active turn changed");
        };
        permissions.deny_pending_for_steer();
        tokio::spawn(async move {
            match delivery.await.unwrap_or(Pickup::Closed) {
                Pickup::Delivered => {
                    out.flush().await;
                    self.queue.lock().unwrap().commit(&r);
                    self.client.messages_consumed(&ids, false, true);
                    let _ = settle.send(());
                }
                outcome => {
                    self.client.echoes.lock().unwrap().discard_id(&id);
                    if matches!(outcome, Pickup::Aborted) {
                        self.indeterminate(&r, &ids);
                    } else {
                        self.restore(&r, &ids).await;
                    }
                }
            }
        });
        json!({"steered":true})
    }
}

enum Update {
    Context(Value),
    Identity(Option<String>),
    Switch {
        sequence: u64,
        previous: String,
        native: String,
        error: Option<String>,
    },
    Effort(bool),
    Compact(Value, Option<u64>),
    Found(String),
}
pub(super) struct Launcher {
    s: Arc<Session>,
    launch: Launch,
    command: mpsc::UnboundedReceiver<Command>,
    permissions: Arc<PermissionHandler>,
    out: OutgoingQueue,
    quotas: super::quota::Quotas,
    converter: Converter,
    remote: Option<Remote>,
    tasks: JoinSet<Update>,
    ongoing: Vec<(String, Option<String>)>,
    plans: HashSet<String>,
    parked: Option<Batch<Value>>,
    inflight: Option<(Batch<Value>, String)>,
    accepting_input: bool,
    applying: bool,
    completing: bool,
    draining: bool,
    awaiting_transcript: bool,
    stream_end: Option<Option<String>>,
    progress: bool,
    failures: usize,
    backoff: Option<Instant>,
    fork_attempt: bool,
    requested_resume: Option<String>,
    previous_id: Option<String>,
    begin_attempt: bool,
}
impl Launcher {
    pub fn new(
        s: Arc<Session>,
        launch: Launch,
        home: &std::path::Path,
        command: mpsc::UnboundedReceiver<Command>,
        mut images: ToolResultImages,
    ) -> Self {
        let client = s.client.clone();
        let out = OutgoingQueue::new(move |v| images.send(&client, v));
        let (get, set, unshift) = (s.clone(), s.clone(), s.clone());
        let release = out.clone();
        let permissions = PermissionHandler::for_session(
            &s.client,
            SessionActions {
                get_mode: Box::new(move || {
                    get.mode.lock().unwrap()["permissionMode"]
                        .as_str()
                        .unwrap_or("default")
                        .into()
                }),
                set_mode: Box::new(move |m| set.set_mode(m)),
                unshift: Box::new(move |text, m| {
                    unshift.enqueue(
                        text.into(),
                        json!({"permissionMode":m}),
                        None,
                        Placement::Unshift,
                    )
                }),
            },
            move |id| release.release_tool_call(id),
        );
        let (session, p, q) = (s.clone(), permissions.clone(), out.clone());
        s.client.rpc.register("steer-queued-message", move |v| {
            let (s, p, q) = (session.clone(), p.clone(), q.clone());
            async move {
                Ok(Some(
                    s.steer(v["localId"].as_str().unwrap_or("").into(), p, q)
                        .await,
                ))
            }
        });
        let tx = s.tx.clone();
        s.client.rpc.register("abort", move |_| {
            let tx = tx.clone();
            async move {
                let (done, wait) = oneshot::channel();
                tx.send(Command::Abort(done))
                    .map_err(|_| "Session closed")?;
                wait.await.map_err(|_| "Session closed")??;
                Ok(None)
            }
        });
        s.client.rpc.register("switch", |_| async { Ok(None) });
        let converter = Converter::new(
            "unknown".into(),
            launch.cwd.to_string_lossy().into(),
            None,
            None,
        );
        let fork_attempt = launch.fork();
        let requested_resume = s.id.lock().unwrap().clone().filter(|_| !fork_attempt);
        Self {
            s,
            launch,
            command,
            permissions,
            out,
            converter,
            quotas: super::quota::Quotas::new(home),
            remote: None,
            tasks: JoinSet::new(),
            ongoing: Vec::new(),
            plans: HashSet::new(),
            parked: None,
            inflight: None,
            accepting_input: true,
            applying: false,
            completing: false,
            draining: false,
            awaiting_transcript: false,
            stream_end: None,
            progress: false,
            failures: 0,
            backoff: None,
            fork_attempt,
            requested_resume,
            previous_id: None,
            begin_attempt: true,
        }
    }
    fn path(&self) -> Option<PathBuf> {
        self.s
            .id
            .lock()
            .unwrap()
            .as_deref()
            .and_then(|id| self.launch.transcript(id))
    }
    fn spawn(&mut self, mode: &Value) -> io::Result<()> {
        let id = self.s.id.lock().unwrap().clone();
        self.remote = Some(Remote::spawn(&self.launch, mode, id.as_deref())?);
        self.s.thinking(true);
        Ok(())
    }
    async fn dispatch(&mut self) -> io::Result<()> {
        if self.launch.fork() && self.remote.is_none() {
            self.fork_attempt = false;
            let mode = self.s.mode.lock().unwrap().clone();
            self.spawn(&mode)?;
            self.accepting_input = false;
            return Ok(());
        }
        self.out.flush().await;
        let was_parked = self.parked.is_some();
        let batch = match self.parked.take() {
            Some(b) => b,
            None => match self.s.queue.lock().unwrap().collect(false)? {
                Some(b) => b,
                None => return Ok(()),
            },
        };
        if batch.isolated && !was_parked {
            self.parked = Some(batch);
            if self.remote.is_some() {
                self.drain()?;
            } else {
                self.finish(false).await?;
            }
            return Ok(());
        }
        if let Some(r) = &self.remote {
            if !r.fork_wait && mode_key(&r.mode, true) != batch.hash {
                let effort_only =
                    !batch.isolated && mode_key(&r.mode, false) == mode_key(&batch.mode, false);
                self.parked = Some(batch);
                if effort_only {
                    let input = r.query.input.clone();
                    let effort = self.parked.as_ref().unwrap().mode["effort"]
                        .as_str()
                        .map(str::to_owned);
                    self.applying = true;
                    self.tasks.spawn(async move {
                        Update::Effort(input.set_effort(effort.as_deref()).await.is_ok())
                    });
                    return Ok(());
                }
                self.drain()?;
                return Ok(());
            }
        }
        if matches!(
            special_commands::parse(&batch.message),
            Some(special_commands::SpecialCommand::Clear)
        ) {
            self.finish(false).await?;
            self.failures = 0;
            self.s.notice("Context was reset");
            self.requested_resume = None;
            self.s.client.set_native_resume_ready(None);
            *self.s.id.lock().unwrap() = None;
            self.launch.consume_flags();
            self.converter.reset_parent_chain();
            return Ok(());
        }
        let text = self.s.expand(&batch.message, "");
        let permission = batch.mode["permissionMode"].as_str().unwrap_or("default");
        self.s.mode.lock().unwrap()["permissionMode"] = json!(permission);
        self.switch(permission.into());
        self.converter.selected_model = batch.mode["model"].as_str().map(str::to_owned);
        let ids = batch
            .items
            .iter()
            .filter_map(|i| i.local_id.clone())
            .collect::<Vec<_>>();
        self.s.client.echoes.lock().unwrap().note(&text, &ids);
        let hash = batch.hash.clone();
        let mode = batch.mode.clone();
        self.inflight = Some((batch, text.clone()));
        if self.remote.is_none() {
            self.spawn(&mode)?;
        }
        let path = self.path();
        let r = self.remote.as_mut().unwrap();
        r.mode = mode;
        r.fork_wait = false;
        r.prompt(&text, &hash, path)?;
        if r.compact.is_some() {
            self.s.notice("📦 Compaction started");
        }
        self.s.target(r.target.clone());
        self.s.thinking(true);
        self.accepting_input = false;
        Ok(())
    }
    fn copy_response(&mut self, block: &Value) {
        if let Some(id) = block["tool_use_id"].as_str() {
            if let Some(response) = self.permissions.response(id) {
                self.converter.responses[id] = response;
            }
        }
    }
    fn relay(&mut self, v: &mut Value) -> bool {
        self.permissions.on_message(v);
        self.quotas.observe(v);
        self.converter.responses = Value::Null;
        let mut roots = Vec::new();
        let mut aborted = false;
        let assistant = v["type"] == "assistant";
        let user = v["type"] == "user";
        let parent = v["parent_tool_use_id"].as_str().map(str::to_owned);
        if v["type"] == "tool_result" {
            self.copy_response(v);
        }
        for block in v
            .pointer_mut("/message/content")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if assistant && block["type"] == "tool_use" {
                if let Some(id) = block["id"].as_str() {
                    if let Some((_, scope)) = self.ongoing.iter_mut().find(|(tool, _)| tool == id) {
                        *scope = parent.clone();
                    } else {
                        self.ongoing.push((id.into(), parent.clone()));
                    }
                    match block["name"].as_str() {
                        Some("ExitPlanMode" | "exit_plan_mode") => {
                            self.plans.insert(id.into());
                        }
                        Some("Task") => {
                            if let Some(p) = block["input"]["prompt"].as_str() {
                                roots.push((id.to_owned(), p.to_owned()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            if user && block["type"] == "tool_result" {
                self.copy_response(block);
                if let Some(id) = block["tool_use_id"].as_str().map(str::to_owned) {
                    self.ongoing.retain(|(i, _)| i != &id);
                    self.out.release_tool_call(&id);
                    aborted |= self.permissions.is_aborted(&id);
                    if self.plans.contains(&id) && block["content"] == PLAN_FAKE_REJECT {
                        block["content"] = "Plan approved".into();
                        block["is_error"] = false.into();
                    }
                }
            }
        }
        if let Some(mut row) = self.converter.convert(v) {
            if row["type"] == "user" {
                for block in row
                    .pointer_mut("/message/content")
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == "tool_result")
                {
                    let Some(resp) = block["tool_use_id"]
                        .as_str()
                        .and_then(|id| self.converter.responses.get(id))
                    else {
                        continue;
                    };
                    let mut p = json!({"date":resp["receivedAt"],"result":if resp["approved"]==true{"approved"}else{"denied"}});
                    if resp["mode"].is_string() {
                        p["mode"] = resp["mode"].clone();
                    }
                    if resp["allowTools"].as_array().is_some_and(|v| !v.is_empty()) {
                        p["allowedTools"] = resp["allowTools"].clone();
                    }
                    block["permissions"] = p;
                }
            }
            self.out.enqueue(row);
        }
        for (id, text) in roots {
            self.out.enqueue(self.converter.sidechain_user(&id, &text));
        }
        aborted
    }
    fn switch(&mut self, mode: String) {
        let Some(r) = &mut self.remote else {
            return;
        };
        if r.requested == mode {
            return;
        }
        r.sequence += 1;
        let previous = std::mem::replace(&mut r.requested, mode.clone());
        let native = super::permission::native_mode(&mode).to_owned();
        if r.native == native {
            return;
        }
        let old = std::mem::replace(&mut r.native, native.clone());
        let reply = r.query.input.set_mode(&native);
        let sequence = r.sequence;
        r.switches += 1;
        self.tasks.spawn(async move {
            let error = reply.await.err().map(|e| e.to_string());
            Update::Switch {
                sequence,
                previous,
                native: old,
                error,
            }
        });
    }
    async fn message(&mut self, mut v: Value) -> io::Result<()> {
        if !self.remote.as_mut().unwrap().accept(&mut v) {
            if let Some(settled) = self.remote.as_mut().unwrap().settling.take() {
                let _ = settled.await;
            }
            return Ok(());
        }
        if self.relay(&mut v) {
            return self.finish(false).await;
        }
        let r = self.remote.as_mut().unwrap();
        if v["type"] == "system" {
            if v["subtype"] == "hook_response"
                && v["hook_name"] == "SessionStart:fork"
                && r.fork_wait
            {
                self.accepting_input = true;
            }
            if v["subtype"] == "init" {
                let input = r.query.input.clone();
                self.tasks.spawn(async move {
                    Update::Context(input.context_usage().await.unwrap_or_default())
                });
                let input = r.query.input.clone();
                self.tasks.spawn(async move {
                    Update::Identity(input.account_identity().await.ok().flatten())
                });
                if r.switches == 0
                    && matches!(r.native.as_str(), "auto" | "plan")
                    && v["permissionMode"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty() && s != r.native)
                {
                    let requested = std::mem::replace(&mut r.native, "default".into());
                    self.s.reject_mode("default",&format!("Claude could not start in {requested} mode, so it is running in default mode"));
                }
                let observed = v["session_id"].as_str().filter(|s| !s.is_empty());
                self.s.client.set_native_resume_ready(observed.filter(|id| {
                    Some(*id) == self.requested_resume.as_deref()
                        && Some(*id) == r.resume_target.as_deref()
                }));
                if let Some(id) = observed {
                    let id = id.to_owned();
                    let path = self.launch.transcript(&id);
                    if path.as_ref().is_some_and(|p| p.exists()) {
                        self.found(id);
                    } else {
                        self.awaiting_transcript = true;
                        self.tasks.spawn(async move {
                            if let Some(path) = path {
                                for _ in 0..10 {
                                    if path.exists() {
                                        break;
                                    }
                                    tokio::time::sleep(Duration::from_secs(1)).await;
                                }
                            }
                            Update::Found(id)
                        });
                    }
                } else if r.fork_wait {
                    self.accepting_input = true;
                }
            }
        } else if v["type"] == "result" {
            if r.first_result {
                r.first_result = false;
                if let Some(text) = r
                    .initial
                    .clone()
                    .filter(|t| special_commands::parse(t).is_none())
                {
                    title_fallback(self.s.client.clone(), self.path(), text);
                }
            }
            let r = self.remote.as_mut().unwrap();
            if r.target.as_ref().is_some_and(|t| t.waiting()) {
                self.inflight = None;
                self.progress = true;
                return Ok(());
            }
            self.s.thinking(false);
            r.close_target(Pickup::Closed);
            self.s.target(None);
            if r.compact_waiting() {
                return Ok(());
            }
            self.inflight = None;
            self.progress = true;
            if let Some(c) = r.compact.take() {
                self.completing = true;
                self.tasks.spawn(async move {
                    let (e, n) = c.completion().await;
                    Update::Compact(e, n)
                });
            } else if !self.completing {
                self.ready(None).await;
            }
        }
        Ok(())
    }
    fn found(&mut self, id: String) {
        self.awaiting_transcript = false;
        self.converter.session_id = id.clone();
        self.s.found(id);
        self.launch.consume_flags();
        if self.remote.as_ref().is_some_and(|r| r.fork_wait) {
            self.accepting_input = true;
        }
    }
    async fn ready(&mut self, compact: Option<(Value, Option<u64>)>) {
        self.out.flush().await;
        if let Some((event, tokens)) = compact {
            self.s.event(event);
            if let Some(n) = tokens {
                let mut info =
                    json!({"total":{"inputTokens":0,"outputTokens":0},"contextTokens":n});
                if let Some(w) = self.converter.current_context_window() {
                    info["modelContextWindow"] = w.clone();
                }
                self.s.client.send_agent_message(json!({"type":"token_count","model":self.s.mode.lock().unwrap()["model"],"usageSchema":"hapi.usage.v1","inputTokenSemantics":"includes-cache","info":info}),None);
            }
        }
        if self.parked.is_none() && self.s.queue.lock().unwrap().size() == 0 {
            self.s.event(json!({"type":"ready"}));
        }
        self.accepting_input = true;
    }
    fn drain(&mut self) -> io::Result<()> {
        self.draining = true;
        self.remote.as_ref().unwrap().query.input.end()
    }
    async fn finish(&mut self, aborted: bool) -> io::Result<()> {
        if self.progress || self.inflight.is_some() {
            self.failures = 0;
        }
        self.s.client.set_native_resume_ready(None);
        let stopped = if let Some(mut r) = self.remote.take() {
            r.close_target(if aborted {
                Pickup::Aborted
            } else {
                Pickup::Closed
            });
            self.s.target(None);
            r.query.stop().await
        } else {
            Ok(())
        };
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
        self.s.thinking(false);
        for (id, parent) in self.ongoing.drain(..) {
            self.s
                .client
                .send_claude_session_message(&self.converter.interrupted(&id, parent.as_deref()));
        }
        self.out.flush().await;
        self.permissions.reset();
        self.plans.clear();
        self.inflight = None;
        self.accepting_input = true;
        self.applying = false;
        self.completing = false;
        self.draining = false;
        self.awaiting_transcript = false;
        self.stream_end = None;
        self.progress = false;
        self.begin_attempt = true;
        stopped
    }
    async fn failed(&mut self, detail: String) -> io::Result<()> {
        if self.progress {
            self.failures = 0;
        } else {
            self.failures += 1;
        }
        let batch = self.inflight.take();
        self.finish(false).await?;
        if self.failures >= 3 {
            if let Some((b, text)) = batch {
                let mut e = self.s.client.echoes.lock().unwrap();
                for id in b.items.iter().filter_map(|i| i.local_id.as_deref()) {
                    e.discard_id(id);
                }
                e.discard_text(&text);
            }
            self.s.notice(&format!("Process exited unexpectedly 3 times in a row: {detail}. Dropping the queued message; resolve the issue and resend it."));
            self.failures = 0;
        } else {
            self.fork_attempt = self.launch.fork();
            if let Some((b, _)) = batch {
                let _ = self.s.queue.lock().unwrap().unshift_batch(&b);
            }
            self.s
                .notice(&format!("Process exited unexpectedly: {detail}"));
            let ms = std::env::var("CLAUDE_REMOTE_RESPAWN_BACKOFF_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1000);
            self.backoff = Some(Instant::now() + Duration::from_millis(ms));
        }
        Ok(())
    }
    async fn ended(&mut self, error: Option<String>) -> io::Result<()> {
        if self.completing {
            self.stream_end = Some(error);
            return Ok(());
        }
        if let Some(e) = error {
            self.failed(e).await?;
        } else {
            self.finish(false).await?;
        }
        Ok(())
    }
    async fn update(&mut self, u: Update) -> io::Result<()> {
        match u {
            Update::Context(v) => self
                .converter
                .apply_context_usage(v["model"].as_str().unwrap_or(""), &v["maxTokens"]),
            Update::Identity(identity) => self.quotas.bind(identity.as_deref()),
            Update::Found(id) => self.found(id),
            Update::Switch {
                sequence,
                previous,
                native,
                error,
            } => {
                if let Some(r) = &mut self.remote {
                    r.switches -= 1;
                    if let Some(e) = error.filter(|_| sequence == r.sequence) {
                        r.native = native;
                        self.s.reject_mode(&previous, &e);
                    }
                }
            }
            Update::Effort(ok) => {
                self.applying = false;
                if ok {
                    if let (Some(r), Some(b)) = (&mut self.remote, &self.parked) {
                        r.mode = b.mode.clone();
                    }
                } else if self.drain().is_err() {
                    self.finish(false).await?;
                }
            }
            Update::Compact(e, n) => {
                self.completing = false;
                self.ready(Some((e, n))).await;
                if let Some(error) = self.stream_end.take() {
                    self.ended(error).await?;
                }
            }
        }
        Ok(())
    }
    pub async fn run(&mut self) -> io::Result<()> {
        loop {
            if self.begin_attempt && self.backoff.is_none() {
                self.begin_attempt = false;
                let id = self.s.id.lock().unwrap().clone();
                if self.previous_id != id {
                    self.converter.reset_parent_chain();
                    self.permissions.reset();
                    self.previous_id = id;
                }
            }
            let can_dispatch = self.accepting_input
                && !self.applying
                && !self.draining
                && !self.awaiting_transcript
                && self.backoff.is_none()
                && (self.fork_attempt
                    || self.parked.is_some()
                    || self.s.queue.lock().unwrap().size() > 0);
            let reading =
                self.remote.is_some() && self.stream_end.is_none() && !self.awaiting_transcript;
            tokio::select! {
                biased;
                cmd = self.command.recv() => match cmd {
                    Some(Command::Mode(mode)) => self.switch(mode),
                    Some(Command::Abort(done)) => {
                        self.backoff = None;
                        if let Err(error) = self.finish(true).await {
                            let _ = done.send(Err(error.to_string()));
                            return Err(error);
                        }
                        self.s.notice("Aborted by user");
                        let _ = done.send(Ok(()));
                    }
                    None => return Ok(()),
                },
                event = async { self.remote.as_mut().unwrap().query.events.recv().await }, if reading => match event {
                    Some(Ok(event)) => {
                        if let Some(message) = self.remote.as_mut().unwrap().control(event, &self.permissions) {
                            self.message(message).await?;
                        }
                    }
                    Some(Err(error)) => self.ended(Some(error.to_string())).await?,
                    None => self.ended(None).await?,
                },
                Some(Ok(update)) = self.tasks.join_next(), if !self.tasks.is_empty() => self.update(update).await?,
                _ = async { tokio::time::sleep_until(self.backoff.unwrap()).await }, if self.backoff.is_some() => self.backoff = None,
                _ = std::future::ready(()), if can_dispatch => {
                    if let Err(error) = self.dispatch().await {
                        self.failed(error.to_string()).await?;
                    }
                }
                _ = self.s.changed.notified() => {}

            }
        }
    }
}
fn title_fallback(client: Arc<SessionClient>, path: Option<PathBuf>, text: String) {
    tokio::spawn(async move {
        let mut title = None;
        if let Some(path) = path {
            if let Ok(data) = tokio::fs::read_to_string(path).await {
                for line in data.lines().rev().filter(|l| l.contains("\"ai-title\"")) {
                    if let Ok(v) = serde_json::from_str::<Value>(line) {
                        if v["type"] == "ai-title" && v["aiTitle"].is_string() {
                            title = v["aiTitle"].as_str().map(str::to_owned);
                            break;
                        }
                    }
                }
            }
        }
        let prefix = format!("@{}/", std::env::temp_dir().join("hapi-blobs").display());
        let fallback = text
            .split_whitespace()
            .filter(|s| !s.starts_with(&prefix))
            .collect::<Vec<_>>()
            .join(" ");
        let title = title
            .unwrap_or(fallback)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let title = if title.encode_utf16().count() > 80 {
            let units = title.encode_utf16().take(79).collect::<Vec<_>>();
            format!("{}…", String::from_utf16_lossy(&units).trim_end())
        } else {
            title
        };
        if !title.is_empty() {
            client.metadata.update(move |m| {
                let mut m = m.clone();
                if [m["name"].as_str(), m["summary"]["text"].as_str()]
                    .into_iter()
                    .flatten()
                    .all(|s| s.trim().is_empty())
                {
                    m["summary"] = json!({"text":title,"updatedAt":now()});
                }
                m
            });
        }
    });
}

#[test]
fn batch_and_process_keys() {
    let a = json!({"model":"m","permissionMode":"default","effort":"low"});
    let mut b = a.clone();
    b["permissionMode"] = "acceptEdits".into();
    assert_eq!(mode_key(&a, true), mode_key(&b, true));
    b["effort"] = "high".into();
    assert_eq!(mode_key(&a, false), mode_key(&b, false));
    assert_ne!(mode_key(&a, true), mode_key(&b, true));
    b["permissionMode"] = "plan".into();
    assert_ne!(mode_key(&a, false), mode_key(&b, false));
}
