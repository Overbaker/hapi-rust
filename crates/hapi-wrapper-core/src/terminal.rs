use crate::lifecycle::proctree::Tree;
use crate::session::metadata::invoked_cwd;
use nix::{
    fcntl::OFlag,
    libc,
    pty::{grantpt, posix_openpt, unlockpt, Winsize},
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
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::ExitStatusExt,
    },
    path::Path,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{unix::AsyncFd, Interest},
    process::{Child, Command},
    sync::{mpsc, oneshot, watch},
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
    active: Arc<Mutex<bool>>,
}
impl Events {
    fn send(&self, name: &str, mut body: Value) {
        let mut active = self.active.lock().unwrap();
        if !*active {
            return;
        }
        if matches!(name, "terminal:error" | "terminal:exit") {
            *active = false;
        }
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
    close: Option<oneshot::Sender<()>>,
    active: Arc<Mutex<bool>>,
}
impl Handle {
    fn running(&self) -> bool {
        self.close.is_some() && !self.tx.is_closed() && *self.active.lock().unwrap()
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        *self.active.lock().unwrap() = false;
    }
}
pub struct TerminalManager {
    session: String,
    emit: Emit,
    env: Env,
    idle: Duration,
    max: u64,
    pub(crate) shutdown: watch::Sender<bool>,
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
            shutdown: watch::channel(false).0,
            terminals: HashMap::new(),
        }
    }
    pub(crate) fn connected(&mut self, socket: crate::sio::client::Client, generation: u64) {
        self.emit = Arc::new(move |name, body| {
            socket.emit_on_connection(generation, name, vec![body]);
        });
    }
    pub(crate) fn disconnected(&mut self) {
        self.terminals.clear();
    }
    pub fn handle(&mut self, name: &str, p: &Value, cwd: Option<&Path>) {
        let Some(id) = p["terminalId"].as_str().filter(|v| !v.is_empty()) else {
            return;
        };
        if p["sessionId"].as_str() != Some(self.session.as_str()) {
            return;
        }
        self.terminals
            .retain(|_, h| *h.active.lock().unwrap() || !h.tx.is_closed());
        let events = Events {
            session: self.session.clone(),
            id: id.into(),
            emit: self.emit.clone(),
            active: Arc::new(Mutex::new(true)),
        };
        match name {
            "terminal:open" | "terminal:resize" => {
                let dim = |k: &str| {
                    p[k].as_u64()
                        .filter(|n| *n > 0)
                        .and_then(|n| u16::try_from(n).ok())
                };
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
                if let Some(h) = self.terminals.get(id).filter(|h| h.running()) {
                    let _ = h.tx.send(Input::Resize(size, open));
                    return;
                }
                if !open {
                    return;
                }
                self.terminals.remove(id);
                if self.terminals.values().filter(|h| h.running()).count() as u64 >= self.max {
                    events.error(&format!("Too many terminals open (max {}).", self.max));
                    return;
                }
                let path = cwd.map(Path::to_owned).map(Ok).unwrap_or_else(invoked_cwd);
                match path.and_then(|path| spawn(&self.env, &path, size)) {
                    Ok((master, child, tree)) => {
                        let (tx, rx) = mpsc::unbounded_channel();
                        let (close, closing) = oneshot::channel();
                        self.terminals.insert(
                            id.into(),
                            Handle {
                                tx,
                                close: Some(close),
                                active: events.active.clone(),
                            },
                        );
                        events.send("terminal:ready", json!({}));
                        tokio::spawn(run(
                            master,
                            child,
                            events,
                            rx,
                            closing,
                            self.idle,
                            Teardown {
                                tree,
                                shutdown: self.shutdown.subscribe(),
                            },
                        ));
                    }
                    Err(_) => events.error("Failed to spawn terminal."),
                }
            }
            "terminal:write" => {
                if let Some(data) = p["data"].as_str() {
                    if let Some(h) = self.terminals.get(id) {
                        if h.running() {
                            let _ = h.tx.send(Input::Write(data.as_bytes().into()));
                        }
                    } else {
                        events.error("Terminal not found.");
                    }
                }
            }
            "terminal:close" => {
                if let Some(h) = self.terminals.get_mut(id) {
                    h.close.take();
                }
            }
            _ => {}
        }
    }
    pub fn close_all(&mut self) {
        for h in self.terminals.values_mut() {
            h.close.take();
        }
    }
}
fn retry_eintr<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match f() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}
fn ioctl(fd: i32, cmd: libc::Ioctl, arg: *const Winsize) -> io::Result<i32> {
    retry_eintr(|| {
        let result = unsafe { libc::ioctl(fd, cmd, arg) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result)
        }
    })
}
fn pty(size: Winsize) -> io::Result<(OwnedFd, OwnedFd)> {
    let flags = OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC;
    let master = posix_openpt(flags | OFlag::O_NONBLOCK)?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let raw = retry_eintr(|| {
        let fd = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGPTPEER, flags.bits()) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(fd)
        }
    })?;
    let slave = unsafe { OwnedFd::from_raw_fd(raw) };
    ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size)?;
    Ok((master.into(), slave))
}
fn spawn(env: &Env, path: &Path, size: Winsize) -> io::Result<(AsyncFd<OwnedFd>, Child, Tree)> {
    let (master, slave) = pty(size)?;
    let master = unsafe { AsyncFd::register(master) }?;
    let mut cmd = Command::new(
        env.get(std::ffi::OsStr::new("SHELL"))
            .cloned()
            .unwrap_or_else(|| "/bin/bash".into()),
    );
    cmd.env_clear()
        .envs(env)
        .current_dir(path)
        .kill_on_drop(true)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            setsid()?;
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    let tree = Tree::terminal(
        child
            .id()
            .ok_or_else(|| io::Error::other("Missing shell PID"))?,
    )?;
    Ok((master, child, tree))
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
    match retry_eintr(|| read(fd, &mut buf).map_err(io::Error::from)) {
        Ok(0) => Ok(false),
        Err(e) if e.raw_os_error() == Some(libc::EIO) => Ok(false),
        Ok(n) => {
            let text = decode(&buf[..n], pending);
            if !text.is_empty() {
                events.send("terminal:output", json!({"data":text}));
            }
            Ok(true)
        }
        Err(e) => Err(e),
    }
}
struct Teardown {
    tree: Tree,
    shutdown: watch::Receiver<bool>,
}
async fn run(
    master: AsyncFd<OwnedFd>,
    mut child: Child,
    events: Events,
    mut rx: mpsc::UnboundedReceiver<Input>,
    mut closing: oneshot::Receiver<()>,
    idle: Duration,
    teardown: Teardown,
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
                        if ioctl(master.get_ref().as_raw_fd(),libc::TIOCSWINSZ,&size).is_err(){
                            events.error("Terminal stream closed unexpectedly.");
                            break None;
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
                        break None;
                    }
                }
            }
            result = master.async_io(Interest::WRITABLE, |fd| retry_eintr(||write(fd, writes.as_slices().0).map_err(io::Error::from))), if !writes.is_empty() => {
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
    let now = Instant::now();
    let soft = now + Duration::from_secs(2);
    let tree = teardown.tree;
    let _ = tree
        .terminate(
            soft,
            soft + Duration::from_secs(1),
            Some(&teardown.shutdown),
        )
        .await;
    drop(master);
    let status = match status {
        Some(status) => status,
        None => {
            let _ = child.start_kill();
            child.wait().await
        }
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
    fn pty_fds_are_cloexec_before_spawn() {
        use nix::fcntl::{fcntl, FcntlArg};
        let size = Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (master, slave) = pty(size).unwrap();
        for fd in [master, slave] {
            assert_ne!(fcntl(&fd, FcntlArg::F_GETFD).unwrap() & libc::FD_CLOEXEC, 0);
        }
    }
    #[test]
    fn interrupted_io_retries_only_eintr() {
        let mut n = 0;
        let result = retry_eintr(|| {
            n += 1;
            if n < 3 {
                Err(io::Error::from_raw_os_error(libc::EINTR))
            } else {
                Ok(n)
            }
        })
        .unwrap();
        assert_eq!(result, 3);
        assert_eq!(
            retry_eintr::<()>(|| Err(io::Error::from_raw_os_error(libc::EAGAIN)))
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }
    #[tokio::test]
    async fn oversize_dimensions_are_ignored() {
        let (mut manager, mut rx) = manager();
        let mut p = open("big");
        p["cols"] = 65536.into();
        manager.handle("terminal:open", &p, None);
        assert!(rx.try_recv().is_err());
        assert!(manager.terminals.is_empty());
        p["cols"] = 80.into();
        manager.handle("terminal:open", &p, None);
        assert_eq!(rx.recv().await.unwrap().0, "terminal:ready");
        p["rows"] = 65536.into();
        manager.handle("terminal:resize", &p, None);
        p["data"] = "stty size; exit\n".into();
        manager.handle("terminal:write", &p, None);
        let mut out = String::new();
        while let Some((name, p)) = rx.recv().await {
            if name == "terminal:exit" {
                break;
            }
            if let Some(data) = p["data"].as_str() {
                out.push_str(data);
            }
        }
        assert!(out.contains("24 80"));
    }
    #[tokio::test]
    async fn close_and_close_all_reap_shell_and_separate_job_groups() {
        let (mut m, mut rx) = manager();
        let tmp = tempfile::tempdir().unwrap();
        for (id, all) in [("one", false), ("all", true)] {
            let mut p = open(id);
            m.handle("terminal:open", &p, Some(tmp.path()));
            p["data"] =
                format!("trap '' HUP TERM; set -m; sleep 60 & echo $$ $! > {id}; wait\n").into();
            m.handle("terminal:write", &p, None);
            let pids = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Ok(s) = tokio::fs::read_to_string(tmp.path().join(id)).await {
                        let v: Vec<i32> =
                            s.split_whitespace().map(|s| s.parse().unwrap()).collect();
                        if v.len() == 2 {
                            break v;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_ne!(
                nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pids[1])))
                    .unwrap()
                    .as_raw(),
                pids[0]
            );
            if all {
                m.close_all()
            } else {
                m.handle("terminal:close", &p, None)
            }
            let final_event = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let (name, p) = rx.recv().await.unwrap();
                    if name == "terminal:error" || name == "terminal:exit" {
                        break (name, p);
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(final_event.0, "terminal:exit");
            assert_eq!(final_event.1["signal"], "SIGKILL");
            for pid in pids {
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
                            == Err(nix::errno::Errno::ESRCH)
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
            }
            assert!(rx.try_recv().is_err());
        }
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
                    "terminal:error" => {
                        errors.push(p["message"].as_str().unwrap().to_string());
                        if p["terminalId"] == "t" {
                            break;
                        }
                    }
                    "terminal:exit" => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(4), m.shutdown.closed())
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
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
        assert!(m.terminals["t"].close.is_none());
    }
}
