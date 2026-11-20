//! agy over `agy acp` (TS `agyAcpRemoteLauncher.ts`). One actor owns all
//! turn state; RPCs, agent requests and backend callbacks reach it through
//! channels, so agent output keeps its order.
use super::{
    model_selection::{agy_acp_mode_for_permission, resolve_agy_acp_model_id},
    question::{
        agy_question_content, parse_agy_question_form, question_card_input,
        question_card_input_from_tool_call, shown_labels, QUESTION_TOOL_NAME,
    },
    reactive_wakeup::{
        add_agy_usage, advance_agy_turn_transcript_cursor, agy_transcript_paths,
        convert_agy_reactive_steps, extract_agy_background_task_ids, inspect_agy_reactive_steps,
        inspect_synthetic_background_command_match, read_agy_transcript_steps_after,
        read_latest_agy_transcript_step_index, resolve_agy_background_task_ids,
        AgyLiveBackgroundCompletionLookup, AgyOpenBackgroundCommand, AgyReactiveInspection,
        AgyReactiveTimingConfig, AgySyntheticCommandMatch, AgyTranscriptStep,
        AGY_WAKEUP_CONTINUE_PROMPT,
    },
    swarm_board::{format_agy_swarm_board, parse_agy_swarm_board_command},
    title::{apply_title_fallback, TitleSync},
    tool_cards::{background_command_result, map_agy_tool_call, map_agy_tool_result},
};
use hapi_wrapper_core::{
    acp::{
        backend::{Backend, Sink, SteerResult, Update},
        generation_clock::GenerationClock,
        permission::{AcpPermissionHandler, AcpPermissionOptions},
        transport::AcpError,
        AgentMessage, AvailableCommand, FormElicitationRequest, FormElicitationResponse,
        PermissionRequest, PermissionResponse, ToolCall, Usage,
    },
    agent::message_converter::{convert_agent_message, num},
    queue::{MessageQueue2, Reservation},
    session::{client::SessionClient, metadata::now},
    watch::DirWatch,
};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
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
    time::Instant,
};

const STEER_CANCEL_GRACE: Duration = Duration::from_secs(3);
const CANCEL_HARD_STOP: Duration = Duration::from_secs(10);
const PROMPT_SEND_POLL: Duration = Duration::from_millis(50);
const ECHO_BUFFER_LIMIT: usize = 64 * 1024;
const WAKEUP_TICK: Duration = Duration::from_millis(50);
const TIMINGS: AgyReactiveTimingConfig = AgyReactiveTimingConfig {
    settle_ms: 250,
    tool_park_ms: 1_500,
    slow_tool_park_ms: 10_000,
    max_idle_ms: 600_000,
};

/// The batch key: `{permissionMode, model, effort}` (TS `hashObject`).
pub fn mode_key(mode: &Value) -> String {
    json!({"permissionMode":mode["permissionMode"],"model":mode["model"],"effort":mode["effort"]})
        .to_string()
}

pub enum Command {
    Abort(oneshot::Sender<()>),
    Steer(String, oneshot::Sender<Value>),
    Kill(oneshot::Sender<()>),
    ListSlashCommands(oneshot::Sender<Value>),
    Question(
        FormElicitationRequest,
        oneshot::Sender<FormElicitationResponse>,
    ),
}

/// State shared with RPC handlers and the run loop.
pub struct Session {
    pub client: Arc<SessionClient>,
    pub queue: Mutex<MessageQueue2<Value>>,
    pub changed: Notify,
    pub mode: Mutex<Value>,
    pub cwd: String,
    thinking: AtomicBool,
    pub tx: mpsc::UnboundedSender<Command>,
}
impl Session {
    pub fn new(
        client: Arc<SessionClient>,
        mode: Value,
        cwd: String,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<Command>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                client,
                queue: Mutex::new(MessageQueue2::new(mode_key)),
                changed: Notify::new(),
                mode: Mutex::new(mode),
                cwd,
                thinking: false.into(),
                tx,
            }),
            rx,
        )
    }
    pub fn runtime(&self) -> Value {
        let m = self.mode.lock().unwrap();
        json!({"permissionMode":m["permissionMode"],"model":m["model"],"effort":m["effort"]})
    }
    pub fn keep_alive(&self) {
        self.client.keep_alive(
            self.thinking.load(Ordering::Relaxed),
            Some("remote"),
            &self.runtime(),
        );
    }
    fn thinking(&self, b: bool) {
        self.thinking.store(b, Ordering::Relaxed);
        self.keep_alive();
    }
    pub fn found(&self, id: &str) {
        let id = id.to_owned();
        self.client.metadata.update(move |m| {
            let mut m = m.clone();
            m["agySessionId"] = json!(id);
            m
        });
    }
    fn event(&self, v: Value) {
        self.client.send_session_event(v, None, None);
    }
    fn notice(&self, s: &str) {
        self.event(json!({"type":"message","message":s}));
    }
    fn steering(&self, active: bool) {
        self.client.agent_state.update(move |s| {
            let mut s = s.clone();
            s["steeringActive"] = json!(active);
            s
        });
    }
    fn queue_size(&self) -> usize {
        self.queue.lock().unwrap().size()
    }
}

fn normalize_echo(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn mentions_ask_question(s: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    s.match_indices("ask_question").any(|(i, m)| {
        !s[..i].chars().next_back().is_some_and(word)
            && !s[i + m.len()..].chars().next().is_some_and(word)
    })
}
/// agy's question tool; its permission request must reach the user.
pub fn is_agy_question_request(r: &PermissionRequest) -> bool {
    r.raw_input
        .as_ref()
        .is_some_and(|i| i["questions"].is_array())
        || r.title.as_deref().is_some_and(mentions_ask_question)
}
fn error_text(e: &AcpError) -> String {
    e.message.clone()
}

/// The turn's response footer; the web reads `provider: 'agy'` summaries.
pub fn agy_round_summary(
    usage: &Usage,
    duration_ms: Option<u64>,
    model: Option<&str>,
    generation_ms: Option<f64>,
    num_turns: Option<f64>,
) -> Value {
    let generated = usage.output_tokens + usage.thought_tokens.unwrap_or(0.0);
    let mut agy =
        json!({"input_tokens":num(usage.input_tokens),"output_tokens":num(usage.output_tokens)});
    if let Some(t) = usage.thought_tokens {
        agy["thinking_tokens"] = num(t);
    }
    if let Some(t) = usage.cache_read_tokens {
        agy["cache_read_tokens"] = num(t);
    }
    if let Some(t) = usage.total_tokens {
        agy["total_tokens"] = num(t);
    }
    let mut summary = json!({"provider":"agy"});
    if let Some(m) = model.filter(|m| !m.is_empty()) {
        summary["model"] = json!(m);
    }
    summary["agy_usage"] = agy;
    if let Some(d) = duration_ms {
        summary["duration_ms"] = json!(d);
    }
    if let Some(n) = num_turns.filter(|n| *n > 0.0) {
        summary["num_turns"] = num(n);
    }
    if let Some(g) = generation_ms.filter(|g| *g > 0.0 && generated > 0.0) {
        summary["generation_ms"] = num(g.round());
        summary["generation_output_tokens"] = num(generated);
    }
    json!({"type":"round-summary","summary":summary})
}

fn background_description(c: &ToolCall) -> String {
    if let Some(cmd) = c
        .native_args
        .as_ref()
        .and_then(|a| a["CommandLine"].as_str())
        .filter(|s| !s.trim().is_empty())
    {
        return cmd.into();
    }
    if let Some(t) = c.title.as_deref().filter(|t| !t.trim().is_empty()) {
        return t.into();
    }
    c.name.clone()
}

struct SteerEntry {
    key: u64,
    id: String,
    reservation: Reservation<Value>,
    text: String,
    echo: String,
    echoed: bool,
    resolved: bool,
    counted: bool,
    answered: bool,
    settled: bool,
    held: bool,
}

struct Turn {
    mode_hash: String,
    internal: bool,
    aborted: bool,
    stop_reason: Option<String>,
    sent_at: Option<u64>,
    answered_at: Option<u64>,
    generation: GenerationClock,
    open_commands: Vec<String>,
    bg_descriptions: HashMap<String, String>,
    command_call_ids: Vec<String>,
    command_arrival: Vec<String>,
    prior_background: HashSet<String>,
    turn_start_step_index: i64,
    streamed_text_counts: HashMap<String, usize>,
    num_turns: Option<f64>,
    transcript_num_turns: Option<f64>,
    usage: Option<Usage>,
    saw_activity: bool,
    acknowledged: bool,
    local_ids: Vec<String>,
    steers: Vec<SteerEntry>,
    steers_taken: i64,
    main_answered: bool,
    prompt_text: String,
    prompt_echo: String,
    prompt_echo_done: bool,
    steer_wait_released: bool,
    cancel_once_sent: bool,
    steer_grace: Option<Instant>,
    hard_stop: Option<Instant>,
}

struct TurnTranscriptState {
    start: i64,
    count_from: Option<i64>,
    counts: HashMap<String, usize>,
}

enum Event {
    SteerAnswered(u64),
    SteerCompleted(u64, SteerResult),
}

enum Work {
    Batch(hapi_wrapper_core::queue::Batch<Value>),
    Wakeup,
    Exit,
}

pub struct Launcher {
    s: Arc<Session>,
    cmd: mpsc::UnboundedReceiver<Command>,
    ev_tx: mpsc::UnboundedSender<Event>,
    ev_rx: mpsc::UnboundedReceiver<Event>,
    up_tx: Sink,
    up_rx: mpsc::UnboundedReceiver<Update>,
    make_backend: Box<dyn Fn() -> Backend + Send>,
    backend: Option<Backend>,
    permissions: Option<Arc<AcpPermissionHandler>>,
    turn_mode: Arc<Mutex<Option<String>>>,
    clock: Arc<Mutex<Option<GenerationClock>>>,
    title: Arc<TitleSync>,
    commands: Arc<Mutex<Option<Vec<AvailableCommand>>>>,
    brain: PathBuf,
    acp_id: Option<String>,
    default_model: Option<String>,
    default_model_known: bool,
    current_model: Option<String>,
    backend_lost: bool,
    native_tool_names: HashMap<String, String>,
    aliases: HashMap<String, String>,
    bg: Vec<AgyOpenBackgroundCommand>,
    active_bg: Vec<(String, String)>,
    last_bg: Vec<String>,
    last_step: i64,
    paused_step: Option<i64>,
    last_turn_state: Option<TurnTranscriptState>,
    question_labels: HashMap<String, Vec<Vec<String>>>,
    turn: Option<Turn>,
    orphan_steers: Vec<SteerEntry>,
    last_turn_mode: Option<Value>,
    last_session_mode: Option<Value>,
    exiting: bool,
    cleaned: bool,
    kill_reply: Vec<oneshot::Sender<()>>,
    wakeup_running: bool,
    wakeup_aborted: bool,
    steer_key: u64,
}

impl Launcher {
    pub fn new(
        s: Arc<Session>,
        cmd: mpsc::UnboundedReceiver<Command>,
        initial: Backend,
        make_backend: Box<dyn Fn() -> Backend + Send>,
        brain: PathBuf,
    ) -> Self {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (up_tx, up_rx) = mpsc::unbounded_channel();
        let title = Arc::new(TitleSync::new(&s.client));
        Self {
            s,
            cmd,
            ev_tx,
            ev_rx,
            up_tx,
            up_rx,
            make_backend,
            backend: Some(initial),
            permissions: None,
            turn_mode: Arc::default(),
            clock: Arc::default(),
            title,
            commands: Arc::default(),
            brain,
            acp_id: None,
            default_model: None,
            default_model_known: false,
            current_model: None,
            backend_lost: false,
            native_tool_names: HashMap::new(),
            aliases: HashMap::new(),
            bg: Vec::new(),
            active_bg: Vec::new(),
            last_bg: Vec::new(),
            last_step: -1,
            paused_step: None,
            last_turn_state: None,
            question_labels: HashMap::new(),
            turn: None,
            orphan_steers: Vec::new(),
            last_turn_mode: None,
            last_session_mode: None,
            exiting: false,
            cleaned: false,
            kill_reply: Vec::new(),
            wakeup_running: false,
            wakeup_aborted: false,
            steer_key: 0,
        }
    }

    /// Whether the session is ending because it was stopped.
    pub fn killed(&self) -> bool {
        self.exiting
    }

    fn alive(&self) -> bool {
        !self.backend_lost
            && self
                .backend
                .as_ref()
                .is_some_and(Backend::is_transport_alive)
    }

    pub async fn run(&mut self, resume: Option<String>) -> io::Result<()> {
        self.s.steering(false);
        self.s.client.metadata.update(|m| {
            let mut m = m.clone();
            m["agyProtocol"] = json!("acp");
            m
        });
        if let Err(e) = self.start_backend(resume).await {
            if self.exiting {
                self.finish_kill();
                return Ok(());
            }
            let message = format!(
                "Failed to start agy acp (requires Antigravity CLI 1.5.0 or newer): {}",
                error_text(&e)
            );
            self.report_turn_error(&message);
            return Err(io::Error::other(message));
        }
        while !self.exiting {
            match self.next_work().await? {
                Work::Exit => break,
                Work::Wakeup => self.run_wakeup().await,
                Work::Batch(b) => {
                    let ids = b
                        .items
                        .iter()
                        .filter_map(|i| i.local_id.clone())
                        .filter(|s| !s.is_empty())
                        .collect();
                    self.run_turn(b.message, b.mode, b.hash, ids, false, None)
                        .await;
                }
            }
        }
        self.finish_kill();
        Ok(())
    }

    fn finish_kill(&mut self) {
        if !self.kill_reply.is_empty() {
            self.final_writes();
        }
        for r in self.kill_reply.drain(..) {
            let _ = r.send(());
        }
    }

    /// TS `cleanup()`: the last state writes before the session is archived.
    fn final_writes(&mut self) {
        if std::mem::replace(&mut self.cleaned, true) {
            return;
        }
        self.s.steering(false);
        if let Some(p) = self.permissions.take() {
            p.cancel_all("Session ended");
        }
    }

    /// Ends the session's agent: late requests are cancelled, not approved.
    pub async fn cleanup(&mut self) {
        self.final_writes();
        if let Some(b) = self.backend.take() {
            b.disconnect().await;
        }
    }

    // ---- backend lifecycle -------------------------------------------------

    async fn start_backend(&mut self, resume: Option<String>) -> Result<(), AcpError> {
        let backend = match self.backend.clone() {
            Some(b) if !self.backend_lost => b,
            _ => (self.make_backend)(),
        };
        self.backend = Some(backend.clone());
        self.backend_lost = false;
        let result = self.start_backend_inner(&backend, resume).await;
        if result.is_err() {
            self.backend_lost = true;
            if self.backend.as_ref().is_some_and(|b| b.same(&backend)) {
                self.backend = None;
            }
            self.retire(backend).await;
        }
        result
    }

    async fn start_backend_inner(
        &mut self,
        backend: &Backend,
        resume: Option<String>,
    ) -> Result<(), AcpError> {
        let (client, title) = (self.s.client.clone(), self.title.clone());
        backend.on_session_title(Some(Arc::new(move |_, t| title.sync(&client, t))));
        let (client, commands) = (self.s.client.clone(), self.commands.clone());
        backend.on_available_commands(Some(Arc::new(move |_, list| {
            record_commands(&client, &commands, list);
        })));
        let s = self.s.clone();
        backend.on_stderr_error(Some(Arc::new(move |e| s.notice(&e.message))));
        backend.initialize().await?;
        let cwd = self.s.cwd.clone();
        let mut id = None;
        if let Some(r) = resume.as_deref() {
            let resumed = if backend.supports_session_resume() {
                backend.resume_session(r, &cwd).await
            } else {
                Err(AcpError::other("agy acp does not advertise session/resume"))
            };
            match resumed {
                Ok(v) => id = Some(v),
                Err(_) => self
                    .s
                    .notice("Antigravity could not resume the conversation; starting a new one."),
            }
        }
        let id = match id {
            Some(id) => id,
            None => {
                let id = backend.new_session(&cwd).await?;
                self.bg.clear();
                self.native_tool_names.clear();
                self.aliases.clear();
                id
            }
        };
        if self.acp_id.as_deref() != Some(id.as_str()) {
            self.last_step = -1;
            self.paused_step = None;
            self.last_turn_state = None;
        }
        self.acp_id = Some(id.clone());
        let reported = backend.available_commands(Some(&id));
        if !reported.is_empty() {
            record_commands(&self.s.client, &self.commands, &reported);
        }
        self.sync_transcript_cursor();
        self.s.found(&id);
        self.s.client.session_ready();
        let current = backend.session_models(&id).and_then(|m| m.current_model_id);
        self.current_model = current.clone();
        if !self.default_model_known {
            self.default_model_known = true;
            self.default_model = current;
        }
        let (turn_mode, s) = (self.turn_mode.clone(), self.s.clone());
        let clock = self.clock.clone();
        let permissions = AcpPermissionHandler::new(
            self.s.client.clone(),
            Arc::new(backend.clone()),
            Box::new(move || {
                turn_mode.lock().unwrap().clone().or_else(|| {
                    s.mode.lock().unwrap()["permissionMode"]
                        .as_str()
                        .map(str::to_owned)
                })
            }),
            AcpPermissionOptions {
                always_tool_name_hints: vec![],
                always_tool_id_hints: vec![],
                auto_approve_once: true,
                requires_user: Some(Box::new(is_agy_question_request)),
                on_user_wait: Some(Box::new(move || match clock.lock().unwrap().as_ref() {
                    Some(c) => c.begin_user_wait(),
                    None => Box::new(|| {}),
                })),
            },
        );
        let p = permissions.clone();
        backend.on_permission_request(Some(Arc::new(move |r| p.handle_request(r))));
        let tx = self.s.tx.clone();
        backend.on_form_elicitation(Some(Arc::new(move |r| {
            let (reply, rx) = oneshot::channel();
            let sent = tx.send(Command::Question(r, reply)).is_ok();
            Box::pin(async move {
                if !sent {
                    return FormElicitationResponse::Cancel;
                }
                rx.await.unwrap_or(FormElicitationResponse::Cancel)
            })
        })));
        self.permissions = Some(permissions);
        backend.refresh_session_info(&id, &cwd);
        Ok(())
    }

    async fn relaunch(&mut self) -> Result<(), AcpError> {
        if let Some(b) = self.backend.take() {
            self.retire(b).await;
        }
        self.backend_lost = true;
        let id = self.acp_id.clone();
        self.start_backend(id).await
    }

    async fn retire(&mut self, backend: Backend) {
        backend.on_available_commands(None);
        let b = backend.clone();
        backend.on_permission_request(Some(Arc::new(move |r: PermissionRequest| {
            b.respond_to_permission(&r, PermissionResponse::Cancelled)
        })));
        backend.on_form_elicitation(Some(Arc::new(|_| {
            Box::pin(async { FormElicitationResponse::Cancel })
        })));
        if let Some(p) = self.permissions.take() {
            p.cancel_all("Antigravity stopped");
        }
        backend.disconnect().await;
    }

    async fn apply_turn_config(&mut self, backend: &Backend, id: &str, mode: &Value) -> bool {
        let acp_mode = agy_acp_mode_for_permission(mode["permissionMode"].as_str());
        if let Err(e) = backend.set_mode(id, acp_mode).await {
            self.report_turn_error(&format!(
                "Failed to switch Antigravity mode: {}",
                error_text(&e)
            ));
            return false;
        }
        let model_id = resolve_agy_acp_model_id(
            mode["model"].as_str(),
            mode["effort"].as_str(),
            self.default_model.as_deref(),
        );
        if let Some(m) = model_id.filter(|m| Some(m) != self.current_model.as_ref()) {
            if let Err(e) = backend.set_model(id, &m).await {
                self.report_turn_error(&format!(
                    "Failed to switch Antigravity model to {m}: {}",
                    error_text(&e)
                ));
                return false;
            }
            self.current_model = Some(m);
        }
        true
    }

    // ---- idle wait and reactive wakeup ---------------------------------------

    fn logs_dir(&self) -> Option<PathBuf> {
        let id = self.acp_id.as_deref()?;
        agy_transcript_paths(&self.brain, id).map(|p| p.logs_dir)
    }

    async fn next_work(&mut self) -> io::Result<Work> {
        let mut watch: Option<DirWatch> = None;
        loop {
            if self.exiting {
                return Ok(Work::Exit);
            }
            let shared = self.s.clone();
            let notified = shared.changed.notified();
            {
                let mut q = self.s.queue.lock().unwrap();
                if let Some(b) = q.collect(false)? {
                    return Ok(Work::Batch(b));
                }
                if q.is_closed() {
                    return Ok(Work::Exit);
                }
            }
            if watch.is_none() {
                if let Some(dir) = self.logs_dir() {
                    let mut w = DirWatch::new(dir);
                    w.prime();
                    watch = Some(w);
                }
            }
            if self.has_pending_wakeup() {
                return Ok(Work::Wakeup);
            }
            tokio::select! {
                biased;
                c = self.cmd.recv() => match c {
                    Some(c) => self.idle_command(c).await,
                    None => return Ok(Work::Exit),
                },
                Some(u) = self.up_rx.recv() => self.idle_update(u),
                Some(e) = self.ev_rx.recv() => self.event(e).await,
                _ = notified => {}
                _ = async { watch.as_mut().unwrap().changed().await }, if watch.is_some() => {}
            }
        }
    }

    fn has_pending_wakeup(&mut self) -> bool {
        let Some(id) = self.acp_id.clone() else {
            return false;
        };
        if !self.alive() {
            return false;
        }
        self.advance_cursor_from_last_turn(None);
        let steps = read_agy_transcript_steps_after(&self.brain, &id, self.last_step);
        if steps.is_empty() || !steps.iter().any(AgyTranscriptStep::is_system_message) {
            return false;
        }
        match self.paused_step {
            Some(p) => steps.last().is_some_and(|s| s.step_index > p),
            None => true,
        }
    }

    fn sync_transcript_cursor(&mut self) {
        let Some(id) = self.acp_id.clone() else {
            return;
        };
        self.advance_cursor_from_last_turn(None);
        self.last_turn_state = None;
        let latest = read_latest_agy_transcript_step_index(&self.brain, &id);
        self.last_step = self.last_step.max(latest);
        self.paused_step = None;
    }

    fn advance_cursor_from_last_turn(&mut self, turn: Option<&mut Turn>) -> Option<i64> {
        let id = self.acp_id.clone()?;
        let state = self.last_turn_state.as_ref()?;
        let after = read_agy_transcript_steps_after(&self.brain, &id, state.start);
        let next = advance_agy_turn_transcript_cursor(&after, state.start, &state.counts);
        self.last_step = self.last_step.max(next);
        if let Some(turn) = turn {
            let from = state.count_from.unwrap_or(state.start);
            let steps = if from < state.start {
                read_agy_transcript_steps_after(&self.brain, &id, from)
            } else {
                after
            };
            turn.transcript_num_turns = Some(
                steps
                    .iter()
                    .filter(|s| {
                        s.is_planner()
                            && s.status.as_deref() == Some("DONE")
                            && s.step_index > from
                            && s.step_index <= next
                    })
                    .count() as f64,
            );
        }
        Some(next)
    }

    fn current_wakeup_mode(&self) -> Value {
        let session = self.s.runtime();
        let pick = |k: &str| match &self.last_session_mode {
            Some(snap) if snap[k] != session[k] => session[k].clone(),
            _ => self
                .last_turn_mode
                .as_ref()
                .map_or_else(|| session[k].clone(), |m| m[k].clone()),
        };
        let mut mode = json!({"permissionMode":pick("permissionMode"),"model":pick("model"),"effort":pick("effort")});
        if mode["permissionMode"].is_null() {
            mode["permissionMode"] = json!("request-review");
        }
        mode
    }

    async fn advance_backend_cursor(&mut self) {
        let (Some(b), Some(id)) = (self.backend.clone(), self.acp_id.clone()) else {
            return;
        };
        if !self.alive() || !b.supports_session_resume() {
            return;
        }
        match b.resume_session(&id, &self.s.cwd).await {
            Ok(_) => {
                if let Some(m) = b.session_models(&id).and_then(|m| m.current_model_id) {
                    self.current_model = Some(m);
                }
            }
            Err(e) if e.indeterminate => self.backend_lost = true,
            Err(_) => {}
        }
    }

    /// Wakeup tick: a transcript change, a command, or the poll delay.
    async fn wakeup_tick(&mut self, watch: &mut Option<DirWatch>) {
        let sleep = tokio::time::sleep(WAKEUP_TICK);
        tokio::select! {
            biased;
            Some(c) = self.cmd.recv() => self.idle_command(c).await,
            Some(u) = self.up_rx.recv() => self.idle_update(u),
            Some(e) = self.ev_rx.recv() => self.event(e).await,
            _ = async { watch.as_mut().unwrap().changed().await }, if watch.is_some() => {}
            _ = sleep => {}
        }
    }

    async fn run_wakeup(&mut self) {
        let Some(id) = self.acp_id.clone() else {
            return;
        };
        if self.exiting {
            return;
        }
        self.wakeup_running = true;
        self.wakeup_aborted = false;
        self.s.thinking(true);
        let mut watched = self.paused_step.unwrap_or(self.last_step);
        let mut progress_at = now() as i64;
        let mut watch = self.logs_dir().map(DirWatch::new);
        if let Some(w) = watch.as_mut() {
            w.prime();
        }
        loop {
            if self.exiting || self.s.queue.lock().unwrap().is_closed() {
                self.s.thinking(false);
                break;
            }
            if self.wakeup_aborted {
                self.sync_transcript_cursor();
                self.advance_backend_cursor().await;
                self.s.thinking(false);
                self.ready();
                break;
            }
            if self.s.queue_size() > 0 {
                break;
            }
            let now_ms = now() as i64;
            let steps = read_agy_transcript_steps_after(&self.brain, &id, self.last_step);
            let inspection =
                inspect_agy_reactive_steps(&steps, &self.s.cwd, now_ms, progress_at, &TIMINGS);
            if matches!(inspection, AgyReactiveInspection::None) {
                self.s.thinking(false);
                break;
            }
            if let Some(latest) = inspection.latest_step_index().filter(|l| *l > watched) {
                watched = latest;
                self.paused_step = None;
                progress_at = now_ms;
                self.wakeup_tick(&mut watch).await;
                continue;
            }
            match inspection {
                AgyReactiveInspection::Completed {
                    steps,
                    latest_step_index,
                } => {
                    self.paused_step = None;
                    self.last_step = self.last_step.max(latest_step_index);
                    let first = steps.first().map(|s| s.step_index - 1);
                    resolve_agy_background_task_ids(&self.brain, &id, &mut self.bg, first);
                    let converted = convert_agy_reactive_steps(&id, &steps, &mut self.bg);
                    self.advance_backend_cursor().await;
                    for m in converted.messages {
                        let m = self.show_question_card(m);
                        self.forward(m);
                    }
                    self.register_transcript_background_tasks();
                    self.retire_settled_background_tasks();
                    if let Some(u) = converted.usage {
                        let turns = steps
                            .iter()
                            .filter(|s| s.is_planner() && s.status.as_deref() == Some("DONE"))
                            .count() as f64;
                        self.s.client.send_agent_message(
                            agy_round_summary(
                                &u,
                                converted.duration_ms.map(|d| d.max(0) as u64),
                                self.current_model.as_deref(),
                                None,
                                Some(turns),
                            ),
                            None,
                        );
                    }
                    self.s.thinking(false);
                    self.ready();
                    break;
                }
                AgyReactiveInspection::TimedOut {
                    latest_step_index, ..
                } => {
                    self.paused_step = Some(latest_step_index);
                    self.s.thinking(false);
                    self.ready();
                    break;
                }
                AgyReactiveInspection::ParkedOnTool { .. } => {
                    self.paused_step = None;
                    self.wakeup_running = false;
                    let replay = self.last_step;
                    let mode = self.current_wakeup_mode();
                    let hash = mode_key(&mode);
                    self.run_turn(
                        AGY_WAKEUP_CONTINUE_PROMPT.into(),
                        mode,
                        hash,
                        vec![],
                        true,
                        Some(replay),
                    )
                    .await;
                    break;
                }
                _ => self.wakeup_tick(&mut watch).await,
            }
        }
        self.wakeup_running = false;
    }

    fn ready(&self) {
        if self.s.queue_size() == 0 && !self.exiting {
            self.s.event(json!({"type":"ready"}));
        }
    }

    // ---- commands outside a turn ---------------------------------------------

    async fn idle_command(&mut self, c: Command) {
        match c {
            Command::Abort(done) => {
                self.handle_abort().await;
                let _ = done.send(());
            }
            Command::Steer(_, reply) => {
                let _ = reply.send(json!({"steered":false,"error":"No active steerable turn"}));
            }
            Command::Kill(done) => {
                self.begin_kill().await;
                self.kill_reply.push(done);
                self.finish_kill();
            }
            Command::ListSlashCommands(reply) => {
                let v = self.list_slash_commands().await;
                let _ = reply.send(v);
            }
            Command::Question(_, reply) => {
                let _ = reply.send(FormElicitationResponse::Cancel);
            }
        }
    }

    fn idle_update(&mut self, u: Update) {
        if let Update::Message(m) = u {
            if !matches!(
                m,
                AgentMessage::Usage(_) | AgentMessage::TurnComplete { .. }
            ) {
                let m = self.show_question_card(m);
                self.forward(m);
            }
        }
    }

    async fn list_slash_commands(&mut self) -> Value {
        if self.commands.lock().unwrap().is_none() && self.alive() {
            if let Some(b) = &self.backend {
                let existing = b.available_commands(self.acp_id.as_deref());
                if !existing.is_empty() {
                    record_commands(&self.s.client, &self.commands, &existing);
                }
            }
        }
        let negotiated = self.commands.lock().unwrap().clone().unwrap_or_default();
        super::slash_commands::list(std::path::Path::new(&self.s.cwd), &negotiated).await
    }

    async fn begin_kill(&mut self) {
        self.clear_background_tasks();
        self.exiting = true;
        self.handle_abort().await;
        if let Some(b) = self.backend.clone() {
            b.disconnect().await;
        }
    }

    async fn handle_abort(&mut self) {
        let had_turn = self.turn.is_some() || self.wakeup_running;
        self.cancel_active_turn();
        self.clear_background_tasks();
        if had_turn {
            self.s.notice("Turn aborted");
        }
    }

    fn cancel_active_turn(&mut self) {
        let wakeup = self.wakeup_running && !self.wakeup_aborted;
        if wakeup {
            self.wakeup_aborted = true;
        }
        let alive = self.alive();
        let (backend, id) = (self.backend.clone(), self.acp_id.clone());
        if let Some(turn) = self.turn.as_mut() {
            turn.aborted = true;
            self.s.steering(false);
            if let (true, Some(b), Some(id)) = (alive, &backend, &id) {
                b.cancel_prompt(id);
                if !b.is_prompt_request_in_flight() {
                    turn.cancel_once_sent = true;
                }
                let now = Instant::now();
                if !turn.steers.is_empty() {
                    turn.steer_grace = Some(now + STEER_CANCEL_GRACE);
                }
                turn.hard_stop = Some(now + CANCEL_HARD_STOP);
            }
        } else if wakeup && alive {
            if let (Some(b), Some(id)) = (&backend, &id) {
                b.cancel_prompt(id);
            }
        }
        if let Some(p) = &self.permissions {
            p.cancel_all("User aborted");
        }
    }

    // ---- the turn --------------------------------------------------------------

    fn report_turn_error(&self, message: &str) {
        self.s.event(json!({"type":"error","message":message}));
    }

    fn acknowledge(&mut self) {
        let Some(t) = self.turn.as_mut() else { return };
        if t.acknowledged || t.local_ids.is_empty() {
            return;
        }
        t.acknowledged = true;
        self.s.client.messages_consumed(&t.local_ids, false, false);
    }

    fn restore_prompt(&mut self, text: &str) {
        if self.turn.as_ref().is_some_and(|t| t.internal) {
            return;
        }
        self.acknowledge();
        self.s.event(json!({"type":"abort-restore","text":text}));
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        &mut self,
        message: String,
        mode: Value,
        hash: String,
        local_ids: Vec<String>,
        internal: bool,
        replay_after: Option<i64>,
    ) {
        let swarm = (!internal)
            .then(|| parse_agy_swarm_board_command(&message))
            .flatten();
        if !internal {
            if swarm.is_none() {
                apply_title_fallback(&self.s.client, &message);
            }
            self.last_turn_mode = Some(mode.clone());
            self.last_session_mode = Some(self.s.runtime());
        }
        self.s.thinking(true);
        *self.turn_mode.lock().unwrap() = mode["permissionMode"].as_str().map(str::to_owned);
        let generation = GenerationClock::new();
        *self.clock.lock().unwrap() = Some(generation.clone());
        self.turn = Some(Turn {
            mode_hash: hash,
            internal,
            aborted: false,
            stop_reason: None,
            sent_at: None,
            answered_at: None,
            generation,
            open_commands: vec![],
            bg_descriptions: HashMap::new(),
            command_call_ids: vec![],
            command_arrival: vec![],
            prior_background: self.bg.iter().map(|e| e.call_id.clone()).collect(),
            turn_start_step_index: self.last_step,
            streamed_text_counts: HashMap::new(),
            num_turns: None,
            transcript_num_turns: None,
            usage: None,
            saw_activity: false,
            acknowledged: false,
            local_ids,
            steers: vec![],
            steers_taken: 0,
            main_answered: false,
            prompt_text: normalize_echo(&message),
            prompt_echo: String::new(),
            prompt_echo_done: false,
            steer_wait_released: false,
            cancel_once_sent: false,
            steer_grace: None,
            hard_stop: None,
        });
        self.turn_body(&message, &mode, swarm, internal, replay_after)
            .await;
        self.finish_turn().await;
    }

    async fn turn_body(
        &mut self,
        message: &str,
        mode: &Value,
        swarm: Option<super::swarm_board::AgySwarmBoardCommand>,
        internal: bool,
        replay_after: Option<i64>,
    ) {
        if let Some(cmd) = swarm {
            if self.acp_id.is_none() && !self.alive() {
                let _ = self.relaunch().await;
            }
            if self.turn.as_ref().is_some_and(|t| t.aborted) {
                self.restore_prompt(message);
                return;
            }
            self.acknowledge();
            let board = format_agy_swarm_board(&self.brain, self.acp_id.as_deref(), &cmd);
            self.forward(AgentMessage::text(board));
            return;
        }
        let mut relaunched = false;
        if !self.alive() {
            if let Err(e) = self.relaunch().await {
                self.report_turn_error(&format!(
                    "Failed to restart Antigravity: {}",
                    error_text(&e)
                ));
                self.restore_prompt(message);
                return;
            }
            relaunched = true;
        }
        let (Some(backend), Some(id)) = (self.backend.clone(), self.acp_id.clone()) else {
            self.restore_prompt(message);
            return;
        };
        if self.turn.as_ref().is_some_and(|t| t.aborted) {
            self.restore_prompt(message);
            return;
        }
        let applied = self.apply_turn_config(&backend, &id, mode).await;
        // TS marks the turn aborted at once; a Stop that queued during the
        // restart or the mode/model switch restores the prompt unsent.
        while let Ok(c) = self.cmd.try_recv() {
            self.turn_command(c).await;
        }
        if !applied || self.turn.as_ref().is_some_and(|t| t.aborted) {
            self.restore_prompt(message);
            return;
        }
        let pre_sync = self.last_step;
        self.sync_transcript_cursor();
        let start = self.last_step;
        let mut window = start;
        if !relaunched {
            let base = replay_after.unwrap_or(pre_sync).min(pre_sync).min(start);
            if base < start {
                let steps = read_agy_transcript_steps_after(&self.brain, &id, base);
                if let Some(first) = steps.iter().find(|s| s.is_system_message()) {
                    window = base.max(first.step_index - 1);
                }
            }
        }
        if let Some(t) = self.turn.as_mut() {
            t.turn_start_step_index = window;
            t.prior_background = self.bg.iter().map(|e| e.call_id.clone()).collect();
        }
        resolve_agy_background_task_ids(&self.brain, &id, &mut self.bg, Some(window));
        for e in self.bg.iter_mut() {
            if e.task_id.is_none() && e.max_step_index.is_none() {
                e.max_step_index = Some(window);
            }
        }
        self.s.steering(true);
        let (answered_tx, mut answered_rx) = oneshot::channel();
        let sink = self.up_tx.clone();
        let content = vec![json!({"type":"text","text":message})];
        let b = backend.clone();
        let pid = id.clone();
        let mut prompt =
            Box::pin(async move { b.prompt(&pid, content, sink, Some(answered_tx)).await });
        let mut outcome = None;
        let mut answered_open = true;
        loop {
            if outcome.is_some() {
                let t = self.turn.as_ref().unwrap();
                if t.steer_wait_released || t.steers.iter().all(|s| s.settled) {
                    break;
                }
            }
            let deadline = self.turn_deadline();
            tokio::select! {
                biased;
                Some(u) = self.up_rx.recv() => self.turn_update(u),
                Some(e) = self.ev_rx.recv() => self.event(e).await,
                Some(c) = self.cmd.recv() => self.turn_command(c).await,
                r = &mut answered_rx, if answered_open => {
                    answered_open = false;
                    if r.is_ok() {
                        if let Some(t) = self.turn.as_mut() {
                            t.main_answered = true;
                        }
                    }
                }
                o = &mut prompt, if outcome.is_none() => {
                    while let Ok(u) = self.up_rx.try_recv() {
                        self.turn_update(u);
                    }
                    if let Some(t) = self.turn.as_mut() {
                        t.sent_at = o.sent_at;
                        t.answered_at = o.answered_at;
                        t.main_answered = true;
                    }
                    self.prompt_finished(message, o.result);
                    outcome = Some(());
                }
                _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => {
                    self.turn_timers().await;
                }
            }
        }
        while let Ok(u) = self.up_rx.try_recv() {
            self.turn_update(u);
        }
        let (aborted, internal_turn) = {
            let t = self.turn.as_ref().unwrap();
            (t.aborted, t.internal)
        };
        if !aborted && !self.backend_lost {
            let counts = self.turn.as_ref().unwrap().streamed_text_counts.clone();
            self.last_turn_state = Some(TurnTranscriptState {
                start,
                count_from: (internal_turn && window < start).then_some(window),
                counts,
            });
            let mut t = self.turn.take();
            self.advance_cursor_from_last_turn(t.as_mut());
            self.turn = t;
            self.paused_step = None;
        }
        let _ = internal;
        self.close_background_commands(window);
    }

    fn prompt_finished(&mut self, message: &str, result: Result<(), AcpError>) {
        let t = self.turn.as_ref().unwrap();
        let (cancelled, activity) = (
            t.stop_reason.as_deref() == Some("cancelled"),
            t.saw_activity || t.steers_taken > 0,
        );
        let aborted = t.aborted;
        match result {
            Ok(()) => {
                if cancelled && !activity {
                    self.restore_prompt(message);
                } else {
                    self.acknowledge();
                }
            }
            Err(e) => {
                if e.indeterminate {
                    self.backend_lost = true;
                }
                if !aborted {
                    self.report_turn_error(&format!(
                        "Antigravity prompt failed: {}",
                        error_text(&e)
                    ));
                }
                if !activity {
                    self.restore_prompt(message);
                } else {
                    self.acknowledge();
                }
            }
        }
    }

    fn turn_deadline(&self) -> Option<Instant> {
        let t = self.turn.as_ref()?;
        [
            t.cancel_once_sent
                .then(|| Instant::now() + PROMPT_SEND_POLL),
            t.steer_grace,
            t.hard_stop,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    async fn turn_timers(&mut self) {
        let now = Instant::now();
        let (Some(b), Some(id)) = (self.backend.clone(), self.acp_id.clone()) else {
            return;
        };
        let Some(t) = self.turn.as_mut() else { return };
        if t.cancel_once_sent && b.is_prompt_request_in_flight() {
            t.cancel_once_sent = false;
            b.cancel_prompt(&id);
        }
        let mut retire = false;
        if t.steer_grace.is_some_and(|d| d <= now) {
            t.steer_grace = None;
            if !t.steers.iter().all(|s| s.answered) {
                retire = true;
            }
        }
        if t.hard_stop.is_some_and(|d| d <= now) {
            t.hard_stop = None;
            if !(t.main_answered && t.steers.iter().all(|s| s.answered)) {
                self.s.notice("Antigravity did not stop; restarting it.");
                retire = true;
            }
        }
        if retire {
            let keys: Vec<u64> = t.steers.iter().map(|s| s.key).collect();
            t.steer_wait_released = true;
            self.backend_lost = true;
            for k in keys {
                self.steer_indeterminate(k);
            }
            if let Some(b) = self.backend.take() {
                self.retire(b).await;
            }
        }
    }

    async fn turn_command(&mut self, c: Command) {
        match c {
            Command::Abort(done) => {
                self.handle_abort().await;
                let _ = done.send(());
            }
            Command::Steer(id, reply) => {
                let v = self.handle_steer(id).await;
                let _ = reply.send(v);
            }
            Command::Kill(done) => {
                self.kill_reply.push(done);
                self.clear_background_tasks();
                self.exiting = true;
                self.handle_abort().await;
                if let Some(b) = self.backend.clone() {
                    b.disconnect().await;
                }
            }
            Command::ListSlashCommands(reply) => {
                let v = self.list_slash_commands().await;
                let _ = reply.send(v);
            }
            Command::Question(r, reply) => self.handle_question(r, reply),
        }
    }

    async fn finish_turn(&mut self) {
        let aborted = self.turn.as_ref().is_some_and(|t| t.aborted);
        if aborted {
            self.sync_transcript_cursor();
        }
        self.question_labels.clear();
        let clean = !aborted && !self.backend_lost;
        let keys: Vec<(u64, bool)> = self
            .turn
            .as_ref()
            .map(|t| {
                t.steers
                    .iter()
                    .filter(|s| s.held)
                    .map(|s| (s.key, true))
                    .collect()
            })
            .unwrap_or_default();
        for (k, _) in keys {
            if clean {
                self.steer_accept(k);
            } else {
                self.steer_indeterminate(k);
            }
        }
        let Some(mut turn) = self.turn.take() else {
            return;
        };
        self.orphan_steers
            .extend(turn.steers.drain(..).filter(|s| !s.settled));
        *self.clock.lock().unwrap() = None;
        self.s.steering(false);
        *self.turn_mode.lock().unwrap() = None;
        if let Some(u) = &turn.usage {
            let duration = match (turn.sent_at, turn.answered_at) {
                (Some(s), Some(a)) => Some(a.saturating_sub(s)),
                _ => None,
            };
            let generation = turn
                .generation
                .generation_ms(turn.sent_at, turn.answered_at);
            self.s.client.send_agent_message(
                agy_round_summary(
                    u,
                    duration,
                    self.current_model.as_deref(),
                    generation,
                    turn.num_turns.or(turn.transcript_num_turns),
                ),
                None,
            );
        }
        self.s.thinking(false);
        if let Some(p) = &self.permissions {
            p.cancel_all("Prompt finished");
        }
        self.ready();
        if self.exiting {
            self.finish_kill();
        }
    }

    // ---- agent output ----------------------------------------------------------

    fn turn_update(&mut self, u: Update) {
        match u {
            Update::Echo(text) => self.handle_user_echo(&text),
            Update::Message(m) => self.handle_turn_message(m),
        }
    }

    fn handle_turn_message(&mut self, m: AgentMessage) {
        let Some(turn) = self.turn.as_mut() else {
            return self.idle_update(Update::Message(m));
        };
        match m {
            AgentMessage::Usage(u) => {
                turn.usage = Some(add_agy_usage(turn.usage.take(), u));
            }
            AgentMessage::TurnComplete {
                stop_reason,
                num_turns,
            } => {
                turn.stop_reason = Some(stop_reason);
                if let Some(n) = num_turns.filter(|n| n.is_finite() && *n > 0.0) {
                    turn.num_turns = Some(n);
                }
            }
            m => {
                turn.saw_activity = true;
                self.acknowledge();
                let (m, suppress) = self.reconcile_synthetic(m);
                let turn = self.turn.as_mut().unwrap();
                turn.generation.observe(&m);
                if let AgentMessage::Text { text, live, .. } = &m {
                    let t = text.trim();
                    if !*live && !t.is_empty() {
                        *turn.streamed_text_counts.entry(t.to_owned()).or_default() += 1;
                    }
                }
                self.track_open_command(&m, !suppress);
                if !suppress {
                    let m = self.show_question_card(m);
                    self.forward(m);
                }
            }
        }
    }

    fn reconcile_synthetic(&mut self, m: AgentMessage) -> (AgentMessage, bool) {
        let alias = m.tool_id().and_then(|id| self.aliases.get(id).cloned());
        if let Some(synthetic) = alias {
            return match m {
                AgentMessage::ToolCall(mut c) => {
                    c.id = synthetic;
                    (AgentMessage::ToolCall(c), true)
                }
                AgentMessage::ToolResult { id, output, status } => {
                    self.aliases.remove(&id);
                    (
                        AgentMessage::ToolResult {
                            id: synthetic,
                            output,
                            status,
                        },
                        false,
                    )
                }
                m => (m, false),
            };
        }
        let Some(sid) = self.acp_id.clone() else {
            return (m, false);
        };
        let turn = self.turn.as_mut().unwrap();
        match m {
            AgentMessage::ToolCall(mut c)
                if c.native_name.as_deref() == Some("run_command")
                    && c.status != "pending"
                    && !turn.command_call_ids.contains(&c.id)
                    && !turn.prior_background.contains(&c.id) =>
            {
                if !turn.command_arrival.contains(&c.id) {
                    turn.command_arrival.push(c.id.clone());
                }
                let lookup = AgyLiveBackgroundCompletionLookup {
                    live_call_id: Some(c.id.clone()),
                    native_args: c.native_args.as_ref().and_then(|a| a.as_object().cloned()),
                    call_status: Some(c.status.clone()),
                    ..Default::default()
                };
                let start = turn.turn_start_step_index;
                if let AgySyntheticCommandMatch::Matched { entry, .. } =
                    inspect_synthetic_background_command_match(
                        &self.brain,
                        &sid,
                        &mut self.bg,
                        start,
                        &lookup,
                    )
                {
                    let turn = self.turn.as_mut().unwrap();
                    turn.command_arrival.retain(|i| i != &c.id);
                    self.aliases.insert(c.id.clone(), entry.call_id.clone());
                    c.id = entry.call_id;
                    return (AgentMessage::ToolCall(c), true);
                }
                (AgentMessage::ToolCall(c), false)
            }
            AgentMessage::ToolResult { id, output, status }
                if !self.native_tool_names.contains_key(&id) =>
            {
                let lookup = AgyLiveBackgroundCompletionLookup {
                    live_call_id: Some(id.clone()),
                    result_output: Some(output.clone()),
                    result_status: Some(status.clone()),
                    ..Default::default()
                };
                let start = turn.turn_start_step_index;
                if let AgySyntheticCommandMatch::Matched { entry, .. } =
                    inspect_synthetic_background_command_match(
                        &self.brain,
                        &sid,
                        &mut self.bg,
                        start,
                        &lookup,
                    )
                {
                    self.native_tool_names
                        .insert(entry.call_id.clone(), "run_command".into());
                    return (
                        AgentMessage::ToolResult {
                            id: entry.call_id,
                            output,
                            status,
                        },
                        false,
                    );
                }
                (AgentMessage::ToolResult { id, output, status }, false)
            }
            m => (m, false),
        }
    }

    fn track_open_command(&mut self, m: &AgentMessage, record: bool) {
        let (Some(id), Some(status)) = (m.tool_id(), m.status()) else {
            return;
        };
        let id = id.to_owned();
        let in_bg = self.bg.iter().any(|e| e.call_id == id);
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let prior = turn.prior_background.contains(&id) || in_bg;
        if prior {
            turn.prior_background.insert(id.clone());
        }
        if let AgentMessage::ToolCall(c) = m {
            if record && !prior && c.native_name.as_deref() == Some("run_command") {
                if !turn.command_arrival.contains(&id) {
                    turn.command_arrival.push(id.clone());
                }
                if !turn.command_call_ids.contains(&id) {
                    turn.command_call_ids.push(id.clone());
                    let order = turn.command_arrival.clone();
                    turn.command_call_ids
                        .sort_by_key(|x| order.iter().position(|o| o == x));
                }
            }
            if c.native_name.as_deref() == Some("run_command") && status == "in_progress" {
                if !turn.open_commands.contains(&id) {
                    turn.open_commands.push(id.clone());
                }
                turn.bg_descriptions.insert(id, background_description(c));
                return;
            }
        }
        if status != "in_progress" {
            turn.open_commands.retain(|x| x != &id);
            self.bg.retain(|e| e.call_id != id);
            self.background_task_settled(&id);
        }
    }

    fn close_background_commands(&mut self, window: i64) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if turn.aborted || self.backend_lost || turn.stop_reason.as_deref() != Some("end_turn") {
            return;
        }
        let mut open = turn.open_commands.clone();
        let order = turn.command_call_ids.clone();
        open.sort_by_key(|id| order.iter().position(|o| o == id).unwrap_or(usize::MAX));
        let steps = match (&self.acp_id, open.is_empty()) {
            (Some(sid), false) => read_agy_transcript_steps_after(&self.brain, sid, window),
            _ => vec![],
        };
        let task_ids = extract_agy_background_task_ids(&steps);
        let descriptions = turn.bg_descriptions.clone();
        turn.open_commands.clear();
        for (idx, id) in open.iter().enumerate() {
            let ordinal = order.iter().position(|o| o == id).unwrap_or(idx);
            let task = task_ids.get(ordinal).cloned().flatten();
            match self.bg.iter_mut().find(|e| &e.call_id == id) {
                None => self.bg.push(AgyOpenBackgroundCommand {
                    call_id: id.clone(),
                    after_step_index: Some(window),
                    ordinal_in_turn: Some(ordinal),
                    task_id: task,
                    ..Default::default()
                }),
                Some(e) => {
                    if e.task_id.is_none() {
                        e.task_id = task;
                    }
                }
            }
            let placeholder = AgentMessage::ToolResult {
                id: id.clone(),
                output: background_command_result(),
                status: "completed".into(),
            };
            if let Some(v) = convert_agent_message(&placeholder, self.current_model.as_deref()) {
                self.s.client.send_agent_message(v, None);
            }
        }
        for id in &open {
            if let Some(d) = descriptions.get(id) {
                self.set_bg(id, d.clone());
            }
        }
        if !open.is_empty() {
            self.emit_bg_snapshot();
        }
    }

    fn set_bg(&mut self, id: &str, d: String) {
        if let Some(e) = self.active_bg.iter_mut().find(|(i, _)| i == id) {
            e.1 = d;
        } else {
            self.active_bg.push((id.into(), d));
        }
    }
    fn emit_bg_snapshot(&mut self) {
        let ids: Vec<String> = self.active_bg.iter().map(|(i, _)| i.clone()).collect();
        if ids == self.last_bg {
            return;
        }
        self.last_bg = ids;
        let tasks: Vec<Value> = self
            .active_bg
            .iter()
            .map(|(id, d)| json!({"task_id":id,"task_type":"local_bash","description":d}))
            .collect();
        self.s.client.send_claude_session_message(&json!({
            "type":"system","uuid":uuid::Uuid::new_v4().to_string(),
            "subtype":"background_tasks_changed","tasks":tasks
        }));
    }
    fn background_task_settled(&mut self, id: &str) {
        let before = self.active_bg.len();
        self.active_bg.retain(|(i, _)| i != id);
        if self.active_bg.len() != before {
            self.emit_bg_snapshot();
        }
    }
    fn retire_settled_background_tasks(&mut self) {
        let before = self.active_bg.len();
        let bg = &self.bg;
        self.active_bg
            .retain(|(i, _)| bg.iter().any(|e| &e.call_id == i));
        if self.active_bg.len() != before {
            self.emit_bg_snapshot();
        }
    }
    fn register_transcript_background_tasks(&mut self) {
        let mut changed = false;
        for e in self.bg.clone() {
            if self.active_bg.iter().any(|(i, _)| i == &e.call_id) {
                continue;
            }
            let d = e
                .command_line
                .clone()
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| e.call_id.clone());
            self.active_bg.push((e.call_id.clone(), d));
            changed = true;
        }
        if changed {
            self.emit_bg_snapshot();
        }
    }
    fn clear_background_tasks(&mut self) {
        if self.active_bg.is_empty() && self.last_bg.is_empty() {
            return;
        }
        self.active_bg.clear();
        self.emit_bg_snapshot();
    }

    fn show_question_card(&mut self, m: AgentMessage) -> AgentMessage {
        let AgentMessage::ToolCall(mut c) = m else {
            return m;
        };
        if !mentions_ask_question(&c.name) {
            return AgentMessage::ToolCall(c);
        }
        let Some(input) = question_card_input_from_tool_call(&c.input) else {
            return AgentMessage::ToolCall(c);
        };
        self.question_labels
            .insert(c.id.clone(), shown_labels(&input));
        c.name = QUESTION_TOOL_NAME.into();
        c.input = input;
        AgentMessage::ToolCall(c)
    }

    fn canonical(&mut self, m: AgentMessage) -> AgentMessage {
        match m {
            AgentMessage::ToolCall(c) => {
                if let Some(n) = &c.native_name {
                    self.native_tool_names.insert(c.id.clone(), n.clone());
                }
                AgentMessage::ToolCall(map_agy_tool_call(&c))
            }
            m @ AgentMessage::ToolResult { .. } => {
                let id = m.tool_id().unwrap_or("").to_owned();
                let native = self.native_tool_names.get(&id).cloned();
                if !self.bg.iter().any(|e| e.call_id == id) {
                    self.native_tool_names.remove(&id);
                }
                map_agy_tool_result(&m, native.as_deref())
            }
            m => m,
        }
    }

    fn forward(&mut self, m: AgentMessage) {
        let m = self.canonical(m);
        if let Some(v) = convert_agent_message(&m, self.current_model.as_deref()) {
            self.s.client.send_agent_message(v, None);
        }
    }

    // ---- questions ---------------------------------------------------------------

    fn handle_question(
        &mut self,
        request: FormElicitationRequest,
        reply: oneshot::Sender<FormElicitationResponse>,
    ) {
        let form = parse_agy_question_form(&request);
        let (Some(form), Some(perms)) = (form, self.permissions.clone()) else {
            let _ = reply.send(FormElicitationResponse::Cancel);
            return;
        };
        if self.turn.as_ref().is_none_or(|t| t.aborted) || self.exiting {
            let _ = reply.send(FormElicitationResponse::Cancel);
            return;
        }
        let preferred = request.request_id.clone().or(form.tool_call_id.clone());
        let id = match preferred {
            Some(p) if !perms.is_used(&p) => p,
            _ => format!("agy-question-{}", uuid::Uuid::new_v4()),
        };
        let shown = form
            .tool_call_id
            .as_ref()
            .and_then(|t| self.question_labels.get(t).cloned());
        tokio::spawn(async move {
            let answer = perms
                .ask_user(
                    &id,
                    QUESTION_TOOL_NAME,
                    question_card_input(&form.questions),
                    form.tool_call_id.as_deref(),
                )
                .await;
            let response = match answer.decision.as_str() {
                "abort" => FormElicitationResponse::Cancel,
                "denied" => FormElicitationResponse::Decline,
                _ => {
                    let content: Map<String, Value> = agy_question_content(
                        &form.questions,
                        answer.answers.as_ref(),
                        shown.as_deref(),
                    );
                    if content.is_empty() {
                        FormElicitationResponse::Decline
                    } else {
                        FormElicitationResponse::Accept(content)
                    }
                }
            };
            let _ = reply.send(response);
        });
    }

    // ---- steer ---------------------------------------------------------------------

    fn find_steer(&mut self, key: u64) -> Option<(&mut SteerEntry, bool)> {
        if let Some(t) = self.turn.as_mut() {
            if let Some(e) = t.steers.iter_mut().find(|s| s.key == key) {
                return Some((e, true));
            }
        }
        self.orphan_steers
            .iter_mut()
            .find(|s| s.key == key)
            .map(|e| (e, false))
    }

    fn steer_accept(&mut self, key: u64) {
        let s = self.s.clone();
        let Some((e, _)) = self.find_steer(key) else {
            return;
        };
        if e.resolved {
            return;
        }
        e.resolved = true;
        s.queue.lock().unwrap().commit(&e.reservation);
        s.client
            .messages_consumed(std::slice::from_ref(&e.id), false, true);
    }

    fn steer_indeterminate(&mut self, key: u64) {
        let s = self.s.clone();
        let Some((e, _)) = self.find_steer(key) else {
            return;
        };
        if e.resolved {
            return;
        }
        e.resolved = true;
        if s.queue.lock().unwrap().mark_indeterminate(&e.reservation) {
            s.client.steer_indeterminate(std::slice::from_ref(&e.id));
        }
    }

    async fn steer_restore(&mut self, key: u64) {
        let s = self.s.clone();
        let (r, id, counted) = {
            let Some((e, _)) = self.find_steer(key) else {
                return;
            };
            if e.resolved {
                return;
            }
            e.resolved = true;
            let counted = std::mem::replace(&mut e.counted, false);
            (e.reservation.clone(), e.id.clone(), counted)
        };
        if counted {
            if let Some(t) = self.turn.as_mut() {
                t.steers_taken -= 1;
            }
        }
        restore_reservation(&s, &r, &id).await;
    }

    fn handle_user_echo(&mut self, text: &str) {
        if self.turn.is_none() {
            return;
        }
        self.acknowledge();
        let turn = self.turn.as_mut().unwrap();
        if !turn.prompt_echo_done {
            let echoed = normalize_echo(&format!("{} {text}", turn.prompt_echo));
            if !echoed.is_empty() && turn.prompt_text.starts_with(&echoed) {
                turn.prompt_echo_done = echoed == turn.prompt_text;
                turn.prompt_echo = echoed;
                return;
            }
        }
        for e in turn.steers.iter_mut().filter(|e| !e.echoed && !e.resolved) {
            let joined = normalize_echo(&format!("{} {text}", e.echo));
            let start = joined.len().saturating_sub(ECHO_BUFFER_LIMIT);
            let start = (start..=joined.len())
                .find(|i| joined.is_char_boundary(*i))
                .unwrap_or(0);
            e.echo = joined[start..].to_owned();
        }
        loop {
            let turn = self.turn.as_mut().unwrap();
            let best = turn
                .steers
                .iter()
                .filter(|e| {
                    !e.echoed && !e.resolved && !e.text.is_empty() && e.echo.contains(&e.text)
                })
                .max_by_key(|e| e.text.len())
                .map(|e| (e.key, e.text.clone()));
            let Some((key, matched)) = best else { return };
            for e in turn.steers.iter_mut().filter(|e| e.key != key && !e.echoed) {
                e.echo = normalize_echo(&e.echo.replacen(&matched, " ", 1));
            }
            if let Some(e) = turn.steers.iter_mut().find(|e| e.key == key) {
                e.echoed = true;
            }
            self.steer_accept(key);
        }
    }

    async fn handle_steer(&mut self, id: String) -> Value {
        let fail = |e: &str| json!({"steered":false,"error":e});
        if id.is_empty() {
            return fail("Missing localId");
        }
        let (Some(backend), Some(acp)) = (self.backend.clone(), self.acp_id.clone()) else {
            return fail("No active steerable turn");
        };
        let Some(turn) = self.turn.as_ref().filter(|t| !t.aborted && !self.exiting) else {
            return fail("No active steerable turn");
        };
        let busy = "Antigravity is waiting for your approval or answer; respond to it, then steer";
        if self
            .permissions
            .as_ref()
            .is_some_and(|p| p.has_pending_requests())
        {
            return fail(busy);
        }
        let hash = turn.mode_hash.clone();
        let r = {
            let mut q = self.s.queue.lock().unwrap();
            let Some(r) = q.take_by_local_id(&id) else {
                return fail("Message not in queue");
            };
            if r.item().isolated {
                q.restore(&r);
                return fail("Control commands cannot be steered");
            }
            if r.item().hash != hash {
                q.restore(&r);
                return fail("Queued message mode differs from the active turn");
            }
            if !q.begin_dispatch(&r) {
                return fail("Steer cancelled");
            }
            r
        };
        let ids = [id.clone()];
        if !self.s.client.steer_state(&ids, "dispatching").await {
            if self.s.queue.lock().unwrap().mark_indeterminate(&r) {
                self.s.client.steer_indeterminate(&ids);
            }
            return fail("Steer state is indeterminate");
        }
        if !self.s.queue.lock().unwrap().is_dispatching(&r) {
            self.s.client.steer_indeterminate(&ids);
            return fail("Steer cancelled");
        }
        let still = self.turn.as_ref().is_some_and(|t| !t.aborted)
            && self.backend.as_ref().is_some_and(|b| b.same(&backend));
        if !still {
            restore_reservation(&self.s, &r, &id).await;
            return fail("Active turn changed");
        }
        if self
            .permissions
            .as_ref()
            .is_some_and(|p| p.has_pending_requests())
        {
            restore_reservation(&self.s, &r, &id).await;
            return fail(busy);
        }
        let text = r.item().message.clone();
        let steer = match backend.begin_soft_steer(&acp, vec![json!({"type":"text","text":text})]) {
            Ok(s) => s,
            Err(e) if e.indeterminate => {
                if self.s.queue.lock().unwrap().mark_indeterminate(&r) {
                    self.s.client.steer_indeterminate(&ids);
                }
                return fail("Steer outcome is being reconciled");
            }
            Err(_) => {
                restore_reservation(&self.s, &r, &id).await;
                return fail("Failed to steer into the active turn");
            }
        };
        self.steer_key += 1;
        let key = self.steer_key;
        let turn = self.turn.as_mut().unwrap();
        turn.steers_taken += 1;
        turn.steers.push(SteerEntry {
            key,
            id: id.clone(),
            reservation: r,
            text: normalize_echo(&text),
            echo: String::new(),
            echoed: false,
            resolved: false,
            counted: true,
            answered: false,
            settled: false,
            held: false,
        });
        let (answered, completed) = (steer.answered, steer.completed);
        let dispatched = steer.dispatched.await;
        let tx = self.ev_tx.clone();
        tokio::spawn(async move {
            let _ = answered.await;
            let _ = tx.send(Event::SteerAnswered(key));
            let r = completed
                .await
                .unwrap_or_else(|_| Err(AcpError::closed("ACP transport closed")));
            let _ = tx.send(Event::SteerCompleted(key, r));
        });
        match dispatched {
            Ok(()) => json!({"steered":true}),
            Err(e) if e.indeterminate => {
                self.steer_indeterminate(key);
                fail("Steer outcome is being reconciled")
            }
            Err(_) => {
                self.steer_restore(key).await;
                fail("Failed to steer into the active turn")
            }
        }
    }

    async fn event(&mut self, e: Event) {
        match e {
            Event::SteerAnswered(key) => {
                let in_turn = match self.find_steer(key) {
                    Some((e, in_turn)) => {
                        e.answered = true;
                        in_turn
                    }
                    None => return,
                };
                if in_turn {
                    if let Some(t) = self.turn.as_mut() {
                        t.answered_at = Some(now());
                    }
                }
            }
            Event::SteerCompleted(key, result) => {
                let Some((e, in_turn)) = self.find_steer(key) else {
                    return;
                };
                e.settled = true;
                e.answered = true;
                match result {
                    Ok((stop, usage)) => {
                        if let (Some(u), true) = (usage, in_turn) {
                            let t = self.turn.as_mut().unwrap();
                            t.usage = Some(add_agy_usage(t.usage.take(), u));
                        }
                        if stop.as_deref() == Some("cancelled") {
                            self.steer_indeterminate(key);
                        } else if in_turn {
                            if let Some((e, _)) = self.find_steer(key) {
                                e.held = true;
                            }
                        } else {
                            self.steer_accept(key);
                        }
                    }
                    Err(err) if err.indeterminate => {
                        if in_turn {
                            self.backend_lost = true;
                        }
                        self.steer_indeterminate(key);
                    }
                    Err(_) => self.steer_restore(key).await,
                }
                self.orphan_steers.retain(|s| !(s.settled && s.resolved));
            }
        }
    }
}

async fn restore_reservation(s: &Session, r: &Reservation<Value>, id: &str) {
    let ids = [id.to_owned()];
    let ambiguous = s.queue.lock().unwrap().is_ambiguous(r);
    if !ambiguous && !s.client.steer_state(&ids, "queued").await {
        if s.queue.lock().unwrap().mark_indeterminate(r) {
            s.client.steer_indeterminate(&ids);
        }
        return;
    }
    let mut q = s.queue.lock().unwrap();
    if q.is_dispatching(r) && q.restore(r) {
        s.changed.notify_one();
    } else {
        s.client.steer_indeterminate(&ids);
    }
}

fn record_commands(
    client: &SessionClient,
    store: &Mutex<Option<Vec<AvailableCommand>>>,
    list: &[AvailableCommand],
) {
    *store.lock().unwrap() = Some(list.to_vec());
    let names: Vec<String> = list.iter().map(|c| c.name.clone()).collect();
    client.metadata.update(move |m| {
        let mut m = m.clone();
        m["slashCommands"] = json!(names);
        m
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_summary_shape() {
        let u = Usage {
            input_tokens: 10.0,
            output_tokens: 5.0,
            thought_tokens: Some(2.0),
            total_tokens: Some(17.0),
            ..Default::default()
        };
        assert_eq!(
            agy_round_summary(&u, Some(1500), Some("m"), Some(1000.4), Some(2.0)),
            json!({"type":"round-summary","summary":{"provider":"agy","model":"m","agy_usage":{"input_tokens":10,"output_tokens":5,"thinking_tokens":2,"total_tokens":17},"duration_ms":1500,"num_turns":2,"generation_ms":1000,"generation_output_tokens":7}})
        );
        assert_eq!(
            agy_round_summary(&Usage::default(), None, None, Some(5.0), None),
            json!({"type":"round-summary","summary":{"provider":"agy","agy_usage":{"input_tokens":0,"output_tokens":0}}})
        );
    }

    #[test]
    fn echo_and_question_helpers() {
        assert_eq!(normalize_echo("  a\n b\t c "), "a b c");
        assert!(mentions_ask_question("Run ask_question"));
        assert!(!mentions_ask_question("ask_questions"));
        let r = PermissionRequest {
            id: "1".into(),
            session_id: "s".into(),
            tool_call_id: "t".into(),
            title: Some("echo".into()),
            kind: None,
            raw_input: Some(json!({"questions":[]})),
            raw_output: None,
            options: vec![],
        };
        assert!(is_agy_question_request(&r));
        assert_eq!(
            mode_key(&json!({"effort":null,"permissionMode":"plan","model":"m"})),
            r#"{"permissionMode":"plan","model":"m","effort":null}"#
        );
    }
}
