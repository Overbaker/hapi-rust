use std::io;
use tokio::time::Instant;

#[cfg(target_os = "linux")]
static OWNED_WRAPPER_GROUP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
pub fn establish_wrapper_group() -> io::Result<()> {
    use nix::unistd::{getpgrp, getpid, setsid};
    let pid = getpid();
    #[cfg(target_os = "linux")]
    if OWNED_WRAPPER_GROUP.load(std::sync::atomic::Ordering::Acquire) == pid.as_raw() {
        return Ok(());
    }
    let created = getpgrp() != pid;
    if created {
        setsid()?;
    }
    if getpgrp() != pid {
        return Err(io::Error::other("Wrapper does not lead its process group"));
    }
    #[cfg(target_os = "linux")]
    if created
        || (nix::unistd::getsid(None).ok() == Some(pid) && linux::group_has_only_self(pid.as_raw()))
    {
        OWNED_WRAPPER_GROUP.store(pid.as_raw(), std::sync::atomic::Ordering::Release);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = created;
    Ok(())
}

#[cfg(not(unix))]
pub fn establish_wrapper_group() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Process groups are only supported on Unix",
    ))
}

#[cfg(target_os = "linux")]
mod linux {
    use nix::{libc, sys::signal::Signal, unistd::getpid};
    use std::{
        collections::{HashMap, HashSet},
        fs, io,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        path::Path,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::{sync::watch, time::Instant};

    const FINAL_SIGNAL_CAP: Duration = Duration::from_millis(25);
    type ScanResult = io::Result<Option<Vec<Process>>>;

    #[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
    pub(super) struct Identity {
        pub(super) pid: i32,
        pub(super) start: u64,
    }

    #[derive(Clone, Copy)]
    pub(super) struct Process {
        pub(super) id: Identity,
        pub(super) parent: i32,
        pub(super) group: i32,
        pub(super) session: i32,
        pub(super) zombie: bool,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(super) enum GroupScope {
        None,
        ProcessGroup,
        Session,
    }

    pub(super) fn wrapper_group_owned() -> bool {
        super::OWNED_WRAPPER_GROUP.load(Ordering::Acquire) == getpid().as_raw()
    }

    pub(super) fn group_has_only_self(pid: i32) -> bool {
        snapshot(Path::new("/proc")).is_ok_and(|rows| {
            rows.iter()
                .all(|p| p.id.pid == pid || (p.group != pid && p.session != pid))
        })
    }

    pub(super) fn skipped(e: &io::Error) -> bool {
        matches!(
            e.raw_os_error(),
            Some(libc::ENOENT | libc::ESRCH | libc::EACCES | libc::EPERM)
        )
    }

    pub(super) fn validate_child_pid(pid: u32) -> io::Result<i32> {
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, "Invalid target PID");
        let id = i32::try_from(pid).map_err(|_| bad())?;
        (id > 1 && id != getpid().as_raw())
            .then_some(id)
            .ok_or_else(bad)
    }

    pub(super) fn stat(path: &Path, pid: i32) -> io::Result<Process> {
        let bytes = fs::read(path.join(pid.to_string()).join("stat"))?;
        let text = String::from_utf8_lossy(&bytes);
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "Invalid process stat");
        let mut fields = text.rsplit_once(')').ok_or_else(bad)?.1.split_whitespace();
        let zombie = matches!(fields.next(), Some("Z" | "X"));
        let mut int = || fields.next().and_then(|s| s.parse().ok()).ok_or_else(bad);
        let (parent, group, session) = (int()?, int()?, int()?);
        let start = fields
            .nth(15)
            .and_then(|s| s.parse().ok())
            .ok_or_else(bad)?;
        let id = Identity { pid, start };
        Ok(Process {
            id,
            parent,
            group,
            session,
            zombie,
        })
    }

    pub(super) fn snapshot(path: &Path) -> io::Result<Vec<Process>> {
        Ok(snapshot_until(path, &AtomicBool::new(false))?.unwrap_or_default())
    }

    pub(super) fn snapshot_until(path: &Path, cancel: &AtomicBool) -> ScanResult {
        let mut rows = Vec::new();
        for entry in fs::read_dir(path)? {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) if skipped(&e) => continue,
                Err(e) => return Err(e),
            };
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            match stat(path, pid) {
                Ok(row) => rows.push(row),
                Err(e) if skipped(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Some(rows))
    }

    struct Scanner {
        req_tx: std::sync::mpsc::SyncSender<()>,
        res_rx: tokio::sync::mpsc::UnboundedReceiver<ScanResult>,
        cancel: Arc<AtomicBool>,
    }

    impl Scanner {
        fn spawn(
            mut scan: impl FnMut(&AtomicBool) -> ScanResult + Send + 'static,
        ) -> io::Result<Self> {
            let (req_tx, req_rx) = std::sync::mpsc::sync_channel::<()>(1);
            let (res_tx, res_rx) = tokio::sync::mpsc::unbounded_channel();
            let cancel = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&cancel);
            std::thread::Builder::new()
                .name("hapi-proctree".into())
                .spawn(move || {
                    while req_rx.recv().is_ok() && !flag.load(Ordering::Acquire) {
                        if res_tx.send(scan(&flag)).is_err() {
                            break;
                        }
                    }
                })?;
            Ok(Self {
                req_tx,
                res_rx,
                cancel,
            })
        }
    }

    impl Drop for Scanner {
        fn drop(&mut self) {
            self.cancel.store(true, Ordering::Release);
        }
    }

    struct Deadlines<'a> {
        soft: Instant,
        hard: Instant,
        shutdown: Option<watch::Receiver<bool>>,
        changes: Option<&'a mut super::super::Deadline>,
    }

    impl Deadlines<'_> {
        fn refresh(&mut self, now: Instant) {
            if self
                .shutdown
                .as_mut()
                .is_some_and(|rx| *rx.borrow_and_update())
            {
                self.soft = self.soft.min(now);
                self.hard = self.hard.min(now + Duration::from_millis(250));
                self.shutdown = None;
            }
            if let Some(cap) = self.changes.as_mut().and_then(|rx| *rx.borrow_and_update()) {
                self.hard = self.hard.min(cap);
                self.soft = self
                    .soft
                    .min(now + self.hard.saturating_duration_since(now) * 2 / 3);
            }
            self.soft = self.soft.min(self.hard);
        }

        async fn wait<T>(
            &mut self,
            limit: Instant,
            fut: impl std::future::Future<Output = T>,
        ) -> Option<Option<T>> {
            let shutdown = &mut self.shutdown;
            super::super::before(limit, self.changes.as_deref_mut(), async {
                tokio::select! {
                    biased;
                    value = fut => Some(value),
                    res = async {
                        match shutdown.as_mut() {
                            Some(rx) => rx.changed().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if res.is_err() { *shutdown = None; }
                        None
                    }
                }
            })
            .await
        }
    }

    #[derive(Clone, Copy)]
    pub struct Tree {
        pub(super) root: Identity,
        pub(super) scope: GroupScope,
    }

    impl Tree {
        pub fn terminal(pid: u32) -> io::Result<Self> {
            Self::new(validate_child_pid(pid)?, GroupScope::Session)
        }

        pub(super) fn new(pid: i32, scope: GroupScope) -> io::Result<Self> {
            Ok(Self {
                root: stat(Path::new("/proc"), pid)?.id,
                scope,
            })
        }

        fn in_group(&self, p: Process) -> bool {
            match self.scope {
                GroupScope::None => false,
                GroupScope::ProcessGroup => p.group == self.root.pid,
                GroupScope::Session => p.session == self.root.pid,
            }
        }

        pub(super) fn members(&self, rows: &[Process], sweep: &mut Sweep) -> Vec<Process> {
            let mut children: HashMap<i32, Vec<Process>> = HashMap::new();
            let reused = rows
                .iter()
                .any(|p| p.id.pid == self.root.pid && p.id != self.root);
            let (mut root_present, mut group_witness) = (false, false);
            for &p in rows {
                children.entry(p.parent).or_default().push(p);
                root_present |= p.id == self.root;
                group_witness |=
                    self.in_group(p) && (p.id == self.root || sweep.group_known.contains(&p.id));
            }
            let allow_group = !sweep.group_closed && !reused && group_witness;
            if reused || (!root_present && !group_witness) {
                sweep.group_closed = true;
            }
            let mut pending = Vec::new();
            for &p in rows {
                let matched = allow_group && self.in_group(p);
                if matched {
                    sweep.group_known.insert(p.id);
                }
                if p.id == self.root || sweep.known.contains(&p.id) || matched {
                    pending.push((p, false));
                }
            }
            let (mut seen, mut out) = (HashSet::new(), Vec::new());
            while let Some((p, visited)) = pending.pop() {
                if visited {
                    if p.id.pid > 1 && p.id.pid != getpid().as_raw() {
                        out.push(p);
                    }
                    continue;
                }
                if !seen.insert(p.id) {
                    continue;
                }
                pending.push((p, true));
                if let Some(kids) = children.remove(&p.id.pid) {
                    pending.extend(
                        kids.into_iter()
                            .filter(|kid| kid.id.start >= p.id.start)
                            .map(|kid| (kid, false)),
                    );
                }
            }
            out
        }

        pub async fn terminate(
            self,
            soft_end: Instant,
            hard_end: Instant,
            shutdown: Option<&watch::Receiver<bool>>,
        ) -> io::Result<bool> {
            self.terminate_by(soft_end, hard_end, shutdown, None).await
        }

        pub(super) async fn terminate_by(
            self,
            soft_end: Instant,
            hard_end: Instant,
            shutdown: Option<&watch::Receiver<bool>>,
            changes: Option<&mut super::super::Deadline>,
        ) -> io::Result<bool> {
            self.terminate_with(soft_end, hard_end, shutdown, changes, |cancel| {
                snapshot_until(Path::new("/proc"), cancel)
            })
            .await
        }

        pub(super) async fn terminate_with(
            self,
            soft_end: Instant,
            hard_end: Instant,
            shutdown: Option<&watch::Receiver<bool>>,
            changes: Option<&mut super::super::Deadline>,
            scan: impl FnMut(&AtomicBool) -> ScanResult + Send + 'static,
        ) -> io::Result<bool> {
            let mut dl = Deadlines {
                soft: soft_end.min(hard_end),
                hard: hard_end,
                shutdown: shutdown.cloned(),
                changes,
            };
            let (mut sweep, mut hard, mut scanner, mut scan) =
                (Sweep::new(self.root), false, None, Some(scan));

            loop {
                dl.refresh(Instant::now());
                let active = if Instant::now() >= dl.hard {
                    None
                } else {
                    if scanner.is_none() {
                        scanner = scan.take().and_then(|f| Scanner::spawn(f).ok());
                    }
                    scanner.as_mut().filter(|s| s.req_tx.send(()).is_ok())
                };
                let rows = match active {
                    None => None,
                    Some(active) => loop {
                        let now = Instant::now();
                        dl.refresh(now);
                        if !hard && now >= dl.soft {
                            sweep.escalate_retained(&mut hard, dl.hard);
                        }
                        if now >= dl.hard {
                            break None;
                        }
                        let wake = if hard { dl.hard } else { dl.soft.min(dl.hard) };
                        match dl.wait(wake, active.res_rx.recv()).await {
                            Some(Some(Some(Ok(Some(rows))))) => break Some(rows),
                            Some(Some(Some(Ok(None)) | None)) => break None,
                            Some(Some(Some(Err(e)))) => return Err(e),
                            Some(None) | None => {}
                        }
                    },
                };
                let Some(rows) = rows else {
                    drop(scanner.take());
                    sweep.escalate_retained(&mut hard, Instant::now() + FINAL_SIGNAL_CAP);
                    return Ok(false);
                };

                let now = Instant::now();
                dl.refresh(now);
                let final_pass = now >= dl.hard;
                if !hard && (final_pass || now >= dl.soft) {
                    sweep.sent.clear();
                    hard = true;
                }
                let members = self.members(&rows, &mut sweep);
                let sig = if hard {
                    Signal::SIGKILL
                } else {
                    Signal::SIGTERM
                };
                let cap = if final_pass {
                    Instant::now() + FINAL_SIGNAL_CAP
                } else {
                    dl.hard
                };
                if sweep.apply(members, Some(cap), |id| signal_member(id, sig)) {
                    return Ok(!sweep.failed);
                }
                if final_pass || Instant::now() >= dl.hard {
                    return Ok(false);
                }
                let next = (Instant::now() + Duration::from_millis(20))
                    .min(if hard { dl.hard } else { dl.soft })
                    .min(dl.hard);
                let _ = dl.wait(next, std::future::pending::<()>()).await;
            }
        }
    }

    pub(super) fn signal_member(id: Identity, sig: Signal) -> io::Result<()> {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, id.pid, 0 as libc::c_uint) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        if stat(Path::new("/proc"), id.pid)?.id != id {
            return Ok(());
        }
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                sig as libc::c_int,
                std::ptr::null::<libc::siginfo_t>(),
                0 as libc::c_uint,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[derive(Default)]
    pub(super) struct Sweep {
        pub(super) known: HashSet<Identity>,
        pub(super) group_known: HashSet<Identity>,
        pub(super) retained: Vec<Identity>,
        pub(super) sent: HashSet<Identity>,
        pub(super) group_closed: bool,
        pub(super) failed: bool,
    }

    impl Sweep {
        pub(super) fn new(root: Identity) -> Self {
            let mut sweep = Self::default();
            if root.pid > 1 && root.pid != getpid().as_raw() {
                sweep.retained.push(root);
            }
            sweep
        }

        fn signal_one(
            &mut self,
            id: Identity,
            signal: &mut impl FnMut(Identity) -> io::Result<()>,
        ) {
            if self.sent.insert(id) {
                if let Err(e) = signal(id) {
                    if !matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) {
                        self.failed = true;
                    }
                }
            }
        }

        pub(super) fn apply(
            &mut self,
            members: Vec<Process>,
            deadline: Option<Instant>,
            mut signal: impl FnMut(Identity) -> io::Result<()>,
        ) -> bool {
            self.retained.clear();
            let mut empty = true;
            for p in members {
                self.known.insert(p.id);
                if p.zombie {
                    continue;
                }
                empty = false;
                self.retained.push(p.id);
                if deadline.is_none_or(|cap| Instant::now() < cap) {
                    self.signal_one(p.id, &mut signal);
                }
            }
            empty
        }

        fn escalate_retained(&mut self, hard: &mut bool, cap: Instant) {
            if !*hard {
                *hard = true;
                self.sent.clear();
            }
            let ids = std::mem::take(&mut self.retained);
            for &id in &ids {
                if Instant::now() >= cap {
                    break;
                }
                self.signal_one(id, &mut |id| signal_member(id, Signal::SIGKILL));
            }
            self.retained = ids;
        }
    }

    pub fn terminate_child_tree(pid: u32) -> io::Result<bool> {
        let tree = Tree::new(validate_child_pid(pid)?, GroupScope::ProcessGroup)?;
        let mut sweep = Sweep::new(tree.root);
        let start = std::time::Instant::now();
        let (soft, hard) = (
            start + Duration::from_secs(2),
            start + Duration::from_secs(3),
        );
        let mut sig = Signal::SIGTERM;
        loop {
            let rows = snapshot(Path::new("/proc"))?;
            let now = std::time::Instant::now();
            let final_pass = now >= hard;
            if final_pass || (sig == Signal::SIGTERM && now >= soft) {
                sig = Signal::SIGKILL;
                sweep.sent.clear();
            }
            let members = tree.members(&rows, &mut sweep);
            if sweep.apply(members, None, |id| signal_member(id, sig)) {
                return Ok(!sweep.failed);
            }
            if final_pass {
                return Ok(false);
            }
            let next = if sig == Signal::SIGKILL { hard } else { soft };
            let wait = Duration::from_millis(20)
                .min(next.saturating_duration_since(std::time::Instant::now()));
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
    }

    pub(in super::super) async fn kill_agent_tree_by(
        deadline: Instant,
        changes: Option<&mut super::super::Deadline>,
    ) -> io::Result<bool> {
        let now = Instant::now();
        let scope = if wrapper_group_owned() {
            GroupScope::ProcessGroup
        } else {
            GroupScope::None
        };
        let tree = Tree::new(getpid().as_raw(), scope)?;
        let soft =
            now + Duration::from_secs(2).min(deadline.saturating_duration_since(now) * 2 / 3);
        let hard = deadline.min(soft + Duration::from_secs(1));
        tree.terminate_by(soft, hard, None, changes).await
    }
}

#[cfg(target_os = "linux")]
pub(super) use linux::kill_agent_tree_by;
#[cfg(target_os = "linux")]
pub use linux::{terminate_child_tree, Tree};

#[cfg(not(target_os = "linux"))]
mod unsupported {
    use std::io;
    use tokio::time::Instant;

    fn error() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "Process-tree control is only supported on Linux",
        )
    }

    #[derive(Clone, Copy)]
    pub struct Tree {
        _private: (),
    }

    impl Tree {
        pub fn terminal(_pid: u32) -> io::Result<Self> {
            Err(error())
        }
        pub async fn terminate(
            self,
            _soft_end: Instant,
            _hard_end: Instant,
            _shutdown: Option<&tokio::sync::watch::Receiver<bool>>,
        ) -> io::Result<bool> {
            Err(error())
        }
    }

    pub fn terminate_child_tree(_pid: u32) -> io::Result<bool> {
        Err(error())
    }
    pub(in super::super) async fn kill_agent_tree_by(
        _deadline: Instant,
        _changes: Option<&mut super::super::Deadline>,
    ) -> io::Result<bool> {
        Err(error())
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) use unsupported::kill_agent_tree_by;
#[cfg(not(target_os = "linux"))]
pub use unsupported::{terminate_child_tree, Tree};

pub async fn kill_agent_tree(deadline: Instant) -> io::Result<bool> {
    kill_agent_tree_by(deadline, None).await
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::linux::*;
    use super::*;
    use nix::{
        libc,
        sys::signal::{kill, signal, SigHandler, Signal},
        unistd::{getpgid, getpgrp, getpid, getsid, setpgid, setsid, Pid},
    };
    use proptest::prelude::*;
    use std::{
        fs,
        io::{BufRead, Read, Write},
        os::unix::process::{CommandExt, ExitStatusExt},
        path::Path,
        process::{Child, Command, Stdio},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    const fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    const fn sec(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn stat_id(pid: i32) -> Identity {
        stat(Path::new("/proc"), pid).unwrap().id
    }
    fn parse_id(line: &str) -> Identity {
        stat_id(line.trim().parse().unwrap())
    }
    fn row(pid: i32, parent: i32, group: i32, session: i32, start: u64) -> Process {
        let id = Identity { pid, start };
        Process {
            id,
            parent,
            group,
            session,
            zombie: false,
        }
    }
    fn sweep_pids(tree: Tree, rows: &[Process], s: &mut Sweep) -> Vec<i32> {
        tree.members(rows, s)
            .into_iter()
            .map(|p| p.id.pid)
            .collect()
    }
    fn wait_cancel(flag: &AtomicBool, max: Duration) -> bool {
        let until = std::time::Instant::now() + max;
        while !flag.load(Ordering::Relaxed) && std::time::Instant::now() < until {
            std::thread::sleep(ms(5));
        }
        flag.load(Ordering::Relaxed)
    }
    fn rt_single(max_blocking: Option<usize>) -> tokio::runtime::Runtime {
        let mut b = tokio::runtime::Builder::new_current_thread();
        b.enable_all();
        if let Some(n) = max_blocking {
            b.max_blocking_threads(n);
        }
        b.build().unwrap()
    }
    fn self_test_cmd(name: &str) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "--nocapture", name]);
        cmd
    }
    fn spawn_sh(script: &str, pgid: Option<i32>) -> (Child, String) {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(pg) = pgid {
            cmd.process_group(pg);
        }
        let mut child = cmd.spawn().unwrap();
        let mut line = String::new();
        io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        (child, line)
    }
    fn spawn_trapping_tree() -> (Child, Tree) {
        let child = spawn_sh("trap '' TERM; echo ready; exec sleep 60", None).0;
        let tree = Tree::terminal(child.id()).unwrap();
        (child, tree)
    }
    fn assert_sigkilled(child: &mut Child, start: Instant) {
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert!(start.elapsed() < sec(1));
    }
    fn spawn_sentinel(pgid: i32) -> (Child, Identity) {
        let mut cmd = Command::new("sleep");
        cmd.arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let c = cmd.process_group(pgid).spawn().unwrap();
        let id = stat_id(c.id() as i32);
        (c, id)
    }
    fn assert_sentinel_alive(sentinel: &mut Child, id: Identity) {
        assert!(sentinel.try_wait().unwrap().is_none());
        let after = stat(Path::new("/proc"), id.pid).unwrap();
        assert!(after.id == id && !after.zombie);
    }
    async fn saturate_pool(
        timeout: Duration,
    ) -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
        let (release, block) = std::sync::mpsc::channel();
        let (started, running) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            block.recv_timeout(timeout).ok();
        });
        running.await.unwrap();
        (release, blocker)
    }
    fn gone(id: Identity, zombie_ok: bool) -> bool {
        stat(Path::new("/proc"), id.pid).map_or(true, |p| p.id != id || (zombie_ok && p.zombie))
    }
    fn drain_adopted() {
        while matches!(
            nix::sys::wait::waitpid(Pid::from_raw(-1), Some(nix::sys::wait::WaitPidFlag::WNOHANG)),
            Ok(s) if s != nix::sys::wait::WaitStatus::StillAlive
        ) {}
    }
    fn reap_until_gone(ids: &[Identity], timeout: Duration) {
        let until = std::time::Instant::now() + timeout;
        loop {
            drain_adopted();
            if ids.iter().all(|&id| gone(id, false)) || std::time::Instant::now() >= until {
                break;
            }
            std::thread::sleep(ms(10));
        }
    }
    fn run_as_subreaper(name: &str, env_key: &str, body: impl FnOnce()) {
        if std::env::var_os(env_key).is_none() {
            let out = self_test_cmd(name).env(env_key, "1").output().unwrap();
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{name}:\n{err}");
            return;
        }
        nix::sys::prctl::set_child_subreaper(true).unwrap();
        body();
    }
    fn parse_pids(raw: &str) -> Vec<Identity> {
        raw.lines()
            .filter_map(|l| {
                let (p, s) = l.strip_prefix("PID:")?.split_once(':')?;
                Some(Identity {
                    pid: p.parse().ok()?,
                    start: s.parse().ok()?,
                })
            })
            .collect()
    }
    fn wait_and_reap_wrapper(wrapper: Child, expected: usize) {
        let out = wrapper.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        let recorded = parse_pids(&String::from_utf8_lossy(&out.stdout));
        assert_eq!(recorded.len(), expected);
        reap_until_gone(&recorded, sec(1));
        assert!(recorded.into_iter().all(|id| gone(id, false)));
    }

    #[test]
    fn per_process_failures_do_not_stop_siblings() {
        for errno in [libc::ENOENT, libc::ESRCH, libc::EPERM, libc::EACCES] {
            let err = io::Error::from_raw_os_error(errno);
            assert!(skipped(&err));
            let (mut sweep, mut seen) = (Sweep::default(), Vec::new());
            let members = (21..24).map(|pid| row(pid, 2, 4, 4, 3)).collect();
            sweep.apply(members, None, |id| {
                seen.push(id.pid);
                (id.pid != 21)
                    .then_some(())
                    .ok_or_else(|| io::Error::from_raw_os_error(errno))
            });
            assert_eq!(seen, [21, 22, 23]);
            assert_eq!(sweep.failed, matches!(errno, libc::EACCES | libc::EPERM));
        }
    }

    #[test]
    fn snapshot_without_task_children_follows_group_and_ancestry() {
        let dir = tempfile::tempdir().unwrap();
        let zeros = ["0"; 15].join(" ");
        for (pid, parent, g) in [(101, 1, 101), (102, 101, 102), (103, 1, 101), (104, 1, 104)] {
            let p = dir.path().join(pid.to_string());
            fs::create_dir(&p).unwrap();
            fs::write(
                p.join("stat"),
                format!("{pid} (a) b) S {parent} {g} {g} {zeros} 123"),
            )
            .unwrap();
        }
        fs::create_dir(dir.path().join("105")).unwrap();
        let tree = Tree {
            root: Identity {
                pid: 101,
                start: 123,
            },
            scope: GroupScope::ProcessGroup,
        };
        let mut pids = sweep_pids(tree, &snapshot(dir.path()).unwrap(), &mut Sweep::default());
        pids.sort();
        assert_eq!(pids, [101, 102, 103]);
    }

    #[test]
    fn members_emits_children_before_parents_regardless_of_snapshot_order() {
        let (a, b, c) = (
            row(101, 1, 101, 101, 10),
            row(102, 101, 101, 101, 11),
            row(103, 102, 101, 101, 12),
        );
        let tree = Tree {
            root: a.id,
            scope: GroupScope::ProcessGroup,
        };
        for rows in [
            [a, b, c],
            [a, c, b],
            [b, a, c],
            [b, c, a],
            [c, a, b],
            [c, b, a],
        ] {
            assert_eq!(
                sweep_pids(tree, &rows, &mut Sweep::default()),
                [103, 102, 101]
            );
        }
    }

    #[test]
    fn recycled_leader_cannot_claim_new_group_or_session() {
        let root = Identity {
            pid: 101,
            start: 123,
        };
        let rows = [
            row(101, 1, 101, 101, 999),
            row(102, 101, 101, 101, 1000),
            row(103, 1, 101, 101, 110),
        ];
        for scope in [GroupScope::Session, GroupScope::ProcessGroup] {
            let tree = Tree { root, scope };
            let mut s = Sweep::default();
            assert!(sweep_pids(tree, &rows, &mut s).is_empty() && s.group_closed);
            s = Sweep::default();
            s.known.insert(rows[2].id);
            s.group_known.insert(rows[2].id);
            assert!(sweep_pids(tree, &rows, &mut s) == [103] && s.group_closed);
            assert_eq!(
                sweep_pids(tree, &[row(104, 1, 101, 101, 1001), rows[2]], &mut s),
                [103]
            );
        }
    }

    #[test]
    fn recycled_session_survivor_without_leader_is_not_claimed() {
        let root = Identity {
            pid: 101,
            start: 100,
        };
        let (survivor, escaped, child) = (
            row(205, 1, 101, 101, 510),
            row(300, 1, 300, 300, 105),
            row(301, 300, 300, 300, 106),
        );
        let (orig_root, orig_member) = (row(101, 1, 101, 101, 100), row(204, 1, 101, 101, 104));
        for scope in [GroupScope::Session, GroupScope::ProcessGroup] {
            let tree = Tree { root, scope };
            let mut s = Sweep::default();
            assert!(sweep_pids(tree, &[survivor], &mut s).is_empty() && s.group_closed);
            s = Sweep::default();
            s.known.insert(escaped.id);
            assert!(
                sweep_pids(tree, &[survivor, escaped, child], &mut s) == [301, 300]
                    && s.group_closed
            );
            let mut s = Sweep::new(root);
            let m = tree.members(&[orig_root, orig_member], &mut s);
            assert!(!s.apply(m, None, |_| Ok(())) && !s.group_closed);
            let mut second = sweep_pids(tree, &[orig_member, survivor], &mut s);
            second.sort();
            assert_eq!(second, [204, 205]);
        }
    }

    #[test]
    fn rejects_invalid_and_self_child_pids() {
        let bad = io::ErrorKind::InvalidInput;
        for pid in [0, 1, u32::MAX, std::process::id()] {
            assert_eq!(terminate_child_tree(pid).unwrap_err().kind(), bad);
            assert_eq!(Tree::terminal(pid).err().unwrap().kind(), bad);
        }
    }

    #[test]
    fn mismatched_start_time_never_signals_live_pid() {
        let mut child = Command::new("sh")
            .args(["-c", "read line; exit 17"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut id = stat_id(child.id() as i32);
        id.start += 1;
        signal_member(id, Signal::SIGKILL).unwrap();
        drop(child.stdin.take());
        assert_eq!(child.wait().unwrap().code(), Some(17));
    }

    #[test]
    fn tighter_deadline_delivers_sigkill_when_blocking_pool_is_busy() {
        rt_single(Some(1)).block_on(async {
            let (mut child, tree) = spawn_trapping_tree();
            let (release, blocker) = saturate_pool(sec(2)).await;
            let start = Instant::now();
            let original = start + sec(3);
            let (tx, mut changes) = tokio::sync::watch::channel(Some(original));
            tokio::spawn(async move {
                tokio::time::sleep(ms(20)).await;
                tx.send_replace(Some(Instant::now()));
            });
            tree.terminate_by(original, original, None, Some(&mut changes))
                .await
                .unwrap();
            assert_sigkilled(&mut child, start);
            release.send(()).unwrap();
            blocker.await.unwrap();
        });
    }

    #[test]
    fn shutdown_interrupts_queued_scan_and_tightens_deadlines() {
        rt_single(Some(1)).block_on(async {
            let (mut child, tree) = spawn_trapping_tree();
            let (release, blocker) = saturate_pool(sec(3)).await;
            let (tx, rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                tokio::time::sleep(ms(30)).await;
                tx.send_replace(true);
            });
            let start = Instant::now();
            assert!(tree
                .terminate(start + sec(3), start + sec(5), Some(&rx))
                .await
                .unwrap());
            assert_sigkilled(&mut child, start);
            release.send(()).unwrap();
            blocker.await.unwrap();

            let (mut child, tree) = spawn_trapping_tree();
            let (started, running) = tokio::sync::oneshot::channel();
            let mut started = Some(started);
            let (tx, rx) = tokio::sync::watch::channel(false);
            let trigger = tokio::spawn(async move {
                running.await.unwrap();
                tx.send_replace(true);
            });
            let start = Instant::now();
            let done = tree
                .terminate_with(start + sec(3), start + sec(5), Some(&rx), None, move |c| {
                    let _ = started.take().map(|s| s.send(()));
                    wait_cancel(c, sec(4));
                    Ok(None)
                })
                .await
                .unwrap();
            trigger.await.unwrap();
            assert!(!done);
            assert_sigkilled(&mut child, start);
        });
    }

    #[test]
    fn hard_deadline_skips_async_runtime_scan_and_returns_incomplete() {
        rt_single(None).block_on(async {
            let (mut child, tree) = spawn_trapping_tree();
            let st = Arc::new((
                AtomicUsize::new(0),
                AtomicBool::new(false),
                AtomicUsize::new(0),
            ));
            let (s1, s2) = (Arc::clone(&st), Arc::clone(&st));
            let ticker = tokio::spawn(async move {
                for _ in 0..10 {
                    tokio::time::sleep(ms(10)).await;
                    s2.2.fetch_add(1, Ordering::Relaxed);
                }
            });
            let start = Instant::now();
            let limit = start + ms(120);
            let done = tree
                .terminate_with(limit, limit, None, None, move |cancel| {
                    s1.0.fetch_add(1, Ordering::Relaxed);
                    if wait_cancel(cancel, sec(2)) {
                        s1.1.store(true, Ordering::Relaxed);
                        return Ok(None);
                    }
                    Ok(Some(Vec::new()))
                })
                .await
                .unwrap();
            assert!(!done && start.elapsed() < ms(500));
            assert_eq!(st.0.load(Ordering::Relaxed), 1);
            assert!(st.2.load(Ordering::Relaxed) >= 2);
            assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
            assert!(wait_cancel(&st.1, ms(300)));
            ticker.abort();
        });
    }

    #[tokio::test]
    async fn soft_end_past_hard_end_still_obeys_hard_deadline() {
        let (mut child, tree) = spawn_trapping_tree();
        let start = Instant::now();
        let _ = tree
            .terminate(start + sec(5), start + ms(150), None)
            .await
            .unwrap();
        assert_sigkilled(&mut child, start);
    }

    #[test]
    fn terminate_child_tree_escalates_and_leaves_sentinel_alive() {
        run_as_subreaper(
            "lifecycle::proctree::tests::terminate_child_tree_escalates_and_leaves_sentinel_alive",
            "HAPI_PROCTREE_TEST_CHILD_TREE_REAPER",
            || {
                let (mut sentinel, sentinel_id) = spawn_sentinel(0);
                let (mut child, line) = spawn_sh(
                    "trap '' TERM; sh -c 'trap \"\" TERM; echo $$; exec sleep 60' & wait",
                    Some(0),
                );
                let grandchild_id = parse_id(&line);
                let start = std::time::Instant::now();
                assert!(terminate_child_tree(child.id()).unwrap());
                assert!(start.elapsed() >= ms(1900) && start.elapsed() < sec(3));
                let _ = child.wait().unwrap();
                reap_until_gone(&[grandchild_id], sec(1));
                assert!(gone(grandchild_id, false));
                assert_sentinel_alive(&mut sentinel, sentinel_id);
                let _ = (sentinel.kill(), sentinel.wait());
            },
        );
    }

    #[test]
    fn establish_wrapper_group_creates_session_when_not_group_leader() {
        const CHILD: &str = "HAPI_PROCTREE_TEST_SETSID";
        if std::env::var_os(CHILD).is_some() {
            assert_ne!(getpgrp(), getpid());
            establish_wrapper_group().unwrap();
            assert!(
                wrapper_group_owned() && getpgrp() == getpid() && getsid(None).unwrap() == getpid()
            );
            establish_wrapper_group().unwrap();
            assert!(wrapper_group_owned() && getpgrp() == getpid());
            return;
        }
        let name = "lifecycle::proctree::tests::establish_wrapper_group_creates_session_when_not_group_leader";
        assert!(self_test_cmd(name)
            .env(CHILD, "1")
            .output()
            .unwrap()
            .status
            .success());
    }

    fn wrapper_cmd(name: &str, child: &str, reaper: &str, stdin: Stdio) -> Command {
        let mut cmd = self_test_cmd(name);
        cmd.env(child, "1")
            .env_remove(reaper)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    fn set_setsid(cmd: &mut Command) {
        unsafe {
            cmd.pre_exec(|| setsid().map(|_| ()).map_err(io::Error::from));
        }
    }

    #[test]
    fn process_group_empty_after_exit() {
        const NAME: &str = "lifecycle::proctree::tests::process_group_empty_after_exit";
        const CHILD: &str = "HAPI_PROCTREE_TEST_CHILD";
        const REAPER: &str = "HAPI_PROCTREE_TEST_GROUP_REAPER";
        if std::env::var_os(CHILD).is_some() {
            let prev = nix::sys::prctl::get_child_subreaper().unwrap();
            establish_wrapper_group().unwrap();
            assert!(
                wrapper_group_owned() && nix::sys::prctl::get_child_subreaper().unwrap() == prev
            );
            let (mut child, ready) = spawn_sh(
                "setsid sh -c 'trap \"\" TERM; echo $$; exec sleep 60' & wait",
                None,
            );
            let escaped = parse_id(&ready);
            let epid = Pid::from_raw(escaped.pid);
            assert_eq!(getsid(Some(epid)).unwrap(), epid);
            let (mut sh, orphan_line) =
                spawn_sh("sleep 60 </dev/null >/dev/null 2>&1 & echo $!", None);
            assert!(sh.wait().unwrap().success());
            let orphan = parse_id(&orphan_line);
            assert_eq!(getpgid(Some(Pid::from_raw(orphan.pid))).unwrap(), getpid());
            assert!(rt_single(None)
                .block_on(kill_agent_tree(Instant::now() + ms(400)))
                .unwrap());
            child.wait().unwrap();
            assert!(gone(escaped, true) && gone(orphan, true));
            println!(
                "\nPID:{}:{}\nPID:{}:{}",
                escaped.pid, escaped.start, orphan.pid, orphan.start
            );
            return;
        }
        run_as_subreaper(NAME, REAPER, || {
            for launcher_setsid in [false, true] {
                let mut cmd = wrapper_cmd(NAME, CHILD, REAPER, Stdio::null());
                if launcher_setsid {
                    set_setsid(&mut cmd);
                }
                let wrapper = cmd.spawn().unwrap();
                let pid = wrapper.id() as i32;
                wait_and_reap_wrapper(wrapper, 2);
                assert_eq!(
                    kill(Pid::from_raw(-pid), None),
                    Err(nix::errno::Errno::ESRCH)
                );
            }
        });
    }

    #[test]
    fn inherited_group_with_sibling_uses_ancestry_only() {
        const NAME: &str =
            "lifecycle::proctree::tests::inherited_group_with_sibling_uses_ancestry_only";
        const CHILD: &str = "HAPI_PROCTREE_TEST_PIPELINE_CHILD";
        const REAPER: &str = "HAPI_PROCTREE_TEST_PIPELINE_REAPER";
        if std::env::var_os(CHILD).is_some() {
            let mut gate = String::new();
            io::stdin().read_to_string(&mut gate).unwrap();
            assert!(getpgrp() == getpid() && getsid(None).unwrap() != getpid());
            establish_wrapper_group().unwrap();
            assert!(!wrapper_group_owned());
            let (mut child, line) = spawn_sh(
                "trap '' TERM; sh -c 'trap \"\" TERM; echo $$; exec sleep 60' & wait",
                None,
            );
            let grandchild = parse_id(&line);
            assert!(rt_single(None)
                .block_on(kill_agent_tree(Instant::now() + ms(600)))
                .unwrap());
            child.wait().unwrap();
            assert!(gone(grandchild, true));
            println!("\nPID:{}:{}", grandchild.pid, grandchild.start);
            return;
        }
        run_as_subreaper(NAME, REAPER, || {
            let mut wrapper = wrapper_cmd(NAME, CHILD, REAPER, Stdio::piped())
                .process_group(0)
                .spawn()
                .unwrap();
            let pgid = wrapper.id() as i32;
            let (mut sibling, sibling_id) = spawn_sentinel(pgid);
            assert_eq!(
                getpgid(Some(Pid::from_raw(sibling_id.pid)))
                    .unwrap()
                    .as_raw(),
                pgid
            );
            drop(wrapper.stdin.take());
            wait_and_reap_wrapper(wrapper, 1);
            assert_sentinel_alive(&mut sibling, sibling_id);
            let _ = (sibling.kill(), sibling.wait());
        });
    }

    const PBT_NAME: &str =
        "lifecycle::proctree::tests::generated_process_trees_are_killed_before_deadline_without_touching_sentinel";
    const PBT_SPEC_ENV: &str = "HAPI_PROCTREE_PBT_SPEC";
    const PBT_REAPER_ENV: &str = "HAPI_PROCTREE_PBT_REAPER";

    #[derive(Clone, Debug)]
    struct TreeNode(char, Vec<TreeNode>);

    impl TreeNode {
        fn encode(&self) -> String {
            if self.1.is_empty() {
                return self.0.to_string();
            }
            let kids: Vec<_> = self.1.iter().map(Self::encode).collect();
            format!("{}({})", self.0, kids.join(","))
        }

        fn decode(s: &str) -> Self {
            fn parse(b: &[u8], i: &mut usize) -> TreeNode {
                let (mode, mut kids) = (b[*i] as char, Vec::new());
                *i += 1;
                if b.get(*i) == Some(&b'(') {
                    loop {
                        *i += 1;
                        kids.push(parse(b, i));
                        if b.get(*i) == Some(&b')') {
                            *i += 1;
                            break;
                        }
                        assert_eq!(b.get(*i), Some(&b','));
                    }
                }
                TreeNode(mode, kids)
            }
            let mut i = 0;
            let node = parse(s.as_bytes(), &mut i);
            assert_eq!(i, s.len());
            node
        }

        fn count(&self) -> usize {
            1 + self.1.iter().map(Self::count).sum::<usize>()
        }
        fn depth(&self) -> usize {
            1 + self.1.iter().map(Self::depth).max().unwrap_or(0)
        }
    }

    fn run_pbt_node(spec: &str) -> ! {
        let node = TreeNode::decode(spec);
        let h = if matches!(node.0, 'i' | 'S' | 'G') {
            SigHandler::SigIgn
        } else {
            SigHandler::SigDfl
        };
        unsafe {
            let _ = signal(Signal::SIGTERM, h);
        }
        if matches!(node.0, 's' | 'S') {
            let _ = setsid();
        } else if matches!(node.0, 'g' | 'G') {
            let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
        }
        let id = stat_id(getpid().as_raw());
        let kids: Vec<_> = node
            .1
            .iter()
            .map(|c| {
                self_test_cmd(PBT_NAME)
                    .env(PBT_SPEC_ENV, c.encode())
                    .stdin(Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let line = format!("\nPID:{}:{}\n", id.pid, id.start);
        let mut out = io::stdout();
        out.write_all(line.as_bytes()).unwrap();
        out.flush().unwrap();
        unsafe {
            libc::close(1);
        }
        if node.0 == 'o' {
            std::process::exit(0);
        }
        for mut c in kids {
            let _ = c.wait();
        }
        loop {
            std::thread::sleep(sec(60));
        }
    }

    struct Builder<'a>(usize, bool, usize, usize, &'a [u8]);
    impl Builder<'_> {
        fn next(&mut self) -> u8 {
            let b = self.4[self.3 % self.4.len()];
            self.3 += 1;
            b
        }

        fn build(&mut self, rem: usize, root: bool, esc: bool, orph: bool, must: bool) -> TreeNode {
            let has_kids = rem > 1 && (self.2 > 0 || must);
            let b = self.next();
            let mode = match (root, b % 6) {
                (true, _) => {
                    if b.is_multiple_of(2) {
                        'n'
                    } else {
                        'i'
                    }
                }
                (false, 1) => 'i',
                (false, 2) if !orph => 's',
                (false, 3) if !orph => 'S',
                (false, 4) if self.1 && !esc => 'g',
                (false, 5) if has_kids && !esc => 'o',
                (false, 2..=5) if self.1 && !esc => 'G',
                _ => 'n',
            };
            let (ce, co, mut kids) = (
                esc || matches!(mode, 's' | 'S'),
                orph || mode == 'o',
                Vec::new(),
            );
            if has_kids {
                let want = if must && root {
                    self.0
                } else {
                    1 + self.next() as usize % self.0
                };
                let count = want.min(if must { self.2.max(1) } else { self.2 });
                self.2 = self.2.saturating_sub(count);
                for idx in 0..count {
                    let cm = must && idx == 0;
                    let d = if cm {
                        rem - 1
                    } else {
                        1 + self.next() as usize % (rem - 1)
                    };
                    kids.push(self.build(d, false, ce, co, cm));
                }
            }
            TreeNode(mode, kids)
        }
    }

    struct CleanupGuard(Vec<Identity>, Option<Child>, Option<Child>);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            for &id in &self.0 {
                if !gone(id, false) {
                    let _ = signal_member(id, Signal::SIGKILL);
                }
            }
            for mut c in [self.1.take(), self.2.take()].into_iter().flatten() {
                let _ = (c.kill(), c.wait());
            }
            drain_adopted();
        }
    }

    #[test]
    fn generated_process_trees_are_killed_before_deadline_without_touching_sentinel() {
        if let Ok(spec) = std::env::var(PBT_SPEC_ENV) {
            run_pbt_node(&spec);
        }
        run_as_subreaper(PBT_NAME, PBT_REAPER_ENV, || {
            let rt = rt_single(None);
            let strat = (
                1usize..=4,
                1usize..=3,
                any::<bool>(),
                proptest::collection::vec(any::<u8>(), 32..=64),
            );
            let cfg = proptest::test_runner::Config {
                cases: 20,
                failure_persistence: None,
                ..Default::default()
            };
            proptest::test_runner::TestRunner::new(cfg)
                .run(&strat, |(depth, fan, terminal, choices)| {
                    let spec = Builder(fan, terminal, 14, 0, &choices)
                        .build(depth, true, false, false, true);
                    prop_assert_eq!(spec.depth(), depth);
                    let (sentinel, sentinel_id) = spawn_sentinel(0);
                    let mut guard = CleanupGuard(Vec::new(), None, Some(sentinel));
                    let mut cmd = self_test_cmd(PBT_NAME);
                    cmd.env(PBT_SPEC_ENV, spec.encode())
                        .env_remove(PBT_REAPER_ENV);
                    cmd.stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null());
                    if terminal {
                        set_setsid(&mut cmd);
                    } else {
                        cmd.process_group(0);
                    }
                    let mut root = cmd.spawn().unwrap();
                    let (root_pid, mut raw) = (root.id(), String::new());
                    root.stdout
                        .take()
                        .unwrap()
                        .read_to_string(&mut raw)
                        .unwrap();
                    guard.1 = Some(root);
                    let recorded = parse_pids(&raw);
                    guard.0 = recorded.clone();
                    prop_assert_eq!(recorded.len(), spec.count());

                    let tree = if terminal {
                        Tree::terminal(root_pid)
                    } else {
                        Tree::new(root_pid as i32, GroupScope::ProcessGroup)
                    }
                    .unwrap();
                    let start = Instant::now();
                    let hard_end = start + sec(3);
                    let killed = rt
                        .block_on(tree.terminate(start + ms(150), hard_end, None))
                        .unwrap();
                    prop_assert!(killed && Instant::now() <= hard_end);
                    prop_assert!(recorded.iter().all(|&id| gone(id, true)));
                    let _ = guard.1.take().unwrap().wait().unwrap();
                    reap_until_gone(&recorded, sec(1));
                    prop_assert!(recorded.iter().all(|&id| gone(id, false)));
                    assert_sentinel_alive(guard.2.as_mut().unwrap(), sentinel_id);
                    Ok(())
                })
                .unwrap();
        });
    }
}
