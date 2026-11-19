use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot},
};
use uuid::Uuid;

#[derive(Default)]
pub struct Options {
    pub executable: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub custom_system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub max_turns: Option<u32>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub can_call_tool: bool,
    pub continue_conversation: bool,
    pub resume: Option<String>,
    pub fork_session: bool,
    pub additional_args: Vec<String>,
    pub settings_path: Option<String>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub additional_directories: Vec<String>,
    pub strict_mcp_config: bool,
    pub permission_mode: Option<String>,
    pub fallback_model: Option<String>,
    pub replay_user_messages: bool,
    pub mcp_servers: Option<Value>,
}

impl Options {
    pub fn argv(&self, prompt: Option<&str>) -> io::Result<Vec<String>> {
        let mut args = vec![
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
        ];
        fn pair(v: &mut Vec<String>, name: &str, value: Option<&str>) {
            if let Some(value) = value.filter(|s| !s.is_empty()) {
                v.extend([name.into(), value.into()]);
            }
        }
        pair(
            &mut args,
            "--system-prompt",
            self.custom_system_prompt.as_deref(),
        );
        pair(
            &mut args,
            "--append-system-prompt",
            self.append_system_prompt.as_deref(),
        );
        pair(
            &mut args,
            "--max-turns",
            self.max_turns
                .filter(|n| *n > 0)
                .map(|n| n.to_string())
                .as_deref(),
        );
        pair(&mut args, "--model", self.model.as_deref());
        pair(&mut args, "--effort", self.effort.as_deref());
        if self.can_call_tool {
            if prompt.is_some() {
                return Err(io::Error::other("canCallTool callback requires --input-format stream-json. Please set prompt as an AsyncIterable."));
            }
            pair(&mut args, "--permission-prompt-tool", Some("stdio"));
        }
        if self.continue_conversation {
            args.push("--continue".into());
        }
        pair(&mut args, "--resume", self.resume.as_deref());
        if self.fork_session {
            args.push("--fork-session".into());
        }
        args.extend_from_slice(&self.additional_args);
        pair(&mut args, "--settings", self.settings_path.as_deref());
        for (flag, tools) in [
            ("--allowedTools", &self.allowed_tools),
            ("--disallowedTools", &self.disallowed_tools),
        ] {
            if !tools.is_empty() {
                args.extend([flag.into(), tools.join(",")]);
            }
        }
        if !self.additional_directories.is_empty() {
            args.push("--add-dir".into());
            args.extend_from_slice(&self.additional_directories);
        }
        if self.strict_mcp_config {
            args.push("--strict-mcp-config".into());
        }
        pair(
            &mut args,
            "--permission-mode",
            Some(self.permission_mode.as_deref().unwrap_or("default")),
        );
        if self.model.as_ref().is_some_and(|s| !s.is_empty()) && self.fallback_model == self.model {
            return Err(io::Error::other("Fallback model cannot be the same as the main model. Please specify a different model for fallbackModel option."));
        }
        pair(
            &mut args,
            "--fallback-model",
            self.fallback_model.as_deref(),
        );
        if let Some(s) = prompt {
            args.extend(["--print".into(), s.trim().into()]);
        } else {
            args.extend(["--input-format".into(), "stream-json".into()]);
            if self.replay_user_messages {
                args.push("--replay-user-messages".into());
            }
        }
        if let Some(m) = self
            .mcp_servers
            .as_ref()
            .filter(|m| m.as_object().is_some_and(|o| !o.is_empty()))
        {
            args.extend(["--mcp-config".into(), json!({"mcpServers":m}).to_string()]);
        }
        Ok(args)
    }
}

type Reply = oneshot::Sender<io::Result<Value>>;
enum Input {
    Prompt(Value),
    End,
    Request(Value, Reply),
    Response(String, io::Result<Value>),
    Fail(String),
}
#[derive(Debug)]
pub enum Event {
    Message(Value),
    Permission {
        id: String,
        tool: Value,
        input: Value,
    },
    Cancel(String),
}
#[derive(Clone)]
pub struct InputStream(mpsc::UnboundedSender<Input>);
impl InputStream {
    fn send(&self, input: Input) -> io::Result<()> {
        self.0
            .send(input)
            .map_err(|_| io::Error::other("Claude Code process closed"))
    }
    pub fn prompt(&self, v: Value) -> io::Result<()> {
        self.send(Input::Prompt(v))
    }
    pub fn forward(&self, prompts: impl Stream<Item = io::Result<Value>> + Send + 'static) {
        let input = self.clone();
        tokio::spawn(async move {
            let mut prompts = Box::pin(prompts);
            loop {
                let next = tokio::select! {
                    _ = input.0.closed() => return,
                    next = prompts.next() => next,
                };
                match next {
                    Some(Ok(message)) => {
                        if input.prompt(message).is_err() {
                            return;
                        }
                    }
                    Some(Err(error)) => {
                        let _ = input.fail(error.to_string());
                        return;
                    }
                    None => {
                        let _ = input.end();
                        return;
                    }
                }
            }
        });
    }

    pub fn end(&self) -> io::Result<()> {
        self.send(Input::End)
    }
    pub fn fail(&self, s: impl Into<String>) -> io::Result<()> {
        self.send(Input::Fail(s.into()))
    }
    pub fn abort(&self) -> io::Result<()> {
        self.fail("Claude Code process aborted by user")
    }
    pub fn respond(&self, id: String, result: io::Result<Value>) -> io::Result<()> {
        self.send(Input::Response(id, result))
    }
    pub async fn request(&self, v: Value) -> io::Result<Value> {
        let (tx, rx) = oneshot::channel();
        self.send(Input::Request(v, tx))?;
        rx.await
            .map_err(|_| io::Error::other("Claude Code process closed"))?
    }
    pub async fn interrupt(&self) -> io::Result<Value> {
        self.request(json!({"subtype":"interrupt"})).await
    }
    pub async fn context_usage(&self) -> io::Result<Value> {
        self.request(json!({"subtype":"get_context_usage"})).await
    }
    pub async fn set_mode(&self, mode: &str) -> io::Result<Value> {
        self.request(json!({"subtype":"set_permission_mode","mode":mode}))
            .await
    }
    pub async fn set_effort(&self, effort: Option<&str>) -> io::Result<Value> {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.request(
                json!({"subtype":"apply_flag_settings","settings":{"effortLevel":effort}}),
            ),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out"))?
    }
    pub async fn account_identity(&self) -> io::Result<Option<String>> {
        let data = self.request(json!({"subtype":"initialize"})).await?;
        Ok(["email", "accountUuid"].iter().find_map(|k| {
            data["account"][k]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| format!("acct:{}", s.to_lowercase()))
        }))
    }
}

pub struct Query {
    pub input: InputStream,
    pub events: mpsc::UnboundedReceiver<io::Result<Event>>,
}
impl Query {
    pub fn spawn_stream(
        options: &Options,
        prompts: impl Stream<Item = io::Result<Value>> + Send + 'static,
    ) -> io::Result<Self> {
        let query = Self::spawn(options, None)?;
        query.input.forward(prompts);
        Ok(query)
    }

    pub fn spawn(options: &Options, prompt: Option<&str>) -> io::Result<Self> {
        let exe = options
            .executable
            .clone()
            .or_else(|| {
                std::env::var("HAPI_CLAUDE_PATH").ok().and_then(|s| {
                    let s = s.trim();
                    (!s.is_empty()).then(|| PathBuf::from(s))
                })
            })
            .unwrap_or_else(|| "claude".into());
        let mut command = Command::new(exe);
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        if std::env::var_os("CLAUDE_CODE_ENTRYPOINT").is_none_or(|s| s.is_empty()) {
            command.env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
        }
        let bypass = std::env::var("NO_PROXY")
            .or_else(|_| std::env::var("no_proxy"))
            .unwrap_or_default();
        let mut entries: Vec<_> = bypass
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if !entries.contains(&"*") {
            for host in ["localhost", "127.0.0.1", "::1"] {
                if !entries.iter().any(|entry| entry.eq_ignore_ascii_case(host)) {
                    entries.push(host);
                }
            }
            let merged = entries.join(",");
            command.env("NO_PROXY", &merged).env("no_proxy", &merged);
        }
        if prompt.is_none() {
            command.env("DISABLE_AUTOUPDATER", "1");
        }
        command
            .args(options.argv(prompt)?)
            .env_remove("BUN_BE_BUN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(
                if std::env::var_os("DEBUG").is_some_and(|s| !s.is_empty()) {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                },
            )
            .kill_on_drop(true);
        let child = command
            .spawn()
            .map_err(|e| io::Error::other(format!("Failed to spawn Claude Code process: {e}")))?;
        Ok(Self::attach(child, prompt.is_none(), options.can_call_tool))
    }

    fn attach(mut child: Child, stream: bool, can_call: bool) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (events, output) = mpsc::unbounded_channel();
        let mut pipe = child.stdin.take().unwrap();
        let (frames, mut writes) = mpsc::unbounded_channel();
        let (failed, mut write_error) = mpsc::unbounded_channel();
        let mut frames = stream.then_some(frames);
        let writer = tokio::spawn(async move {
            while let Some(frame) = writes.recv().await {
                if let Err(error) = write_line(&mut pipe, &frame).await {
                    let _ = failed.send(error);
                    break;
                }
            }
        });
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::spawn(async move {
            let mut protocol = Protocol::default();
            let mut stdout_open = true;
            let result: io::Result<()> = async {
                loop {
                    let frame = tokio::select! {
                        biased;
                        _ = events.closed() => return Err(io::Error::other("Query closed")),
                        action = rx.recv() => match action {
                            None => return Err(io::Error::other("Query closed")),
                            Some(Input::Fail(reason)) => return Err(io::Error::other(reason)),
                            Some(Input::End) => { frames.take(); continue; }
                            Some(Input::Prompt(value)) => Some(value),
                            Some(Input::Request(value, reply)) => {
                                if frames.is_none() {
                                    let _ = reply.send(Err(io::Error::other("Request requires --input-format stream-json")));
                                    continue;
                                }
                                Some(protocol.request(value, reply))
                            }
                            Some(Input::Response(id, result)) => protocol.permissions.remove(&id)
                                .then(|| control_response(&id, result)),
                        },
                        Some(error) = write_error.recv() => return Err(error),
                        line = lines.next_line(), if stdout_open => {
                            let Some(line) = line? else { stdout_open = false; continue; };
                            let Ok(value) = serde_json::from_str(&line) else { continue; };
                            protocol.incoming(value, frames.is_some(), can_call, &events)
                        }
                        status = child.wait(), if !stdout_open => {
                            let status = status?;
                            if status.success() { return Ok(()); }
                            let code = status.code().map(|n| n.to_string()).unwrap_or_else(|| "null".into());
                            return Err(io::Error::other(format!("Claude Code process exited with code {code}")));
                        }
                    };
                    if let Some(frame) = frame {
                        frames.as_ref().ok_or_else(|| io::Error::other("Claude stdin closed"))?
                            .send(frame).map_err(|_| io::Error::other("Claude stdin closed"))?;
                    }
                }
            }.await;
            writer.abort();
            if let Err(e) = result {
                super::child_tree::stop(&mut child).await;
                let _ = events.send(Err(e));
            }
        });
        Self {
            input: InputStream(tx),
            events: output,
        }
    }
}
#[derive(Default)]
struct Protocol {
    pending: HashMap<String, Reply>,
    permissions: HashSet<String>,
}
impl Protocol {
    fn request(&mut self, request: Value, reply: Reply) -> Value {
        let mut bits = Uuid::new_v4().as_u128();
        let id: String = (0..11)
            .map(|_| {
                let digit = (bits % 36) as u32;
                bits /= 36;
                char::from_digit(digit, 36).unwrap()
            })
            .collect();
        self.pending.insert(id.clone(), reply);
        json!({"type":"control_request","request_id":id,"request":request})
    }

    fn incoming(
        &mut self,
        value: Value,
        writable: bool,
        can_call: bool,
        events: &mpsc::UnboundedSender<io::Result<Event>>,
    ) -> Option<Value> {
        let id = value["request_id"].as_str().unwrap_or_default();
        match value["type"].as_str() {
            Some("control_response") => {
                let response = &value["response"];
                if let Some(reply) = response["request_id"]
                    .as_str()
                    .and_then(|id| self.pending.remove(id))
                {
                    let result = if response["subtype"] == "success" {
                        Ok(response.get("response").cloned().unwrap_or(json!({})))
                    } else {
                        Err(io::Error::other(
                            response["error"].as_str().unwrap_or_default().to_owned(),
                        ))
                    };
                    let _ = reply.send(result);
                }
            }
            Some("control_cancel_request") => {
                if self.permissions.remove(id) {
                    let _ = events.send(Ok(Event::Cancel(id.into())));
                }
            }
            Some("control_request") if writable => {
                let request = &value["request"];
                if request["subtype"] != "can_use_tool" {
                    let subtype = request["subtype"].as_str().unwrap_or("undefined");
                    return Some(control_response(
                        id,
                        Err(io::Error::other(format!(
                            "Unsupported control request subtype: {subtype}"
                        ))),
                    ));
                }
                if !can_call {
                    return Some(control_response(
                        id,
                        Err(io::Error::other("canCallTool callback is not provided.")),
                    ));
                }
                self.permissions.insert(id.into());
                let _ = events.send(Ok(Event::Permission {
                    id: id.into(),
                    tool: request["tool_name"].clone(),
                    input: request["input"].clone(),
                }));
            }
            Some("control_request") => {}
            _ => {
                let _ = events.send(Ok(Event::Message(value)));
            }
        }
        None
    }
}

async fn write_line(pipe: &mut ChildStdin, v: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(v)?;
    bytes.push(b'\n');
    pipe.write_all(&bytes).await
}
fn control_response(id: &str, result: io::Result<Value>) -> Value {
    let body = match result {
        Ok(v) => json!({"subtype":"success","request_id":id,"response":v}),
        Err(e) => json!({"subtype":"error","request_id":id,"error":e.to_string()}),
    };
    json!({"type":"control_response","response":body})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn child(script: &str) -> Query {
        let child = Command::new("python3")
            .args(["-uc", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Query::attach(child, true, true)
    }
    #[test]
    fn settings_after_extra_arguments() {
        let o = Options {
            additional_args: vec!["--plugin-dir".into(), "plugin".into()],
            settings_path: Some("settings".into()),
            ..Default::default()
        };
        assert_eq!(
            o.argv(Some(" hello ")).unwrap(),
            [
                "--output-format",
                "stream-json",
                "--verbose",
                "--plugin-dir",
                "plugin",
                "--settings",
                "settings",
                "--permission-mode",
                "default",
                "--print",
                "hello"
            ]
        );
    }
    #[tokio::test]
    async fn concurrent_requests_and_effort() {
        let q = child(
            r#"import json
a,b=[json.loads(input()) for _ in range(2)]
for v in (b,a):
 print(json.dumps(dict(type='control_response',response=dict(subtype='success',request_id=v['request_id'],response=v['request']))))
for _ in range(2):
 v=json.loads(input())
 print(json.dumps(dict(type='control_response',response=dict(subtype='success',request_id=v['request_id'],response=v['request']))))"#,
        );
        let (a, b) = tokio::join!(q.input.context_usage(), q.input.set_mode("auto"));
        assert_eq!(a.unwrap(), json!({"subtype":"get_context_usage"}));
        assert_eq!(
            b.unwrap(),
            json!({"subtype":"set_permission_mode","mode":"auto"})
        );
        for effort in [Some("low"), None] {
            assert_eq!(
                q.input.set_effort(effort).await.unwrap(),
                json!({"subtype":"apply_flag_settings","settings":{"effortLevel":effort}})
            );
        }
    }
    #[tokio::test]
    async fn unsupported_request_keeps_stream() {
        let mut q = child(
            r#"import json
v=json.loads(input())
print(json.dumps(dict(type='control_response',response=dict(subtype='error',request_id=v['request_id'],error='Unsupported request'))))
print('{"type":"system"}')"#,
        );
        assert_eq!(
            q.input.context_usage().await.unwrap_err().to_string(),
            "Unsupported request"
        );
        assert!(matches!(q.events.recv().await, Some(Ok(Event::Message(_)))));
        assert!(q.events.recv().await.is_none());
    }
    #[tokio::test]
    async fn canceled_permissions_drop_late_responses() {
        let mut q = child(
            r#"import json
for id in ('canceled','answered'):
 print(json.dumps(dict(type='control_request',request_id=id,request=dict(subtype='can_use_tool',tool_name='Read',input={'file':'notes'}))))
 if id=='canceled': print(json.dumps(dict(type='control_cancel_request',request_id=id)))
print(json.dumps(dict(type='user',message=json.loads(input()))))
raise SystemExit(7)"#,
        );
        assert!(
            matches!(q.events.recv().await,Some(Ok(Event::Permission{id,..})) if id=="canceled")
        );
        assert!(matches!(q.events.recv().await,Some(Ok(Event::Cancel(id))) if id=="canceled"));
        q.input.respond("canceled".into(), Ok(json!({}))).unwrap();
        assert!(
            matches!(q.events.recv().await,Some(Ok(Event::Permission{id,tool,input})) if id=="answered" && tool=="Read" && input==json!({"file":"notes"}))
        );
        q.input
            .respond("answered".into(), Ok(json!({"behavior":"allow"})))
            .unwrap();
        let Some(Ok(Event::Message(v))) = q.events.recv().await else {
            panic!("missing message")
        };
        assert_eq!(
            v["message"],
            json!({"type":"control_response","response":{"request_id":"answered","subtype":"success","response":{"behavior":"allow"}}})
        );
        assert_eq!(
            q.events.recv().await.unwrap().unwrap_err().to_string(),
            "Claude Code process exited with code 7"
        );
    }

    #[tokio::test]
    async fn abort_interrupts_full_stdin_pipe() {
        let mut q = child("import time; time.sleep(60)");
        q.input.prompt(json!("a".repeat(1024 * 1024))).unwrap();
        tokio::task::yield_now().await;
        q.input.abort().unwrap();
        let error = tokio::time::timeout(Duration::from_secs(4), q.events.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.to_string(), "Claude Code process aborted by user");
    }

    #[tokio::test]
    async fn prompt_failure_survives_exit_and_hung_child() {
        for script in ["pass", "import time; time.sleep(60)"] {
            let mut q = child(script);
            q.input.forward(futures_util::stream::iter([
                Ok(json!({"type":"user"})),
                Err(io::Error::other("prompt failed")),
            ]));
            let err = tokio::time::timeout(Duration::from_secs(4), q.events.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(err.to_string(), "prompt failed");
        }
    }
}
