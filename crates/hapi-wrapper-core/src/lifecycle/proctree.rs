//! Kernel parentage enforces wrapper ownership. Linux makes the wrapper a subreaper,
//! so orphaned descendants, including double-forked daemons, return to it.
//! Spawn each tool in a private process group for a single group SIGTERM.
//! At the soft deadline, kill surviving groups and descendants. Walk only the
//! wrapper's task children, reap exited children, and stop at the hard deadline.
//! After adoption and setsid, the kernel no longer identifies the original tool:
//! wrapper shutdown collects such daemons even when per-tool cleanup cannot.

use std::io;
use tokio::{process::Command, time::Instant};

pub fn configure_child_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    let _ = command;
}

#[cfg(unix)]
mod platform {
    use super::*;
    use nix::{
        errno::Errno,
        sys::{
            signal::{kill, Signal},
            wait::{waitpid, WaitPidFlag},
        },
        unistd::{getpgrp, getpid, setsid, Pid},
    };
    use std::{collections::HashSet, time::Duration};
    use tokio::sync::watch;

    pub fn establish_wrapper_group() -> io::Result<()> {
        #[cfg(target_os = "linux")]
        nix::sys::prctl::set_child_subreaper(true)?;
        if getpgrp() != getpid() {
            setsid()?;
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn children(pid: Pid) -> io::Result<Vec<Pid>> {
        use std::fs;
        let mut children = Vec::new();
        let tasks = fs::read_dir(format!("/proc/{pid}/task"))?;
        for task in tasks {
            let file = task?.path().join("children");
            match fs::read_to_string(file) {
                Ok(list) => {
                    for raw in list.split_whitespace() {
                        children.push(Pid::from_raw(raw.parse().map_err(io::Error::other)?));
                    }
                }
                Err(e) if vanished(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(children)
    }

    #[cfg(target_os = "linux")]
    fn vanished(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(nix::libc::ENOENT | nix::libc::ESRCH))
    }

    #[derive(Clone, Copy)]
    pub(crate) struct Tree(Option<Pid>);

    impl Tree {
        pub(crate) fn terminal(pid: u32) -> io::Result<Self> {
            let raw = i32::try_from(pid)
                .ok()
                .filter(|p| *p > 1 && *p != getpid().as_raw())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid child PID"))?;
            Ok(Self(Some(Pid::from_raw(raw))))
        }

        fn members(self, known: &HashSet<Pid>) -> io::Result<Vec<(Pid, Pid)>> {
            let mut members = Vec::new();
            #[cfg(target_os = "linux")]
            {
                use nix::unistd::{getpgid, getsid};
                let mut pending = vec![(getpid(), self.0.is_none())];
                let mut visited = HashSet::new();
                while let Some((parent, selected)) = pending.pop() {
                    if !visited.insert(parent) {
                        continue;
                    }
                    let kids = match children(parent) {
                        Ok(kids) => kids,
                        Err(e) if parent != getpid() && vanished(&e) => continue,
                        Err(e) => return Err(e),
                    };
                    for pid in kids {
                        let group = match getpgid(Some(pid)) {
                            Ok(group) => group,
                            Err(Errno::ESRCH) => continue,
                            Err(e) => return Err(e.into()),
                        };
                        let owned = selected
                            || known.contains(&pid)
                            || Some(pid) == self.0
                            || Some(group) == self.0
                            || self.0.is_some_and(|root| getsid(Some(pid)) == Ok(root));
                        pending.push((pid, owned));
                        if owned {
                            members.push((pid, group));
                        }
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let output = std::process::Command::new("ps")
                    .args(["-A", "-o", "pid=,ppid=,pgid="])
                    .output()?;
                if !output.status.success() {
                    return Err(io::Error::other("ps failed"));
                }
                let rows: Vec<_> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| {
                        let mut ids = line
                            .split_whitespace()
                            .map(|v| v.parse::<i32>().ok().map(Pid::from_raw));
                        Some((ids.next()??, ids.next()??, ids.next()??))
                    })
                    .collect();
                let groups: HashSet<_> = rows
                    .iter()
                    .filter(|(pid, parent, group)| {
                        *parent == getpid()
                            && pid == group
                            && self.0.is_none_or(|root| root == *pid)
                    })
                    .map(|row| row.2)
                    .collect();
                for (pid, _, group) in rows {
                    if groups.contains(&group) || known.contains(&group) {
                        members.push((pid, group));
                    }
                }
            }
            Ok(members)
        }

        pub(crate) async fn terminate(
            self,
            soft: Instant,
            hard: Instant,
            shutdown: Option<&watch::Receiver<bool>>,
        ) -> io::Result<bool> {
            let mut sweep = Sweep::new(self, soft, hard);
            loop {
                if shutdown.is_some_and(|rx| *rx.borrow()) {
                    sweep.soft = Instant::now();
                    sweep.hard = sweep.hard.min(sweep.soft + Duration::from_millis(250));
                }
                match sweep.step()? {
                    Some(done) => return Ok(done),
                    None => tokio::time::sleep_until(sweep.next()).await,
                }
            }
        }
    }

    struct Sweep {
        tree: Tree,
        known: HashSet<Pid>,
        warned: HashSet<i32>,
        soft: Instant,
        hard: Instant,
    }

    impl Sweep {
        fn new(tree: Tree, soft: Instant, hard: Instant) -> Self {
            Self {
                tree,
                known: HashSet::new(),
                warned: HashSet::new(),
                soft,
                hard,
            }
        }

        fn next(&self) -> Instant {
            let now = Instant::now();
            (now + Duration::from_millis(10)).min(if now < self.soft {
                self.soft
            } else {
                self.hard
            })
        }

        fn step(&mut self) -> io::Result<Option<bool>> {
            let members = self.tree.members(&self.known)?;
            if members.is_empty() {
                return Ok(Some(true));
            }
            self.known.extend(members.iter().map(|m| m.0));
            let hard = Instant::now() >= self.soft;
            let mut targets = HashSet::new();
            for (pid, group) in members {
                #[cfg(all(target_os = "linux", not(target_env = "uclibc")))]
                if Some(pid) == self.tree.0 {
                    use nix::sys::wait::{waitid, Id, WaitStatus};
                    let flags = WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT | WaitPidFlag::WEXITED;
                    match waitid(Id::Pid(pid), flags) {
                        Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) => continue,
                        Ok(_) | Err(Errno::ECHILD | Errno::EINTR) => (),
                        Err(e) => return Err(e.into()),
                    }
                }
                #[cfg(all(target_os = "linux", not(target_env = "uclibc")))]
                let preserve_status = Some(pid) == self.tree.0;
                #[cfg(not(all(target_os = "linux", not(target_env = "uclibc"))))]
                let preserve_status = false;
                if !preserve_status {
                    match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                        Ok(_) | Err(Errno::ECHILD | Errno::EINTR) => (),
                        Err(e) => return Err(e.into()),
                    }
                }
                let target = if group != getpgrp() && self.known.contains(&group) {
                    -group.as_raw()
                } else {
                    pid.as_raw()
                };
                targets.insert(target);
                if hard {
                    targets.insert(pid.as_raw());
                }
            }
            if targets.is_empty() {
                return Ok(Some(true));
            }
            for target in targets {
                if hard || self.warned.insert(target) {
                    let signal = if hard {
                        Signal::SIGKILL
                    } else {
                        Signal::SIGTERM
                    };
                    match kill(Pid::from_raw(target), signal) {
                        Ok(()) | Err(Errno::ESRCH) => (),
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            Ok((Instant::now() >= self.hard).then_some(false))
        }
    }

    pub fn terminate_child_tree(pid: u32) -> io::Result<bool> {
        let now = Instant::now();
        let mut sweep = Sweep::new(
            Tree::terminal(pid)?,
            now + Duration::from_secs(2),
            now + Duration::from_secs(3),
        );
        loop {
            if let Some(done) = sweep.step()? {
                return Ok(done);
            }
            std::thread::sleep(sweep.next().saturating_duration_since(Instant::now()));
        }
    }

    pub(in crate::lifecycle) async fn kill_agent_tree_by(
        deadline: Instant,
        changes: Option<&mut watch::Receiver<Option<Instant>>>,
    ) -> io::Result<bool> {
        let deadline = changes
            .and_then(|rx| *rx.borrow())
            .map_or(deadline, |cap| cap.min(deadline));
        let now = Instant::now();
        let soft =
            now + Duration::from_secs(2).min(deadline.saturating_duration_since(now) * 2 / 3);
        Tree(None).terminate(soft, deadline, None).await
    }

    pub async fn kill_agent_tree(deadline: Instant) -> io::Result<bool> {
        kill_agent_tree_by(deadline, None).await
    }
}

#[cfg(not(unix))]
mod platform {
    use super::*;
    pub fn establish_wrapper_group() -> io::Result<()> {
        Ok(())
    }
    pub fn terminate_child_tree(_: u32) -> io::Result<bool> {
        Ok(true)
    }
    pub async fn kill_agent_tree(_: Instant) -> io::Result<bool> {
        Ok(true)
    }
}

#[cfg(unix)]
#[allow(unused_imports)]
pub(super) use platform::kill_agent_tree_by;
#[cfg(unix)]
#[allow(unused_imports)]
pub(crate) use platform::Tree;
pub use platform::{establish_wrapper_group, kill_agent_tree, terminate_child_tree};

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    use std::{
        io::{BufRead, BufReader},
        process::{Command as StdCommand, Stdio},
        time::Duration,
    };
    use tokio::io::{AsyncBufReadExt, BufReader as AsyncReader};

    #[test]
    fn linux_process_trees() {
        const INNER: &str = "HAPI_PROCTREE_INNER";
        if std::env::var_os(INNER).is_none() {
            let mut sentinel = StdCommand::new("sleep").arg("60").spawn().unwrap();
            let status = StdCommand::new(std::env::current_exe().unwrap())
                .args(["--exact", "lifecycle::proctree::tests::linux_process_trees"])
                .env(INNER, sentinel.id().to_string())
                .status()
                .unwrap();
            let alive = sentinel.try_wait().unwrap().is_none();
            sentinel.kill().unwrap();
            sentinel.wait().unwrap();
            assert!(status.success() && alive);
            return;
        }

        establish_wrapper_group().unwrap();
        let sentinel = std::env::var(INNER).unwrap().parse().unwrap();
        assert!(terminate_child_tree(sentinel).unwrap());
        let mut sibling = StdCommand::new("sleep").arg("60").spawn().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "sh -c 'sleep 60 & echo $$ $!; wait' & wait"])
                .stdout(Stdio::piped());
            configure_child_group(&mut cmd);
            let mut root = cmd.spawn().unwrap();
            let pid = root.id().unwrap();
            let ids = AsyncReader::new(root.stdout.take().unwrap())
                .lines()
                .next_line()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                nix::unistd::getpgid(Some(Pid::from_raw(pid as i32)))
                    .unwrap()
                    .as_raw(),
                pid as i32
            );
            assert!(terminate_child_tree(pid).unwrap());
            root.wait().await.unwrap();
            for id in ids.split_whitespace() {
                assert_eq!(
                    kill(Pid::from_raw(id.parse().unwrap()), None),
                    Err(Errno::ESRCH)
                );
            }
        });
        assert!(sibling.try_wait().unwrap().is_none());
        sibling.kill().unwrap();
        sibling.wait().unwrap();

        let mut leader = StdCommand::new("sh")
            .args([
                "-c",
                "setsid sh -c 'sh -c \"trap \\\"\\\" TERM; echo \\$\\$; exec sleep 60\" &' & wait",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let daemon = Pid::from_raw(ready.trim().parse().unwrap());
        assert!(leader.wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(format!("/proc/{daemon}/status"))
                .unwrap()
                .lines()
                .find(|l| l.starts_with("PPid:"))
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap(),
            std::process::id().to_string()
        );
        let start = Instant::now();
        let deadline = start + Duration::from_millis(900);
        assert!(runtime.block_on(kill_agent_tree(deadline)).unwrap());
        assert!(Instant::now() <= deadline);
        assert!(start.elapsed() >= Duration::from_millis(600));
        assert_eq!(kill(daemon, None), Err(Errno::ESRCH));
    }
}
