use super::{
    app_server_client::{Client, Error, Event, ServerRequest},
    async_questions::{AsyncAnswer, AsyncQuestions},
    compaction_summary::Lookup,
    generation_ledger::GenerationLedger,
    permissions::{Permissions, Reply, Submission},
    plan,
    projection::{self, Projection},
    queue::{self, Queue},
    round_duration::RoundDuration,
    settings, slash_commands,
    value::*,
};
use futures_util::{future::BoxFuture, FutureExt};
use hapi_wrapper_core::{
    rpc::handlers::uploads,
    session::{
        client::{Cancellation, SessionClient, SessionEvent},
        metadata::now,
    },
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex, RwLock};
use uuid::Uuid;

#[path = "root_controls.rs"]
mod controls;
#[path = "root_projection.rs"]
pub(super) mod output;
pub use controls::latest_completed_cutoff;
type Result<T> = std::result::Result<T, String>;
pub trait RootHost: Send + Sync {
    fn create<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        parent: Option<&'a Root>,
    ) -> BoxFuture<'a, Result<Arc<Root>>>;
    fn end<'a>(
        &'a self,
        root: &'a Root,
        native_archive: bool,
        reason: Option<&'a str>,
    ) -> BoxFuture<'a, Result<()>>;
    fn settings_for(&self, thread: &str) -> Value;
    fn instructions(&self) -> String;
    fn prepare_bridge<'a>(
        &'a self,
        session: &'a SessionClient,
        cwd: &'a Path,
    ) -> BoxFuture<'a, Result<Value>>;
    fn stop_bridge(&self, sid: &str);
    fn before_subscribe<'a>(&'a self, _thread: &'a str) -> BoxFuture<'a, Result<()>> {
        async { Ok(()) }.boxed()
    }
    fn validate_history<'a>(
        &'a self,
        _thread: &'a str,
        _snapshot: &'a Value,
    ) -> BoxFuture<'a, Result<()>> {
        async { Ok(()) }.boxed()
    }
}
pub struct Options {
    pub images: hapi_wrapper_core::rpc::handlers::GeneratedImages,
    pub directory: PathBuf,
    pub cwd: PathBuf,
    pub endpoint: super::transport::Endpoint,
    pub token: Option<String>,
    pub generation: String,
    pub attached: bool,
}
#[derive(Default)]
struct State {
    settings: Value,
    native: Value,
    replay: Option<Value>,
    settings_revision: u64,
    turn: Option<String>,
    latest: Option<Value>,
    interrupted: bool,
    steering: Option<bool>,
    plan: Option<Value>,
    compaction: Option<Vec<String>>,
    delivery: Option<String>,
    compacting: Option<String>,
    dequeued: HashSet<String>,
    ancestry: HashMap<String, Option<String>>,
    reconnecting: bool,
    submitting_plan: bool,
}
type Refresh = watch::Receiver<Option<queue::Result<()>>>;
pub struct Root {
    pub session: Arc<SessionClient>,
    options: Options,
    host: Arc<dyn RootHost>,
    bridge: Value,
    thread: OnceLock<String>,
    queue: OnceLock<Arc<Queue>>,
    client: Mutex<Option<Arc<Client>>>,
    state: Mutex<State>,
    turn_revision: Arc<AtomicU64>,
    settings_changed: watch::Sender<u64>,
    permissions: Mutex<Permissions>,
    questions: Mutex<AsyncQuestions>,
    responses: Mutex<HashMap<(String, String), ServerRequest>>,
    projections: Mutex<HashMap<String, Projection>>,
    clocks: Mutex<output::Clocks>,
    summaries: AsyncMutex<HashMap<String, Lookup>>,
    stopping: AtomicBool,
    closed: AtomicBool,
    activated: watch::Sender<bool>,
    refreshing: Mutex<Option<Refresh>>,
    work: AsyncMutex<()>,
    controls: RwLock<()>,
    close_lock: AsyncMutex<()>,
    notify: mpsc::UnboundedSender<(String, Value, Option<String>, i64, u64)>,
    generation: AtomicU64,
    notification_work: AsyncMutex<()>,
}
impl Root {
    pub async fn prepare(
        session: Arc<SessionClient>,
        events: mpsc::UnboundedReceiver<SessionEvent>,
        options: Options,
        host: Arc<dyn RootHost>,
    ) -> Result<Arc<Self>> {
        let bridge = if options.attached {
            Value::Null
        } else {
            host.prepare_bridge(&session, &options.cwd).await?
        };
        let (notify, mut notifications) = mpsc::unbounded_channel();
        let permissions = Permissions::new(options.generation.clone());
        let questions =
            AsyncQuestions::restore(&session.agent_state.get().value["completedRequests"]);
        let root = Arc::new(Self {
            session,
            options,
            host,
            bridge,
            thread: OnceLock::new(),
            queue: OnceLock::new(),
            client: Mutex::new(None),
            state: Mutex::new(State {
                settings: json!({}),
                native: json!({}),
                ..Default::default()
            }),
            turn_revision: Arc::new(AtomicU64::new(0)),
            settings_changed: watch::channel(0).0,
            permissions: Mutex::new(permissions),
            questions: Mutex::new(questions),
            responses: Default::default(),
            projections: Default::default(),
            clocks: Default::default(),
            summaries: Default::default(),
            stopping: false.into(),
            closed: false.into(),
            activated: watch::channel(false).0,
            refreshing: Default::default(),
            work: Default::default(),
            controls: Default::default(),
            close_lock: Default::default(),
            notify,
            generation: 0.into(),
            notification_work: Default::default(),
        });
        if let Err(e) = root.connect().await {
            root.host.stop_bridge(&root.session.id);
            return Err(e);
        }
        let weak = Arc::downgrade(&root);
        tokio::spawn(async move {
            while let Some((method, p, model, at, generation)) = notifications.recv().await {
                let Some(root) = weak.upgrade() else {
                    break;
                };
                let _guard = root.notification_work.lock().await;
                if root.closed.load(Ordering::Acquire)
                    || root.generation.load(Ordering::Acquire) != generation
                {
                    continue;
                }
                if root
                    .notification(&method, &p, model.as_deref(), at)
                    .await
                    .is_err()
                {
                    root.fail_attached();
                }
            }
        });
        root.register_delivery(events);
        Ok(root)
    }
    fn connect(self: &Arc<Self>) -> BoxFuture<'_, Result<()>> {
        async {
            let mut client = Client::connect(&self.options.endpoint, self.options.token.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            client.initialize().await.map_err(|e| e.to_string())?;
            let mut events = std::mem::replace(&mut client.events, mpsc::unbounded_channel().1);
            let epoch = {
                let mut current = self.client.lock().unwrap();
                if self.stopped() {
                    return Err("Codex execution is stopping".into());
                }
                let epoch = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
                if epoch > 1 {
                    let mut permissions = self.permissions.lock().unwrap();
                    *permissions =
                        Permissions::new(format!("{}:{}", self.options.generation, Uuid::new_v4()));
                    self.responses.lock().unwrap().clear();
                }
                if let Some(previous) = current.replace(Arc::new(client)) {
                    previous.disconnect();
                }
                epoch
            };
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    let Some(root) = weak.upgrade() else {
                        break;
                    };
                    if root.generation.load(Ordering::Acquire) != epoch
                        || root.closed.load(Ordering::Acquire)
                    {
                        break;
                    }
                    match event {
                        Event::Notification { method, params } => root.receive(method, params),
                        Event::Request(r) => {
                            let weak = weak.clone();
                            tokio::spawn(async move {
                                if let Some(root) = weak.upgrade() {
                                    if root.ready().await
                                        && root.generation.load(Ordering::Acquire) == epoch
                                    {
                                        root.receive_request(r, epoch).await;
                                    }
                                }
                            });
                        }
                        Event::Disconnected(_) => {
                            {
                                let mut current = root.client.lock().unwrap();
                                if root.generation.load(Ordering::Acquire) != epoch {
                                    break;
                                }
                                current.take();
                            }
                            if root.options.attached {
                                root.fail_attached();
                            } else {
                                tokio::spawn(root.clone().reconnect());
                            }
                            break;
                        }
                    }
                }
            });
            Ok(())
        }
        .boxed()
    }
    fn q(&self) -> Result<&Arc<Queue>> {
        self.queue
            .get()
            .ok_or_else(|| "Codex thread is not bound".into())
    }
    pub(super) fn cwd(&self) -> &Path {
        &self.options.cwd
    }
    pub fn thread_id(&self) -> &str {
        self.thread.get().map_or("", String::as_str)
    }
    fn stopped(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }
    pub(super) fn connected(&self) -> bool {
        self.client.lock().unwrap().is_some()
    }
    async fn ready(&self) -> bool {
        let mut rx = self.activated.subscribe();
        let _ = rx.wait_for(|v| *v).await;
        !self.stopped()
    }
    pub(super) async fn request(&self, m: &str, p: Value) -> queue::Result<Value> {
        let c = self.client.lock().unwrap().clone().ok_or_else(|| {
            Error::NotReady(
                "Shared Codex client disconnected; reconnect and initialize before sending".into(),
            )
        })?;
        c.request(m, p).await
    }
    async fn rpc(&self, m: &str, p: Value) -> Result<Value> {
        self.request(m, p).await.map_err(|e| e.to_string())
    }
    pub fn config(&self, mut p: Value) -> Result<Value> {
        if self.options.attached {
            return Err("An attached runtime cannot inject native configuration".into());
        }
        if p["cwd"].is_null() {
            p["cwd"] = json!(self.options.cwd);
        }
        if p["developerInstructions"].is_null() {
            p["developerInstructions"] = json!(self.host.instructions());
        }
        p["config"] = object(&p["config"]);
        p["config"]["mcp_servers.hapi"] = self.bridge.clone();
        p["config"]["shell_environment_policy.set.HAPI_SESSION_ID"] = json!(self.session.id);
        Ok(p)
    }
    pub async fn bind(
        self: &Arc<Self>,
        thread: &str,
        mut response: Value,
        subscribe: bool,
    ) -> Result<()> {
        if self.thread.get_or_init(|| thread.into()) != thread {
            return Err("Cannot retarget a shared HAPI session".into());
        }
        let q = Queue::load(
            thread.into(),
            self.options
                .directory
                .join(format!("{}.queue.json", self.session.id)),
            Arc::new(output::QueuePort(Arc::downgrade(self))),
        )
        .map_err(|e| e.to_string())?;
        self.queue
            .set(Arc::new(q))
            .map_err(|_| "Thread already bound")?;
        self.projections.lock().unwrap().insert(
            thread.into(),
            Projection::new(thread.into(), None, &self.session.metadata.get().value),
        );
        let id = thread.to_owned();
        let owned = !self.options.attached;
        self.session.metadata.update(move |m| {
            let mut m = m.clone();
            m["codexSessionId"] = json!(id);
            m["capabilities"]["concurrentClients"] = json!(true);
            m["capabilities"]["terminal"] = json!(owned);
            m["capabilities"]["conversationHistory"] = json!({"forkCurrent":owned,"forkAtMessage":owned,"forkLatestCompleted":false,"rewindToMessage":false});
            m
        });
        self.session.agent_state.update(|s| {
            let mut s = object(s);
            let mut done = object(&s["completedRequests"]);
            for (id, r) in s["requests"].as_object().into_iter().flatten() {
                done[id] = merge(r.clone(), &json!({"completedAt":now(),"status":"canceled"}));
            }
            s["controlledByUser"] = json!(false);
            s.as_object_mut().unwrap().remove("startingMode");
            s["codexPlanProposalId"] = Value::Null;
            s["codexCompactionPendingMessageIds"] = json!([]);
            s["requests"] = json!({});
            s["completedRequests"] = done;
            s
        });
        if subscribe {
            self.host.before_subscribe(thread).await?;
            response = self
                .rpc("thread/resume", json!({"threadId":thread}))
                .await?;
        }
        self.accept_settings(&response);
        self.accept_settings(&self.host.settings_for(thread));
        if owned {
            self.history(thread, &response["thread"]).await?;
        }
        self.refresh().await?;
        self.children(true).await
    }
    pub fn seed_fresh_usage(&self) {
        self.clocks.lock().unwrap().generation.usage(0);
    }
    pub async fn activate(self: &Arc<Self>, options: &Value) -> Result<()> {
        if !self.options.attached {
            self.initial_settings(options).await?;
        }
        if self.stopped() {
            return Err("Codex execution is stopping".into());
        }
        self.register_controls();
        self.activated.send_replace(true);
        self.publish();
        {
            let s = self.state.lock().unwrap();
            self.session
                .session_ready_with_runtime(s.turn.is_some(), &s.settings);
        }
        if self.session.hub_archived() && !self.stopped() {
            self.stop_accepting();
            self.end_async(true, None);
        }
        Ok(())
    }
    fn end_async(self: &Arc<Self>, archive: bool, reason: Option<&'static str>) {
        let r = self.clone();
        tokio::spawn(async move {
            let _ = r.host.end(&r, archive, reason).await;
        });
    }
    fn fail_attached(self: &Arc<Self>) {
        if self.options.attached && !self.stopped() {
            self.stop_accepting();
            self.end_async(false, Some("unavailable"));
        }
    }
    fn notice(&self, message: &str) {
        self.session
            .send_session_event(json!({"type":"message","message":message}), None, None);
    }
    fn available(&self, s: &State) -> Option<String> {
        if s.turn.is_some()
            || s.latest.as_ref()?["status"] != "completed"
            || s.settings["collaborationMode"] != "plan"
            || self.stopped()
            || s.reconnecting
            || s.submitting_plan
            || !self.connected()
        {
            return None;
        }
        let id = text(s.latest.as_ref()?, "planId")?;
        (!self.queue.get()?.owns(&plan::implementation_id(id))).then(|| id.into())
    }
    fn publish_field(&self, key: &'static str, value: Value) {
        self.session
            .agent_state
            .update(move |v| merge(object(v), &json!({key:value})));
    }
    fn publish_plan(&self) {
        let mut s = self.state.lock().unwrap();
        let id = json!(self.available(&s));
        if s.plan.as_ref() != Some(&id) {
            s.plan = Some(id.clone());
            self.publish_field("codexPlanProposalId", id);
        }
    }
    fn publish_compaction(&self) {
        let mut s = self.state.lock().unwrap();
        let steering = s.turn.is_some() && !self.stopped() && !s.reconnecting && self.connected();
        let mut ids = if steering && s.compacting.is_some() && s.compacting == s.turn {
            s.dequeued
                .iter()
                .filter(|id| {
                    self.queue
                        .get()
                        .is_some_and(|q| q.state(id) == Some(queue::State::Unknown))
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        ids.sort();
        if s.compaction.as_ref() != Some(&ids) {
            self.publish_field("codexCompactionPendingMessageIds", json!(ids));
            s.compaction = Some(ids);
        }
    }
    fn publish(&self) {
        self.publish_compaction();
        {
            let mut s = self.state.lock().unwrap();
            let steering =
                s.turn.is_some() && !self.stopped() && !s.reconnecting && self.connected();
            if s.steering != Some(steering) {
                s.steering = Some(steering);
                self.publish_field("steeringActive", json!(steering));
            }
        }
        self.publish_plan();
    }
    fn alive(&self) {
        self.publish();
        if !self.closed.load(Ordering::Acquire) {
            let s = self.state.lock().unwrap();
            self.session.keep_alive(s.turn.is_some(), None, &s.settings);
        }
    }
    pub fn accept_settings(&self, v: &Value) {
        if !v["model"].is_string() {
            return;
        }
        {
            let mut s = self.state.lock().unwrap();
            s.settings_revision += 1;
            s.native = merge(s.native.clone(), v);
            if v.get("collaborationMode").is_some() && v.get("sandboxPolicy").is_some() {
                s.replay = Some(v.clone());
            }
            s.settings["model"] = v["model"].clone();
            s.settings["modelReasoningEffort"] = json!(text(v, "effort reasoningEffort"));
            if let Some(kind) = text(field(v, "sandboxPolicy sandbox"), "type") {
                s.settings["permissionMode"] = json!(match kind {
                    "dangerFullAccess" => "yolo",
                    "readOnly" => "read-only",
                    _ if s.native["approvalsReviewer"] == "auto_review" => "autoReview",
                    _ => "default",
                });
            }
            if v.get("serviceTier").is_some() {
                s.settings["serviceTier"] = json!(if v["serviceTier"] == "priority" {
                    "fast"
                } else {
                    "standard"
                });
            }
            if v.get("collaborationMode").is_some() {
                s.settings["collaborationMode"] =
                    json!(if v["collaborationMode"]["mode"] == "plan" {
                        "plan"
                    } else {
                        "default"
                    });
            }
            self.settings_changed.send_replace(s.settings_revision);
        }
        self.publish_plan();
    }
    pub fn replay_settings(&self) -> Vec<Value> {
        self.state.lock().unwrap().replay.iter().map(|v| json!({"method":"thread/settings/updated","params":{"threadId":self.thread_id(),"threadSettings":v}})).collect()
    }
    fn receive(self: &Arc<Self>, method: String, p: Value) {
        let id = self.thread_id();
        if self.options.attached
            && matches!(method.as_str(), "thread/reverted" | "thread/rolledBack")
            && text(&p, "threadId")
                .is_some_and(|t| self.projections.lock().unwrap().contains_key(t))
        {
            self.invalidate();
            return;
        }
        if method == "thread/archived"
            && !id.is_empty()
            && text(&p, "threadId")
                .or_else(|| text(&p["thread"], "id"))
                .unwrap_or(id)
                == id
        {
            self.stop_accepting();
            self.end_async(false, Some("archived"));
            return;
        }
        if method == "serverRequest/resolved" {
            let t = text(&p, "threadId").unwrap_or("");
            self.permissions.lock().unwrap().resolved(
                &mut output::Controls(self),
                t,
                &p["requestId"],
                false,
                now(),
            );
            self.responses.lock().unwrap().remove(&(
                t.into(),
                hapi_wrapper_core::jsjson::stringify(&p["requestId"]),
            ));
            return;
        }
        let mut model = None;
        if p["threadId"] == id {
            if method == "thread/settings/updated" {
                self.accept_settings(&p["threadSettings"]);
            }
            {
                let mut s = self.state.lock().unwrap();
                let turn = text(&p["turn"], "id");
                match method.as_str() {
                    "turn/started" => {
                        self.turn_revision.fetch_add(1, Ordering::Relaxed);
                        s.turn = turn.map(str::to_owned);
                        s.interrupted = false;
                        if let Some(t) = turn {
                            s.latest = Some(json!({"id":t,"status":"inProgress"}));
                        }
                    }
                    "turn/completed" => {
                        if s.turn.is_none() || s.turn.as_deref() == turn {
                            self.turn_revision.fetch_add(1, Ordering::Relaxed);
                            s.turn = None;
                            s.interrupted = p["turn"]["status"] == "interrupted";
                            if !self.options.attached && p["turn"]["status"] == "completed" {
                                self.session.metadata.update(|m| {
                                    let mut m = m.clone();
                                    m["capabilities"]["conversationHistory"]
                                        ["forkLatestCompleted"] = json!(true);
                                    m
                                });
                            }
                        }
                        if let Some(t) =
                            turn.filter(|t| s.latest.as_ref().is_none_or(|v| v["id"] == *t))
                        {
                            let plan = plan::proposal_for_turn(id, &p["turn"]).or_else(|| {
                                s.latest
                                    .as_ref()
                                    .and_then(|v| text(v, "planId"))
                                    .map(str::to_owned)
                            });
                            s.latest = Some(
                                json!({"id":t,"status":text(&p["turn"],"status").unwrap_or("unknown"),"planId":plan}),
                            );
                        }
                    }
                    "item/completed" => {
                        if let Some(t) = text(&p, "turnId") {
                            if let Some(latest) = s.latest.as_mut().filter(|v| v["id"] == t) {
                                if let Some(plan) = plan::proposal_for_item(id, t, &p["item"]) {
                                    self.turn_revision.fetch_add(1, Ordering::Relaxed);
                                    latest["planId"] = json!(plan);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                model = text(&s.settings, "model").map(str::to_owned);
            }
            self.publish();
        }
        let _ = self.notify.send((
            method,
            p,
            model,
            now() as i64,
            self.generation.load(Ordering::Acquire),
        ));
    }
    fn invalidate(self: &Arc<Self>) {
        self.stop_accepting();
        let sid = self.session.id.clone();
        self.session.metadata.update(move |m| {
            let mut m = m.clone();
            m["conversationHistoryDiverged"] = json!(true);
            m["codexRuntime"] = json!({"kind":"attached","bindingId":text(&m["codexRuntime"],"bindingId").unwrap_or(&sid),"state":"history-changed"});
            m
        });
        self.notice("Native history changed outside HAPI. This view is frozen; use hapi attach --new-view for a fresh projection.");
        self.end_async(false, Some("history-changed"));
    }
    async fn owns(&self, thread: &str) -> bool {
        let mut visited = HashSet::new();
        let mut candidate = Some(thread.to_string());
        while let Some(id) = candidate {
            if id == self.thread_id() {
                return true;
            }
            if !visited.insert(id.clone()) {
                return false;
            }
            let cached = self.state.lock().unwrap().ancestry.get(&id).cloned();
            candidate = match cached {
                Some(p) => p,
                None => {
                    let Ok(v) = self
                        .rpc("thread/read", json!({"threadId":id,"includeTurns":false}))
                        .await
                    else {
                        return false;
                    };
                    let p = text(&v["thread"], "parentThreadId").map(str::to_owned);
                    self.state.lock().unwrap().ancestry.insert(id, p.clone());
                    p
                }
            };
        }
        false
    }
    async fn receive_request(self: &Arc<Self>, r: ServerRequest, epoch: u64) {
        if self.options.attached && self.refresh().await.is_err() {
            return;
        }
        let Some(t) = text(&r.params, "threadId") else {
            return;
        };
        if !self.owns(t).await || self.stopped() || epoch != self.generation.load(Ordering::Acquire)
        {
            return;
        }
        let mut perm = self.permissions.lock().unwrap();
        if self.generation.load(Ordering::Acquire) != epoch || self.stopped() {
            return;
        }
        if let Some(v) = perm.receive(
            &mut output::Controls(self),
            &r.id,
            &r.method,
            &r.params,
            now(),
        ) {
            let _ = r.respond(v);
        } else if perm.has_pending(t, &r.id) {
            self.responses
                .lock()
                .unwrap()
                .entry((t.into(), hapi_wrapper_core::jsjson::stringify(&r.id)))
                .or_insert(r);
        }
    }
    async fn notification(
        self: &Arc<Self>,
        method: &str,
        p: &Value,
        model: Option<&str>,
        at: i64,
    ) -> Result<()> {
        if self.queue.get().is_none()
            || self.closed.load(Ordering::Acquire)
            || self.options.attached && self.stopped()
        {
            return Ok(());
        }
        let id = self.thread_id();
        if method == "thread/started" {
            if let Some(t) = text(&p["thread"], "id") {
                self.state.lock().unwrap().ancestry.insert(
                    t.into(),
                    text(&p["thread"], "parentThreadId").map(str::to_owned),
                );
            }
        }
        let thread = text(p, "threadId")
            .or_else(|| text(&p["thread"], "id"))
            .unwrap_or(id);
        if self.options.attached
            && matches!(method, "thread/reverted" | "thread/rolledBack")
            && self.owns(thread).await
        {
            self.invalidate();
            return Ok(());
        }
        if thread != id {
            if self.owns(thread).await {
                if self.options.attached && matches!(method, "turn/started" | "turn/completed") {
                    self.history(thread, &self.read_thread(thread, false).await?)
                        .await?;
                }
                self.project_live(thread, method, p, None, at).await?;
            }
            return Ok(());
        }
        match method {
            "turn/completed" => {
                self.session
                    .send_session_event(json!({"type":"ready"}), None, None)
            }
            "thread/name/updated"
                if p.get("threadName")
                    .is_some_and(|n| n.is_string() || n.is_null()) =>
            {
                let name = p["threadName"].clone();
                self.session.metadata.update(move |m| {
                    let mut m = m.clone();
                    if name.is_null() {
                        m.as_object_mut().unwrap().remove("name");
                    } else {
                        m["name"] = name.clone();
                    }
                    m
                });
            }
            "thread/archived" => {
                self.stop_accepting();
                self.end_async(false, Some("archived"));
                return Ok(());
            }
            "thread/closed" | "thread/deleted" if self.options.attached => {
                self.fail_attached();
                return Ok(());
            }
            _ => {}
        }
        {
            let mut s = self.state.lock().unwrap();
            let turn = text(&p["turn"], "id");
            if method == "turn/started" {
                if s.delivery.is_some() && s.delivery.as_deref() != turn {
                    s.dequeued.clear();
                }
                s.delivery = turn.map(str::to_owned);
                s.compacting = None;
            }
            if method == "turn/completed" && turn == s.delivery.as_deref() {
                s.dequeued.clear();
                s.delivery = None;
                s.compacting = None;
            }
            if text(p, "turnId") == s.turn.as_deref() && p["item"]["type"] == "contextCompaction" {
                if method == "item/started" {
                    s.compacting = s.turn.clone();
                } else if method == "item/completed" {
                    s.compacting = None;
                    s.dequeued.clear();
                }
            }
        }
        if method == "thread/queue/changed" {
            let ids = self.q()?.reconcile().await.map_err(|e| e.to_string())?;
            self.state.lock().unwrap().dequeued.extend(ids);
        }
        if self.options.attached && method == "turn/started" {
            self.refresh().await?;
        }
        self.project_live(thread, method, p, model, at).await?;
        if self.options.attached && method == "turn/completed" {
            self.refresh().await?;
        }
        self.alive();
        Ok(())
    }
    fn lazy(&self) -> bool {
        !self.options.attached
            && std::env::var("HAPI_CODEX_LAZY_HISTORY_IDS")
                .is_ok_and(|s| s.split(',').any(|s| s.trim() == self.thread_id()))
    }
    pub async fn read_thread(&self, id: &str, full: bool) -> Result<Value> {
        self.read_thread_typed(id, full)
            .await
            .map_err(|e| e.to_string())
    }
    async fn read_thread_typed(&self, id: &str, full: bool) -> queue::Result<Value> {
        let mut t = self
            .request("thread/read", json!({"threadId":id,"includeTurns":false}))
            .await?["thread"]
            .clone();
        if t["historyMode"] != "paginated" {
            return Ok(self
                .request("thread/read", json!({"threadId":id,"includeTurns":true}))
                .await?["thread"]
                .clone());
        }
        let lazy = id == self.thread_id() && !full && self.lazy();
        let mut params =
            json!({"threadId":id,"sortDirection":if lazy{"desc"}else{"asc"},"itemsView":"full"});
        if lazy {
            params["limit"] = json!(1);
        }
        let mut turns = vec![];
        loop {
            let page = self.request("thread/turns/list", params.clone()).await?;
            turns.extend(
                page["data"]
                    .as_array()
                    .ok_or_else(|| Error::Rejected("Invalid Codex history".into()))?
                    .iter()
                    .cloned(),
            );
            if lazy {
                turns.reverse();
                break;
            }
            let Some(cursor) = text(&page, "nextCursor") else {
                break;
            };
            params["cursor"] = json!(cursor);
        }
        t["turns"] = json!(turns);
        Ok(t)
    }
    pub async fn refresh(self: &Arc<Self>) -> Result<()> {
        self.refresh_typed().await.map_err(|e| e.to_string())
    }
    async fn wait_refresh(mut rx: Refresh) -> queue::Result<()> {
        let result = rx
            .wait_for(Option::is_some)
            .await
            .map_err(|_| Error::Indeterminate("History refresh interrupted".into()))?;
        result.as_ref().unwrap().clone()
    }
    async fn refresh_typed(self: &Arc<Self>) -> queue::Result<()> {
        let rx = {
            let mut slot = self.refreshing.lock().unwrap();
            slot.get_or_insert_with(|| {
                let (tx, rx) = watch::channel(None);
                let root = self.clone();
                tokio::spawn(async move {
                    let result = root.refresh_now().await;
                    if result.is_err() {
                        root.fail_attached();
                    }
                    let mut slot = root.refreshing.lock().unwrap();
                    slot.take();
                    tx.send_replace(Some(result));
                });
                rx
            })
            .clone()
        };
        Self::wait_refresh(rx).await
    }
    async fn refresh_now(self: &Arc<Self>) -> queue::Result<()> {
        if self.closed.load(Ordering::Acquire) || !self.connected() {
            return Ok(());
        }

        let rev = self.turn_revision.load(Ordering::Relaxed);
        let id = self.thread_id();
        let thread = self.read_thread_typed(id, false).await?;
        let turns = thread["turns"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        if self.turn_revision.load(Ordering::Relaxed) == rev {
            let mut s = self.state.lock().unwrap();
            s.turn = turns
                .iter()
                .find(|v| v["status"] == "inProgress")
                .and_then(|t| text(t, "id"))
                .map(str::to_owned);
            s.interrupted = turns.last().is_some_and(|v| v["status"] == "interrupted");
            s.latest = turns.last().and_then(|v| Some(json!({"id":text(v,"id")?,"status":text(v,"status").unwrap_or("unknown"),"planId":plan::proposal_for_turn(id,v)})));
        }
        let current = self.session.metadata.get().value["capabilities"]["conversationHistory"]
            ["forkLatestCompleted"]
            .clone();
        let can = !self.options.attached
            && (turns.iter().any(|t| t["status"] == "completed") || self.lazy() && current == true);
        if current != can && (can || self.turn_revision.load(Ordering::Relaxed) == rev) {
            let revision = self.turn_revision.clone();
            self.session.metadata.update(move |m| {
                let mut m = m.clone();
                if can || revision.load(Ordering::Relaxed) == rev {
                    m["capabilities"]["conversationHistory"]["forkLatestCompleted"] = json!(can);
                }
                m
            });
        }
        self.history(id, &thread).await.map_err(Error::Rejected)?;
        self.q().map_err(Error::Rejected)?.reconcile().await?;
        self.alive();
        Ok(())
    }
    async fn children(self: &Arc<Self>, subscribe: bool) -> Result<()> {
        let mut params = json!({"ancestorThreadId":self.thread_id(),"sourceKinds":["subAgent","subAgentReview","subAgentCompact","subAgentThreadSpawn","subAgentOther"]});
        loop {
            let page = self.rpc("thread/list", params.clone()).await?;
            for t in page["data"].as_array().into_iter().flatten() {
                if let Some(id) = text(t, "id").filter(|id| *id != self.thread_id()) {
                    self.state
                        .lock()
                        .unwrap()
                        .ancestry
                        .insert(id.into(), text(t, "parentThreadId").map(str::to_owned));
                    if self.owns(id).await {
                        self.ensure_projection(id);
                    }
                }
            }
            if let Some(cursor) = text(&page, "nextCursor") {
                params["cursor"] = json!(cursor);
            } else {
                break;
            }
        }
        let ids: Vec<_> = self
            .projections
            .lock()
            .unwrap()
            .keys()
            .filter(|id| id.as_str() != self.thread_id())
            .cloned()
            .collect();
        let mut loaded = HashSet::new();
        if subscribe && !ids.is_empty() {
            let mut p = json!({});
            loop {
                let page = self.rpc("thread/loaded/list", p.clone()).await?;
                loaded.extend(strings(&page["data"]).into_iter().map(str::to_owned));
                if let Some(c) = text(&page, "nextCursor") {
                    p["cursor"] = json!(c);
                } else {
                    break;
                }
            }
        }
        for id in ids {
            let result: Result<()> = async {
                if loaded.contains(&id) {
                    self.host.before_subscribe(&id).await?;
                    self.rpc("thread/resume", json!({"threadId":id,"excludeTurns":true}))
                        .await?;
                }
                if !self.lazy() {
                    self.projections
                        .lock()
                        .unwrap()
                        .get_mut(&id)
                        .unwrap()
                        .reset();
                    self.history(&id, &self.read_thread(&id, false).await?)
                        .await?;
                }
                Ok(())
            }
            .await;
            if self.options.attached {
                result?;
            }
        }
        Ok(())
    }
    fn reconnect(self: Arc<Self>) -> BoxFuture<'static, ()> {
        async move {
            if self.stopped() || self.thread_id().is_empty() {
                return;
            }
            {
                let mut s = self.state.lock().unwrap();
                if s.reconnecting {
                    return;
                }
                s.reconnecting = true;
                s.dequeued.clear();
                s.delivery = None;
                s.compacting = None;
            }
            self.publish();
            self.withdraw();
            while !self.stopped() {
                let result: Result<()> = async {
                    self.connect().await?;
                    if self.stopped() {
                        return Ok(());
                    }

                    let rev = self.state.lock().unwrap().settings_revision;
                    let r = self
                        .rpc(
                            "thread/resume",
                            json!({"threadId":self.thread_id(),"excludeTurns":true}),
                        )
                        .await?;
                    let observed = self.state.lock().unwrap().settings_revision != rev;
                    self.accept_settings(&r);
                    if !observed {
                        self.accept_settings(&self.host.settings_for(self.thread_id()));
                    }
                    self.projections
                        .lock()
                        .unwrap()
                        .get_mut(self.thread_id())
                        .unwrap()
                        .reset();
                    self.clocks.lock().unwrap().generation.reset();
                    self.refresh().await?;
                    self.q()?.replay();
                    self.children(true).await?;
                    Ok(())
                }
                .await;
                {
                    let mut state = self.state.lock().unwrap();
                    if result.is_ok() && self.connected() {
                        state.reconnecting = false;
                        drop(state);
                        self.publish();
                        return;
                    }
                }
                self.withdraw();
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            self.state.lock().unwrap().reconnecting = false;
            self.publish();
        }
        .boxed()
    }
    fn withdraw(&self) {
        self.permissions
            .lock()
            .unwrap()
            .close(&mut output::Controls(self), now());
        self.responses.lock().unwrap().clear();
    }
    pub async fn native_queue_mutation(&self, method: &str, p: Value) -> Result<Value> {
        if !self.ready().await {
            return Err("Codex execution is stopping".into());
        }
        self.q()?
            .native_mutation(method, p)
            .await
            .map_err(|e| e.to_string())
    }
    pub async fn native_queue_deleted(&self, id: &str) -> Result<()> {
        self.q()?.deleted(id).await.map_err(|e| e.to_string())
    }
    pub fn stop_accepting(&self) {
        self.stopping.store(true, Ordering::Release);
        self.activated.send_replace(true);
        self.publish();
    }
    pub async fn suspend(self: &Arc<Self>) -> Result<()> {
        self.stop_accepting();
        let _controls = self.controls.write().await;
        let _work = self.work.lock().await;
        if let Some(q) = self.queue.get() {
            q.suspend().await.map_err(|e| e.to_string())?;
            self.refresh().await?;
        }
        Ok(())
    }
    pub async fn close(&self, archived: bool) {
        let _close = self.close_lock.lock().await;
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.stop_accepting();
        self.session.stop_keep_alive();
        self.withdraw();
        self.questions
            .lock()
            .unwrap()
            .close(&mut output::Controls(self), now());
        if let Some(client) = self.client.lock().unwrap().take() {
            client.disconnect();
        }
        let _controls = if self.options.attached {
            Some(self.controls.write().await)
        } else {
            None
        };
        let _work = if self.options.attached {
            Some(self.work.lock().await)
        } else {
            None
        };
        let _notifications = self.notification_work.lock().await;
        if let Some(q) = self.queue.get() {
            q.flush().await;
        }
        self.host.stop_bridge(&self.session.id);
        if archived {
            self.session.metadata.update(|m| {
                merge(
                    object(m),
                    &json!({"lifecycleState":"archived","lifecycleStateSince":now()}),
                )
            });
            uploads::cleanup_upload_dir(&self.session.id);
        } else {
            uploads::preserve_upload_dir_on_exit(&self.session.id);
        }
        self.session.session_end(None);
        self.session.flush(Duration::from_secs(5)).await;
        self.session.shutdown_extensions().await;
        self.session.close();
    }
}

#[cfg(all(test, unix))]
#[path = "root_tests.rs"]
mod tests;
