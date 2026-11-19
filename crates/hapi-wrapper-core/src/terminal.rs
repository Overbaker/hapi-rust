use crate::session::metadata::invoked_cwd;
use nix::{
    fcntl::{fcntl, FcntlArg, FdFlag, OFlag},
    libc,
    pty::{openpty, Winsize},
    sys::signal::Signal,
    unistd::{read, setsid, write},
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    env,
    ffi::OsString,
    io,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::process::ExitStatusExt,
    },
    path::Path,
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{unix::AsyncFd, Interest},
    process::{Child, Command},
    sync::{mpsc, oneshot},
    time::{sleep_until, Instant},
};

type Env = HashMap<OsString, OsString>;
fn filtered_env(source: impl IntoIterator<Item = (OsString, OsString)>) -> Env {
    let mut env: Env = source
        .into_iter()
        .filter(|(k, v)| {
            !v.is_empty()
                && !matches!(
                    k.to_str(),
                    Some(
                        "CLI_API_TOKEN"
                            | "HAPI_API_URL"
                            | "HAPI_HTTP_MCP_URL"
                            | "TELEGRAM_BOT_TOKEN"
                            | "OPENAI_API_KEY"
                            | "ANTHROPIC_API_KEY"
                            | "GEMINI_API_KEY"
                            | "GOOGLE_API_KEY"
                            | "HAPI_AGENT_CONTEXT"
                            | "CURSOR_AGENT"
                            | "CURSOR_AGENT_SESSION_ID"
                            | "CLAUDECODE"
                    )
                )
        })
        .collect();
    env.insert("HAPI_USER_TERMINAL".into(), "1".into());
    for (k, v) in [
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("LANG", "C.UTF-8"),
    ] {
        env.entry(k.into()).or_insert_with(|| v.into());
    }
    env
}
fn env_number(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|s| {
            s.trim_start()
                .trim_start_matches('+')
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
        .filter(|n| *n > 0)
        .unwrap_or(default)
}
type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;
#[derive(Clone)]
struct Events {
    session: String,
    id: String,
    emit: Emit,
}
impl Events {
    fn send(&self, name: &str, mut body: Value) {
        body["sessionId"] = json!(self.session);
        body["terminalId"] = json!(self.id);
        (self.emit)(name, body);
    }
    fn error(&self, msg: &str) {
        self.send("terminal:error", json!({"message":msg}));
    }
}
enum Input {
    Write(Vec<u8>),
    Resize(Winsize, bool),
}
struct Handle {
    tx: mpsc::UnboundedSender<Input>,
    _close: oneshot::Sender<()>,
}
pub struct TerminalManager {
    session: String,
    emit: Emit,
    env: Env,
    idle: Duration,
    max: u64,
    terminals: HashMap<String, Handle>,
}
impl TerminalManager {
    pub fn new(session: &str, emit: impl Fn(&str, Value) + Send + Sync + 'static) -> Self {
        Self {
            session: session.into(),
            emit: Arc::new(emit),
            env: filtered_env(env::vars_os()),
            idle: Duration::from_millis(env_number("HAPI_TERMINAL_IDLE_TIMEOUT_MS", 900000)),
            max: env_number("HAPI_TERMINAL_MAX_TERMINALS", 4),
            terminals: HashMap::new(),
        }
    }
    pub fn handle(&mut self, name: &str, p: &Value, cwd: Option<&Path>) {
        let Some(id) = p["terminalId"].as_str().filter(|v| !v.is_empty()) else {
            return;
        };
        if p["sessionId"].as_str() != Some(self.session.as_str()) {
            return;
        }
        self.terminals.retain(|_, h| !h.tx.is_closed());
        let events = Events {
            session: self.session.clone(),
            id: id.into(),
            emit: self.emit.clone(),
        };
        match name {
            "terminal:open" | "terminal:resize" => {
                let dim = |k: &str| p[k].as_u64().filter(|n| *n > 0).map(|n| n as u16);
                let (Some(cols), Some(rows)) = (dim("cols"), dim("rows")) else {
                    return;
                };
                let size = Winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                let open = name == "terminal:open";
                if let Some(h) = self.terminals.get(id) {
                    let _ = h.tx.send(Input::Resize(size, open));
                    return;
                }
                if !open {
                    return;
                }
                if self.terminals.len() as u64 >= self.max {
                    events.error(&format!("Too many terminals open (max {}).", self.max));
                    return;
                }
                let path = cwd.map(Path::to_owned).map(Ok).unwrap_or_else(invoked_cwd);
                match path.and_then(|path| spawn(&self.env, &path, size)) {
                    Ok((master, child)) => {
                        let (tx, rx) = mpsc::unbounded_channel();
                        let (close, closing) = oneshot::channel();
                        self.terminals
                            .insert(id.into(), Handle { tx, _close: close });
                        events.send("terminal:ready", json!({}));
                        tokio::spawn(run(master, child, events, rx, closing, self.idle));
                    }
                    Err(_) => events.error("Failed to spawn terminal."),
                }
            }
            "terminal:write" => {
                if let Some(data) = p["data"].as_str() {
                    if let Some(h) = self.terminals.get(id) {
                        let _ = h.tx.send(Input::Write(data.as_bytes().into()));
                    } else {
                        events.error("Terminal not found.");
                    }
                }
            }
            "terminal:close" => {
                self.terminals.remove(id);
            }
            _ => {}
        }
    }
    pub fn close_all(&mut self) {
        self.terminals.clear();
    }
}
fn spawn(env: &Env, path: &Path, size: Winsize) -> io::Result<(AsyncFd<OwnedFd>, Child)> {
    let pair = openpty(Some(&size), None)?;
    for fd in [&pair.master, &pair.slave] {
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    }
    fcntl(&pair.master, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
    let master = unsafe { AsyncFd::register(pair.master) }?;
    let mut cmd = Command::new(
        env.get(std::ffi::OsStr::new("SHELL"))
            .cloned()
            .unwrap_or_else(|| "/bin/bash".into()),
    );
    cmd.env_clear()
        .envs(env)
        .current_dir(path)
        .kill_on_drop(true)
        .stdin(Stdio::from(pair.slave.try_clone()?))
        .stdout(Stdio::from(pair.slave.try_clone()?))
        .stderr(Stdio::from(pair.slave));
    unsafe {
        cmd.pre_exec(|| {
            setsid()?;
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok((master, cmd.spawn()?))
}
fn decode(bytes: &[u8], pending: &mut Vec<u8>) -> String {
    pending.extend_from_slice(bytes);
    let mut text = String::new();
    let mut bytes = pending.as_slice();
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(s) => {
                text.push_str(s);
                bytes = &[]
            }
            Err(e) => {
                let (valid, rest) = bytes.split_at(e.valid_up_to());
                text.push_str(std::str::from_utf8(valid).unwrap());
                let Some(n) = e.error_len() else {
                    bytes = rest;
                    break;
                };
                text.push('\u{fffd}');
                bytes = &rest[n..];
            }
        }
    }
    let used = pending.len() - bytes.len();
    pending.drain(..used);
    text
}
fn output(fd: &OwnedFd, events: &Events, pending: &mut Vec<u8>) -> io::Result<bool> {
    let mut buf = [0; 8192];
    match read(fd, &mut buf) {
        Ok(0) | Err(nix::errno::Errno::EIO) => Ok(false),
        Ok(n) => {
            let text = decode(&buf[..n], pending);
            if !text.is_empty() {
                events.send("terminal:output", json!({"data":text}));
            }
            Ok(true)
        }
        Err(e) => Err(e.into()),
    }
}
async fn run(
    master: AsyncFd<OwnedFd>,
    mut child: Child,
    events: Events,
    mut rx: mpsc::UnboundedReceiver<Input>,
    mut closing: oneshot::Receiver<()>,
    idle: Duration,
) {
    let mut utf8 = Vec::new();
    let mut writes = VecDeque::new();
    let mut reading = true;
    let mut last = Instant::now();
    let status: Option<io::Result<ExitStatus>> = loop {
        tokio::select! {
            _ = &mut closing => break None,
            _ = async {
                if let Some(deadline) = last.checked_add(idle) {
                    sleep_until(deadline).await
                } else {
                    std::future::pending().await
                }
            }, if !idle.is_zero() => {
                events.error("Terminal closed due to inactivity.");
                break None;
            }
            status = child.wait() => {
                while matches!(output(master.get_ref(), &events, &mut utf8), Ok(true)) {}
                break Some(status);
            }
            msg = rx.recv() => {
                match msg {
                    Some(Input::Write(data)) => writes.extend(data),
                    Some(Input::Resize(size, ready)) => {
                        if unsafe { libc::ioctl(master.get_ref().as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
                            events.error("Terminal stream closed unexpectedly.");
                        } else if ready {
                            events.send("terminal:ready", json!({}));
                        }
                    }
                    None => break None,
                }
                last = Instant::now();
            }
            result = master.async_io(Interest::READABLE, |fd| output(fd, &events, &mut utf8)), if reading => {
                match result {
                    Ok(true) => last = Instant::now(),
                    Ok(false) => reading = false,
                    Err(_) => {
                        events.error("Terminal stream closed unexpectedly.");
                        reading = false;
                    }
                }
            }
            result = master.async_io(Interest::WRITABLE, |fd| write(fd, writes.as_slices().0).map_err(io::Error::from)), if !writes.is_empty() => {
                match result {
                    Ok(count) => { writes.drain(..count); }
                    Err(_) => {
                        events.error("Terminal stream closed unexpectedly.");
                        break None;
                    }
                }
            }
        }
    };
    rx.close();
    if status.is_none() {
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGTERM);
        }
    }
    drop(master);
    let status = match status {
        Some(s) => s,
        None => match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(status) => status,
            Err(_) => {
                let _ = child.start_kill();
                child.wait().await
            }
        },
    };
    let status = status.ok();
    let signal = status
        .and_then(|s| s.signal())
        .and_then(|n| Signal::try_from(n).ok())
        .map(|sig| sig.as_str());
    events.send(
        "terminal:exit",
        json!({"code":status.and_then(|s|s.code()),"signal":signal}),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    #[test]
    fn strips_secrets_and_markers() {
        let env = filtered_env(
            [
                ("PATH", "/usr/bin"),
                ("HOME", "/home/op"),
                ("HAPI_SESSION_ID", "s"),
                ("CURSOR_AGENT", "1"),
                ("CURSOR_AGENT_SESSION_ID", "s"),
                ("CLAUDECODE", "1"),
                ("HAPI_AGENT_CONTEXT", "1"),
                ("CLI_API_TOKEN", "s"),
                ("OPENAI_API_KEY", "s"),
            ]
            .map(|(k, v)| (k.into(), v.into())),
        );
        assert_eq!(env.len(), 7);
        assert_eq!(env[OsStr::new("HAPI_USER_TERMINAL")], "1");
        assert_eq!(env[OsStr::new("HAPI_SESSION_ID")], "s");
    }
    #[test]
    fn split_utf8() {
        let mut buf = Vec::new();
        assert_eq!(decode(&[65, 0xe2], &mut buf), "A");
        assert_eq!(decode(&[0x82, 0xac, 255], &mut buf), "€�");
    }
    fn manager() -> (TerminalManager, mpsc::UnboundedReceiver<(String, Value)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut m = TerminalManager::new("s", move |event, p| {
            let _ = tx.send((event.into(), p));
        });
        m.env.insert("SHELL".into(), "/bin/sh".into());
        (m, rx)
    }
    fn open(id: &str) -> Value {
        json!({"sessionId":"s","terminalId":id,"cols":80,"rows":24})
    }
    #[tokio::test]
    async fn shell_has_filtered_env_and_real_pty() {
        let (mut m, mut rx) = manager();
        let cwd = tempfile::tempdir().unwrap();
        let mut p = open("pty");
        p["sessionId"] = "wrong".into();
        m.handle("terminal:open", &p, None);
        assert!(rx.try_recv().is_err());
        p["sessionId"] = "s".into();
        m.handle("terminal:open", &p, Some(cwd.path()));
        assert_eq!(rx.recv().await.unwrap().0, "terminal:ready");
        p["rows"] = 42.into();
        m.handle("terminal:resize", &p, None);
        p["data"]="printf '\\n%s|%s|%s\\n' \"$HAPI_USER_TERMINAL\" \"${CLI_API_TOKEN:-clean}\" \"$PWD\"; stty size; exit 7\n".into();
        m.handle("terminal:write", &p, None);
        let mut output = String::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (name, body) = rx.recv().await.unwrap();
                assert_eq!(body["terminalId"], "pty");
                assert_eq!(body["sessionId"], "s");
                match name.as_str() {
                    "terminal:output" => output.push_str(body["data"].as_str().unwrap()),
                    "terminal:exit" => {
                        assert_eq!(body["code"], 7);
                        assert!(body["signal"].is_null());
                        break;
                    }
                    other => panic!("{other} {body}"),
                }
            }
        })
        .await
        .unwrap();
        assert!(output.contains(&format!("1|clean|{}", cwd.path().display())));
        assert!(output.contains("42 80"));
    }
    #[tokio::test]
    async fn limits_reopen_and_idle_close() {
        let (mut m, mut rx) = manager();
        m.max = 1;
        m.idle = Duration::from_millis(100);
        let p = open("t");
        m.handle("terminal:open", &p, None);
        m.handle("terminal:open", &p, None);
        m.handle("terminal:open", &open("other"), None);
        let mut ready = 0;
        let mut errors = Vec::new();
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let (name, p) = rx.recv().await.unwrap();
                match name.as_str() {
                    "terminal:ready" => ready += 1,
                    "terminal:error" => errors.push(p["message"].as_str().unwrap().to_string()),
                    "terminal:exit" => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(ready, 2);
        assert_eq!(
            errors,
            [
                "Too many terminals open (max 1).",
                "Terminal closed due to inactivity."
            ]
        );
        let mut p = open("t");
        p["data"] = "x".into();
        m.handle("terminal:write", &p, None);
        assert_eq!(rx.recv().await.unwrap().1["message"], "Terminal not found.");
        m.handle("terminal:open", &p, None);
        m.handle("terminal:close", &p, None);
        assert!(m.terminals.is_empty());
    }
}
