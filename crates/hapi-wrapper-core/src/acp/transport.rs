//! ndjson JSON-RPC over an agent's stdio (TS `AcpStdioTransport.ts`).
//!
//! One reader task handles stdout in order: notifications and responses are
//! dispatched synchronously, so every `session/update` that precedes a
//! response has been handled when the request future resolves. Agent
//! requests run on their own task and answer through the writer task.
use super::types::StderrError;
use crate::lifecycle::proctree;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot, watch},
};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const RECENT_STDERR_WINDOW: usize = 8_000;
const CLOSE_STDERR_CAP: usize = 4_000;
const MODEL_PRESETS: &str = "gemini-3.1-pro-preview, gemini-3-flash-preview, gemini-2.5-pro, gemini-2.5-flash, gemini-2.5-flash-lite";

/// A failed request. `indeterminate` marks transport-level failures whose
/// outcome is unknown (unlike an explicit JSON-RPC error response).
#[derive(Clone, Debug, PartialEq)]
pub struct AcpError {
    pub message: String,
    pub code: Option<i64>,
    pub data: Option<Value>,
    pub indeterminate: bool,
}
impl AcpError {
    pub fn closed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            data: None,
            indeterminate: true,
        }
    }
    pub fn other(message: impl Into<String>) -> Self {
        Self {
            indeterminate: false,
            ..Self::closed(message)
        }
    }
}
impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for AcpError {}

/// Dispatch (stdin accepted the line) and completion (the response).
pub type DispatchedRequest = (
    BoxFuture<'static, Result<(), AcpError>>,
    BoxFuture<'static, Result<Value, AcpError>>,
);
pub type RequestHandler =
    Arc<dyn Fn(Value, Value) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;
pub type NotificationHandler = Arc<dyn Fn(&str, &Value) + Send + Sync>;
pub type StderrHandler = Arc<dyn Fn(StderrError) + Send + Sync>;

pub struct SpawnOptions {
    pub command: String,
    pub args: Vec<String>,
    /// Full environment; `None` inherits.
    pub env: Option<HashMap<String, String>>,
    pub cwd: Option<PathBuf>,
}

struct Pending {
    done: oneshot::Sender<Result<Value, AcpError>>,
}
#[derive(Default)]
struct State {
    next_id: u64,
    pending: HashMap<u64, Pending>,
    closed: Option<AcpError>,
    exited: bool,
}
#[derive(Default)]
struct Stderr {
    recent: String,
    parse: String,
    partial_reported: bool,
    model_rejection: bool,
}
struct Inner {
    state: Mutex<State>,
    writer: mpsc::UnboundedSender<(String, Option<oneshot::Sender<()>>)>,
    handlers: Mutex<HashMap<String, RequestHandler>>,
    notify: Mutex<Option<NotificationHandler>>,
    stderr_handler: Mutex<Option<StderrHandler>>,
    stderr: Mutex<Stderr>,
    pid: Option<u32>,
    alive: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct Transport(Arc<Inner>);

fn truncate_chars(s: &str, max: usize, from_end: bool) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.into();
    }
    if from_end {
        s.chars().skip(n - max).collect()
    } else {
        s.chars().take(max).collect()
    }
}
fn model_rejection_index(s: &str) -> Option<usize> {
    s.to_lowercase().find("cannot use this model:")
}

impl Transport {
    pub fn spawn(opts: SpawnOptions) -> io::Result<Self> {
        let mut cmd = Command::new(&opts.command);
        proctree::configure_child_group(&mut cmd);
        cmd.args(&opts.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        if let Some(env) = &opts.env {
            cmd.env_clear().envs(env);
        }
        if let Some(cwd) = &opts.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Failed to spawn {}: {e}. Is it installed and on PATH?",
                    opts.command
                ),
            )
        })?;
        let pid = child.id();
        if let Some(pid) = pid {
            if let Err(e) = proctree::track_child(pid) {
                let _ = child.start_kill();
                return Err(e);
            }
        }
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (writer, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                next_id: 1,
                ..Default::default()
            }),
            writer,
            handlers: Mutex::default(),
            notify: Mutex::default(),
            stderr_handler: Mutex::default(),
            stderr: Mutex::default(),
            pid,
            alive: watch::channel(true).0,
        });
        let t = Transport(inner);
        tokio::spawn(write_loop(Arc::downgrade(&t.0), stdin, rx));
        let (out_done_tx, out_done) = oneshot::channel();
        let (err_done_tx, err_done) = oneshot::channel();
        let weak = Arc::downgrade(&t.0);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(inner) = weak.upgrade() else { break };
                Transport(inner).handle_line(line.trim());
            }
            let _ = out_done_tx.send(());
        });
        let weak = Arc::downgrade(&t.0);
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut buf = vec![0u8; 8192];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let Some(inner) = weak.upgrade() else { break };
                        Transport(inner).handle_stderr(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            }
            let _ = err_done_tx.send(());
        });
        let weak = Arc::downgrade(&t.0);
        tokio::spawn(async move { wait_loop(weak, child, out_done, err_done).await });
        Ok(t)
    }

    pub fn pid(&self) -> Option<u32> {
        self.0.pid
    }
    pub fn is_alive(&self) -> bool {
        let s = self.0.state.lock().unwrap();
        s.closed.is_none() && !s.exited
    }
    /// Resolves once the transport is closed or the process exited.
    pub async fn closed(&self) {
        let mut rx = self.0.alive.subscribe();
        let _ = rx.wait_for(|alive| !alive).await;
    }
    pub fn on_notification(&self, f: Option<NotificationHandler>) {
        *self.0.notify.lock().unwrap() = f;
    }
    pub fn on_stderr_error(&self, f: Option<StderrHandler>) {
        *self.0.stderr_handler.lock().unwrap() = f;
    }
    pub fn register_request_handler(&self, method: &str, f: RequestHandler) {
        self.0.handlers.lock().unwrap().insert(method.into(), f);
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, AcpError> {
        let (_, completed) = self.request_with_dispatch(method, params, timeout, None);
        completed.await
    }

    /// Splits a request into dispatch (stdin accepted the line) and
    /// completion (the JSON-RPC response). `timeout: None` waits forever;
    /// the dispatch timeout defaults to the request timeout.
    pub fn request_with_dispatch(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
        dispatch_timeout: Option<Duration>,
    ) -> DispatchedRequest {
        let (tx, rx) = oneshot::channel();
        let (wtx, wrx) = oneshot::channel();
        let id = {
            let mut s = self.0.state.lock().unwrap();
            if let Some(e) = s.closed.clone().or_else(|| {
                s.exited
                    .then(|| AcpError::closed("ACP transport is closed"))
            }) {
                let e = AcpError {
                    indeterminate: true,
                    ..e
                };
                let e2 = e.clone();
                return (
                    Box::pin(async move { Err(e) }),
                    Box::pin(async move { Err(e2) }),
                );
            }
            let id = s.next_id;
            s.next_id += 1;
            s.pending.insert(id, Pending { done: tx });
            id
        };
        let line = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
        let _ = self.0.writer.send((line, Some(wtx)));
        let dispatch_timeout = dispatch_timeout.or(timeout);
        let this = self.clone();
        let m = method.to_owned();
        let (dtx, drx) = watch::channel(None::<Result<(), AcpError>>);
        let completion = rx;
        tokio::spawn({
            let this = this.clone();
            async move {
                let written = async {
                    match wrx.await {
                        Ok(()) => Ok(()),
                        Err(_) => Err(this
                            .close_error()
                            .unwrap_or_else(|| AcpError::closed("ACP transport is closed"))),
                    }
                };
                let result = match dispatch_timeout {
                    Some(t) => match tokio::time::timeout(t, written).await {
                        Ok(r) => r,
                        Err(_) => {
                            let e = AcpError::closed(format!(
                                "ACP request '{m}' dispatch timed out after {}ms",
                                t.as_millis()
                            ));
                            this.mark_closed(e.clone());
                            Err(e)
                        }
                    },
                    None => written.await,
                };
                let _ = dtx.send(Some(result));
            }
        });
        let dispatched = {
            let mut drx = drx;
            Box::pin(async move {
                let v = drx.wait_for(|v| v.is_some()).await;
                match v {
                    Ok(v) => v.clone().unwrap(),
                    Err(_) => Err(AcpError::closed("ACP transport is closed")),
                }
            }) as BoxFuture<'static, Result<(), AcpError>>
        };
        let m = method.to_owned();
        let completed = Box::pin(async move {
            let wait = async {
                completion
                    .await
                    .unwrap_or_else(|_| Err(AcpError::closed("ACP transport is closed")))
            };
            match timeout {
                Some(t) => match tokio::time::timeout(t, wait).await {
                    Ok(r) => r,
                    Err(_) => {
                        this.0.state.lock().unwrap().pending.remove(&id);
                        Err(AcpError::closed(format!(
                            "ACP request '{m}' timed out after {}ms",
                            t.as_millis()
                        )))
                    }
                },
                None => wait.await,
            }
        }) as BoxFuture<'static, Result<Value, AcpError>>;
        (dispatched, completed)
    }

    pub fn notify(&self, method: &str, params: Value) {
        if !self.is_alive() {
            return;
        }
        let line = json!({"jsonrpc":"2.0","method":method,"params":params}).to_string();
        let _ = self.0.writer.send((line, None));
    }

    /// Ends the agent; true once its process tree is confirmed gone.
    pub async fn close(&self) -> bool {
        // Like TS `stdin.end()`: queued lines (a `session/cancel`) still
        // reach the agent, which may then exit on its own.
        let (tx, rx) = oneshot::channel();
        if self.0.writer.send((String::new(), Some(tx))).is_ok() {
            let _ = tokio::time::timeout(Duration::from_millis(500), rx).await;
            let mut alive = self.0.alive.subscribe();
            let _ =
                tokio::time::timeout(Duration::from_millis(250), alive.wait_for(|alive| !*alive))
                    .await;
        }
        let gone = match self.0.pid {
            Some(pid) => tokio::task::spawn_blocking(move || proctree::terminate_child_tree(pid))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(false),
            _ => true,
        };
        self.mark_closed(AcpError::closed("ACP transport closed"));
        gone
    }

    fn close_error(&self) -> Option<AcpError> {
        self.0.state.lock().unwrap().closed.clone()
    }
    fn mark_closed(&self, error: AcpError) {
        let pending = {
            let mut s = self.0.state.lock().unwrap();
            if s.closed.is_some() {
                return;
            }
            s.closed = Some(error.clone());
            std::mem::take(&mut s.pending)
        };
        self.0.alive.send_replace(false);
        let e = AcpError {
            indeterminate: true,
            ..error
        };
        for (_, p) in pending {
            let _ = p.done.send(Err(e.clone()));
        }
    }

    fn handle_line(&self, line: &str) {
        if line.is_empty() || self.close_error().is_some() {
            return;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                self.mark_closed(AcpError::closed("Failed to parse JSON-RPC from ACP agent"));
                if let Some(pid) = self.0.pid {
                    tokio::task::spawn_blocking(move || proctree::terminate_child_tree(pid));
                }
                return;
            }
        };
        if !msg.is_object() {
            return;
        }
        if let Some(method) = msg["method"].as_str() {
            if msg.get("id").is_some() {
                self.handle_request(method.to_owned(), msg);
                return;
            }
            let f = self.0.notify.lock().unwrap().clone();
            if let Some(f) = f {
                f(method, msg.get("params").unwrap_or(&Value::Null));
            }
            return;
        }
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            return;
        };
        let Some(p) = self.0.state.lock().unwrap().pending.remove(&id) else {
            return;
        };
        let result = if let Some(e) = msg.get("error").filter(|e| e.is_object()) {
            Err(AcpError {
                message: e["message"].as_str().unwrap_or("").into(),
                code: e["code"].as_i64(),
                data: e.get("data").cloned(),
                indeterminate: false,
            })
        } else {
            Ok(msg.get("result").cloned().unwrap_or(Value::Null))
        };
        let _ = p.done.send(result);
    }

    fn handle_request(&self, method: String, msg: Value) {
        let id = msg["id"].clone();
        let handler = self.0.handlers.lock().unwrap().get(&method).cloned();
        let writer = self.0.writer.clone();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(handler) = handler else {
            let line = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("Method not found: {method}")}});
            let _ = writer.send((line.to_string(), None));
            return;
        };
        let this = self.clone();
        tokio::spawn(async move {
            let reply = match handler(params, id.clone()).await {
                Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                Err(message) => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":message}})
                }
            };
            if this.close_error().is_none() {
                let _ = writer.send((reply.to_string(), None));
            }
        });
    }

    fn handle_stderr(&self, raw: &str) {
        let records = {
            let mut st = self.0.stderr.lock().unwrap();
            let next = format!("{}{raw}", st.recent);
            st.recent = match model_rejection_index(&next) {
                Some(i) if next.is_char_boundary(i) => {
                    truncate_chars(&next[i..], RECENT_STDERR_WINDOW, false)
                }
                _ => truncate_chars(&next, RECENT_STDERR_WINDOW, true),
            };
            let joined = format!("{}{raw}", st.parse);
            let mut parts: Vec<String> = joined
                .split("\r\n")
                .flat_map(|p| p.split(['\r', '\n']))
                .map(str::to_owned)
                .collect();
            st.parse = parts.pop().unwrap_or_default();
            parts
        };
        for line in records {
            let text = line.trim();
            if !text.is_empty() {
                self.classify(text, true);
                self.0.stderr.lock().unwrap().partial_reported = false;
            }
        }
        let pending = self.0.stderr.lock().unwrap().parse.trim().to_owned();
        if !pending.is_empty() && self.classify(&pending, false) == Classified::Complete {
            let mut st = self.0.stderr.lock().unwrap();
            st.parse.clear();
            st.partial_reported = false;
        }
        let mut st = self.0.stderr.lock().unwrap();
        st.parse = truncate_chars(&st.parse, RECENT_STDERR_WINDOW, true);
    }
    fn flush_stderr(&self) {
        let text = std::mem::take(&mut self.0.stderr.lock().unwrap().parse);
        let text = text.trim();
        if !text.is_empty() {
            self.classify(text, true);
        }
        self.0.stderr.lock().unwrap().partial_reported = false;
    }
    fn stderr_for_close(&self) -> Option<String> {
        let st = self.0.stderr.lock().unwrap();
        if st.recent.is_empty() {
            return None;
        }
        let source = match model_rejection_index(&st.recent) {
            Some(i) if st.recent.is_char_boundary(i) => st.recent[i..].trim(),
            _ => st.recent.trim(),
        };
        (!source.is_empty()).then(|| truncate_chars(source, CLOSE_STDERR_CAP, false))
    }

    fn classify(&self, text: &str, complete: bool) -> Classified {
        let Some(handler) = self.0.stderr_handler.lock().unwrap().clone() else {
            return Classified::None;
        };
        let Some((kind, message, partial)) = classify_stderr(text, complete) else {
            return Classified::None;
        };
        let mut st = self.0.stderr.lock().unwrap();
        let (message, raw) = match message {
            Some(m) => (m, text.to_owned()),
            None if st.model_rejection => return Classified::Complete,
            None => {
                st.model_rejection = true;
                let i = model_rejection_index(text).unwrap_or(0);
                let m = text.get(i..).unwrap_or(text).trim().to_owned();
                (m.clone(), m)
            }
        };
        if partial {
            if st.partial_reported {
                return Classified::Partial;
            }
            st.partial_reported = true;
        }
        drop(st);
        handler(StderrError { kind, message, raw });
        if partial {
            Classified::Partial
        } else {
            Classified::Complete
        }
    }
}

#[derive(PartialEq)]
enum Classified {
    None,
    Partial,
    Complete,
}

fn matches_retry_backoff(text: &str) -> bool {
    text.to_lowercase().contains("retrying in")
}
fn matches_http2_cancel(text: &str) -> bool {
    let l = text.to_lowercase();
    l.contains("http/2") && (l.contains("cancel") || l.contains("0x8"))
}

/// `(kind, message, partial)`; `message: None` means the model-rejection
/// text itself (the caller trims it from the match).
fn classify_stderr(text: &str, complete: bool) -> Option<(&'static str, Option<String>, bool)> {
    let lower = text.to_lowercase();
    if let Some(i) = lower.find("cannot use this model:") {
        let rest = &text[i + "cannot use this model:".len()..];
        if rest.trim_start().chars().next().is_some() {
            return Some(("model_not_found", None, false));
        }
    }
    let has = |s: &str| lower.contains(s);
    if has("status 429") || has("ratelimitexceeded") || has("rate limit") {
        return Some((
            "rate_limit",
            Some("Rate limit exceeded. Please wait before sending more requests.".into()),
            false,
        ));
    }
    if has("status 404") || has("model not found") || has("not_found") {
        return Some((
            "model_not_found",
            Some(format!(
                "Model not found. Available models: {MODEL_PRESETS}"
            )),
            false,
        ));
    }
    if has("status 401")
        || has("status 403")
        || has("unauthenticated")
        || has("permission denied")
        || has("authentication")
    {
        return Some(("authentication", Some("Authentication failed. Please check your credentials or run \"gemini auth login\".".into()), false));
    }
    if has("quota") || has("resource exhausted") || has("resourceexhausted") {
        return Some((
            "quota_exceeded",
            Some("API quota exceeded. Please check your billing or wait for quota reset.".into()),
            false,
        ));
    }
    if matches_retry_backoff(text) {
        return Some((
            "unknown",
            Some(
                "The ACP agent is retrying after an upstream failure. The turn may be stalled."
                    .into(),
            ),
            false,
        ));
    }
    if matches_http2_cancel(text) {
        return Some((
            "unknown",
            Some("Upstream request was cancelled. The agent may be retrying or stalled.".into()),
            false,
        ));
    }
    if has("canceled") && !complete {
        return None;
    }
    if has("error") || has("failed") || has("exception") {
        return Some(("unknown", Some(text.into()), true));
    }
    None
}

async fn write_loop(
    inner: std::sync::Weak<Inner>,
    mut stdin: ChildStdin,
    mut rx: mpsc::UnboundedReceiver<(String, Option<oneshot::Sender<()>>)>,
) {
    while let Some((line, done)) = rx.recv().await {
        if line.is_empty() {
            let _ = stdin.shutdown().await;
            drop(stdin);
            if let Some(done) = done {
                let _ = done.send(());
            }
            return;
        }
        let result = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        }
        .await;
        match result {
            Ok(()) => {
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            Err(e) => {
                if let Some(inner) = inner.upgrade() {
                    Transport(inner).mark_closed(AcpError::closed(e.to_string()));
                }
                break;
            }
        }
        if inner
            .upgrade()
            .is_none_or(|i| i.state.lock().unwrap().closed.is_some() && rx.is_empty())
        {
            break;
        }
    }
    let _ = stdin.shutdown().await;
}

async fn wait_loop(
    inner: std::sync::Weak<Inner>,
    mut child: Child,
    out_done: oneshot::Receiver<()>,
    err_done: oneshot::Receiver<()>,
) {
    let status = child.wait().await;
    if let Some(i) = inner.upgrade() {
        i.state.lock().unwrap().exited = true;
        i.alive.send_replace(false);
    }
    // 'close': both pipes drained (bounded, a grandchild may hold them open).
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        let _ = out_done.await;
        let _ = err_done.await;
    })
    .await;
    let Some(i) = inner.upgrade() else { return };
    let t = Transport(i);
    t.flush_stderr();
    let attribution = match &status {
        Ok(s) => {
            #[cfg(unix)]
            let signal = std::os::unix::process::ExitStatusExt::signal(s);
            #[cfg(not(unix))]
            let signal: Option<i32> = None;
            format!(
                "code={}, signal={}",
                s.code().map_or("null".into(), |c| c.to_string()),
                signal.map_or("null".into(), signal_name)
            )
        }
        Err(e) => e.to_string(),
    };
    let mut message = format!("ACP process exited ({attribution})");
    if let Some(stderr) = t.stderr_for_close() {
        message = format!("{message}. stderr: {stderr}");
    }
    t.mark_closed(AcpError::closed(message));
}

fn signal_name(n: i32) -> String {
    match n {
        1 => "SIGHUP",
        2 => "SIGINT",
        9 => "SIGKILL",
        15 => "SIGTERM",
        _ => return n.to_string(),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_stderr_like_ts() {
        assert_eq!(
            classify_stderr("Error: status 429 Too Many Requests", true)
                .unwrap()
                .0,
            "rate_limit"
        );
        assert_eq!(
            classify_stderr("request failed: status 401", true)
                .unwrap()
                .0,
            "authentication"
        );
        assert!(classify_stderr("context canceled", false).is_none());
        assert_eq!(
            classify_stderr("something failed", true).unwrap(),
            ("unknown", Some("something failed".into()), true)
        );
        assert!(classify_stderr("just a log line", true).is_none());
        assert_eq!(
            classify_stderr("Cannot use this model: x", true).unwrap().0,
            "model_not_found"
        );
        assert!(classify_stderr("Cannot use this model:", true).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn frames_requests_and_survives_chunking() {
        let script = r#"while IFS= read -r line; do id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p'); printf '{"jsonrpc":"2.0","method":"note","params":{"n":%s}}\n{"jsonrpc":"2.0",' "$id"; sleep 0.01; printf '"id":%s,"result":{"ok":%s}}\n' "$id" "$id"; done"#;
        let t = Transport::spawn(SpawnOptions {
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: None,
            cwd: None,
        })
        .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        t.on_notification(Some(Arc::new(move |_, p| {
            s.lock().unwrap().push(p["n"].clone())
        })));
        let a = t.request("a", json!({}), None).await.unwrap();
        let b = t.request("b", json!({}), None).await.unwrap();
        assert_eq!((a["ok"].clone(), b["ok"].clone()), (json!(1), json!(2)));
        assert_eq!(*seen.lock().unwrap(), vec![json!(1), json!(2)]);
        assert!(t.close().await);
        let e = t.request("c", json!({}), None).await.unwrap_err();
        assert!(e.indeterminate);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exit_rejects_pending_as_indeterminate() {
        let t = Transport::spawn(SpawnOptions {
            command: "sh".into(),
            args: vec!["-c".into(), "read line; echo boom >&2; exit 3".into()],
            env: None,
            cwd: None,
        })
        .unwrap();
        let e = t.request("x", json!({}), None).await.unwrap_err();
        assert!(e.indeterminate);
        assert!(
            e.message.starts_with("ACP process exited (code=3"),
            "{}",
            e.message
        );
        assert!(e.message.contains("stderr: boom"));
    }
}
