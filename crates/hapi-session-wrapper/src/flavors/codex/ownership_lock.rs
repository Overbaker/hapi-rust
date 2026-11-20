use std::{
    fs::{self, File},
    future::Future,
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::{sleep, Instant};
const STALE: Duration = Duration::from_secs(30);
const UPDATE: Duration = Duration::from_secs(10);
struct Lease {
    path: PathBuf,
    #[cfg(unix)]
    dir: File,
    #[cfg(windows)]
    created: SystemTime,
    stamp: u128,
    seconds: bool,
    released: bool,
}
fn millis(time: SystemTime) -> io::Result<u128> {
    Ok(time
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis())
}
fn stamp(dir: &File) -> io::Result<u128> {
    millis(dir.metadata()?.modified()?)
}
fn set_mtime(dir: &File, ms: u128) -> io::Result<()> {
    let time = UNIX_EPOCH + Duration::from_millis(ms as u64);
    dir.set_times(fs::FileTimes::new().set_accessed(time).set_modified(time))
}
#[cfg(windows)]
fn directory_handle(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .access_mode(0x0180)
        .custom_flags(0x02000000)
        .open(path)
}
impl Lease {
    fn create(path: &Path) -> io::Result<Self> {
        fs::create_dir(path)?;
        let init = || {
            #[cfg(unix)]
            let dir = File::open(path)?;
            #[cfg(windows)]
            let dir = directory_handle(path)?;
            set_mtime(&dir, millis(SystemTime::now())?.div_ceil(1000) * 1000 + 5)?;
            let stamp = stamp(&dir)?;
            Ok(Self {
                #[cfg(windows)]
                created: dir.metadata()?.created()?,
                #[cfg(unix)]
                dir,
                path: path.into(),
                stamp,
                seconds: stamp % 1000 == 0,
                released: false,
            })
        };
        init().inspect_err(|_| {
            let _ = fs::remove_dir(path);
        })
    }
    fn attempt(path: &Path) -> io::Result<Self> {
        match Self::create(path) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            other => return other,
        }
        match fs::metadata(path) {
            Ok(m) if millis(m.modified()?)? + STALE.as_millis() < millis(SystemTime::now())? => {
                match fs::remove_dir(path) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Lock file is already being held",
                ))
            }
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        Self::create(path).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                io::Error::new(io::ErrorKind::WouldBlock, "Lock file is already being held")
            } else {
                e
            }
        })
    }
    fn owns(&self) -> io::Result<bool> {
        let path = fs::metadata(&self.path)?;
        #[cfg(windows)]
        return Ok(path.created()? == self.created && millis(path.modified()?)? == self.stamp);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let dir = self.dir.metadata()?;
            Ok(path.ino() == dir.ino()
                && path.dev() == dir.dev()
                && millis(path.modified()?)? == self.stamp)
        }
    }
    fn release(&mut self) -> io::Result<()> {
        self.released = true;
        if self.owns()? {
            fs::remove_dir(&self.path)
        } else {
            Err(io::Error::other("Lock is no longer owned"))
        }
    }
    fn refresh(&mut self) -> io::Result<()> {
        if !self.owns()? {
            self.released = true;
            return Err(io::Error::other(
                "Unable to update lock within the stale threshold",
            ));
        }
        let now = millis(SystemTime::now())?;
        let next = if self.seconds {
            now.div_ceil(1000) * 1000
        } else {
            now
        };
        #[cfg(unix)]
        set_mtime(&self.dir, next)?;
        #[cfg(windows)]
        set_mtime(&directory_handle(&self.path)?, next)?;
        self.stamp = next;
        Ok(())
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.release();
        }
    }
}

pub async fn with_lock<T>(
    directory: &Path,
    work: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    super::registry::mkdir(directory)?;
    let path = directory.join("ownership.hapi.lock");
    let mut retries = 50;
    let lease = loop {
        match Lease::attempt(&path) {
            Ok(l) => break l,
            Err(e) if retries == 0 => return Err(e),
            Err(_) => {
                retries -= 1;
                sleep(Duration::from_millis(100)).await;
            }
        }
    };
    hold(lease, work, Lease::refresh, SystemTime::now).await
}

async fn hold<T>(
    mut lease: Lease,
    work: impl Future<Output = io::Result<T>>,
    mut refresh: impl FnMut(&mut Lease) -> io::Result<()>,
    wall: impl Fn() -> SystemTime,
) -> io::Result<T> {
    let mut delay = UPDATE;
    let mut last = Instant::now();
    let mut last_wall = wall();
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            _=sleep(delay)=> {
                match refresh(&mut lease){
                    Ok(())=>{last=Instant::now();last_wall=wall();delay=UPDATE;}
                    Err(e)=>{
                        let expired=last.elapsed()>=STALE || wall().duration_since(last_wall).is_ok_and(|age|age>=STALE);
                        if lease.released || expired || e.kind()==io::ErrorKind::NotFound {
                            lease.released=true;
                            return Err(e);
                        }
                        delay=Duration::from_secs(1);
                    }
                }
            }
            result=&mut work=>{lease.release()?;return result;}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        process::Command,
    };
    #[tokio::test]
    async fn contention_between_typescript_and_rust() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let spawn = || {
            Command::new("bun")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/src/flavors/codex/lock_holder.ts"
                ))
                .arg(dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap()
        };
        let mut ts = spawn();
        let mut lines = BufReader::new(ts.stdout.take().unwrap()).lines();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("waiting"));
        ts.stdin
            .as_mut()
            .unwrap()
            .write_all(b"start\n")
            .await
            .unwrap();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("held"));
        let acquire = with_lock(dir, async { Ok(()) });
        tokio::pin!(acquire);
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut acquire)
                .await
                .is_err()
        );
        ts.stdin
            .take()
            .unwrap()
            .write_all(b"release\n")
            .await
            .unwrap();
        assert!(ts.wait().await.unwrap().success());
        acquire.await.unwrap();
        let mut ts = spawn();
        let mut lines = BufReader::new(ts.stdout.take().unwrap()).lines();
        with_lock(dir, async {
            assert_eq!(lines.next_line().await?.as_deref(), Some("waiting"));
            ts.stdin.as_mut().unwrap().write_all(b"start\n").await?;
            assert!(
                tokio::time::timeout(Duration::from_millis(250), lines.next_line())
                    .await
                    .is_err()
            );
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("held"));
        ts.stdin
            .take()
            .unwrap()
            .write_all(b"release\n")
            .await
            .unwrap();
        assert!(ts.wait().await.unwrap().success());
    }
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn stale_refresh_and_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ownership.hapi.lock");
        fs::create_dir(&path).unwrap();
        set_mtime(&File::open(&path).unwrap(), 1234).unwrap();
        let mut lease = Lease::attempt(&path).unwrap();
        set_mtime(&lease.dir, 1_000).unwrap();
        lease.stamp = stamp(&lease.dir).unwrap();
        let before = lease.stamp;
        hold(
            lease,
            async {
                sleep(UPDATE + Duration::from_secs(1)).await;
                assert!(stamp(&File::open(&path)?)? > before);
                Ok(())
            },
            Lease::refresh,
            SystemTime::now,
        )
        .await
        .unwrap();
        let lost = with_lock(tmp.path(), async {
            fs::remove_dir(&path)?;
            fs::create_dir(&path)?;
            sleep(STALE).await;
            Ok(())
        })
        .await;
        assert!(lost.is_err());
        assert!(path.is_dir());
    }
}

#[cfg(test)]
#[tokio::test(start_paused = true)]
async fn retry_and_compromise_boundaries() {
    use std::{cell::Cell, future::pending};
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("ownership.hapi.lock");
    let lease = Lease::create(&path).unwrap();
    let start = Instant::now();
    let e = with_lock::<()>(tmp.path(), async {
        panic!("second owner acquired live lease")
    })
    .await
    .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    fs::remove_dir(&path).unwrap();
    let e = hold(
        lease,
        pending::<io::Result<()>>(),
        Lease::refresh,
        SystemTime::now,
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
    for suspended in [false, true] {
        let lease = Lease::create(&path).unwrap();
        let start = Instant::now();
        let calls = Cell::new(0u32);
        let wall = || {
            UNIX_EPOCH
                + if suspended && calls.get() > 0 {
                    STALE + UPDATE
                } else {
                    Duration::ZERO
                }
        };
        let e = hold(
            lease,
            pending::<io::Result<()>>(),
            |_| {
                calls.set(calls.get() + 1);
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
            wall,
        )
        .await
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(start.elapsed(), if suspended { UPDATE } else { STALE });
        assert_eq!(calls.get(), if suspended { 1 } else { 21 });
        assert!(path.is_dir(), "a compromised lease must not rmdir on drop");
        fs::remove_dir(&path).unwrap();
    }
}
