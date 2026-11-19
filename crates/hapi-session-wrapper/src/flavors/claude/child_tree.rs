use std::{
    collections::HashMap,
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::Duration,
};
use tokio::{process::Child, time::Instant};

struct Member {
    pid: i32,
    fd: OwnedFd,
}
impl Member {
    fn open(pid: i32) -> io::Result<Self> {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            pid,
            fd: unsafe { OwnedFd::from_raw_fd(raw as i32) },
        })
    }

    fn alive(&self) -> bool {
        let mut p = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut p, 1, 0) == 0 }
    }

    fn signal(&self, signal: i32) {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}

fn collect(members: &mut HashMap<i32, Member>) -> Vec<i32> {
    let mut work: Vec<_> = members.keys().copied().collect();
    let mut new = Vec::new();
    while let Some(pid) = work.pop() {
        if !members[&pid].alive() {
            continue;
        }
        let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
            continue;
        };
        for task in tasks.flatten() {
            let Ok(children) = fs::read_to_string(task.path().join("children")) else {
                continue;
            };
            for child in children
                .split_whitespace()
                .filter_map(|s| s.parse::<i32>().ok())
            {
                if members.contains_key(&child) {
                    continue;
                }
                let Ok(member) = Member::open(child) else {
                    continue;
                };
                let parent = fs::read_to_string(format!("/proc/{child}/stat"))
                    .ok()
                    .and_then(|s| {
                        s.rsplit_once(") ")?
                            .1
                            .split_whitespace()
                            .nth(1)?
                            .parse::<i32>()
                            .ok()
                    });
                if parent == Some(pid) && members[&pid].alive() {
                    members.insert(child, member);
                    work.push(child);
                    new.push(child);
                }
            }
        }
    }
    new
}

pub(super) async fn stop(child: &mut Child) {
    let Some(pid) = child.id() else {
        return;
    };
    let Ok(root) = Member::open(pid as i32) else {
        let _ = child.kill().await;
        return;
    };
    let mut members = HashMap::from([(root.pid, root)]);
    let start = Instant::now();
    let mut signal = libc::SIGTERM;
    let mut first = true;
    loop {
        let new = collect(&mut members);
        if first || (signal == libc::SIGTERM && start.elapsed() >= Duration::from_secs(2)) {
            if !first {
                signal = libc::SIGKILL;
            }
            first = false;
            for m in members.values().filter(|m| m.pid != pid as i32) {
                m.signal(signal);
            }
            members[&(pid as i32)].signal(signal);
        } else {
            for id in new {
                members[&id].signal(signal);
            }
        }
        if members.values().all(|m| !m.alive()) || start.elapsed() >= Duration::from_secs(3) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = child.try_wait();
}
