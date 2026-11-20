use nix::{
    libc,
    sys::signal::Signal,
    unistd::{getpgrp, getpid, setsid},
};
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    time::Duration,
};
use tokio::time::Instant;

pub fn establish_wrapper_group() -> io::Result<()> {
    if getpgrp() != getpid() {
        setsid()?;
    }
    if getpgrp() != getpid() {
        return Err(io::Error::other("Wrapper does not lead its process group"));
    }
    Ok(())
}

#[derive(Copy, Clone, Hash, Eq, PartialEq)]
struct Identity {
    pid: i32,
    start: u64,
}
#[derive(Clone, Copy)]
struct Process {
    id: Identity,
    parent: i32,
    group: i32,
    session: i32,
    zombie: bool,
}

fn skipped(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOENT | libc::ESRCH | libc::EACCES | libc::EPERM)
    )
}
fn stat(path: &Path, pid: i32) -> io::Result<Process> {
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
fn snapshot(path: &Path) -> io::Result<Vec<Process>> {
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
pub(crate) struct Tree {
    root: Identity,
    terminal: bool,
}
impl Tree {
    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) fn terminal(pid: u32) -> io::Result<Self> {
        Self::new(pid as i32, true)
    }
    fn new(pid: i32, terminal: bool) -> io::Result<Self> {
        Ok(Self {
            root: stat(Path::new("/proc"), pid)?.id,
            terminal,
        })
    }
    fn members(&self, rows: &[Process], known: &HashSet<Identity>) -> Vec<Process> {
        let mut children: HashMap<i32, Vec<Process>> = HashMap::new();
        let mut pending = Vec::new();
        let reused = rows
            .iter()
            .any(|p| p.id.pid == self.root.pid && p.id != self.root);
        for &p in rows {
            children.entry(p.parent).or_default().push(p);
            let in_group = if self.terminal {
                p.session == self.root.pid
            } else {
                p.group == self.root.pid
            };
            if p.id == self.root || known.contains(&p.id) || (!reused && in_group) {
                pending.push((p, false));
            }
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
                pending.extend(kids.into_iter().map(|kid| (kid, false)));
            }
        }
        out
    }
}

fn signal_member(p: Process, sig: Signal) -> io::Result<()> {
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, p.id.pid, 0) };
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
            sig as i32,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[derive(Default)]
struct Sweep {
    known: HashSet<Identity>,
    sent: HashSet<Identity>,
    failed: bool,
}
impl Sweep {
    fn apply(
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
impl Tree {
    #[expect(dead_code)]
    pub(crate) async fn terminate(
        self,
        soft_end: Instant,
        hard_end: Instant,
        shutdown: Option<&tokio::sync::watch::Receiver<bool>>,
    ) -> io::Result<bool> {
        self.terminate_by(soft_end, hard_end, shutdown, None).await
    }

    async fn terminate_by(
        self,
        mut soft_end: Instant,
        mut hard_end: Instant,
        shutdown: Option<&tokio::sync::watch::Receiver<bool>>,
        mut changes: Option<&mut super::Deadline>,
    ) -> io::Result<bool> {
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
                match super::before(hard_end, changes.as_deref_mut(), scan).await {
                    Some(result) => (result?, false),
                    None => (snapshot(Path::new("/proc"))?, true),
                }
            };
            if final_pass || (!hard && Instant::now() >= soft_end) {
                sweep.sent.clear();
                hard = true;
            }
            let members = self.members(&rows, &sweep.known);
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
            let next = (Instant::now() + Duration::from_millis(20)).min(if hard {
                hard_end
            } else {
                soft_end
            });
            super::before(next, changes.as_deref_mut(), std::future::pending::<()>()).await;
        }
    }
}
pub fn terminate_child_tree(pid: u32) -> io::Result<bool> {
    let tree = Tree::new(pid as i32, false)?;
    let mut sweep = Sweep::default();
    let start = std::time::Instant::now();
    let mut signal = Signal::SIGTERM;
    loop {
        let rows = snapshot(Path::new("/proc"))?;
        if signal == Signal::SIGTERM && start.elapsed() >= Duration::from_secs(2) {
            signal = Signal::SIGKILL;
            sweep.sent.clear();
        }
        if sweep.apply(tree.members(&rows, &sweep.known), |p| {
            signal_member(p, signal)
        }) {
            return Ok(!sweep.failed);
        }
        if start.elapsed() >= Duration::from_secs(3) {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub async fn kill_agent_tree(deadline: Instant) -> io::Result<bool> {
    kill_agent_tree_by(deadline, None).await
}
pub(super) async fn kill_agent_tree_by(
    deadline: Instant,
    changes: Option<&mut super::Deadline>,
) -> io::Result<bool> {
    let now = Instant::now();
    let tree = Tree::new(getpid().as_raw(), false)?;
    let soft = now + Duration::from_secs(2).min(deadline.saturating_duration_since(now) * 2 / 3);
    tree.terminate_by(
        soft,
        deadline.min(soft + Duration::from_secs(1)),
        None,
        changes,
    )
    .await
}
#[cfg(test)]
mod tests {
    use super::*;
    use nix::{sys::signal::kill, unistd::Pid};
    use std::process::{Command, Stdio};
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
        let mut pids: Vec<_> = tree
            .members(&rows, &HashSet::new())
            .iter()
            .map(|p| p.id.pid)
            .collect();
        pids.sort();
        assert_eq!(pids, [101, 102, 103]);
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
            assert!(tree.members(&rows, &HashSet::new()).is_empty());
            let known = HashSet::from([rows[2].id]);
            let found = tree.members(&rows, &known);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].id.pid, 103);
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
        use std::os::unix::process::CommandExt;
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
}
