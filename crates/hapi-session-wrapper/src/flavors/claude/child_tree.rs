use hapi_wrapper_core::lifecycle::proctree;
use std::{io, path::Path, time::Duration};
use tokio::process::{Child, Command};

pub(super) fn configure(command: &mut Command) {
    if !Path::new("/proc/self/stat").is_file() {
        command.process_group(0);
    }
}

fn stop_group(pid: i32) {
    if unsafe { libc::getpgid(pid) } != pid {
        return;
    }
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    std::thread::sleep(Duration::from_secs(2));
    unsafe { libc::kill(-pid, libc::SIGKILL) };
}

pub(super) async fn stop(child: &mut Child) -> io::Result<()> {
    let Some(pid) = child.id() else { return Ok(()) };
    let private_group = !Path::new("/proc/self/stat").is_file();
    let stopped = tokio::task::spawn_blocking(move || -> io::Result<bool> {
        if private_group && unsafe { libc::getpgid(pid as i32) } == pid as i32 {
            stop_group(pid as i32);
            return Ok(true);
        }
        proctree::terminate_child_tree(pid)
    })
    .await;
    if !matches!(stopped, Ok(Ok(true))) {
        let _ = child.start_kill();
    }
    tokio::time::timeout(Duration::from_secs(1), child.wait()).await??;
    if !stopped.map_err(io::Error::other)?? {
        return Err(io::Error::other("Could not confirm Claude tool tree exit"));
    }
    if private_group
        && (unsafe { libc::kill(-(pid as i32), 0) } == 0
            || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH))
    {
        return Err(io::Error::other(
            "Could not confirm Claude process group exit",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn escalates_tree_without_blocking_runtime() {
        let mut child = Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 40 & echo $!; wait"])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let pid = lines.next_line().await.unwrap().unwrap();
        let mut stop = Box::pin(stop(&mut child));
        let start = std::time::Instant::now();
        tokio::select! {
            _ = &mut stop => panic!("missing escalation"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        assert!(start.elapsed() < Duration::from_secs(1));
        stop.await.unwrap();
        assert!(child.id().is_none());
        assert!(std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_or(true, |s| s.contains(") Z ")));
    }

    #[tokio::test]
    async fn fallback_signals_a_private_group() {
        let mut child = Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 40 & echo ready; wait"])
            .process_group(0)
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("ready"));
        let pid = child.id().unwrap();
        tokio::task::spawn_blocking(move || stop_group(pid as i32))
            .await
            .unwrap();
        let status = child.wait().await.unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
    }
}
