use hapi_wrapper_core::lifecycle::proctree;
use std::{io, time::Duration};
use tokio::process::Child;

pub(super) async fn stop(child: &mut Child) -> io::Result<()> {
    let Some(pid) = child.id() else { return Ok(()) };
    let stopped = tokio::task::spawn_blocking(move || proctree::terminate_child_tree(pid)).await;
    if !matches!(stopped, Ok(Ok(true))) {
        let _ = child.start_kill();
    }
    tokio::time::timeout(Duration::from_secs(1), child.wait()).await??;
    if !stopped.map_err(io::Error::other)?? {
        return Err(io::Error::other("Could not confirm Claude tool tree exit"));
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
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "trap '' TERM; sleep 40 & echo $!; wait"])
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        let mut child = proctree::spawn_child(&mut command).unwrap();
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
}
