use super::*;
use crate::flavors::codex::models::normalize_model;
use crate::flavors::common::format_message;
use hapi_wrapper_core::jsjson::stringify;
use regex_lite::Regex;
use std::sync::LazyLock;
fn user_input(s: &str) -> queue::Input {
    static MENTION: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(^|\s)@"((?:\\.|[^"\\])*)""#).unwrap());
    let mut parts = vec![];
    let mut last = 0;
    for hit in MENTION.captures_iter(s) {
        let raw = &hit[2];
        if raw.is_empty() {
            continue;
        }
        let mut path = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' && chars.peek().is_some_and(|c| matches!(c, '"' | '\\')) {
                path.push(chars.next().unwrap());
            } else {
                path.push(c);
            }
        }
        let at = hit.get(0).unwrap().start() + hit[1].len();
        if at > last {
            parts.push(json!({"type":"text","text":&s[last..at]}));
        }
        parts.push(json!({"type":"mention","name":path.rsplit(['/', '\\']).find(|s|!s.is_empty()).unwrap_or(&path),"path":path}));
        last = hit.get(0).unwrap().end();
    }
    if last < s.len() || parts.is_empty() {
        parts.push(json!({"type":"text","text":&s[last..]}));
    }
    parts
}
pub fn latest_completed_cutoff(turns: &[Value]) -> Result<String> {
    let mut ids = HashSet::new();
    if turns
        .iter()
        .any(|t| text(t, "id").is_none_or(|id| !ids.insert(id)))
    {
        return Err("Native turn history is ambiguous".into());
    }
    let mut last = None;
    for t in turns {
        match text(t, "status") {
            Some("inProgress" | "in_progress") => break,
            Some("completed") => last = text(t, "id"),
            Some("interrupted" | "failed") => {}
            _ => return Err("Native history prefix is not complete".into()),
        }
    }
    last.map(str::to_owned)
        .ok_or_else(|| "No completed turn is available to fork".into())
}
impl Root {
    pub(super) fn register_delivery(
        self: &Arc<Self>,
        mut events: mpsc::UnboundedReceiver<SessionEvent>,
    ) {
        let (send, mut messages) = mpsc::unbounded_channel();
        self.session.on_user_message(Arc::new(move |m, id| {
            let _ = send.send((m, id));
        }));
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some((m, id)) = messages.recv().await {
                let Some(root) = weak.upgrade() else {
                    break;
                };
                if !root.ready().await {
                    break;
                }
                let _guard = root.work.lock().await;
                if root.stopped() {
                    break;
                }
                if let Err(e) = root.deliver(m, id).await {
                    root.notice(&format!(
                        "Message not confirmed: {e}. Inspect the queue before retrying."
                    ));
                }
            }
        });
        let weak = Arc::downgrade(self);
        self.session.on_cancel(Arc::new(move |id| {
            let weak = weak.clone();
            async move {
                let Some(root) = weak.upgrade() else {
                    return Ok(Cancellation::Indeterminate);
                };
                if !root.ready().await {
                    return Ok(Cancellation::Indeterminate);
                }
                let result = root
                    .q()
                    .map_err(std::io::Error::other)?
                    .cancel(&id)
                    .await
                    .map_err(std::io::Error::other)?;
                Ok(if result == true {
                    Cancellation::Removed
                } else if result == "consumed" {
                    Cancellation::Consumed
                } else {
                    Cancellation::Indeterminate
                })
            }
            .boxed()
        }));
        let weak = Arc::downgrade(self);
        self.session.on_retry(Arc::new(move |id| {
            let weak = weak.clone();
            async move {
                let Some(root) = weak.upgrade() else {
                    return Ok(false);
                };
                if !root.ready().await {
                    return Ok(false);
                }
                root.refresh().await.map_err(std::io::Error::other)?;
                Ok(matches!(
                    root.q().map_err(std::io::Error::other)?.state(&id),
                    Some(queue::State::Rejected | queue::State::Released | queue::State::Canceled)
                ))
            }
            .boxed()
        }));
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut online = false;
            while let Some(event) = events.recv().await {
                let Some(root) = weak.upgrade() else {
                    break;
                };
                if root.closed.load(Ordering::Acquire) {
                    break;
                }
                match event {
                    SessionEvent::HubArchived if *root.activated.borrow() && !root.stopped() => {
                        root.stop_accepting();
                        root.end_async(true, None)
                    }
                    SessionEvent::Connected => {
                        if online
                            && root.queue.get().is_some()
                            && !root.stopped()
                            && root.hub_reconnected().await.is_err()
                        {
                            root.fail_attached();
                        }
                        online = true;
                    }
                    _ => {}
                }
            }
        });
    }
    async fn hub_reconnected(self: &Arc<Self>) -> Result<()> {
        let active = *self.activated.borrow();
        if let Some(p) = self.projections.lock().unwrap().get_mut(self.thread_id()) {
            p.reset();
        }
        {
            let mut s = self.state.lock().unwrap();
            s.steering = None;
            s.compaction = None;
            s.plan = None;
        }
        self.refresh().await?;
        self.children(false).await?;
        self.q()?.replay();
        if active && !self.stopped() && self.connected() {
            self.session.session_ready();
        }
        Ok(())
    }
    async fn deliver(self: &Arc<Self>, msg: Value, id: Option<String>) -> Result<()> {
        if self.options.attached {
            self.refresh().await?;
        }
        if self.stopped() {
            return Ok(());
        }
        let id = id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let paths = msg["content"]["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| text(a, "path").map(str::to_owned))
            .collect::<Vec<_>>();
        let body = format_message(text(&msg["content"], "text").unwrap_or(""), &paths);
        let slash = body.trim().starts_with('/');
        let q = self.q()?;
        let resolved = if slash {
            q.command(&id, || async {
                self.command(&body).await.map_err(Error::Rejected)
            })
            .await
            .map_err(|e| e.to_string())?
        } else {
            Some(body)
        };
        let Some(s) = resolved else {
            self.session.messages_consumed(&[id], true, false);
            return Ok(());
        };
        let (interrupted, key) = {
            let s = self.state.lock().unwrap();
            (
                s.interrupted,
                (!slash && msg["meta"]["isNativeQueuedMessage"] != true)
                    .then(|| queue::batch_key(&s.settings, &msg["meta"])),
            )
        };
        q.enqueue(&id, user_input(&s), interrupted, key.as_deref())
            .await
            .map_err(|e| e.to_string())
    }
    pub async fn apply_settings(&self, raw: &Value) -> Result<Value> {
        self.apply_settings_typed(raw)
            .await
            .map_err(|e| e.to_string())
    }
    async fn apply_settings_typed(&self, raw: &Value) -> queue::Result<Value> {
        let mut cfg = json!({});
        let Some(obj) = raw.as_object() else {
            return Err(Error::Rejected("Invalid Codex settings".into()));
        };
        for (k, v) in obj {
            let ok = match k.as_str() {
                "permissionMode" => matches!(
                    v.as_str(),
                    Some("default" | "read-only" | "autoReview" | "yolo")
                ),
                "model" => v.as_str().is_some_and(|s| !s.is_empty()),
                "modelReasoningEffort" => {
                    v.is_null() || v.as_str().is_some_and(|s| !s.trim().is_empty())
                }
                "serviceTier" => v.is_null() || matches!(v.as_str(), Some("fast" | "standard")),
                "collaborationMode" => matches!(v.as_str(), Some("default" | "plan")),
                "personality" => matches!(v.as_str(), Some("friendly" | "pragmatic" | "none")),
                _ => continue,
            };
            if !ok {
                return Err(Error::Rejected(format!("Invalid {k}")));
            }
            cfg[k] = v.clone();
        }
        if cfg.as_object().unwrap().is_empty() {
            return Ok(json!({"applied":self.state.lock().unwrap().settings}));
        }
        if let Some(effort) = cfg["modelReasoningEffort"].as_str() {
            cfg["modelReasoningEffort"] = json!(effort.trim().to_lowercase());
        } else if cfg.get("modelReasoningEffort").is_some() {
            let model = {
                let s = self.state.lock().unwrap();
                cfg.get("model").unwrap_or(&s.native["model"]).clone()
            };
            let mut p = json!({"includeHidden":true});
            loop {
                let page = self.request("model/list", p.clone()).await?;
                if let Some(effort) = page["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|v| field(v, "model id") == &model)
                    .and_then(|v| text(v, "defaultReasoningEffort"))
                {
                    cfg["modelReasoningEffort"] = json!(effort);
                    break;
                }
                let Some(cursor) = text(&page, "nextCursor") else {
                    return Err(Error::Rejected("Codex did not report a default reasoning effort; choose an explicit effort".into()));
                };
                p["cursor"] = json!(cursor);
            }
        }
        let mut params = json!({"threadId":self.thread_id()});
        copy_fields(
            &mut params,
            &cfg,
            &[
                ("model", "model"),
                ("personality", "personality"),
                ("effort", "modelReasoningEffort"),
            ],
        );
        if cfg.get("serviceTier").is_some() {
            params["serviceTier"] = json!((cfg["serviceTier"] == "fast").then_some("priority"));
        }
        if let Some(mode) = text(&cfg, "permissionMode") {
            let (policy, sandbox) = match mode {
                "yolo" => ("never", "dangerFullAccess"),
                "read-only" => ("never", "readOnly"),
                _ => ("on-request", "workspaceWrite"),
            };
            params["approvalPolicy"] = json!(policy);
            params["sandboxPolicy"] = json!({"type":sandbox});
            params["approvalsReviewer"] = json!(if mode == "autoReview" {
                "auto_review"
            } else {
                "user"
            });
        }
        let mut revisions = self.settings_changed.subscribe();
        let initial = {
            let s = self.state.lock().unwrap();
            if let Some(mode) = text(&cfg, "collaborationMode") {
                let mut settings = json!({});
                for (k, src) in [
                    ("model", "model"),
                    ("reasoning_effort", "modelReasoningEffort"),
                ] {
                    if let Some(v) = cfg.get(src).or(s.settings.get(src)) {
                        settings[k] = v.clone();
                    }
                }
                settings["developer_instructions"] = Value::Null;
                params["collaborationMode"] = json!({"mode":mode,"settings":settings});
            }
            s.settings_revision
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        self.request("thread/settings/update", params.clone())
            .await?;
        loop {
            {
                let s = self.state.lock().unwrap();
                if s.settings_revision > initial && settings::matches(&s.native, &params) {
                    let applied = s.settings.clone();
                    drop(s);
                    self.alive();
                    return Ok(json!({"applied":applied}));
                }
            }
            tokio::time::timeout_at(deadline, revisions.changed())
                .await
                .map_err(|_| {
                    Error::Indeterminate(
                        "Native settings update is not confirmed; reconnect before retrying".into(),
                    )
                })?
                .map_err(|e| Error::Indeterminate(e.to_string()))?;
        }
    }
    pub async fn initial_settings(&self, opts: &Value) -> Result<()> {
        let native = self.state.lock().unwrap().native["collaborationMode"]["mode"].clone();
        // Codex may acknowledge a no-op without broadcasting a settings update.
        if let Some(mode) = text(opts, "collaborationMode").filter(|m| native != *m) {
            self.apply_settings(&json!({"collaborationMode":mode}))
                .await?;
        }
        Ok(())
    }
    fn fresh_params(&self) -> Result<Value> {
        let s = self.state.lock().unwrap();
        let mut p = settings::inherited_sandbox(&s.native)?;
        p["cwd"] = s
            .native
            .get("cwd")
            .cloned()
            .unwrap_or(json!(self.options.cwd));
        for k in [
            "model",
            "modelProvider",
            "approvalPolicy",
            "serviceTier",
            "approvalsReviewer",
            "personality",
        ] {
            if let Some(v) = s.native.get(k) {
                p[k] = v.clone();
            }
        }
        p["developerInstructions"] = json!(self.host.instructions());
        if !p["config"].is_object() {
            p["config"] = json!({});
        }
        if let Some(e) = text(&s.settings, "modelReasoningEffort") {
            p["config"]["model_reasoning_effort"] = json!(e);
        }
        Ok(p)
    }
    async fn new_conversation(&self) -> Result<Arc<Root>> {
        if self.options.attached {
            return Err(
                "Create new conversations in the owning desktop, then attach them explicitly"
                    .into(),
            );
        }
        let child = self
            .host
            .create("thread/start", self.fresh_params()?, None)
            .await?;
        let settings = self.state.lock().unwrap().settings.clone();
        child.initial_settings(&settings).await?;
        Ok(child)
    }
    async fn command(&self, body: &str) -> Result<Option<String>> {
        match body.trim() {
            "/new" | "/clear" => {
                let child = self.new_conversation().await?;
                self.notice(&format!("New conversation: /sessions/{}", child.session.id));
                return Ok(None);
            }
            "/compact" => {
                self.rpc("thread/compact/start", json!({"threadId":self.thread_id()}))
                    .await?;
                return Ok(None);
            }
            "/exit" => {
                self.notice(if self.options.attached {
                    "Use hapi detach to disconnect HAPI without stopping the external thread."
                } else {
                    "/exit closes a native terminal, not this Web view. Use End session to archive this conversation."
                });
                return Ok(None);
            }
            _ => {}
        }
        let mut s = self.state.lock().unwrap().settings.clone();
        if s["permissionMode"].is_null() {
            s["permissionMode"] = json!("default");
        }
        if s["collaborationMode"].is_null() {
            s["collaborationMode"] = json!("default");
        }
        let list = self.session.rpc.handle_request(&json!({"method":format!("{}:listSlashCommands",self.session.id),"params":"{\"agent\":\"codex\"}"})).await;
        if let Some(v) = list.and_then(|v| serde_json::from_str::<Value>(&v).ok()) {
            s["commands"] = v["commands"].clone();
        }
        let slash = slash_commands::resolve(body, &s);
        if slash["kind"] == "passthrough" {
            return Ok(Some(body.into()));
        }
        if slash["kind"] == "goal" {
            let action = text(&slash, "action").unwrap();
            let mut p = json!({"threadId":self.thread_id()});
            if action == "set" {
                let g = self.rpc("thread/goal/get", p.clone()).await?;
                if g["goal"]["status"] == "complete" {
                    self.rpc("thread/goal/clear", p.clone()).await?;
                    p["status"] = json!("active");
                }
                p["objective"] = slash["objective"].clone();
            } else if matches!(action, "pause" | "resume") {
                p["status"] = json!(if action == "pause" {
                    "paused"
                } else {
                    "active"
                });
            }
            let result = self
                .rpc(
                    match action {
                        "show" => "thread/goal/get",
                        "clear" => "thread/goal/clear",
                        _ => "thread/goal/set",
                    },
                    p,
                )
                .await?;
            self.notice(&stringify(&result));
            return Ok(None);
        }
        if let Some(updates) = slash.get("updates") {
            if updates.get("proactiveMultiAgent").is_some() {
                return Err("This Codex version uses Ultra reasoning effort instead of a multi-agent toggle".into());
            }
            if updates.get("model") == Some(&Value::Null) {
                return Err("Choose an explicit model in a shared thread".into());
            }
            self.apply_settings(updates).await?;
        }
        if let Some(msg) = text(&slash, "message") {
            self.notice(msg);
        }
        Ok(text(&slash, "text").map(str::to_owned))
    }
    pub(super) fn register_controls(self: &Arc<Self>) {
        for method in [
            "permission",
            "listCodexModels",
            "switch",
            "handoff-local",
            "abort",
            "killSession",
            "set-session-config",
            "implement-codex-plan",
            "steer-queued-message",
            "clear-conversation",
            "fork-conversation",
            "rewind-conversation",
        ] {
            let weak = Arc::downgrade(self);
            self.session.rpc.register(method, move |p| {
                let weak = weak.clone();
                async move {
                    let r = weak.upgrade().ok_or("Codex execution is stopping")?;
                    r.control(method, p).await
                }
            });
        }
    }
    pub async fn control(self: &Arc<Self>, method: &str, p: Value) -> Result<Option<Value>> {
        if self.stopped() {
            return Err("Codex execution is stopping; resume when inactive".into());
        }
        if method == "killSession" {
            self.host.end(self, true, None).await?;
            return Ok(Some(json!({"success":true})));
        }
        let _control = self.controls.read().await;
        if self.stopped() {
            return Err("Codex execution is stopping".into());
        }
        if self.options.attached {
            if matches!(
                method,
                "clear-conversation" | "fork-conversation" | "rewind-conversation"
            ) {
                return Err("Create/fork/revert this external thread in its owning desktop; HAPI only attaches".into());
            }
            self.refresh().await?;
            if self.stopped() {
                return Err("Codex execution is stopping".into());
            }
        }
        Ok(Some(match method {
            "permission" => {
                self.reply(serde_json::from_value(p).map_err(|e| e.to_string())?).await?;
                return Ok(None);
            }
            "listCodexModels" => {
                let mut models = vec![];
                let mut params = json!({"includeHidden":p["includeHidden"]==true});
                loop {
                    let page = self.rpc("model/list", params.clone()).await?;
                    models.extend(page["data"].as_array().into_iter().flatten().filter_map(normalize_model));
                    if let Some(cursor) = text(&page, "nextCursor") {
                        params["cursor"] = json!(cursor);
                    } else {
                        break;
                    }
                }
                json!({"success":true,"models":models})
            }
            "switch" | "handoff-local" => return Err("control_mode_not_applicable".into()),
            "abort" => {
                let turn = self.state.lock().unwrap().turn.clone();
                if let Some(turn) = turn {
                    self.rpc("turn/interrupt", json!({"threadId":self.thread_id(),"turnId":turn})).await?;
                }
                return Ok(None);
            }
            "set-session-config" => self.apply_settings(&p).await?,
            "steer-queued-message" => {
                let id = text(&p, "localId").ok_or("Missing localId")?;
                let turn = self.state.lock().unwrap().turn.clone();
                let Some(t) = turn else {
                    return Ok(Some(json!({"steered":false,"error":"No active turn"})));
                };
                self.q()?.steer(id, &t, None).await.map_err(|e| e.to_string())?
            }
            "clear-conversation" => json!({"sessionId":self.new_conversation().await?.session.id}),
            "fork-conversation" => self.fork(&p).await?,
            "rewind-conversation" => return Err("In-place rewind is not supported for concurrent Codex clients; fork at the message instead".into()),
            "implement-codex-plan" => {
                let _work = self.work.lock().await;
                self.implement(text(&p, "planId").ok_or("Missing planId")?).await
            }
            _ => return Err("Method not found".into()),
        }))
    }
    async fn reply(self: &Arc<Self>, reply: Reply) -> Result<()> {
        let outcome =
            self.questions
                .lock()
                .unwrap()
                .answer(&mut output::Controls(self), &reply, now())?;
        match outcome {
            AsyncAnswer::Dismissed => return Ok(()),
            AsyncAnswer::Submit {
                thread,
                text,
                client_id,
            } => {
                let result: queue::Result<()> = async {
                    let turn = if thread == self.thread_id() {
                        self.state.lock().unwrap().turn.clone()
                    } else {
                        let t = self.read_thread(&thread, false).await.map_err(Error::Rejected)?;
                        self.host.validate_history(&thread, &t).await.map_err(Error::Rejected)?;
                        t["turns"].as_array().into_iter().flatten().find(|t| t["status"] == "inProgress").and_then(|t| crate::flavors::codex::value::text(t, "id")).map(str::to_owned)
                    };
                    let mut p = json!({"threadId":thread,"clientUserMessageId":client_id,"input":user_input(&text)});
                    let m = if let Some(t) = turn {
                        p["expectedTurnId"] = json!(t);
                        "turn/steer"
                    } else {
                        "turn/start"
                    };
                    self.request(m, p).await?;
                    Ok(())
                }
                .await;
                if let Err(e) = result {
                    self.questions
                        .lock()
                        .unwrap()
                        .submit_failed(&reply.id, matches!(e, Error::Indeterminate(_)));
                    return Err(e.to_string());
                }
                return Ok(());
            }
            AsyncAnswer::Other => {}
        }
        let interrupt = {
            let mut permissions = self.permissions.lock().unwrap();
            let thread = permissions
                .pending_thread(&reply.id)
                .ok_or("Request already resolved or submitted")?;
            match permissions.submit(&mut output::Controls(self), &reply)? {
                Submission::Response(id, v) => {
                    if let Some(req) = self
                        .responses
                        .lock()
                        .unwrap()
                        .remove(&(thread, stringify(&id)))
                    {
                        req.respond(v).map_err(|e| e.to_string())?;
                    }
                    return Ok(());
                }
                Submission::Interrupt(p) => p,
            }
        };
        if let Err(e) = self.rpc("turn/interrupt", interrupt).await {
            self.permissions.lock().unwrap().interrupt_failed(&reply.id);
            return Err(e);
        }
        Ok(())
    }
    async fn implement(self: &Arc<Self>, id: &str) -> Value {
        let local = plan::implementation_id(id);
        let unavailable = json!({"ok":false,"code":"unavailable","error":"Codex is disconnected or stopping. Reconnect before implementing the plan."});
        let stale = json!({"ok":false,"code":"stale_plan","error":"This plan is no longer actionable. Refresh the conversation."});
        let blocked =
            || self.stopped() || !self.connected() || self.state.lock().unwrap().reconnecting;
        if blocked() {
            return unavailable;
        }
        let result: queue::Result<Value> = async {
            let previous=self.refreshing.lock().unwrap().clone();
            if let Some(rx)=previous{Self::wait_refresh(rx).await?;}
            self.refresh_typed().await?;
            let q = self.q().map_err(Error::Rejected)?;
            match q.state(&local) {
                Some(queue::State::Queued | queue::State::Consumed) => return Ok(json!({"ok":true})),
                Some(_) => return Ok(json!({"ok":false,"code":"indeterminate","error":"This implementation was already submitted. Check the queue and conversation before sending again."})),
                _ => {}
            }
            let before = q.list().await?;
            let rev = {
                let mut s = self.state.lock().unwrap();
                if self.available(&s).as_deref() != Some(id) || !before.is_empty() {
                    return Ok(stale.clone());
                }
                s.submitting_plan = true;
                self.turn_revision.load(Ordering::Relaxed)
            };
            self.publish_plan();
            self.apply_settings_typed(&json!({"collaborationMode":"default"})).await?;
            let queued = q.list().await?;
            if blocked() {
                return Ok(unavailable);
            }
            {
                let s = self.state.lock().unwrap();
                if rev != self.turn_revision.load(Ordering::Relaxed) || s.turn.is_some() || s.latest.as_ref().and_then(|v| text(v, "planId")) != Some(id) || s.settings["collaborationMode"] != "default" || !queued.is_empty() {
                    return Ok(stale);
                }
            }
            q.enqueue(&local, user_input("Implement the plan."), false, None).await?;
            Ok(json!({"ok":true}))
        }
        .await;
        self.state.lock().unwrap().submitting_plan = false;
        self.publish_plan();
        result.unwrap_or_else(|e| json!({"ok":false,"code":if matches!(e,Error::Indeterminate(_))||self.queue.get().and_then(|q|q.state(&local))==Some(queue::State::Unknown){"indeterminate"}else{"failed"},"error":format!("Plan implementation was not confirmed: {e}. Check the mode, queue and conversation before retrying.")}))
    }
    async fn fork(&self, raw: &Value) -> Result<Value> {
        if !raw.is_object()
            || raw.get("latestCompleted").is_some_and(|v| !v.is_boolean())
            || raw.get("messageLocalId").is_some_and(|v| !v.is_string())
        {
            return Err("Invalid fork request".into());
        }
        let local = text(raw, "messageLocalId");
        let latest = raw["latestCompleted"] == true;
        if latest && local.is_some() {
            return Err("Conflicting fork boundaries".into());
        }
        let mut p = self.fresh_params()?;
        p["threadId"] = json!(self.thread_id());
        let mut expected = None;
        if latest {
            let thread = self.read_thread(self.thread_id(), true).await?;
            let turns = thread["turns"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let cutoff = latest_completed_cutoff(turns)?;
            let count = turns.iter().position(|v| v["id"] == cutoff).unwrap() + 1;
            expected = Some((
                cutoff.clone(),
                turns[..count]
                    .iter()
                    .map(|v| v["id"].clone())
                    .collect::<Vec<_>>(),
            ));
            p["lastTurnId"] = json!(cutoff);
        } else if let Some(local) = local {
            let t = self
                .projections
                .lock()
                .unwrap()
                .get(self.thread_id())
                .and_then(|p| p.turn_for(local))
                .ok_or("No native history point for this message")?
                .to_owned();
            let thread = self.read_thread(self.thread_id(), true).await?;
            let turns = thread["turns"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let index = turns.iter().position(|v| v["id"] == t);
            let first = index.and_then(|n| {
                turns[n]["items"]
                    .as_array()?
                    .iter()
                    .find(|v| v["type"] == "userMessage")
            });
            let boundary = first.and_then(|v| {
                let id = text(v, "clientId clientUserMessageId")
                    .map(str::to_owned)
                    .or_else(|| {
                        Some(format!(
                            "codex:{}:user:{}",
                            self.thread_id(),
                            text(v, "id")?
                        ))
                    })?;
                self.queue.get()?.original_boundary_id(&id, &v["content"])
            });
            if boundary.as_deref() != Some(local) {
                return Err(
                    "Cannot cut inside a steered native turn. Select its first message.".into(),
                );
            }
            if index.is_none_or(|n| {
                turns[..=n].iter().any(|t| {
                    !matches!(
                        text(t, "status"),
                        Some("completed" | "interrupted" | "failed")
                    )
                })
            }) {
                return Err("Historical fork requires a completed turn and history prefix".into());
            }
            p["beforeTurnId"] = json!(t);
        } else if self.state.lock().unwrap().turn.is_some() {
            return Err("Session is busy".into());
        }
        let child = self.host.create("thread/fork", p, Some(self)).await?;
        let settings = self.state.lock().unwrap().settings.clone();
        let result = async {
            let mut r = json!({"nativeSessionId":child.thread_id(),"sessionId":child.session.id});
            if let Some((cutoff, ids)) = &expected {
                let thread = child.read_thread(child.thread_id(), true).await?;
                if !thread["turns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|v| &v["id"])
                    .eq(ids.iter())
                {
                    return Err(
                        "Native fork history does not match the completed-turn cutoff".into(),
                    );
                }
                r["completedTurnId"] = json!(cutoff);
            }
            child.initial_settings(&settings).await?;
            Ok(r)
        }
        .await;
        if result.is_err() && expected.is_some() {
            let _ = self.host.end(&child, true, None).await;
        }
        result
    }
}
