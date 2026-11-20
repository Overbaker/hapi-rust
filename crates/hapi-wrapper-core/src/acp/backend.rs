//! ACP client backend (TS `AcpSdkBackend.ts`, the branches ACP agents like
//! agy use; OpenCode child sessions, Cursor and Grok specifics are not
//! ported).
use super::{
    message_handler::{MessageHandler, MessageHandlerOptions, TextChunkMode},
    tool_lifetimes::{ToolCallEventKind, ToolCallLifetimes, MAIN_SCOPE},
    transport::{AcpError, RequestHandler, SpawnOptions, Transport},
    types::*,
};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, Notify},
    time::Instant,
};

const PRE_PROMPT_QUIET: Duration = Duration::from_millis(200);
const PRE_PROMPT_DRAIN: Duration = Duration::from_millis(1200);
const UPDATE_QUIET: Duration = Duration::from_millis(120);
const UPDATE_DRAIN: Duration = Duration::from_millis(2000);
const LATE_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
const LATE_FLUSH_QUIET: Duration = Duration::from_millis(250);
const LATE_FLUSH_WINDOW: Duration = Duration::from_millis(6000);
const TITLE_REFRESH_DELAYS_MS: [u64; 2] = [1000, 3000];

/// What a prompt's sink receives, in agent order.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Update {
    Message(AgentMessage),
    /// `user_message_chunk` text (agy echoes prompts and steers).
    Echo(String),
}
pub type Sink = mpsc::UnboundedSender<Update>;

pub struct BackendOptions {
    pub command: String,
    pub args: Vec<String>,
    pub env: Option<HashMap<String, String>>,
    pub cwd: Option<PathBuf>,
    pub flavor: Option<String>,
    pub text_chunk_mode: TextChunkMode,
    /// Advertise `elicitation.form`; the flavor answers `elicitation/create`.
    pub elicitation_form: bool,
    pub client_version: String,
}

pub type PermissionListener = Arc<dyn Fn(PermissionRequest) + Send + Sync>;
pub type ElicitationListener = Arc<
    dyn Fn(FormElicitationRequest) -> BoxFuture<'static, FormElicitationResponse> + Send + Sync,
>;
pub type CommandsListener = Arc<dyn Fn(Option<&str>, &[AvailableCommand]) + Send + Sync>;
/// `(sessionId, title)`; `title: None` is an explicit null.
pub type TitleListener = Arc<dyn Fn(Option<&str>, Option<&str>) + Send + Sync>;

#[derive(Default)]
struct Listeners {
    permission: Option<PermissionListener>,
    elicitation: Option<ElicitationListener>,
    commands: Option<CommandsListener>,
    title: Option<TitleListener>,
    stderr: Option<Arc<dyn Fn(StderrError) + Send + Sync>>,
}

struct Elicitation {
    key: u64,
    session_id: String,
    settle: oneshot::Sender<FormElicitationResponse>,
}

#[derive(Default)]
struct State {
    handler: Option<MessageHandler>,
    sink: Option<Sink>,
    active_session: Option<String>,
    initialize: Option<Value>,
    set_mode_supported: Option<bool>,
    models: HashMap<String, SessionModelsMetadata>,
    config_options: HashMap<String, Vec<ConfigOption>>,
    initial_commands: Vec<AvailableCommand>,
    session_commands: HashMap<String, Vec<AvailableCommand>>,
    lifetimes: Option<ToolCallLifetimes>,
    permissions: HashMap<String, oneshot::Sender<Value>>,
    live_requests: HashSet<String>,
    elicitations: Vec<Elicitation>,
    next_key: u64,
    last_update_at: Option<Instant>,
    latest_usage: Option<(Option<f64>, Option<f64>)>,
    forwarded_usage: Option<(Option<f64>, Option<f64>)>,
    usage_sink: Option<Sink>,
    active_prompts: usize,
    foreground_prompts: usize,
    epoch: u64,
    prompt_in_flight: bool,
}
impl State {
    fn lifetimes(&mut self) -> &mut ToolCallLifetimes {
        self.lifetimes.get_or_insert_with(ToolCallLifetimes::new)
    }
}

struct Inner {
    opts: BackendOptions,
    transport: Mutex<Option<Transport>>,
    state: Mutex<State>,
    listeners: Mutex<Listeners>,
    idle: Arc<Notify>,
    complete: Notify,
}

#[derive(Clone)]
pub struct Backend(Arc<Inner>);

/// Prompt timing: when the request was written and when the agent answered.
pub struct PromptOutcome {
    pub sent_at: Option<u64>,
    pub answered_at: Option<u64>,
    pub result: Result<(), AcpError>,
}

/// A steer's stop reason and the usage its response carried.
pub type SteerResult = Result<(Option<String>, Option<Usage>), AcpError>;

pub struct SoftSteer {
    pub dispatched: BoxFuture<'static, Result<(), AcpError>>,
    /// Settles when the agent answers (before late output is drained).
    pub answered: oneshot::Receiver<()>,
    /// The stop reason and the response's usage.
    pub completed: oneshot::Receiver<SteerResult>,
}

fn now_ms() -> u64 {
    crate::session::metadata::now()
}
fn not_initialized() -> AcpError {
    AcpError::other("ACP transport not initialized")
}

pub fn flatten_config_choices(raw: &[Value]) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for e in raw.iter().filter(|e| e.is_object()) {
        if let Some(v) = e["value"].as_str().filter(|s| !s.is_empty()) {
            out.push((v.to_owned(), e["name"].as_str().map(str::to_owned)));
        } else if let Some(nested) = e["options"].as_array().filter(|a| !a.is_empty()) {
            out.extend(flatten_config_choices(nested));
        }
    }
    out
}

pub fn extract_prompt_usage(response: &Value) -> Option<Usage> {
    let u = response.get("usage").filter(|u| u.is_object())?;
    let pick = |keys: &[&str]| keys.iter().find_map(|k| u.get(*k).and_then(finite));
    Some(Usage {
        input_tokens: pick(&["inputTokens", "input_tokens"])?,
        output_tokens: pick(&["outputTokens", "output_tokens"])?,
        total_tokens: pick(&["totalTokens", "total_tokens"]),
        thought_tokens: pick(&["thoughtTokens", "thought_tokens"]),
        cache_read_tokens: pick(&[
            "cachedReadTokens",
            "cached_read_tokens",
            "cachedInputTokens",
            "cached_input_tokens",
        ]),
        cache_creation_tokens: pick(&[
            "cachedWriteTokens",
            "cached_write_tokens",
            "cacheCreationInputTokens",
            "cache_creation_input_tokens",
        ]),
        context_tokens: None,
        context_window: None,
    })
}

/// The agent's own turn count (`usage`, top level or `_meta`).
pub fn extract_num_turns(response: &Value) -> Option<f64> {
    let mut c = vec![];
    if response["usage"].is_object() {
        c.extend([
            &response["usage"]["num_turns"],
            &response["usage"]["numTurns"],
        ]);
    }
    c.extend([&response["num_turns"], &response["numTurns"]]);
    if response["_meta"].is_object() {
        c.extend([
            &response["_meta"]["num_turns"],
            &response["_meta"]["numTurns"],
        ]);
    }
    c.into_iter()
        .find_map(|v| v.as_f64().filter(|f| f.is_finite() && *f > 0.0))
        .map(f64::round)
}

fn parse_commands(source: &Value) -> Option<Vec<AvailableCommand>> {
    let raw = source["availableCommands"]
        .as_array()
        .or_else(|| source["_meta"]["availableCommands"].as_array())?;
    let mut out: Vec<AvailableCommand> = Vec::new();
    for e in raw.iter().filter(|e| e.is_object()) {
        let raw = e["name"].as_str().unwrap_or("").trim();
        let name = raw.strip_prefix('/').map_or(raw, str::trim);
        if name.is_empty() {
            continue;
        }
        let description = e["description"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let cmd = AvailableCommand {
            name: name.into(),
            description,
        };
        if let Some(i) = out.iter().position(|c| c.name == cmd.name) {
            out[i] = cmd;
        } else {
            out.push(cmd);
        }
    }
    Some(out)
}

fn is_placeholder_title(title: &str) -> bool {
    let t = title.trim();
    if t.is_empty() || t == "Untitled" {
        return true;
    }
    ["New session - ", "Child session - "].iter().any(|p| {
        t.strip_prefix(p).is_some_and(|rest| {
            let b = rest.as_bytes();
            b.len() >= 11
                && b[..4].iter().all(u8::is_ascii_digit)
                && b[4] == b'-'
                && b[5..7].iter().all(u8::is_ascii_digit)
                && b[7] == b'-'
                && b[8..10].iter().all(u8::is_ascii_digit)
                && b[10] == b'T'
        })
    })
}

fn with_lifetime_id(state: &mut State, update: &Value) -> Value {
    let kind = update["sessionUpdate"].as_str();
    let start = kind == Some("tool_call");
    if !start && kind != Some("tool_call_update") {
        return update.clone();
    }
    let Some(agent_id) = update["toolCallId"].as_str() else {
        return update.clone();
    };
    let terminal = matches!(update["status"].as_str(), Some("completed" | "failed"));
    let lifetimes = state.lifetimes();
    let event = if start {
        ToolCallEventKind::Start
    } else if terminal {
        ToolCallEventKind::Terminal
    } else {
        ToolCallEventKind::Update
    };
    let mut id = lifetimes.resolve(agent_id, event, MAIN_SCOPE);
    if start && terminal {
        id = lifetimes.resolve(agent_id, ToolCallEventKind::Terminal, MAIN_SCOPE);
    }
    let mut out = update.clone();
    out["toolCallId"] = json!(id);
    out
}

impl Backend {
    pub fn new(opts: BackendOptions) -> Self {
        Self(Arc::new(Inner {
            opts,
            transport: Mutex::default(),
            state: Mutex::default(),
            listeners: Mutex::default(),
            idle: Arc::new(Notify::new()),
            complete: Notify::new(),
        }))
    }
    /// Whether both handles are the same backend.
    pub fn same(&self, other: &Backend) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    fn transport(&self) -> Result<Transport, AcpError> {
        self.0
            .transport
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(not_initialized)
    }
    pub fn is_transport_alive(&self) -> bool {
        self.0
            .transport
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(Transport::is_alive)
    }
    pub fn pid(&self) -> Option<u32> {
        self.0
            .transport
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Transport::pid)
    }
    pub async fn closed(&self) {
        let t = self.0.transport.lock().unwrap().clone();
        match t {
            Some(t) => t.closed().await,
            None => std::future::pending().await,
        }
    }
    pub fn on_permission_request(&self, f: Option<PermissionListener>) {
        self.0.listeners.lock().unwrap().permission = f;
    }
    pub fn on_form_elicitation(&self, f: Option<ElicitationListener>) {
        self.0.listeners.lock().unwrap().elicitation = f;
    }
    pub fn on_available_commands(&self, f: Option<CommandsListener>) {
        self.0.listeners.lock().unwrap().commands = f;
    }
    pub fn on_session_title(&self, f: Option<TitleListener>) {
        self.0.listeners.lock().unwrap().title = f;
    }
    pub fn on_stderr_error(&self, f: Option<Arc<dyn Fn(StderrError) + Send + Sync>>) {
        self.0.listeners.lock().unwrap().stderr = f;
    }

    /// Spawns the agent only (no ACP traffic yet).
    pub fn spawn(&self) -> io::Result<()> {
        if self.0.transport.lock().unwrap().is_some() {
            return Ok(());
        }
        let o = &self.0.opts;
        let t = Transport::spawn(SpawnOptions {
            command: o.command.clone(),
            args: o.args.clone(),
            env: o.env.clone(),
            cwd: o.cwd.clone(),
        })?;
        let weak = Arc::downgrade(&self.0);
        t.on_notification(Some(Arc::new(move |method, params| {
            if let Some(i) = weak.upgrade() {
                if method == "session/update" {
                    Backend(i).handle_session_update(params);
                }
            }
        })));
        let weak = Arc::downgrade(&self.0);
        t.on_stderr_error(Some(Arc::new(move |e| {
            if let Some(i) = weak.upgrade() {
                let f = i.listeners.lock().unwrap().stderr.clone();
                if let Some(f) = f {
                    f(e);
                }
            }
        })));
        let weak = Arc::downgrade(&self.0);
        let permission: RequestHandler = Arc::new(move |params, _| {
            let weak = weak.clone();
            Box::pin(async move {
                let Some(i) = weak.upgrade() else {
                    return Ok(PermissionResponse::Cancelled.to_result());
                };
                Ok(Backend(i).handle_permission_request(params).await)
            })
        });
        t.register_request_handler("session/request_permission", permission);
        if o.elicitation_form {
            let weak = Arc::downgrade(&self.0);
            let elicitation: RequestHandler = Arc::new(move |params, _| {
                let weak = weak.clone();
                Box::pin(async move {
                    let Some(i) = weak.upgrade() else {
                        return Ok(FormElicitationResponse::Cancel.to_result());
                    };
                    Ok(Backend(i).handle_elicitation(params).await.to_result())
                })
            });
            t.register_request_handler("elicitation/create", elicitation);
        }
        *self.0.transport.lock().unwrap() = Some(t);
        let weak = Arc::downgrade(&self.0);
        tokio::spawn(async move { idle_flusher(weak).await });
        Ok(())
    }

    /// Spawn (if needed) and `initialize`, retried 3 times.
    pub async fn initialize(&self) -> Result<(), AcpError> {
        if self.0.state.lock().unwrap().initialize.is_some() {
            return Ok(());
        }
        self.spawn().map_err(|e| AcpError::closed(e.to_string()))?;
        let o = &self.0.opts;
        let mut caps = json!({"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false});
        if o.elicitation_form {
            caps["elicitation"] = json!({"form":{}});
        }
        caps["_meta"] = json!({"parameterizedModelPicker":true});
        let params = json!({"protocolVersion":1,"clientCapabilities":caps,"clientInfo":{"name":"hapi","version":o.client_version}});
        let response = retry(
            3,
            |_| true,
            || {
                let p = params.clone();
                let this = self.clone();
                async move {
                    this.transport()?
                        .request("initialize", p, Some(super::transport::DEFAULT_TIMEOUT))
                        .await
                }
            },
        )
        .await?;
        if !response["protocolVersion"].is_number() {
            return Err(AcpError::other(
                "Invalid initialize response from ACP agent",
            ));
        }
        self.capture_commands(None, &response);
        self.0.state.lock().unwrap().initialize = Some(response);
        Ok(())
    }

    pub fn supports_session_resume(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .initialize
            .as_ref()
            .is_some_and(|r| {
                r["agentCapabilities"]["sessionCapabilities"]
                    .as_object()
                    .is_some_and(|c| c.contains_key("resume"))
            })
    }
    pub fn supports_image_prompts(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .initialize
            .as_ref()
            .is_some_and(|r| r["agentCapabilities"]["promptCapabilities"]["image"] == true)
    }

    async fn open_session(
        &self,
        method: &str,
        session_id: Option<&str>,
        cwd: &str,
    ) -> Result<String, AcpError> {
        let mut params = json!({"cwd":cwd,"mcpServers":[]});
        if let Some(id) = session_id {
            params = json!({"sessionId":id,"cwd":cwd,"mcpServers":[]});
        }
        let resume = method == "session/resume";
        let response = retry(
            3,
            |e: &AcpError| !resume || e.indeterminate,
            || {
                let p = params.clone();
                let this = self.clone();
                let m = method.to_owned();
                async move {
                    this.transport()?
                        .request(&m, p, Some(super::transport::DEFAULT_TIMEOUT))
                        .await
                }
            },
        )
        .await?;
        let id = match (response["sessionId"].as_str(), session_id) {
            (Some(id), _) => id.to_owned(),
            (None, Some(id)) => id.to_owned(),
            (None, None) => {
                return Err(AcpError::other(
                    "Invalid session/new response from ACP agent",
                ))
            }
        };
        self.set_active_session(&id);
        self.capture_session_metadata(&id, &response);
        Ok(id)
    }
    pub async fn new_session(&self, cwd: &str) -> Result<String, AcpError> {
        self.open_session("session/new", None, cwd).await
    }
    pub async fn resume_session(&self, session_id: &str, cwd: &str) -> Result<String, AcpError> {
        self.open_session("session/resume", Some(session_id), cwd)
            .await
    }

    fn set_active_session(&self, id: &str) {
        let mut s = self.0.state.lock().unwrap();
        if s.active_session.as_deref() == Some(id) {
            return;
        }
        if s.active_session.is_some() {
            s.lifetimes = Some(ToolCallLifetimes::new());
        }
        s.active_session = Some(id.into());
    }

    pub fn session_models(&self, session_id: &str) -> Option<SessionModelsMetadata> {
        self.0.state.lock().unwrap().models.get(session_id).cloned()
    }
    pub fn config_option_by_category(
        &self,
        session_id: &str,
        category: &str,
    ) -> Option<ConfigOption> {
        self.0
            .state
            .lock()
            .unwrap()
            .config_options
            .get(session_id)
            .and_then(|o| {
                o.iter()
                    .find(|o| o.category.as_deref() == Some(category))
                    .cloned()
            })
    }
    pub fn available_commands(&self, session_id: Option<&str>) -> Vec<AvailableCommand> {
        let s = self.0.state.lock().unwrap();
        session_id
            .and_then(|id| s.session_commands.get(id))
            .unwrap_or(&s.initial_commands)
            .clone()
    }

    /// Resolves once no prompt request (main or steer) is active.
    pub async fn wait_for_response_complete(&self) {
        loop {
            let notified = self.0.complete.notified();
            if self.0.state.lock().unwrap().active_prompts == 0 {
                return;
            }
            notified.await;
        }
    }

    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), AcpError> {
        let t = self.transport()?;
        self.wait_for_response_complete().await;
        if self.0.state.lock().unwrap().set_mode_supported != Some(false) {
            match t
                .request(
                    "session/set_mode",
                    json!({"sessionId":session_id,"modeId":mode_id}),
                    Some(super::transport::DEFAULT_TIMEOUT),
                )
                .await
            {
                Ok(_) => {
                    let mut s = self.0.state.lock().unwrap();
                    s.set_mode_supported = Some(true);
                    if let Some(opts) = s.config_options.get_mut(session_id) {
                        for o in opts
                            .iter_mut()
                            .filter(|o| o.category.as_deref() == Some("thought_level"))
                        {
                            o.current_value = Some(mode_id.into());
                        }
                    }
                    return Ok(());
                }
                Err(e) if e.message.to_lowercase().contains("method not found") => {
                    self.0.state.lock().unwrap().set_mode_supported = Some(false);
                }
                Err(e) => return Err(e),
            }
        }
        let Some(option) = self.config_option_by_category(session_id, "mode") else {
            return Err(AcpError::other("ACP agent does not support session/set_mode and no mode config option is available"));
        };
        self.set_config_option(session_id, &option.id, mode_id)
            .await
    }

    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<(), AcpError> {
        let t = self.transport()?;
        self.wait_for_response_complete().await;
        let r = t
            .request(
                "session/set_config_option",
                json!({"sessionId":session_id,"configId":config_id,"value":value}),
                Some(super::transport::DEFAULT_TIMEOUT),
            )
            .await?;
        self.capture_session_metadata(session_id, &r);
        Ok(())
    }

    pub async fn set_model(&self, session_id: &str, model_id: &str) -> Result<(), AcpError> {
        let t = self.transport()?;
        self.wait_for_response_complete().await;
        let r = t
            .request(
                "session/set_model",
                json!({"sessionId":session_id,"modelId":model_id}),
                Some(super::transport::DEFAULT_TIMEOUT),
            )
            .await?;
        self.capture_session_metadata(session_id, &r);
        Ok(())
    }

    /// Reads the agent's native title through `session/list` (retried while
    /// the title is a placeholder). Runs in the background.
    pub fn refresh_session_info(&self, session_id: &str, cwd: &str) {
        let (this, id, cwd) = (self.clone(), session_id.to_owned(), cwd.to_owned());
        tokio::spawn(async move {
            for attempt in 0..=TITLE_REFRESH_DELAYS_MS.len() {
                let Ok(t) = this.transport() else { return };
                let Ok(r) = t
                    .request(
                        "session/list",
                        json!({"cwd":cwd}),
                        Some(Duration::from_secs(5)),
                    )
                    .await
                else {
                    return;
                };
                let Some(m) = r["sessions"]
                    .as_array()
                    .and_then(|a| a.iter().find(|e| e["sessionId"] == id.as_str()))
                else {
                    return;
                };
                if !(m["title"].is_string() || m["title"].is_null()) || m.get("title").is_none() {
                    return;
                }
                let title = m["title"].as_str();
                let f = this.0.listeners.lock().unwrap().title.clone();
                if let Some(f) = f {
                    f(Some(&id), title);
                }
                if title.is_none_or(|t| !is_placeholder_title(t)) {
                    return;
                }
                let Some(delay) = TITLE_REFRESH_DELAYS_MS.get(attempt) else {
                    return;
                };
                tokio::time::sleep(Duration::from_millis(*delay)).await;
            }
        });
    }

    fn begin_prompt_request(&self) -> u64 {
        let mut s = self.0.state.lock().unwrap();
        s.active_prompts += 1;
        s.epoch
    }
    fn finish_prompt_request(&self, epoch: u64) {
        let mut s = self.0.state.lock().unwrap();
        if epoch != s.epoch {
            return;
        }
        s.active_prompts = s.active_prompts.saturating_sub(1);
        if s.active_prompts == 0 {
            self.0.complete.notify_waiters();
        }
    }
    pub fn is_prompt_request_in_flight(&self) -> bool {
        self.0.state.lock().unwrap().prompt_in_flight
    }
    pub fn is_processing(&self) -> bool {
        self.0.state.lock().unwrap().active_prompts > 0
    }

    async fn wait_quiet(&self, quiet: Duration, timeout: Duration, baseline: Option<Instant>) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let last = self.0.state.lock().unwrap().last_update_at;
            let anchor = match (last, baseline) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            let Some(anchor) = anchor else { return };
            let elapsed = Instant::now().saturating_duration_since(anchor);
            if elapsed >= quiet {
                return;
            }
            let wait = (quiet - elapsed)
                .min(deadline.saturating_duration_since(Instant::now()))
                .max(Duration::from_millis(1));
            tokio::time::sleep(wait).await;
        }
    }
    async fn drain_late(&self) {
        let baseline = Instant::now();
        let deadline = baseline + LATE_FLUSH_WINDOW;
        while Instant::now() < deadline {
            let last = self
                .0
                .state
                .lock()
                .unwrap()
                .last_update_at
                .map_or(baseline, |l| l.max(baseline));
            if Instant::now().saturating_duration_since(last) >= LATE_FLUSH_QUIET {
                return;
            }
            let wait = LATE_FLUSH_INTERVAL
                .min(deadline.saturating_duration_since(Instant::now()))
                .max(Duration::from_millis(1));
            tokio::time::sleep(wait).await;
            self.drain_handler();
        }
    }
    fn drain_handler(&self) {
        if let Some(h) = self.0.state.lock().unwrap().handler.as_mut() {
            h.drain_buffers();
        }
    }
    fn send(&self, sink: &Sink, m: AgentMessage) {
        let _ = sink.send(Update::Message(m));
    }

    /// One foreground turn. Messages go to `sink`; `answered` fires when the
    /// agent answers (before late output is drained).
    pub async fn prompt(
        &self,
        session_id: &str,
        content: Vec<Value>,
        sink: Sink,
        answered: Option<oneshot::Sender<()>>,
    ) -> PromptOutcome {
        let t = match self.transport() {
            Ok(t) => t,
            Err(e) => {
                return PromptOutcome {
                    sent_at: None,
                    answered_at: None,
                    result: Err(e),
                }
            }
        };
        self.set_active_session(session_id);
        self.wait_quiet(PRE_PROMPT_QUIET, PRE_PROMPT_DRAIN, None)
            .await;
        let epoch = {
            let mut s = self.0.state.lock().unwrap();
            if let Some(h) = s.handler.as_mut() {
                h.drain_buffers();
            }
            let tx = sink.clone();
            s.handler = Some(MessageHandler::new(
                Box::new(move |m| {
                    let _ = tx.send(Update::Message(m));
                }),
                MessageHandlerOptions {
                    text_chunk_mode: self.0.opts.text_chunk_mode,
                    flavor: self.0.opts.flavor.clone(),
                    text_idle_flush_ms: None,
                },
            ));
            s.sink = Some(sink.clone());
            s.lifetimes().begin_generation(None);
            s.foreground_prompts += 1;
            s.last_update_at = Some(Instant::now());
            s.latest_usage = None;
            s.forwarded_usage = None;
            s.usage_sink = Some(sink.clone());
            s.prompt_in_flight = true;
            drop(s);
            self.begin_prompt_request()
        };
        let request = t.request(
            "session/prompt",
            json!({"sessionId":session_id,"prompt":content}),
            None,
        );
        let sent_at = Some(now_ms());
        let response = request.await;
        self.0.state.lock().unwrap().prompt_in_flight = false;
        let answered_at = Some(now_ms());
        if let Some(a) = answered {
            let _ = a.send(());
        }
        let (stop, usage, turns) = match &response {
            Ok(r) => (
                r["stopReason"].as_str().map(str::to_owned),
                extract_prompt_usage(r),
                extract_num_turns(r),
            ),
            Err(_) => (None, None, None),
        };
        self.wait_quiet(UPDATE_QUIET, UPDATE_DRAIN, None).await;
        self.drain_handler();
        self.drain_late().await;
        self.drain_handler();
        let latest = {
            let mut s = self.0.state.lock().unwrap();
            s.lifetimes().settle_cancelled();
            s.latest_usage
        };
        if let Some(mut u) = usage {
            u.context_tokens = latest.and_then(|l| l.0);
            u.context_window = latest.and_then(|l| l.1);
            self.send(&sink, AgentMessage::Usage(u));
        } else if let Some(l) = latest.filter(|l| l.0.is_some() || l.1.is_some()) {
            if self.0.state.lock().unwrap().forwarded_usage != Some(l) {
                self.send(
                    &sink,
                    AgentMessage::Usage(Usage {
                        context_tokens: l.0,
                        context_window: l.1,
                        ..Default::default()
                    }),
                );
            }
        }
        if let Some(stop_reason) = stop {
            self.send(
                &sink,
                AgentMessage::TurnComplete {
                    stop_reason,
                    num_turns: turns,
                },
            );
        }
        {
            let mut s = self.0.state.lock().unwrap();
            s.usage_sink = None;
            s.foreground_prompts = s.foreground_prompts.saturating_sub(1);
            if epoch != s.epoch {
                s.active_prompts = s.active_prompts.saturating_sub(1);
                if s.active_prompts == 0 {
                    self.0.complete.notify_waiters();
                }
                drop(s);
            } else {
                drop(s);
                self.finish_prompt_request(epoch);
            }
        }
        PromptOutcome {
            sent_at,
            answered_at,
            result: response.map(|_| ()),
        }
    }

    /// A concurrent `session/prompt` folded into the running turn (steer).
    pub fn begin_soft_steer(
        &self,
        session_id: &str,
        content: Vec<Value>,
    ) -> Result<SoftSteer, AcpError> {
        let t = self.transport()?;
        if !self.is_processing() {
            return Err(AcpError::other("No active ACP prompt to soft-steer into"));
        }
        let epoch = self.begin_prompt_request();
        let (dispatched, completed) = t.request_with_dispatch(
            "session/prompt",
            json!({"sessionId":session_id,"prompt":content}),
            None,
            Some(Duration::from_secs(20)),
        );
        let (atx, arx) = oneshot::channel();
        let (ctx, crx) = oneshot::channel();
        let this = self.clone();
        tokio::spawn(async move {
            let response = completed.await;
            let _ = atx.send(());
            let result = response.map(|r| {
                let usage = extract_prompt_usage(&r).map(|mut u| {
                    let l = this.0.state.lock().unwrap().latest_usage;
                    u.context_tokens = l.and_then(|l| l.0);
                    u.context_window = l.and_then(|l| l.1);
                    u
                });
                (r["stopReason"].as_str().map(str::to_owned), usage)
            });
            this.wait_quiet(UPDATE_QUIET, UPDATE_DRAIN, None).await;
            this.drain_handler();
            this.drain_late().await;
            this.drain_handler();
            this.finish_prompt_request(epoch);
            let _ = ctx.send(result);
        });
        Ok(SoftSteer {
            dispatched,
            answered: arx,
            completed: crx,
        })
    }

    pub fn cancel_prompt(&self, session_id: &str) {
        let Ok(t) = self.transport() else { return };
        self.0.state.lock().unwrap().lifetimes().cancel(None);
        t.notify("session/cancel", json!({"sessionId":session_id}));
        self.settle_elicitations(Some(session_id));
    }

    /// Drop soft-steer bookkeeping after an abort.
    pub fn abort_soft_steers(&self) {
        let mut s = self.0.state.lock().unwrap();
        if let Some(h) = s.handler.as_mut() {
            h.drain_buffers();
            h.deactivate();
        }
        s.epoch += 1;
        s.active_prompts = s.foreground_prompts;
        if s.active_prompts == 0 {
            self.0.complete.notify_waiters();
        }
    }

    pub fn respond_to_permission(&self, request: &PermissionRequest, response: PermissionResponse) {
        let pending = {
            let mut s = self.0.state.lock().unwrap();
            let Some(p) = s.permissions.remove(&request.id) else {
                return;
            };
            s.live_requests.remove(&request.id);
            s.lifetimes().settle_request(&request.tool_call_id);
            p
        };
        let _ = pending.send(response.to_result());
    }
    pub fn has_live_request(&self, id: &str) -> bool {
        self.0.state.lock().unwrap().live_requests.contains(id)
    }

    fn settle_elicitations(&self, session_id: Option<&str>) {
        let settled: Vec<_> = {
            let mut s = self.0.state.lock().unwrap();
            let (out, keep) = std::mem::take(&mut s.elicitations)
                .into_iter()
                .partition(|e: &Elicitation| session_id.is_none_or(|id| e.session_id == id));
            s.elicitations = keep;
            out
        };
        for e in settled {
            let _ = e.key;
            let _ = e.settle.send(FormElicitationResponse::Cancel);
        }
    }

    async fn handle_permission_request(&self, params: Value) -> Value {
        if !params.is_object() {
            return PermissionResponse::Cancelled.to_result();
        }
        let (request, rx) = {
            let mut s = self.0.state.lock().unwrap();
            let session_id = params["sessionId"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| s.active_session.clone())
                .unwrap_or_else(|| "unknown".into());
            let tool = &params["toolCall"];
            let (tool_call_id, id) = match tool["toolCallId"].as_str() {
                Some(agent) => {
                    let l = s.lifetimes();
                    let lifetime = l.resolve(agent, ToolCallEventKind::Request, MAIN_SCOPE);
                    let id = l.allocate_request_id(&lifetime);
                    (lifetime, id)
                }
                None => {
                    let id = format!("tool-{}", uuid::Uuid::new_v4());
                    (id.clone(), id)
                }
            };
            let options = params["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| o.is_object())
                .enumerate()
                .map(|(i, o)| PermissionOption {
                    option_id: o["optionId"]
                        .as_str()
                        .map_or_else(|| format!("option-{}", i + 1), str::to_owned),
                    name: o["name"]
                        .as_str()
                        .map_or_else(|| format!("Option {}", i + 1), str::to_owned),
                    kind: o["kind"].as_str().unwrap_or("allow_once").into(),
                })
                .collect();
            let request = PermissionRequest {
                id: id.clone(),
                session_id,
                tool_call_id,
                title: tool["title"].as_str().map(str::to_owned),
                kind: tool["kind"].as_str().map(str::to_owned),
                raw_input: tool.get("rawInput").cloned(),
                raw_output: tool.get("rawOutput").cloned(),
                options,
            };
            let (tx, rx) = oneshot::channel();
            s.permissions.insert(id.clone(), tx);
            s.live_requests.insert(id);
            (request, rx)
        };
        let listener = self.0.listeners.lock().unwrap().permission.clone();
        let Some(listener) = listener else {
            let mut s = self.0.state.lock().unwrap();
            s.permissions.remove(&request.id);
            s.live_requests.remove(&request.id);
            return PermissionResponse::Cancelled.to_result();
        };
        listener(request);
        rx.await
            .unwrap_or_else(|_| PermissionResponse::Cancelled.to_result())
    }

    async fn handle_elicitation(&self, params: Value) -> FormElicitationResponse {
        let listener = self.0.listeners.lock().unwrap().elicitation.clone();
        let (Some(listener), Some(session_id)) = (listener, params["sessionId"].as_str()) else {
            return FormElicitationResponse::Cancel;
        };
        if params["mode"] != "form" || !params["requestedSchema"].is_object() {
            return FormElicitationResponse::Cancel;
        }
        let (request, rx, key) = {
            let mut s = self.0.state.lock().unwrap();
            let (tool_call_id, request_id) = match params["toolCallId"].as_str() {
                Some(agent) => {
                    let l = s.lifetimes();
                    let lifetime = l.resolve(agent, ToolCallEventKind::Request, MAIN_SCOPE);
                    let req = l.allocate_request_id(&lifetime);
                    (Some(lifetime), Some(req))
                }
                None => (None, None),
            };
            let (tx, rx) = oneshot::channel();
            s.next_key += 1;
            let key = s.next_key;
            s.elicitations.push(Elicitation {
                key,
                session_id: session_id.into(),
                settle: tx,
            });
            if let Some(r) = &request_id {
                s.live_requests.insert(r.clone());
            }
            (
                FormElicitationRequest {
                    session_id: session_id.into(),
                    tool_call_id,
                    request_id,
                    message: params["message"].as_str().unwrap_or("").into(),
                    requested_schema: params["requestedSchema"].clone(),
                },
                rx,
                key,
            )
        };
        let request_id = request.request_id.clone();
        let tool_call_id = request.tool_call_id.clone();
        let answer = listener(request);
        let response = tokio::select! {
            r = answer => r,
            r = rx => r.unwrap_or(FormElicitationResponse::Cancel),
        };
        let mut s = self.0.state.lock().unwrap();
        s.elicitations.retain(|e| e.key != key);
        if let Some(r) = request_id {
            s.live_requests.remove(&r);
        }
        if let Some(t) = tool_call_id {
            s.lifetimes().settle_request(&t);
        }
        response
    }

    fn handle_session_update(&self, params: &Value) {
        if !params.is_object() {
            return;
        }
        let session_id = params["sessionId"].as_str();
        let raw = &params["update"];
        let (update, echo, sink) = {
            let mut s = self.0.state.lock().unwrap();
            if let (Some(active), Some(id)) = (&s.active_session, session_id) {
                if active != id {
                    return;
                }
            }
            s.last_update_at = Some(Instant::now());
            let update = if s.handler.is_some() {
                with_lifetime_id(&mut s, raw)
            } else {
                raw.clone()
            };
            let echo = (update["sessionUpdate"] == "user_message_chunk"
                && update["content"]["type"] == "text")
                .then(|| update["content"]["text"].as_str().map(str::to_owned))
                .flatten();
            (update, echo, s.sink.clone())
        };
        if let Some(id) = session_id {
            self.capture_commands(Some(id), &update);
        }
        if update["sessionUpdate"] == "session_info_update"
            && (update["title"].is_string() || update["title"].is_null())
            && update.get("title").is_some()
        {
            let f = self.0.listeners.lock().unwrap().title.clone();
            if let Some(f) = f {
                f(session_id, update["title"].as_str());
            }
        }
        self.capture_usage(&update);
        let mut s = self.0.state.lock().unwrap();
        if let Some(h) = s.handler.as_mut() {
            h.handle_update(&update);
            if echo.is_some() {
                h.drain_buffers();
            }
        }
        drop(s);
        self.0.idle.notify_one();
        if let (Some(text), Some(sink)) = (echo, sink) {
            let _ = sink.send(Update::Echo(text));
        }
    }

    fn capture_usage(&self, update: &Value) {
        let n = |keys: &[&str]| keys.iter().find_map(|k| update.get(*k).and_then(finite));
        let (tokens, window) = match update["sessionUpdate"].as_str() {
            Some("usage_update") => (n(&["used"]), n(&["size"])),
            Some("session_info_update") => {
                let pick = |keys: &[&str]| {
                    keys.iter()
                        .find_map(|k| update.get(*k).filter(|v| !v.is_null()))
                        .and_then(finite)
                };
                (
                    pick(&["used", "contextTokens", "context_tokens", "contextUsed"]),
                    pick(&["size", "contextWindow", "context_window", "contextLimit"]),
                )
            }
            _ => return,
        };
        let sink = {
            let mut s = self.0.state.lock().unwrap();
            s.latest_usage = Some((tokens, window));
            if (tokens.is_none() && window.is_none()) || s.forwarded_usage == Some((tokens, window))
            {
                return;
            }
            s.forwarded_usage = Some((tokens, window));
            s.usage_sink.clone()
        };
        if let Some(sink) = sink {
            self.send(
                &sink,
                AgentMessage::Usage(Usage {
                    context_tokens: tokens,
                    context_window: window,
                    ..Default::default()
                }),
            );
        }
    }

    fn capture_commands(&self, session_id: Option<&str>, source: &Value) {
        let Some(commands) = parse_commands(source) else {
            return;
        };
        {
            let mut s = self.0.state.lock().unwrap();
            match session_id {
                Some(id) => {
                    s.session_commands.insert(id.into(), commands.clone());
                }
                None => s.initial_commands = commands.clone(),
            }
        }
        let f = self.0.listeners.lock().unwrap().commands.clone();
        if let Some(f) = f {
            f(session_id, &commands);
        }
    }

    fn capture_session_metadata(&self, session_id: &str, r: &Value) {
        if !r.is_object() {
            return;
        }
        self.capture_models(session_id, r);
        let options: Vec<ConfigOption> = r["configOptions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                Some(ConfigOption {
                    id: e["id"].as_str()?.into(),
                    category: e["category"].as_str().map(str::to_owned),
                    current_value: e["currentValue"].as_str().map(str::to_owned),
                    options: flatten_config_choices(e["options"].as_array().map_or(&[], |v| v)),
                })
            })
            .collect();
        if !options.is_empty() {
            self.0
                .state
                .lock()
                .unwrap()
                .config_options
                .insert(session_id.into(), options);
        }
        self.capture_commands(Some(session_id), r);
    }

    fn capture_models(&self, session_id: &str, r: &Value) {
        let config = r["configOptions"].as_array().and_then(|a| {
            a.iter()
                .find(|e| e["category"] == "model" || e["id"] == "model")
        });
        let raw_models = r["availableModels"]
            .as_array()
            .or_else(|| r["models"]["availableModels"].as_array())
            .or_else(|| config.and_then(|c| c["options"].as_array()));
        let raw_current = r["currentModelId"]
            .as_str()
            .or_else(|| r["models"]["currentModelId"].as_str())
            .or_else(|| config.and_then(|c| c["currentValue"].as_str()));
        if raw_models.is_none() && raw_current.is_none() {
            return;
        }
        let mut s = self.0.state.lock().unwrap();
        let existing = s.models.get(session_id).cloned().unwrap_or_default();
        let mut models: Vec<ModelDescriptor> = Vec::new();
        let mut add = |id: &str, name: Option<&str>, efforts: Option<Vec<ReasoningEffort>>| {
            let id = id.trim();
            if id.is_empty() {
                return;
            }
            let name = name.map(str::trim).filter(|n| !n.is_empty() && *n != id);
            if let Some(m) = models.iter_mut().find(|m| m.model_id == id) {
                if m.name.is_none() {
                    m.name = name.map(str::to_owned);
                }
                return;
            }
            models.push(ModelDescriptor {
                model_id: id.into(),
                name: name.map(str::to_owned),
                reasoning_efforts: efforts,
            });
        };
        match raw_models {
            Some(list) => {
                for e in list.iter().filter(|e| e.is_object()) {
                    let Some(id) = e["modelId"].as_str().or_else(|| e["value"].as_str()) else {
                        continue;
                    };
                    let efforts = e["_meta"]["reasoningEfforts"].as_array().map(|a| {
                        a.iter()
                            .filter(|x| x.is_object())
                            .filter_map(|x| {
                                let value = x["value"].as_str().or_else(|| x["id"].as_str())?;
                                (!value.is_empty()).then(|| ReasoningEffort {
                                    value: value.into(),
                                    name: x["label"].as_str().map(str::to_owned),
                                    is_default: x["default"] == true,
                                })
                            })
                            .collect()
                    });
                    add(id, e["name"].as_str(), efforts);
                }
            }
            None => {
                for m in &existing.available_models {
                    add(&m.model_id, m.name.as_deref(), m.reasoning_efforts.clone());
                }
            }
        }
        if let Some(c) = config {
            for (v, n) in flatten_config_choices(c["options"].as_array().map_or(&[], |v| v)) {
                add(&v, n.as_deref(), None);
            }
        }
        let current = raw_current.map(str::to_owned).or(existing.current_model_id);
        s.models.insert(
            session_id.into(),
            SessionModelsMetadata {
                available_models: models,
                current_model_id: current,
            },
        );
    }

    /// Ends the agent process and settles everything still waiting on it.
    pub async fn disconnect(&self) {
        let t = self.0.transport.lock().unwrap().take();
        let Some(t) = t else { return };
        let pending = {
            let mut s = self.0.state.lock().unwrap();
            if let Some(h) = s.handler.as_mut() {
                h.drain_buffers();
            }
            s.handler = None;
            s.sink = None;
            s.usage_sink = None;
            s.active_session = None;
            s.active_prompts = 0;
            s.foreground_prompts = 0;
            s.models.clear();
            s.initial_commands.clear();
            s.session_commands.clear();
            s.initialize = None;
            std::mem::take(&mut s.permissions)
        };
        self.0.complete.notify_waiters();
        self.0.idle.notify_one();
        self.settle_elicitations(None);
        drop(pending);
        t.close().await;
    }
}

impl PermissionBackend for Backend {
    fn respond_to_permission(&self, request: &PermissionRequest, response: PermissionResponse) {
        Backend::respond_to_permission(self, request, response);
    }
    fn cancel_prompt(&self, session_id: &str) {
        Backend::cancel_prompt(self, session_id);
    }
    fn has_live_request(&self, request_id: &str) -> bool {
        Backend::has_live_request(self, request_id)
    }
}

/// Arms one timer only while a text buffer waits for its idle flush.
async fn idle_flusher(weak: std::sync::Weak<Inner>) {
    loop {
        let Some(inner) = weak.upgrade() else { return };
        if inner.transport.lock().unwrap().is_none() {
            return;
        }
        let idle = inner.idle.clone();
        let notified = idle.notified();
        let deadline = inner
            .state
            .lock()
            .unwrap()
            .handler
            .as_ref()
            .and_then(MessageHandler::idle_deadline);
        drop(inner);
        match deadline {
            Some(d) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(d) => {
                        let Some(inner) = weak.upgrade() else { return };
                        let mut s = inner.state.lock().unwrap();
                        if let Some(h) = s.handler.as_mut() {
                            if h.idle_deadline().is_some_and(|d| d <= Instant::now()) {
                                h.flush_idle();
                            }
                        }
                    }
                    _ = notified => {}
                }
            }
            None => notified.await,
        }
    }
}

async fn retry<T, F, Fut>(
    attempts: u32,
    should_retry: impl Fn(&AcpError) -> bool,
    mut f: F,
) -> Result<T, AcpError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, AcpError>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                attempt += 1;
                if !should_retry(&e) || attempt >= attempts {
                    return Err(e);
                }
                let delay = (1000u64 << (attempt - 1)).min(5000);
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_and_turns_extraction() {
        let r = json!({"stopReason":"end_turn","usage":{"inputTokens":10,"output_tokens":5,"thoughtTokens":2,"cachedReadTokens":1}});
        let u = extract_prompt_usage(&r).unwrap();
        assert_eq!(
            (
                u.input_tokens,
                u.output_tokens,
                u.thought_tokens,
                u.cache_read_tokens
            ),
            (10.0, 5.0, Some(2.0), Some(1.0))
        );
        assert!(extract_prompt_usage(&json!({"usage":{"inputTokens":1}})).is_none());
        assert_eq!(
            extract_num_turns(&json!({"_meta":{"numTurns":2.4}})),
            Some(2.0)
        );
        assert_eq!(extract_num_turns(&json!({"numTurns":0})), None);
    }

    #[test]
    fn commands_and_titles() {
        let c = parse_commands(&json!({"_meta":{"availableCommands":[{"name":"/plan ","description":" Plan "},{"name":""},{"name":"x"}]}})).unwrap();
        assert_eq!(
            c,
            vec![
                AvailableCommand {
                    name: "plan".into(),
                    description: Some("Plan".into())
                },
                AvailableCommand {
                    name: "x".into(),
                    description: None
                }
            ]
        );
        assert!(is_placeholder_title("New session - 2026-01-02T03:04"));
        assert!(is_placeholder_title(" Untitled "));
        assert!(!is_placeholder_title("Fix the bug"));
        assert_eq!(
            flatten_config_choices(&[
                json!({"group":"g","options":[{"value":"a","name":"A"}]}),
                json!({"value":"b"})
            ]),
            vec![("a".into(), Some("A".into())), ("b".into(), None)]
        );
    }
}
