use std::io;
use tokio::time::Instant;

#[cfg(unix)]
pub fn establish_wrapper_group() -> io::Result<()> {
    use nix::unistd::{getpgrp, getpid, setsid};
    if getpgrp() != getpid() {
        setsid()?;
    }
    if getpgrp() != getpid() {
        return Err(io::Error::other("Wrapper does not lead its process group"));
    }
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
        time::Duration,
    };
    use tokio::time::Instant;

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

    pub(super) fn skipped(e: &io::Error) -> bool {
        matches!(
            e.raw_os_error(),
            Some(libc::ENOENT | libc::ESRCH | libc::EACCES | libc::EPERM)
        )
    }

    pub(super) fn validate_child_pid(pid: u32) -> io::Result<i32> {
        let id = i32::try_from(pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid target PID"))?;
        if id <= 1 || id == getpid().as_raw() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid target PID",
            ));
        }
        Ok(id)
    }

    pub(super) fn stat(path: &Path, pid: i32) -> io::Result<Process> {
        let bytes = fs::read(path.join(pid.to_string()).join("stat"))?;
        let text = String::from_utf8_lossy(&bytes);
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "Invalid process stat");
        let mut fields = text
            .rsplit_once(')')
            .ok_or_else(invalid)?
            .1
            .split_whitespace();
        let zombie = matches!(fields.next(), Some("Z" | "X"));
        let mut integer = || {
            fields
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(invalid)
        };
        let parent = integer()?;
        let group = integer()?;
        let session = integer()?;
        let start = fields
            .nth(15)
            .and_then(|s| s.parse().ok())
            .ok_or_else(invalid)?;
        Ok(Process {
            id: Identity { pid, start },
            parent,
            group,
            session,
            zombie,
        })
    }

    pub(super) fn snapshot(path: &Path) -> io::Result<Vec<Process>> {
        let mut rows = Vec::new();
        for entry in fs::read_dir(path)? {
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
        Ok(rows)
    }

    #[derive(Clone, Copy)]
    pub struct Tree {
        pub(super) root: Identity,
        pub(super) terminal: bool,
    }

    impl Tree {
        pub fn terminal(pid: u32) -> io::Result<Self> {
            Self::new(validate_child_pid(pid)?, true)
        }

        pub(super) fn new(pid: i32, terminal: bool) -> io::Result<Self> {
            Ok(Self {
                root: stat(Path::new("/proc"), pid)?.id,
                terminal,
            })
        }

        pub(super) fn members(
            &self,
            rows: &[Process],
            known: &HashSet<Identity>,
            group_closed: &mut bool,
        ) -> Vec<Process> {
            let mut children: HashMap<i32, Vec<Process>> = HashMap::new();
            let mut pending = Vec::new();
            let reused = rows
                .iter()
                .any(|p| p.id.pid == self.root.pid && p.id != self.root);
            let mut root_present = false;
            let mut any_in_group = false;
            for &p in rows {
                children.entry(p.parent).or_default().push(p);
                if p.id == self.root {
                    root_present = true;
                }
                let in_group = if self.terminal {
                    p.session == self.root.pid
                } else {
                    p.group == self.root.pid
                };
                if in_group {
                    any_in_group = true;
                }
                if p.id == self.root
                    || known.contains(&p.id)
                    || (!*group_closed && !reused && in_group)
                {
                    pending.push((p, false));
                }
            }
            if reused || (!root_present && !any_in_group) {
                *group_closed = true;
            }
            let mut seen = HashSet::new();
            let mut out = Vec::new();
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
            shutdown: Option<&tokio::sync::watch::Receiver<bool>>,
        ) -> io::Result<bool> {
            self.terminate_by(soft_end, hard_end, shutdown, None).await
        }

        pub(super) async fn terminate_by(
            self,
            soft_end: Instant,
            mut hard_end: Instant,
            shutdown: Option<&tokio::sync::watch::Receiver<bool>>,
            mut changes: Option<&mut super::super::Deadline>,
        ) -> io::Result<bool> {
            let mut soft_end = soft_end.min(hard_end);
            let mut sweep = Sweep::default();
            let mut hard = false;
            loop {
                let now = Instant::now();
                if shutdown.is_some_and(|s| *s.borrow()) {
                    soft_end = soft_end.min(now);
                    hard_end = hard_end.min(now + Duration::from_millis(250));
                }
                if let Some(cap) = changes.as_ref().and_then(|rx| *rx.borrow()) {
                    hard_end = hard_end.min(cap);
                    soft_end = soft_end.min(now + hard_end.saturating_duration_since(now) * 2 / 3);
                }
                let scan =
                    async { tokio::task::spawn_blocking(|| snapshot(Path::new("/proc"))).await? };
                let (rows, final_pass) = if now >= hard_end {
                    (snapshot(Path::new("/proc"))?, true)
                } else {
                    match super::super::before(hard_end, changes.as_deref_mut(), scan).await {
                        Some(result) => (result?, false),
                        None => (snapshot(Path::new("/proc"))?, true),
                    }
                };
                if final_pass || (!hard && Instant::now() >= soft_end) {
                    sweep.sent.clear();
                    hard = true;
                }
                let members = self.members(&rows, &sweep.known, &mut sweep.group_closed);
                let signal = if hard {
                    Signal::SIGKILL
                } else {
                    Signal::SIGTERM
                };
                if sweep.apply(members, |p| signal_member(p, signal)) {
                    return Ok(!sweep.failed);
                }
                if final_pass {
                    return Ok(false);
                }
                let next = (Instant::now() + Duration::from_millis(20))
                    .min(if hard { hard_end } else { soft_end })
                    .min(hard_end);
                super::super::before(next, changes.as_deref_mut(), std::future::pending::<()>())
                    .await;
            }
        }
    }

    pub(super) fn signal_member(p: Process, sig: Signal) -> io::Result<()> {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, p.id.pid, 0 as libc::c_uint) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        if stat(Path::new("/proc"), p.id.pid)?.id != p.id {
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
        pub(super) sent: HashSet<Identity>,
        pub(super) group_closed: bool,
        pub(super) failed: bool,
    }

    impl Sweep {
        pub(super) fn apply(
            &mut self,
            members: Vec<Process>,
            mut signal: impl FnMut(Process) -> io::Result<()>,
        ) -> bool {
            let mut empty = true;
            for p in members {
                self.known.insert(p.id);
                if p.zombie {
                    continue;
                }
                empty = false;
                if self.sent.insert(p.id) {
                    if let Err(e) = signal(p) {
                        if !matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) {
                            self.failed = true;
                        }
                    }
                }
            }
            empty
        }
    }

    pub fn terminate_child_tree(pid: u32) -> io::Result<bool> {
        let pid = validate_child_pid(pid)?;
        let tree = Tree::new(pid, false)?;
        let mut sweep = Sweep::default();
        let start = std::time::Instant::now();
        let soft_end = start + Duration::from_secs(2);
        let hard_end = start + Duration::from_secs(3);
        let mut signal = Signal::SIGTERM;
        loop {
            let rows = snapshot(Path::new("/proc"))?;
            let now = std::time::Instant::now();
            let final_pass = now >= hard_end;
            if final_pass || (signal == Signal::SIGTERM && now >= soft_end) {
                signal = Signal::SIGKILL;
                sweep.sent.clear();
            }
            let members = tree.members(&rows, &sweep.known, &mut sweep.group_closed);
            if sweep.apply(members, |p| signal_member(p, signal)) {
                return Ok(!sweep.failed);
            }
            if final_pass {
                return Ok(false);
            }
            let now = std::time::Instant::now();
            let next_end = if signal == Signal::SIGKILL {
                hard_end
            } else {
                soft_end
            };
            let wait = Duration::from_millis(20).min(next_end.saturating_duration_since(now));
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
        let tree = Tree::new(getpid().as_raw(), false)?;
        let soft =
            now + Duration::from_secs(2).min(deadline.saturating_duration_since(now) * 2 / 3);
        tree.terminate_by(
            soft,
            deadline.min(soft + Duration::from_secs(1)),
            None,
            changes,
        )
        .await
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
        sys::signal::{kill, Signal},
        unistd::{getpid, Pid},
    };
    use proptest::prelude::*;
    use std::{
        collections::HashSet,
        fs,
        io::{Read, Write},
        os::unix::process::CommandExt,
        path::Path,
        process::{Command, Stdio},
        time::Duration,
    };

    #[test]
    fn per_process_failures_do_not_stop_siblings() {
        for errno in [libc::ENOENT, libc::ESRCH, libc::EPERM, libc::EACCES] {
            let error = io::Error::from_raw_os_error(errno);
            assert!(skipped(&error));
            let mut sweep = Sweep::default();
            let mut seen = Vec::new();
            let members = (21..24)
                .map(|pid| Process {
                    id: Identity { pid, start: 3 },
                    parent: 2,
                    group: 4,
                    session: 4,
                    zombie: false,
                })
                .collect();
            sweep.apply(members, |p| {
                seen.push(p.id.pid);
                if p.id.pid == 21 {
                    Err(io::Error::from_raw_os_error(errno))
                } else {
                    Ok(())
                }
            });
            assert_eq!(seen, [21, 22, 23]);
            assert_eq!(sweep.failed, matches!(errno, libc::EACCES | libc::EPERM));
        }
    }

    #[test]
    fn snapshot_without_task_children_follows_group_and_ancestry() {
        let dir = tempfile::tempdir().unwrap();
        for (pid, parent, group) in [(101, 1, 101), (102, 101, 102), (103, 1, 101), (104, 1, 104)] {
            let p = dir.path().join(pid.to_string());
            fs::create_dir(&p).unwrap();
            fs::write(
                p.join("stat"),
                format!(
                    "{pid} (a) b) S {parent} {group} {group} {} 123",
                    ["0"; 15].join(" ")
                ),
            )
            .unwrap();
        }
        fs::create_dir(dir.path().join("105")).unwrap();
        let tree = Tree {
            root: Identity {
                pid: 101,
                start: 123,
            },
            terminal: false,
        };
        let rows = snapshot(dir.path()).unwrap();
        let mut group_closed = false;
        let mut pids: Vec<_> = tree
            .members(&rows, &HashSet::new(), &mut group_closed)
            .iter()
            .map(|p| p.id.pid)
            .collect();
        pids.sort();
        assert_eq!(pids, [101, 102, 103]);
    }

    #[test]
    fn members_emits_children_before_parents_regardless_of_snapshot_order() {
        let root = Identity {
            pid: 101,
            start: 10,
        };
        let a = Process {
            id: root,
            parent: 1,
            group: 101,
            session: 101,
            zombie: false,
        };
        let b = Process {
            id: Identity {
                pid: 102,
                start: 11,
            },
            parent: 101,
            group: 101,
            session: 101,
            zombie: false,
        };
        let c = Process {
            id: Identity {
                pid: 103,
                start: 12,
            },
            parent: 102,
            group: 101,
            session: 101,
            zombie: false,
        };
        let tree = Tree {
            root,
            terminal: false,
        };
        for rows in [
            [a, b, c],
            [a, c, b],
            [b, a, c],
            [b, c, a],
            [c, a, b],
            [c, b, a],
        ] {
            let mut group_closed = false;
            let pids: Vec<_> = tree
                .members(&rows, &HashSet::new(), &mut group_closed)
                .into_iter()
                .map(|p| p.id.pid)
                .collect();
            assert_eq!(pids, [103, 102, 101]);
        }
    }

    #[test]
    fn recycled_leader_cannot_claim_new_group_or_session() {
        let root = Identity {
            pid: 101,
            start: 123,
        };
        let rows =
            [(101, 1, 999), (102, 101, 1000), (103, 1, 110)].map(|(pid, parent, start)| Process {
                id: Identity { pid, start },
                parent,
                group: 101,
                session: 101,
                zombie: false,
            });
        for terminal in [true, false] {
            let tree = Tree { root, terminal };
            let mut group_closed = false;
            assert!(tree
                .members(&rows, &HashSet::new(), &mut group_closed)
                .is_empty());
            assert!(group_closed);
            let known = HashSet::from([rows[2].id]);
            let mut group_closed = false;
            let found = tree.members(&rows, &known, &mut group_closed);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].id.pid, 103);
            assert!(group_closed);
            // Even if the recycled leader exits leaving an orphan in group/session 101,
            // the latched closure keeps that orphan untouched.
            let after_recycled_exit = [
                Process {
                    id: Identity {
                        pid: 104,
                        start: 1001,
                    },
                    parent: 1,
                    group: 101,
                    session: 101,
                    zombie: false,
                },
                rows[2],
            ];
            let found = tree.members(&after_recycled_exit, &known, &mut group_closed);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].id.pid, 103);
        }
    }

    #[test]
    fn rejects_invalid_and_self_child_pids() {
        for pid in [0, 1, u32::MAX, std::process::id()] {
            assert_eq!(
                terminate_child_tree(pid).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                Tree::terminal(pid).err().unwrap().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn mismatched_start_time_never_signals_live_pid() {
        let mut child = Command::new("sh")
            .args(["-c", "read line; exit 17"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut row = stat(Path::new("/proc"), child.id() as i32).unwrap();
        row.id.start += 1;
        signal_member(row, Signal::SIGKILL).unwrap();
        drop(child.stdin.take());
        assert_eq!(child.wait().unwrap().code(), Some(17));
    }

    #[test]
    fn tighter_deadline_delivers_sigkill_when_blocking_pool_is_busy() {
        use std::os::unix::process::ExitStatusExt;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        rt.block_on(async {
            let mut child = Command::new("sleep").arg("10").spawn().unwrap();
            let tree = Tree::terminal(child.id()).unwrap();
            let (release, block) = std::sync::mpsc::channel();
            let (started, running) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                block.recv_timeout(Duration::from_secs(2)).ok();
            });
            running.await.unwrap();
            let start = Instant::now();
            let original = start + Duration::from_secs(3);
            let (tx, mut changes) = tokio::sync::watch::channel(Some(original));
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                tx.send_replace(Some(Instant::now()));
            });
            tree.terminate_by(original, original, None, Some(&mut changes))
                .await
                .unwrap();
            assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
            assert!(start.elapsed() < Duration::from_secs(1));
            release.send(()).unwrap();
            blocker.await.unwrap();
        });
    }

    #[tokio::test]
    async fn soft_end_past_hard_end_still_obeys_hard_deadline() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = Command::new("sh")
            .args(["-c", "trap '' TERM; echo ready; exec sleep 60"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        io::BufRead::read_line(
            &mut io::BufReader::new(child.stdout.take().unwrap()),
            &mut ready,
        )
        .unwrap();
        let tree = Tree::terminal(child.id()).unwrap();
        let start = Instant::now();
        let _ = tree
            .terminate(
                start + Duration::from_secs(5),
                start + Duration::from_millis(150),
                None,
            )
            .await
            .unwrap();
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn terminate_child_tree_escalates_and_leaves_sentinel_alive() {
        let mut sentinel = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        let sentinel_id = stat(Path::new("/proc"), sentinel.id() as i32).unwrap().id;
        let mut child = Command::new("sh")
            .args([
                "-c",
                "trap '' TERM; sh -c 'trap \"\" TERM; echo $$; exec sleep 60' & wait",
            ])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        io::BufRead::read_line(
            &mut io::BufReader::new(child.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let grandchild_pid: i32 = line.trim().parse().unwrap();
        let grandchild_id = stat(Path::new("/proc"), grandchild_pid).unwrap().id;

        let start = std::time::Instant::now();
        let result = terminate_child_tree(child.id()).unwrap();
        let elapsed = start.elapsed();
        assert!(result);
        assert!(elapsed >= Duration::from_millis(1900));
        assert!(elapsed < Duration::from_secs(3));
        let _ = child.wait().unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(1);
        while stat(Path::new("/proc"), grandchild_id.pid).is_ok_and(|p| p.id == grandchild_id)
            && std::time::Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(process_gone(grandchild_id));

        assert!(sentinel.try_wait().unwrap().is_none());
        let after = stat(Path::new("/proc"), sentinel_id.pid).unwrap();
        assert_eq!(after.id, sentinel_id);
        assert!(!after.zombie);
        let _ = sentinel.kill();
        let _ = sentinel.wait();
    }

    fn process_gone(id: Identity) -> bool {
        stat(Path::new("/proc"), id.pid).map_or(true, |p| p.id != id)
    }

    #[test]
    fn establish_wrapper_group_creates_session_when_not_group_leader() {
        const CHILD: &str = "HAPI_PROCTREE_TEST_SETSID";
        if std::env::var_os(CHILD).is_some() {
            assert_ne!(nix::unistd::getpgrp(), getpid());
            establish_wrapper_group().unwrap();
            assert_eq!(nix::unistd::getpgrp(), getpid());
            assert_eq!(nix::unistd::getsid(None).unwrap(), getpid());
            // Calling again once already leading the group is a no-op.
            establish_wrapper_group().unwrap();
            assert_eq!(nix::unistd::getpgrp(), getpid());
            return;
        }
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lifecycle::proctree::tests::establish_wrapper_group_creates_session_when_not_group_leader",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn process_group_empty_after_exit() {
        const CHILD: &str = "HAPI_PROCTREE_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let previous = nix::sys::prctl::get_child_subreaper().unwrap();
            establish_wrapper_group().unwrap();
            assert_eq!(nix::sys::prctl::get_child_subreaper().unwrap(), previous);
            let mut child = Command::new("sh")
                .args([
                    "-c",
                    "setsid sh -c 'trap \"\" TERM; echo $$; exec sleep 60' & wait",
                ])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::BufRead;
            let mut ready = String::new();
            io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            let escaped = Pid::from_raw(ready.trim().parse().unwrap());
            assert_eq!(nix::unistd::getsid(Some(escaped)).unwrap(), escaped);
            let orphan = Command::new("sh")
                .args(["-c", "sleep 60 </dev/null >/dev/null 2>&1 & echo $!"])
                .output()
                .unwrap();
            assert!(orphan.status.success());
            let orphan = Pid::from_raw(
                String::from_utf8(orphan.stdout)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap(),
            );
            assert_eq!(nix::unistd::getpgid(Some(orphan)).unwrap(), getpid());
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            assert!(runtime
                .block_on(kill_agent_tree(Instant::now() + Duration::from_millis(400)))
                .unwrap());
            child.wait().unwrap();
            for pid in [escaped, orphan] {
                let until = std::time::Instant::now() + Duration::from_secs(1);
                while kill(pid, None).is_ok() && std::time::Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert_eq!(kill(pid, None), Err(nix::errno::Errno::ESRCH));
            }
            return;
        }
        let mut wrapper = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lifecycle::proctree::tests::process_group_empty_after_exit",
            ])
            .env(CHILD, "1")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = Pid::from_raw(-(wrapper.id() as i32));
        assert!(wrapper.wait().unwrap().success());
        assert_eq!(kill(group, None), Err(nix::errno::Errno::ESRCH));
    }

    const PBT_SPEC_ENV: &str = "HAPI_PROCTREE_PBT_SPEC";

    #[derive(Clone, Debug)]
    struct TreeNode {
        mode: char,
        children: Vec<TreeNode>,
    }

    impl TreeNode {
        fn encode(&self) -> String {
            let mut out = String::new();
            out.push(self.mode);
            if !self.children.is_empty() {
                out.push('(');
                for (i, child) in self.children.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&child.encode());
                }
                out.push(')');
            }
            out
        }

        fn decode(s: &str) -> Self {
            fn parse(bytes: &[u8], pos: &mut usize) -> TreeNode {
                let mode = bytes[*pos] as char;
                *pos += 1;
                let mut children = Vec::new();
                if bytes.get(*pos) == Some(&b'(') {
                    *pos += 1;
                    loop {
                        children.push(parse(bytes, pos));
                        match bytes.get(*pos) {
                            Some(&b',') => *pos += 1,
                            Some(&b')') => {
                                *pos += 1;
                                break;
                            }
                            other => panic!("Unexpected byte {other:?} in tree spec"),
                        }
                    }
                }
                TreeNode { mode, children }
            }
            let mut pos = 0;
            let node = parse(s.as_bytes(), &mut pos);
            assert_eq!(pos, s.len());
            node
        }

        fn count(&self) -> usize {
            1 + self.children.iter().map(Self::count).sum::<usize>()
        }

        fn depth(&self) -> usize {
            1 + self.children.iter().map(Self::depth).max().unwrap_or(0)
        }
    }

    fn run_pbt_node(spec: &str) -> ! {
        use nix::sys::signal::{signal, SigHandler};
        let node = TreeNode::decode(spec);
        unsafe {
            let _ = signal(Signal::SIGTERM, SigHandler::SigDfl);
        }
        match node.mode {
            's' | 'S' => {
                let _ = nix::unistd::setsid();
            }
            'g' | 'G' => {
                let _ = nix::unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0));
            }
            _ => {}
        }
        if matches!(node.mode, 'i' | 'S' | 'G') {
            unsafe {
                let _ = signal(Signal::SIGTERM, SigHandler::SigIgn);
            }
        }
        let id = stat(Path::new("/proc"), getpid().as_raw()).unwrap().id;
        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::with_capacity(node.children.len());
        for child in &node.children {
            children.push(
                Command::new(&exe)
                    .args([
                        "--exact",
                        "--nocapture",
                        "lifecycle::proctree::tests::generated_process_trees_are_killed_before_deadline_without_touching_sentinel",
                    ])
                    .env(PBT_SPEC_ENV, child.encode())
                    .stdin(Stdio::null())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap(),
            );
        }
        let line = format!("\nPID:{}:{}\n", id.pid, id.start);
        let mut stdout = io::stdout();
        stdout.write_all(line.as_bytes()).unwrap();
        stdout.flush().unwrap();
        unsafe {
            libc::close(1);
        }
        if node.mode == 'o' {
            std::process::exit(0);
        }
        for mut child in children {
            let _ = child.wait();
        }
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    struct TreeBuilder<'a> {
        fan_out: usize,
        terminal: bool,
        budget: usize,
        cursor: usize,
        choices: &'a [u8],
    }

    impl TreeBuilder<'_> {
        fn next_byte(&mut self) -> u8 {
            let b = self.choices[self.cursor % self.choices.len()];
            self.cursor += 1;
            b
        }

        fn build_node(
            &mut self,
            remaining_depth: usize,
            is_root: bool,
            escaped_group: bool,
            orphaned: bool,
            must_hit_depth: bool,
        ) -> TreeNode {
            let will_have_children = remaining_depth > 1 && (self.budget > 0 || must_hit_depth);
            let mode = if is_root {
                if self.next_byte().is_multiple_of(2) {
                    'n'
                } else {
                    'i'
                }
            } else {
                match self.next_byte() % 6 {
                    0 => 'n',
                    1 => 'i',
                    2 if !orphaned => 's',
                    3 if !orphaned => 'S',
                    4 if self.terminal && !escaped_group => 'g',
                    5 if will_have_children && !escaped_group => 'o',
                    _ if self.terminal && !escaped_group => 'G',
                    _ => 'n',
                }
            };
            let child_escaped = escaped_group || matches!(mode, 's' | 'S');
            let child_orphaned = orphaned || mode == 'o';
            let mut children = Vec::new();
            if will_have_children {
                let desired = if must_hit_depth && is_root {
                    self.fan_out
                } else {
                    1 + (self.next_byte() as usize % self.fan_out)
                };
                let count = if must_hit_depth {
                    desired.min(self.budget.max(1))
                } else {
                    desired.min(self.budget)
                };
                self.budget = self.budget.saturating_sub(count);
                for idx in 0..count {
                    let child_must = must_hit_depth && idx == 0;
                    let child_depth = if child_must {
                        remaining_depth - 1
                    } else {
                        1 + (self.next_byte() as usize % (remaining_depth - 1))
                    };
                    children.push(self.build_node(
                        child_depth,
                        false,
                        child_escaped,
                        child_orphaned,
                        child_must,
                    ));
                }
            }
            TreeNode { mode, children }
        }
    }

    fn build_tree(depth: usize, fan_out: usize, terminal: bool, choices: &[u8]) -> TreeNode {
        let mut builder = TreeBuilder {
            fan_out,
            terminal,
            budget: 14,
            cursor: 0,
            choices,
        };
        builder.build_node(depth, true, false, false, true)
    }

    struct CleanupGuard {
        pids: Vec<Identity>,
        root: Option<std::process::Child>,
        sentinel: Option<std::process::Child>,
    }

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            for &id in &self.pids {
                if let Ok(p) = stat(Path::new("/proc"), id.pid) {
                    if p.id == id {
                        let _ = signal_member(p, Signal::SIGKILL);
                    }
                }
            }
            if let Some(mut root) = self.root.take() {
                let _ = root.kill();
                let _ = root.wait();
            }
            if let Some(mut sentinel) = self.sentinel.take() {
                let _ = sentinel.kill();
                let _ = sentinel.wait();
            }
        }
    }

    #[test]
    fn generated_process_trees_are_killed_before_deadline_without_touching_sentinel() {
        if let Ok(spec) = std::env::var(PBT_SPEC_ENV) {
            run_pbt_node(&spec);
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let exe = std::env::current_exe().unwrap();
        let strategy = (
            1usize..=4,
            1usize..=3,
            any::<bool>(),
            proptest::collection::vec(any::<u8>(), 32..=64),
        );
        let mut runner = proptest::test_runner::TestRunner::new(proptest::test_runner::Config {
            cases: 20,
            failure_persistence: None,
            ..proptest::test_runner::Config::default()
        });
        runner
            .run(&strategy, |(depth, fan_out, terminal, choices)| {
                let spec = build_tree(depth, fan_out, terminal, &choices);
                prop_assert_eq!(spec.depth(), depth);
                let expected = spec.count();

                let sentinel = Command::new("sleep")
                    .arg("60")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(0)
                    .spawn()
                    .unwrap();
                let sentinel_id = stat(Path::new("/proc"), sentinel.id() as i32).unwrap().id;
                let mut guard = CleanupGuard {
                    pids: Vec::new(),
                    root: None,
                    sentinel: Some(sentinel),
                };

                let mut cmd = Command::new(&exe);
                cmd.args([
                    "--exact",
                    "--nocapture",
                    "lifecycle::proctree::tests::generated_process_trees_are_killed_before_deadline_without_touching_sentinel",
                ])
                .env(PBT_SPEC_ENV, spec.encode())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
                if terminal {
                    unsafe {
                        cmd.pre_exec(|| {
                            nix::unistd::setsid().map(|_| ()).map_err(io::Error::from)
                        });
                    }
                } else {
                    cmd.process_group(0);
                }
                let mut root = cmd.spawn().unwrap();
                let root_pid = root.id();
                let mut root_stdout = root.stdout.take().unwrap();
                guard.root = Some(root);
                let mut raw = String::new();
                root_stdout.read_to_string(&mut raw).unwrap();
                let recorded: Vec<Identity> = raw
                    .lines()
                    .filter_map(|line| {
                        let rest = line.strip_prefix("PID:")?;
                        let (pid, start) = rest.split_once(':')?;
                        Some(Identity {
                            pid: pid.parse().ok()?,
                            start: start.parse().ok()?,
                        })
                    })
                    .collect();
                guard.pids = recorded.clone();
                prop_assert_eq!(
                    recorded.len(),
                    expected,
                    "tree spec {} reported {} pids, expected {}",
                    spec.encode(),
                    recorded.len(),
                    expected
                );

                let tree = if terminal {
                    Tree::terminal(root_pid).unwrap()
                } else {
                    Tree::new(root_pid as i32, false).unwrap()
                };
                let start = Instant::now();
                let soft_end = start + Duration::from_millis(150);
                let hard_end = start + Duration::from_secs(3);
                let killed = runtime
                    .block_on(tree.terminate(soft_end, hard_end, None))
                    .unwrap();
                let finished = Instant::now();
                prop_assert!(killed, "tree {} failed to terminate", spec.encode());
                prop_assert!(
                    finished <= hard_end,
                    "tree {} exceeded deadline by {:?}",
                    spec.encode(),
                    finished.saturating_duration_since(hard_end)
                );

                for &id in &recorded {
                    if let Ok(p) = stat(Path::new("/proc"), id.pid) {
                        prop_assert!(
                            p.id != id || p.zombie,
                            "tree {} left live pid {}",
                            spec.encode(),
                            id.pid
                        );
                    }
                }
                let _ = guard.root.take().unwrap().wait().unwrap();
                for &id in &recorded {
                    let until = std::time::Instant::now() + Duration::from_secs(1);
                    while stat(Path::new("/proc"), id.pid).is_ok_and(|p| p.id == id)
                        && std::time::Instant::now() < until
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    prop_assert!(
                        process_gone(id),
                        "tree {} pid {} still present after reap",
                        spec.encode(),
                        id.pid
                    );
                }

                let sentinel = guard.sentinel.as_mut().unwrap();
                prop_assert!(sentinel.try_wait().unwrap().is_none());
                let sentinel_after = stat(Path::new("/proc"), sentinel_id.pid).unwrap();
                prop_assert_eq!(sentinel_after.id, sentinel_id);
                prop_assert!(!sentinel_after.zombie);
                Ok(())
            })
            .unwrap();
    }
}
