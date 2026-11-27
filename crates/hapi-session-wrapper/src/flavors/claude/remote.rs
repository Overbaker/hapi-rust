use super::{
    permission::{native_mode, PermissionHandler},
    query::{Event, InputStream, Options, Query},
    special_commands::{self, SpecialCommand},
    system_prompt,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncSeekExt, BufReader},
    sync::oneshot,
    task::JoinHandle,
};
use uuid::Uuid;

pub(super) fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
pub(super) fn catalog_args(args: &[String]) -> Vec<String> {
    let value_flags = [
        "--add-dir",
        "--plugin-dir",
        "--plugin-url",
        "--settings",
        "--setting-sources",
    ];
    let mut out = Vec::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if matches!(
            arg.as_str(),
            "--bare" | "--disable-slash-commands" | "--safe-mode"
        ) || arg
            .split_once('=')
            .is_some_and(|(flag, _)| value_flags.contains(&flag))
        {
            out.push(arg.clone());
        } else if value_flags.contains(&arg.as_str()) {
            out.push(arg.clone());
            while let Some(value) = iter.next_if(|s| !s.starts_with('-')) {
                out.push(value.clone());
                if arg != "--add-dir" {
                    break;
                }
            }
        }
    }
    out
}

pub(super) struct Launch {
    pub cwd: PathBuf,
    pub project: PathBuf,
    pub settings: PathBuf,
    pub mcp_url: String,
    pub args: Vec<String>,
    pub summary: bool,
}
impl Launch {
    pub fn transcript(&self, id: &str) -> Option<PathBuf> {
        if id.is_empty() || id.contains(['/', '\0']) {
            return None;
        }
        Some(self.project.join(format!("{id}.jsonl")))
    }
    fn valid_resume(&self, id: &str) -> bool {
        use std::io::BufRead;
        self.transcript(id)
            .and_then(|p| std::fs::File::open(p).ok())
            .is_some_and(|f| {
                std::io::BufReader::new(f)
                    .lines()
                    .map_while(Result::ok)
                    .any(|line| {
                        serde_json::from_str::<Value>(&line).is_ok_and(|v| v["uuid"].is_string())
                    })
            })
    }
    pub fn fork(&self) -> bool {
        self.args.iter().any(|s| s == "--fork-session")
    }
    pub fn consume_flags(&mut self) {
        let mut it = self.args.iter().peekable();
        let mut out = Vec::new();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--resume" => {
                    it.next_if(|s| !s.starts_with('-') && s.contains('-'));
                }
                "--fork-session" => {}
                _ => out.push(a.clone()),
            }
        }
        self.args = out;
    }
    fn options(&self, mode: &Value, session: Option<&str>) -> Options {
        let str_field = |k: &str| mode[k].as_str().map(str::to_owned);
        let prompt = system_prompt::system_prompt(self.summary);
        let append = |k: &str| {
            mode[k]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| format!("{s}\n\n{prompt}"))
        };
        let mut tools = strings(&mode["allowedTools"]);
        tools.extend(["change_title", "display_image"].map(|s| format!("mcp__hapi__{s}")));
        if !matches!(
            std::env::var("HAPI_DISABLE_PEER_TOOLS")
                .unwrap_or_default()
                .to_lowercase()
                .as_str(),
            "1" | "yes" | "true"
        ) {
            tools.push("mcp__hapi__list_peers".into());
        }
        tools.push("mcp__hapi__session_job".into());
        let resume = session
            .filter(|id| self.valid_resume(id))
            .map(str::to_owned)
            .or_else(|| {
                self.args
                    .windows(2)
                    .find(|w| w[0] == "--resume")
                    .map(|w| w[1].clone())
                    .filter(|id| !id.starts_with('-') && id.contains('-'))
            });
        Options {
            cwd: Some(self.cwd.clone()),
            resume,
            fork_session: self.fork(),
            additional_args: catalog_args(&self.args),
            settings_path: Some(self.settings.to_string_lossy().into_owned()),
            allowed_tools: tools,
            disallowed_tools: strings(&mode["disallowedTools"]),
            additional_directories: vec![std::env::temp_dir()
                .join("hapi-blobs")
                .to_string_lossy()
                .into_owned()],
            permission_mode: Some(
                native_mode(mode["permissionMode"].as_str().unwrap_or("default")).into(),
            ),
            model: str_field("model"),
            effort: str_field("effort"),
            fallback_model: str_field("fallbackModel"),
            custom_system_prompt: append("customSystemPrompt"),
            append_system_prompt: Some(append("appendSystemPrompt").unwrap_or(prompt)),
            can_call_tool: true,
            replay_user_messages: true,
            mcp_servers: Some(json!({"hapi":{"type":"http","url":self.mcp_url}})),
            ..Default::default()
        }
    }
}

fn push(input: &InputStream, text: &str) -> io::Result<String> {
    let id = Uuid::new_v4().to_string();
    input.prompt(json!({"type":"user","uuid":id,"message":{"role":"user","content":text}}))?;
    Ok(id)
}
#[derive(Clone, Copy)]
pub(super) enum Pickup {
    Delivered,
    Closed,
    Aborted,
}
type Pending = (oneshot::Sender<Pickup>, oneshot::Receiver<()>);
pub(super) struct Target {
    input: InputStream,
    pub hash: String,
    pending: Mutex<Option<HashMap<String, Pending>>>,
}
impl Target {
    /// Returns the pickup outcome and a sender the steer settles once its
    /// consumed ACK is out; output after the pickup waits for it (TS order).
    pub fn steer(
        &self,
        text: &str,
    ) -> Option<(String, oneshot::Receiver<Pickup>, oneshot::Sender<()>)> {
        let mut state = self.pending.lock().unwrap();
        let pending = state.as_mut()?;
        let (tx, rx) = oneshot::channel();
        let (settle, settled) = oneshot::channel();
        let id = push(&self.input, text).ok()?;
        pending.insert(id.clone(), (tx, settled));
        Some((id, rx, settle))
    }
    fn replay(&self, id: &str) -> Option<oneshot::Receiver<()>> {
        let (tx, settled) = self.pending.lock().unwrap().as_mut()?.remove(id)?;
        let _ = tx.send(Pickup::Delivered);
        Some(settled)
    }
    pub fn waiting(&self) -> bool {
        self.pending
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| !p.is_empty())
    }
    pub fn close(&self, reason: Pickup) {
        if let Some(p) = self.pending.lock().unwrap().take() {
            for (tx, _) in p.into_values() {
                let _ = tx.send(reason);
            }
        }
    }
}

#[derive(Default)]
pub(super) struct Compact {
    baseline: Option<(PathBuf, u64)>,
    signal: bool,
    failure: Option<String>,
    before: Option<u64>,
    after: Option<u64>,
}
impl Compact {
    fn start(path: Option<PathBuf>) -> Self {
        Self {
            baseline: path.and_then(|p| Some((p.clone(), std::fs::metadata(p).ok()?.len()))),
            ..Default::default()
        }
    }
    fn observe(&mut self, v: &Value) -> bool {
        if v["type"] == "system" && v["subtype"] == "status" {
            self.signal |= v["status"] == "compacting" || v.get("compact_result").is_some();
            if v["compact_result"] == "failed" {
                self.failure = Some(v["compact_error"].as_str().unwrap_or("").trim().into());
            }
        }
        if v["type"] == "system"
            && v["subtype"] == "compact_boundary"
            && v["compact_metadata"]["trigger"] == "manual"
        {
            let meta = &v["compact_metadata"];
            self.signal = true;
            self.before = meta["pre_tokens"].as_u64();
            self.after = meta["post_tokens"].as_u64();
            return false;
        }
        !(v["type"] == "user"
            && v["message"]["content"]
                .as_str()
                .and_then(|s| s.trim().strip_prefix("<local-command-stdout>"))
                .and_then(|s| s.trim_start().strip_prefix("Compacted"))
                .is_some_and(|s| !s.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')))
    }
    pub async fn completion(self) -> (Value, Option<u64>) {
        if let Some(reason) = self.failure {
            return (
                json!({"type":"message","message":if reason.is_empty(){"📦 Compaction failed".into()}else{format!("📦 Compaction failed: {reason}")}}),
                self.after,
            );
        }
        let mut summary = None;
        if let Some((path, offset)) = self.baseline {
            for i in 0..10 {
                summary = read_summary(&path, offset).await.unwrap_or(None);
                if summary.is_some() || i == 9 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        if summary.is_some() || self.before.is_some() || self.after.is_some() {
            let mut event = json!({"type":"compact-summary","summary":summary.unwrap_or_default()});
            if let Some(n) = self.before {
                event["tokensBefore"] = json!(n);
            }
            if let Some(n) = self.after {
                event["tokensAfter"] = json!(n);
            }
            (event, self.after)
        } else {
            (json!({"type":"message","message":"📦 Compacted"}), None)
        }
    }
}
async fn read_summary(path: &Path, offset: u64) -> io::Result<Option<String>> {
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(io::SeekFrom::Start(offset)).await?;
    let mut lines = BufReader::new(f).lines();
    let mut latest = None;
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v["type"] != "user" || v["isCompactSummary"] != true {
            continue;
        }
        let content = &v["message"]["content"];
        let text: String = if let Some(s) = content.as_str() {
            s.trim().into()
        } else {
            content
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str().map(str::trim))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .into()
        };
        if !text.is_empty() {
            latest = Some(text);
        }
    }
    Ok(latest)
}

pub(super) struct Remote {
    pub query: Query,
    pub fork_wait: bool,
    /// Started without --resume or a fork (TS `onSpawn(fresh)`).
    pub fresh: bool,
    pub resume_target: Option<String>,
    pub target: Option<Arc<Target>>,
    pushed: HashSet<String>,
    permissions: HashMap<String, JoinHandle<()>>,
    pub compact: Option<Compact>,
    pub settling: Option<oneshot::Receiver<()>>,
    pub initial: Option<String>,
    pub first_result: bool,
    pub native: String,
    pub requested: String,
    pub sequence: u64,
    pub switches: usize,
    pub mode: Value,
}
impl Remote {
    pub fn spawn(launch: &Launch, mode: &Value, id: Option<&str>) -> io::Result<Self> {
        let options = launch.options(mode, id);
        let query = Query::spawn(&options, None)?;
        Ok(Self {
            query,
            fork_wait: launch.fork(),
            // A fresh process starts its cumulative usage counters at zero.
            fresh: options.resume.is_none() && !options.fork_session,
            resume_target: options.resume.filter(|_| !options.fork_session),
            target: None,
            pushed: HashSet::new(),
            permissions: HashMap::new(),
            compact: None,
            settling: None,
            initial: None,
            first_result: true,
            native: options.permission_mode.unwrap_or_default(),
            requested: mode["permissionMode"].as_str().unwrap_or("default").into(),
            sequence: 0,
            switches: 0,
            mode: mode.clone(),
        })
    }
    pub fn prompt(&mut self, text: &str, hash: &str, path: Option<PathBuf>) -> io::Result<String> {
        if self.initial.is_none() {
            self.initial = Some(text.into());
        }
        match special_commands::parse(text) {
            None => {
                self.target = Some(Arc::new(Target {
                    input: self.query.input.clone(),
                    hash: hash.into(),
                    pending: Mutex::new(Some(HashMap::new())),
                }))
            }
            Some(SpecialCommand::Compact(_)) => self.compact = Some(Compact::start(path)),
            _ => {}
        }
        let uuid = push(&self.query.input, text)?;
        self.pushed.insert(uuid.clone());
        Ok(uuid)
    }
    pub fn accept(&mut self, v: &mut Value) -> bool {
        if v["type"] == "user" && v["isReplay"] == true {
            if let Some(id) = v["uuid"].as_str() {
                if self.pushed.remove(id) {
                    return false;
                }
                if let Some(settled) = self.target.as_ref().and_then(|s| s.replay(id)) {
                    self.settling = Some(settled);
                    return false;
                }
            }
        }
        if let Some(c) = &mut self.compact {
            if !c.observe(v) {
                return false;
            }
            if c.signal && v["type"] == "result" {
                v["hapi_compact_turn"] = true.into();
            }
        }
        true
    }
    pub fn compact_waiting(&self) -> bool {
        self.compact.as_ref().is_some_and(|c| !c.signal)
    }
    pub fn control(&mut self, event: Event, handler: &Arc<PermissionHandler>) -> Option<Value> {
        match event {
            Event::Message(v) => return Some(v),
            Event::Cancel(id) => {
                if let Some(task) = self.permissions.remove(&id) {
                    task.abort();
                }
            }
            Event::Permission { id, tool, input } => {
                self.permissions.retain(|_, task| !task.is_finished());
                let pipe = self.query.input.clone();
                let handler = handler.clone();
                let key = id.clone();
                self.permissions.insert(
                    id,
                    tokio::spawn(async move {
                        let result = handler
                            .can_use_tool(tool.as_str().unwrap_or(""), input)
                            .await
                            .map_err(io::Error::other);
                        let _ = pipe.respond(key, result);
                    }),
                );
            }
        }
        None
    }
    pub fn close_target(&mut self, reason: Pickup) {
        if let Some(t) = self.target.take() {
            t.close(reason);
        }
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        self.close_target(Pickup::Closed);
        for t in self.permissions.values() {
            t.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn compact_requires_signal_and_only_reads_appended_rows() {
        let path = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let row = |s: &str| {
            format!(
                "{}\n",
                json!({"type":"user","isCompactSummary":true,"message":{"content":s}})
            )
        };
        std::fs::write(&path, row("previous")).unwrap();
        let mut c = Compact::start(Some(path.clone()));
        assert!(c.observe(&json!({"type":"result"})));
        assert!(!c.signal);
        assert!(c.observe(&json!({"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"auto"}})));
        assert!(!c.observe(&json!({"type":"user","message":{"content":"<local-command-stdout>Compacted PostCompact [script]"}})));
        assert!(!c.observe(&json!({"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","post_tokens":12}})));
        std::fs::write(&path, row("previous") + &row("new")).unwrap();
        assert_eq!(
            c.completion().await,
            (
                json!({"type":"compact-summary","summary":"new","tokensAfter":12}),
                Some(12)
            )
        );
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn catalog_filters_and_retry_flags() {
        assert_eq!(
            catalog_args(
                &[
                    "--resume",
                    "a-b",
                    "--safe-mode",
                    "--add-dir",
                    "a",
                    "b",
                    "--settings=x",
                    "--other"
                ]
                .map(str::to_owned)
            ),
            ["--safe-mode", "--add-dir", "a", "b", "--settings=x"]
        );
    }
}
