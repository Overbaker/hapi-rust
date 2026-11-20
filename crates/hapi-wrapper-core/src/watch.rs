//! Directory change notification (design A18). Linux uses inotify on the
//! directory, or on its nearest existing ancestor until the directory appears;
//! other targets and inotify failures fall back to a 350 ms timer.
use std::{path::PathBuf, time::Duration};

pub const FALLBACK_POLL: Duration = Duration::from_millis(350);

pub struct DirWatch {
    dir: PathBuf,
    #[cfg(target_os = "linux")]
    state: Option<linux::Armed>,
    #[cfg(target_os = "linux")]
    failed: bool,
}

impl DirWatch {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            #[cfg(target_os = "linux")]
            state: None,
            #[cfg(target_os = "linux")]
            failed: false,
        }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Arms the watch now, so changes made before the next `changed()` call
    /// are not lost (callers re-check their condition after priming).
    pub fn prime(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if !self.failed {
                if let Err(e) = linux::prime(&self.dir, &mut self.state) {
                    self.failed = true;
                    log_fallback(&e);
                }
            }
        }
    }

    /// Resolves on the next change inside the directory (including its creation).
    pub async fn changed(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if !self.failed {
                match linux::changed(&self.dir, &mut self.state).await {
                    Ok(()) => return,
                    Err(e) => {
                        self.failed = true;
                        self.state = None;
                        log_fallback(&e);
                    }
                }
            }
        }
        tokio::time::sleep(FALLBACK_POLL).await;
    }
}

#[cfg(target_os = "linux")]
fn log_fallback(e: &std::io::Error) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "[watch] inotify unavailable ({e}); polling every {} ms",
            FALLBACK_POLL.as_millis()
        );
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
    use std::{
        io,
        os::fd::{AsFd, AsRawFd, RawFd},
        path::{Path, PathBuf},
    };
    use tokio::io::unix::AsyncFd;

    struct Fd(Inotify);
    impl AsRawFd for Fd {
        fn as_raw_fd(&self) -> RawFd {
            self.0.as_fd().as_raw_fd()
        }
    }

    pub(super) struct Armed {
        fd: AsyncFd<Fd>,
        /// The watched path: the directory itself or an ancestor.
        target: PathBuf,
    }

    fn nearest_existing(dir: &Path) -> Option<PathBuf> {
        dir.ancestors().find(|p| p.is_dir()).map(Path::to_path_buf)
    }

    fn arm(dir: &Path) -> io::Result<Armed> {
        let ino = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)?;
        loop {
            let target =
                nearest_existing(dir).ok_or_else(|| io::Error::other("no existing ancestor"))?;
            let mask = if target == dir {
                AddWatchFlags::IN_MODIFY
                    | AddWatchFlags::IN_CLOSE_WRITE
                    | AddWatchFlags::IN_CREATE
                    | AddWatchFlags::IN_DELETE
                    | AddWatchFlags::IN_MOVED_FROM
                    | AddWatchFlags::IN_MOVED_TO
                    | AddWatchFlags::IN_DELETE_SELF
                    | AddWatchFlags::IN_MOVE_SELF
            } else {
                AddWatchFlags::IN_CREATE
                    | AddWatchFlags::IN_MOVED_TO
                    | AddWatchFlags::IN_DELETE_SELF
                    | AddWatchFlags::IN_MOVE_SELF
                    | AddWatchFlags::IN_ONLYDIR
            };
            let wd = match ino.add_watch(target.as_path(), mask) {
                Ok(wd) => wd,
                // Removed between the existence check and the watch.
                Err(nix::errno::Errno::ENOENT) => continue,
                Err(e) => return Err(e.into()),
            };
            // A deeper component may have appeared before the watch existed.
            if nearest_existing(dir).as_deref() != Some(target.as_path()) {
                let _ = ino.rm_watch(wd);
                continue;
            }
            // The fd is owned by `Fd` for the lifetime of the registration.
            #[allow(deprecated)]
            let fd = AsyncFd::new(Fd(ino))?;
            return Ok(Armed { fd, target });
        }
    }

    /// Waits for events; `Ok(true)` when the watch must be re-armed.
    async fn next_events(armed: &Armed) -> io::Result<bool> {
        loop {
            let mut guard = armed.fd.readable().await?;
            match guard.get_inner().0.read_events() {
                Ok(events) if !events.is_empty() => {
                    return Ok(events.iter().any(|e| {
                        e.mask.intersects(
                            AddWatchFlags::IN_DELETE_SELF
                                | AddWatchFlags::IN_MOVE_SELF
                                | AddWatchFlags::IN_IGNORED
                                | AddWatchFlags::IN_Q_OVERFLOW,
                        )
                    }))
                }
                Ok(_) | Err(nix::errno::Errno::EAGAIN) => guard.clear_ready(),
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub(super) fn prime(dir: &Path, state: &mut Option<Armed>) -> io::Result<()> {
        if state.is_none() {
            *state = Some(arm(dir)?);
        }
        Ok(())
    }

    pub(super) async fn changed(dir: &Path, state: &mut Option<Armed>) -> io::Result<()> {
        let mut from_ancestor = false;
        loop {
            if state.is_none() {
                let armed = arm(dir)?;
                let reached = armed.target == dir;
                *state = Some(armed);
                if from_ancestor && reached {
                    return Ok(());
                }
            }
            let armed = state.as_ref().unwrap();
            let on_dir = armed.target == dir;
            let rearm = next_events(armed).await?;
            if on_dir {
                if rearm {
                    *state = None;
                }
                return Ok(());
            }
            // Watching an ancestor: move the watch downward on progress.
            let target = armed.target.clone();
            if rearm || nearest_existing(dir).as_deref() != Some(target.as_path()) {
                *state = None;
                if dir.is_dir() {
                    *state = Some(arm(dir)?);
                    return Ok(());
                }
                from_ancestor = true;
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::DirWatch;
    use std::time::Duration;

    #[tokio::test]
    async fn notices_creation_below_missing_ancestors_and_file_writes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a").join("b");
        let mut watch = DirWatch::new(dir.clone());
        let created = dir.clone();
        let maker = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            std::fs::create_dir_all(&created).unwrap();
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.is_dir() {
                watch.changed().await;
            }
        })
        .await
        .unwrap();
        maker.await.unwrap();
        let file = dir.join("transcript.jsonl");
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            std::fs::write(file, "{}\n").unwrap();
        });
        tokio::time::timeout(Duration::from_secs(5), watch.changed())
            .await
            .unwrap();
        writer.await.unwrap();
    }
}
