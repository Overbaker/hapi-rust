use nix::{
    libc,
    sys::{
        prctl::set_child_subreaper,
        signal::Signal,
        wait::{waitpid, WaitPidFlag},
    },
    unistd::{getpgrp, getpid, setsid, Pid},
};
use std::{
    collections::HashSet,
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::Duration,
};
use tokio::time::{sleep_until, Instant};

pub fn establish_wrapper_group() -> io::Result<()> {
    if getpgrp() != getpid() {
        setsid()?;
    }
    if getpgrp() != getpid() {
        return Err(io::Error::other("Wrapper does not lead its process group"));
    }
    set_child_subreaper(true)?;
    Ok(())
}
struct Member {
    pid: Pid,
    fd: OwnedFd,
    start: u64,
}
fn children(parent: i32, seen: &mut HashSet<i32>, out: &mut Vec<Member>) -> io::Result<()> {
    let tasks = match fs::read_dir(format!("/proc/{parent}/task")) {
        Ok(tasks) => tasks,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for task in tasks {
        let task = task?;
        let list = match fs::read_to_string(task.path().join("children")) {
            Ok(l) => l,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for id in list.split_whitespace() {
            let pid: i32 = id.parse().map_err(io::Error::other)?;
            if pid <= 1 || !seen.insert(pid) {
                continue;
            }
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if raw < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(e);
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
            let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(v) => v,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let mut fields = stat
                .rsplit_once(')')
                .ok_or_else(|| io::Error::other("Invalid process stat"))?
                .1
                .split_whitespace();
            if fields.nth(1).and_then(|s| s.parse().ok()) != Some(parent) {
                continue;
            }
            let start = fields
                .nth(17)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| io::Error::other("Missing process start time"))?;
            children(pid, seen, out)?;
            out.push(Member {
                pid: Pid::from_raw(pid),
                fd,
                start,
            });
        }
    }
    Ok(())
}
fn sweep(sig: Signal, sent_to: &mut HashSet<(Pid, u64)>) -> io::Result<bool> {
    let mut members = Vec::new();
    children(getpid().as_raw(), &mut HashSet::new(), &mut members)?;
    for member in &members {
        if sent_to.insert((member.pid, member.start)) {
            let sent = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    member.fd.as_raw_fd(),
                    sig as i32,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if sent < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::ESRCH) {
                    return Err(e);
                }
            }
        }
        let _ = waitpid(member.pid, Some(WaitPidFlag::WNOHANG));
    }
    Ok(members.is_empty())
}
pub async fn kill_agent_tree(deadline: Instant) -> io::Result<bool> {
    let start = Instant::now();
    let remaining = deadline.saturating_duration_since(start);
    let soft_end = start + Duration::from_secs(2).min(remaining * 2 / 3);
    let hard_end = deadline.min(soft_end + Duration::from_secs(1));
    let mut sent_to = HashSet::new();
    let mut hard = false;
    loop {
        let now = Instant::now();
        let signal = if now < soft_end {
            Signal::SIGTERM
        } else {
            Signal::SIGKILL
        };
        if signal == Signal::SIGKILL && !hard {
            sent_to.clear();
            hard = true;
        }
        let (result, seen) =
            tokio::task::spawn_blocking(move || (sweep(signal, &mut sent_to), sent_to)).await?;
        sent_to = seen;
        if result? {
            return Ok(true);
        }
        if Instant::now() >= hard_end {
            return Ok(false);
        }
        sleep_until((Instant::now() + Duration::from_millis(20)).min(
            if signal == Signal::SIGTERM {
                soft_end
            } else {
                hard_end
            },
        ))
        .await;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::kill;
    use std::process::{Command, Stdio};
    #[test]
    fn process_group_empty_after_exit() {
        const CHILD: &str = "HAPI_PROCTREE_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            establish_wrapper_group().unwrap();
            let mut child = Command::new("sh")
                .args(["-c", "trap '' TERM; sleep 60 & echo ready; wait"])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::BufRead;
            let mut ready = String::new();
            io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            assert_eq!(ready, "ready\n");
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            assert!(runtime
                .block_on(kill_agent_tree(Instant::now() + Duration::from_millis(400)))
                .unwrap());
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
